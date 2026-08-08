//! Starting the relay, running it, and stopping it again.
//!
//! What `MQTTRelay` in `main.py` did. The startup order is the load-bearing
//! part and is unchanged, for the reason the Python carried in a comment:
//! websocket first so writes have somewhere to go, then the whitelist sync, and
//! only then MQTT. Subscribing with an empty whitelist while
//! `sync_with_miniserver` is on would drop everything until the sync finished,
//! and the UDP bind is awaited rather than spawned because a failed bind has to
//! abort the start instead of vanishing into a discarded task.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use log::{error, info, warn};
use tokio::sync::watch;

use crate::config::{ConfigSection, ConfigStore, ListMode};
use crate::config::value::CfgValue;
use crate::control::{ControlSink, NativeControlSink};
use crate::error::RelayError;
use crate::miniserver::{LoxEgress, MiniserverClient};
use crate::mqtt::MqttClient;
use crate::process::{Core, CoreConfig, MqttTopics};
use crate::signals::{Signals, StopReason};
use crate::udp::UdpServer;
use crate::whitelist::{self, Endpoint};

/// What the caller should do once the relay has stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Normal,
    Restart,
}

/// The relay: four components, the configuration they were built from, and the
/// signals that stop them.
pub struct Relay {
    config: Arc<ConfigStore>,
    mqtt: MqttClient,
    miniserver: MiniserverClient,
    udp: UdpServer,
    core: Arc<Core<LoxEgress>>,
    topics: MqttTopics,
    signals: Signals,
    udp_running: AtomicBool,
    shutdown_done: AtomicBool,
}

impl Relay {
    /// Build every component from the loaded configuration.
    ///
    /// Ordered the way `MQTTRelay.__init__` was, and for the same reason: both
    /// clients own connection state the core shares, so they have to exist
    /// before it does.
    pub fn build(config: Arc<ConfigStore>, signals: Signals) -> Result<Self, RelayError> {
        let snapshot = config.snapshot();
        let topics = MqttTopics::from_base(&snapshot.general.base_topic);

        let mqtt = MqttClient::build(&snapshot);
        let miniserver = MiniserverClient::build(&snapshot);
        let udp = UdpServer::build(&snapshot, mqtt.shared());

        let core = Core::new(
            CoreConfig {
                topics: topics.clone(),
                subscription_filters: snapshot.topics.subscription_filters.clone(),
                do_not_forward: snapshot.topics.do_not_forward.clone(),
                topic_whitelist: snapshot.topics.topic_whitelist.iter().cloned().collect(),
                // Sync on => the Miniserver's virtual inputs are the allow-list.
                // An empty set must then block, not open the floodgates.
                whitelist_required: snapshot.miniserver.sync_with_miniserver,
                cache_size: usize::try_from(snapshot.general.cache_size).unwrap_or(usize::MAX),
                expand_json: snapshot.processing.expand_json,
                convert_booleans: snapshot.processing.convert_booleans,
            },
            LoxEgress::new(miniserver.shared()),
        )?;

        Ok(Relay {
            config,
            mqtt,
            miniserver,
            udp,
            core: Arc::new(core),
            topics,
            signals,
            udp_running: AtomicBool::new(false),
            shutdown_done: AtomicBool::new(false),
        })
    }

    /// Run until a signal or a config change asks for shutdown.
    pub async fn run(
        self: &Arc<Self>,
        mut stop_rx: watch::Receiver<Option<StopReason>>,
    ) -> Result<Exit, RelayError> {
        self.install_signal_handlers();

        // The resync worker has to be listening before the websocket connects,
        // or the reconnect that follows a failed first attempt would ask for a
        // resync nobody is waiting for.
        let worker = tokio::spawn({
            let relay = Arc::clone(self);
            let signals = self.signals.clone();
            async move {
                loop {
                    signals.resync_requested().await;
                    relay.handle_miniserver_sync().await;
                }
            }
        });

        if let Err(e) = self.start().await {
            // Python left the websocket open here and exited. Closing it is the
            // difference between a clean restart and a Miniserver holding a
            // token for a process that is gone.
            error!("{e}");
            self.shutdown().await;
            worker.abort();
            return Err(e);
        }

        info!("MQTT Relay started");
        let _ = stop_rx.wait_for(Option::is_some).await;
        let restart = stop_rx
            .borrow()
            .as_ref()
            .is_some_and(|stop| stop.restart);

        worker.abort();
        self.shutdown().await;
        Ok(if restart { Exit::Restart } else { Exit::Normal })
    }

    /// The startup sequence, in the order that matters.
    async fn start(&self) -> Result<(), RelayError> {
        crate::miniserver::connect_with(
            self.miniserver.shared(),
            self.miniserver.connect_config(),
            self.miniserver.url(),
            Arc::new(self.signals.clone()),
        )
        .await?;

        self.handle_miniserver_sync().await;

        let control: Arc<dyn ControlSink> = Arc::new(NativeControlSink::new(
            Arc::clone(&self.config),
            self.mqtt.shared(),
            self.topics.config_response.clone(),
            self.signals.clone(),
        ));
        let mut subscriptions = self.config.snapshot().topics.subscriptions;
        subscriptions.extend(self.topics.subscriptions());
        crate::mqtt::connect_with(self.mqtt.connect_params(
            subscriptions,
            Arc::clone(&self.core),
            control,
        ))
        .await?;

        // Awaited rather than spawned: a failed bind has to abort startup, not
        // leave a relay with no inbound path.
        crate::udp::start_with(self.udp.start_params()).await?;
        self.udp_running.store(true, Ordering::Release);
        Ok(())
    }

    /// Close the inputs first, then the connections.
    ///
    /// UDP and MQTT are the two ways work enters the relay, so closing them
    /// first means the Miniserver writes still in flight are not joined by new
    /// ones. Idempotent, and each step survives the one before it failing.
    pub(crate) async fn shutdown(&self) {
        if self.shutdown_done.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.udp_running.swap(false, Ordering::AcqRel) {
            crate::udp::stop_with(self.udp.signal_stop()).await;
        }
        crate::mqtt::disconnect_with(self.mqtt.shared(), self.mqtt.status_topic()).await;
        self.miniserver.shared().shutdown().await;
        info!("Shutdown complete");
    }

    /// Fetch the whitelist from the Miniserver, if that is switched on.
    ///
    /// Two things here look inconsistent and are deliberate, because they are
    /// what the Python did and what an operator's config file has been through:
    /// an empty result keeps the configured whitelist *without* writing it back,
    /// while a failure keeps it *and* writes it. See the note on each branch.
    pub(crate) async fn handle_miniserver_sync(&self) {
        let snapshot = self.config.snapshot();
        if !snapshot.miniserver.sync_with_miniserver {
            return;
        }
        let initial = snapshot.topics.topic_whitelist.clone();

        let endpoint = Endpoint::new(
            &snapshot.miniserver.miniserver_ip,
            u16::try_from(snapshot.miniserver.miniserver_port).unwrap_or(80),
        );
        let fetched = match endpoint {
            Ok(endpoint) => {
                whitelist::sync_whitelist(
                    &endpoint,
                    &snapshot.miniserver.miniserver_user,
                    &snapshot.miniserver.miniserver_pass,
                )
                .await
            }
            Err(e) => Err(e),
        };

        match fetched {
            Ok(inputs) if inputs.is_empty() => {
                // An empty extract would install a fail-closed gate and stop
                // every forward. That is almost never intentional - keep what we
                // had and say so, rather than wiping a working list.
                warn!(
                    "Miniserver sync returned no virtual inputs; keeping whitelist from config \
                     ({} entries)",
                    initial.len()
                );
                // NOTE: no save here, matching main.py:174-177, which only
                // pushes the list into the processor.
                self.core
                    .update_topic_whitelist(initial.into_iter().collect());
            }
            Ok(inputs) => {
                self.config.update_section(
                    ConfigSection::Topics,
                    &[(
                        "topic_whitelist".to_owned(),
                        CfgValue::from_strings(inputs.clone()),
                    )],
                    ListMode::Set,
                );
                let count = inputs.len();
                self.core.update_topic_whitelist(inputs);
                info!("Whitelist updated from miniserver configuration ({count} entries)");
            }
            Err(e) => {
                error!("Failed to sync with miniserver: {e}");
                info!("Keeping whitelist from config");
                // NOTE: this branch DOES save, writing the unchanged whitelist
                // back out - which is observable in the file's mtime and in the
                // None-to-"" normalization a first save performs. Preserved
                // because it is what the Python did; making the two branches
                // agree is a behaviour change and belongs in its own commit.
                self.config.update_section(
                    ConfigSection::Topics,
                    &[("topic_whitelist".to_owned(), CfgValue::from_set(&initial))],
                    ListMode::Set,
                );
                self.core
                    .update_topic_whitelist(initial.into_iter().collect());
            }
        }
    }

    /// Route SIGINT and SIGTERM into the shutdown path.
    ///
    /// Without this, SIGTERM - what `docker stop` sends - kills the process
    /// outright: no DISCONNECT, and the broker keeps the last status message
    /// until the keep-alive expires.
    #[cfg(unix)]
    fn install_signal_handlers(&self) {
        use tokio::signal::unix::{SignalKind, signal};

        let signals = self.signals.clone();
        match (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) {
            (Ok(mut interrupt), Ok(mut terminate)) => {
                tokio::spawn(async move {
                    tokio::select! {
                        _ = interrupt.recv() => signals.request_stop("SIGINT", false),
                        _ = terminate.recv() => signals.request_stop("SIGTERM", false),
                    }
                });
            }
            _ => warn!("Could not install signal handlers; SIGTERM will kill the process outright"),
        }
    }

    #[cfg(not(unix))]
    fn install_signal_handlers(&self) {
        let signals = self.signals.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signals.request_stop("Ctrl-C", false);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn store() -> Arc<ConfigStore> {
        Arc::new(ConfigStore::new("unused.toml", AppConfig::default()))
    }

    #[test]
    fn the_control_topics_come_off_the_base_topic() {
        let topics = MqttTopics::from_base("myrelay/");
        assert_eq!(topics.config_set, "myrelay/config/set");
        assert_eq!(topics.config_response, "myrelay/config/response");
        assert_eq!(topics.miniserver_startup, "myrelay/miniserverevent/startup");
    }

    /// The order the relay subscribes in is the order the SUBACK reasons come
    /// back in, so `review_suback` reads them against this list.
    #[test]
    fn the_subscription_order_is_the_one_main_py_used() {
        let topics = MqttTopics::from_base("r/");
        assert_eq!(
            topics.subscriptions(),
            [
                "r/config/set",
                "r/config/add",
                "r/config/remove",
                "r/config/update",
                "r/config/restart",
                "r/config/get",
                "r/miniserverevent/startup",
            ]
            .map(str::to_owned)
        );
    }

    /// A relay builds from the defaults without reaching for the network.
    #[test]
    fn a_relay_builds_from_the_default_configuration() {
        let (signals, _stop_rx) = Signals::new();
        assert!(Relay::build(store(), signals).is_ok());
    }

    /// A filter that the regex engine refuses stops the build, rather than
    /// surfacing later as a relay that came up and forwards nothing.
    #[test]
    fn an_unusable_filter_stops_the_build() {
        let mut config = AppConfig::default();
        config.topics.subscription_filters = vec!["^(unclosed".to_owned()];
        let store = Arc::new(ConfigStore::new("unused.toml", config));
        let (signals, _stop_rx) = Signals::new();
        assert!(Relay::build(store, signals).is_err());
    }

    /// Shutting down twice does the work once.
    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let (signals, _stop_rx) = Signals::new();
        let relay = Relay::build(store(), signals).expect("a relay");
        relay.shutdown().await;
        relay.shutdown().await;
    }

    /// The first stop request wins, including its restart decision.
    #[tokio::test]
    async fn a_second_stop_request_cannot_change_the_first() {
        let (signals, stop_rx) = Signals::new();
        signals.request_stop("configuration changed", true);
        signals.request_stop("SIGTERM", false);
        let decision = stop_rx.borrow().clone().expect("a decision");
        assert!(decision.restart, "the restart decision was overwritten");
        assert_eq!(decision.reason, "configuration changed");
    }
}

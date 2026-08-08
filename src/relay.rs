//! Starting the relay, running it, and stopping it again.
//!
//! The startup order is the load-bearing part. Websocket first, so writes have
//! somewhere to go; then the whitelist sync; and only then MQTT, because
//! subscribing with an empty whitelist while `sync_with_miniserver` is on would
//! drop everything that arrived until the sync finished. The UDP bind is
//! awaited rather than spawned, because a failed bind has to abort the start
//! instead of vanishing into a discarded task and leaving a relay that looks
//! healthy and forwards nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use log::{debug, error, info, warn};
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
            // Shut down even though the start failed: the difference is a
            // Miniserver left holding a token for a process that is gone.
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

        // What the relay ended up with, as opposed to what the file asked for.
        // `connected` follows the MQTT *session* rather than the handle, so this
        // is the difference between "connect returned" and "there is a session"
        // - which is the distinction that went missing once already.
        info!(
            "Connected: broker={}, miniserver={}",
            self.mqtt.shared().connected(),
            self.miniserver.shared().connected()
        );
        debug!(
            "Compiled filters: subscription_filters={:?} do_not_forward={:?}",
            self.core.subscription_filters(),
            self.core.do_not_forward_patterns()
        );
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
        self.report_losses();
        self.report_shape_cache();
        info!("Shutdown complete");
    }

    /// Say what was lost during this run, if anything was.
    ///
    /// Each loss was already logged as it happened, at WARNING, with everything
    /// in it. This is the count, at the one moment it can be complete - and it
    /// is what makes the two bounded rings worth keeping: they are a diagnostic,
    /// and a diagnostic nothing ever reads is just a memory leak with good
    /// intentions.
    fn report_losses(&self) {
        let publishes = self.mqtt.shared().take_undelivered();
        let writes = self.miniserver.shared().take_undelivered();
        if publishes.is_empty() && writes.is_empty() {
            return;
        }
        warn!(
            "Messages were lost during this run: {} MQTT publish(es) and {} Miniserver write(s) \
             could not be delivered. The last {} and {} of each are above, at WARNING.",
            publishes.len(),
            writes.len(),
            publishes.len(),
            writes.len()
        );
    }

    /// Fetch the whitelist from the Miniserver, if that is switched on.
    ///
    /// Either failure - nothing came back, or nothing could be fetched - keeps
    /// the configured whitelist and writes nothing. Nothing changed, so there
    /// is nothing to write - and writing anyway would touch the file on the one
    /// path where the relay has just been told it cannot reach the Miniserver.
    pub(crate) async fn handle_miniserver_sync(&self) {
        let snapshot = self.config.snapshot();
        if !snapshot.miniserver.sync_with_miniserver {
            return;
        }

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
        self.apply_whitelist(fetched);
    }

    /// What to do with whatever the sync came back with.
    ///
    /// Separate from the fetch so the three outcomes can be exercised without a
    /// Miniserver, a stub server or a network at all - which is what
    /// `tests::a_*_sync_*` do. The split is honest either way: getting the list
    /// and deciding what it means are different jobs.
    fn apply_whitelist(&self, fetched: Result<Vec<String>, whitelist::SyncError>) {
        let initial = self.config.snapshot().topics.topic_whitelist;
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
                self.core
                    .update_topic_whitelist(initial.into_iter().collect());
            }
        }
    }

    /// What the learned-layout cache did over this run.
    ///
    /// A relay forwarding steady JSON builds one plan per topic and then replays
    /// them; a build count that keeps climbing means the payloads carry
    /// something the scanner refuses, and the relay has been paying for the DOM
    /// route the whole time. That is worth knowing and there is nowhere else to
    /// read it from.
    fn report_shape_cache(&self) {
        let m = self.core.shape_metrics();
        if m.hits == 0 && m.learns == 0 {
            return;
        }
        info!(
            "Shape cache: {} plan(s) held, {} built, {} message(s) replayed, {} fell back to the \
             DOM route ({} of those held back), {} document(s) no plan could be built for, {} \
             topic(s) currently held back",
            m.plans, m.learns, m.hits, m.dom_fallbacks, m.negative_skips, m.learn_failures,
            m.unplannable
        );
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
    use std::collections::BTreeSet;
    use std::path::PathBuf;

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

    // -- the whitelist sync -------------------------------------------------
    //
    // All three outcomes, driven through `apply_whitelist` so none of them
    // needs a Miniserver, a stub server or a network.

    fn relay_with(config: AppConfig) -> (Arc<Relay>, PathBuf) {
        let dir = scratch("relay");
        let store = Arc::new(ConfigStore::new(dir.join("config.toml"), config));
        let (signals, _stop) = Signals::new();
        let relay = Arc::new(Relay::build(store, signals).expect("a relay"));
        (relay, dir)
    }

    fn scratch(tag: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("loxmqttrelay-{tag}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    fn synced() -> AppConfig {
        let mut config = AppConfig::default();
        config.miniserver.sync_with_miniserver = true;
        config.topics.topic_whitelist =
            ["initial_topic1".to_owned(), "initial_topic2".to_owned()].into();
        config
    }

    /// A successful sync replaces the whitelist, in the config and in the core.
    #[test]
    fn a_successful_sync_replaces_the_whitelist() {
        let (relay, dir) = relay_with(synced());

        relay.apply_whitelist(Ok(vec![
            "synced_topic1".to_owned(),
            "synced_topic2".to_owned(),
        ]));

        let expected: BTreeSet<String> =
            ["synced_topic1".to_owned(), "synced_topic2".to_owned()].into();
        assert_eq!(relay.config.snapshot().topics.topic_whitelist, expected);
        assert_eq!(
            relay.core.topic_whitelist(),
            expected.iter().cloned().collect()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty extract must not wipe a working whitelist: with sync on, an
    /// empty list is a fail-closed gate that stops every forward.
    #[test]
    fn an_empty_sync_keeps_the_configured_whitelist() {
        let (relay, dir) = relay_with(synced());

        relay.apply_whitelist(Ok(Vec::new()));

        let expected: BTreeSet<String> =
            ["initial_topic1".to_owned(), "initial_topic2".to_owned()].into();
        assert_eq!(relay.config.snapshot().topics.topic_whitelist, expected);
        assert_eq!(
            relay.core.topic_whitelist(),
            expected.iter().cloned().collect()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And neither must a failure to reach the Miniserver at all.
    #[test]
    fn a_failed_sync_keeps_the_configured_whitelist() {
        let (relay, dir) = relay_with(synced());

        relay.apply_whitelist(Err(crate::whitelist::SyncError::NoHost));

        let expected: BTreeSet<String> =
            ["initial_topic1".to_owned(), "initial_topic2".to_owned()].into();
        assert_eq!(relay.config.snapshot().topics.topic_whitelist, expected);
        assert_eq!(
            relay.core.topic_whitelist(),
            expected.iter().cloned().collect()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Neither failure writes the file. Nothing changed, so there is nothing to
    /// write - and the error branch used to write anyway, on the one path where
    /// the relay had just been told it could not reach the Miniserver.
    #[test]
    fn a_failed_sync_does_not_write_the_config_file() {
        let (relay, dir) = relay_with(synced());
        let file = dir.join("config.toml");
        assert!(!file.exists(), "nothing has saved yet");

        relay.apply_whitelist(Ok(Vec::new()));
        assert!(!file.exists(), "an empty sync wrote the file");

        relay.apply_whitelist(Err(crate::whitelist::SyncError::NoHost));
        assert!(!file.exists(), "a failed sync wrote the file");

        // A successful one does, which is what makes the above meaningful.
        relay.apply_whitelist(Ok(vec!["something".to_owned()]));
        assert!(file.exists(), "a successful sync did not write the file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With sync off nothing is fetched and nothing changes.
    #[tokio::test]
    async fn sync_switched_off_leaves_the_whitelist_alone() {
        let mut config = synced();
        config.miniserver.sync_with_miniserver = false;
        // An address that would fail immediately if it were ever dialled.
        config.miniserver.miniserver_ip = String::new();
        let (relay, dir) = relay_with(config);

        relay.handle_miniserver_sync().await;

        let expected: BTreeSet<String> =
            ["initial_topic1".to_owned(), "initial_topic2".to_owned()].into();
        assert_eq!(relay.config.snapshot().topics.topic_whitelist, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The filters configured at startup survive a sync that replaces the
    /// whitelist wholesale - including one that would match a synced entry.
    #[test]
    fn the_configured_filters_survive_a_sync() {
        let mut config = synced();
        config.topics.topic_whitelist = BTreeSet::new();
        config.topics.do_not_forward = vec![r"^private\/.*".to_owned()];
        config.topics.subscription_filters = vec!["^skip/".to_owned()];
        let (relay, dir) = relay_with(config);

        assert_eq!(relay.core.do_not_forward_patterns(), [r"^private\/.*"]);

        relay.apply_whitelist(Ok(vec![
            "private_secret".to_owned(),
            "public_sensor".to_owned(),
        ]));

        assert_eq!(
            relay.core.topic_whitelist(),
            ["private_secret".to_owned(), "public_sensor".to_owned()].into()
        );
        assert_eq!(relay.core.do_not_forward_patterns(), [r"^private\/.*"]);
        assert_eq!(relay.core.subscription_filters(), ["^skip/"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whitelist configured in the file reaches the core at construction,
    /// before any sync has run.
    #[test]
    fn the_configured_whitelist_reaches_the_core() {
        let (relay, dir) = relay_with(synced());
        assert_eq!(
            relay.core.topic_whitelist(),
            ["initial_topic1".to_owned(), "initial_topic2".to_owned()].into()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- shutdown -----------------------------------------------------------

    /// Shutting down closes the inputs and does it once, however often it is
    /// asked. The second call is what a signal arriving during a config-change
    /// shutdown looks like.
    #[tokio::test]
    async fn shutdown_closes_the_inputs_once() {
        let (relay, dir) = relay_with(AppConfig::default());
        relay.udp_running.store(true, Ordering::Release);

        relay.shutdown().await;
        assert!(!relay.udp_running.load(Ordering::Acquire));
        assert!(relay.shutdown_done.load(Ordering::Acquire));

        // Idempotent: the second call finds the guard set and returns.
        relay.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A shutdown with nothing connected still completes. Every step is
    /// tolerant of the one before it having failed, which is what a broker that
    /// is already gone looks like.
    #[tokio::test]
    async fn shutdown_survives_connections_that_were_never_up() {
        let (relay, dir) = relay_with(AppConfig::default());
        relay.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The restart decision reaches the runner from another task, and nothing
    /// re-execs there - `main` does that, after the runtime has been shut down.
    #[tokio::test]
    async fn a_restart_request_from_another_task_reaches_the_runner() {
        let (signals, mut stop) = Signals::new();
        let elsewhere = signals.clone();
        tokio::task::spawn_blocking(move || {
            elsewhere.request_stop("configuration changed", true);
        })
        .await
        .expect("the task");

        stop.changed().await.expect("a decision");
        let decision = stop.borrow().clone().expect("a decision");
        assert!(decision.restart);
        assert_eq!(decision.reason, "configuration changed");
    }
}

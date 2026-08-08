//! Acting on the relay's own control topics.
//!
//! The message path recognises control topics in [`Core::control_kind`] and
//! hands them here; everything else goes straight to the Miniserver. Behind a
//! trait because [`crate::mqtt::ingress_worker`] has no business knowing what
//! acting on one involves, and because the tests want to watch.
//!
//! `config/get` answers on the response topic, `config/set`, `add` and `remove`
//! apply an update and then ask for a restart, `config/update` and
//! `config/restart` only ask for the restart, and the Miniserver's startup event
//! asks for a whitelist resync. The two "ask for" are signals rather than calls:
//! see [`crate::signals`].
//!
//! [`Core::control_kind`]: crate::process::Core::control_kind

use std::sync::Arc;

use log::{error, info};

use crate::config::value::CfgValue;
use crate::config::{ConfigStore, ListMode};
use crate::mqtt::MqttShared;
use crate::process::{self, ControlTopic};
use crate::signals::{ResyncTrigger as _, Signals};
use crate::util::loggable;

/// Somewhere a recognised control topic can be acted on.
pub(crate) trait ControlSink: Send + Sync {
    /// Act on one control topic.
    ///
    /// Reports failure as text rather than as an error type: the caller only
    /// logs it, next to the topic the message arrived on, and implementors do
    /// not share an error type.
    fn dispatch(&self, kind: ControlTopic, payload: &[u8]) -> Result<(), String>;
}

/// The four actions a control topic can trigger.
///
/// Two of them are signals rather than work done here: a resync wakes the
/// worker that owns it, and a restart is the stop channel carrying
/// `restart: true`, so the re-exec happens once the runtime has been shut down
/// rather than from inside the message path. Nothing here can fail in a way the
/// caller could act on, which is why it never reports anything.
pub(crate) struct NativeControlSink {
    config: Arc<ConfigStore>,
    mqtt: Arc<MqttShared>,
    config_response: String,
    signals: Signals,
}

impl NativeControlSink {
    pub(crate) fn new(
        config: Arc<ConfigStore>,
        mqtt: Arc<MqttShared>,
        config_response: String,
        signals: Signals,
    ) -> Self {
        NativeControlSink {
            config,
            mqtt,
            config_response,
            signals,
        }
    }
}

impl ControlSink for NativeControlSink {
    fn dispatch(&self, kind: ControlTopic, payload: &[u8]) -> Result<(), String> {
        match kind {
            ControlTopic::MiniserverStartup => {
                if self.config.snapshot().miniserver.sync_with_miniserver {
                    info!("Miniserver startup detected, resyncing whitelist");
                    self.signals.request_resync();
                }
            }
            ControlTopic::ConfigGet => {
                self.mqtt
                    .publish_detached(self.config_response.clone(), self.config.safe_json());
            }
            ControlTopic::ConfigSet | ControlTopic::ConfigAdd | ControlTopic::ConfigRemove => {
                let mode = kind
                    .update_mode()
                    .and_then(ListMode::parse)
                    .expect("set/add/remove carry a mode");
                let text = process::decode_payload("config", payload);
                match crate::config::value::parse_json(&text) {
                    Ok(CfgValue::Table(updates)) => {
                        match self.config.update_fields(&updates, mode) {
                            Err(e) => error!("Error updating configuration: {e}"),
                            Ok(()) => {
                                info!("Configuration updated via MQTT. Restarting program.");
                                self.signals.request_stop("configuration changed", true);
                            }
                        }
                    }
                    // A payload that is valid JSON but not an object named no
                    // fields, so there is nothing to apply.
                    Ok(other) => error!(
                        "Configuration update on '{}' is a {}, not an object",
                        loggable(&text),
                        other.type_name()
                    ),
                    Err(e) => error!(
                        "Invalid JSON format in MQTT message '{}': {e}",
                        loggable(&text)
                    ),
                }
            }
            ControlTopic::ConfigReload => {
                info!("Reloading configuration. Restarting program.");
                self.signals.request_stop("configuration changed", true);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use std::path::PathBuf;

    const RESPONSE: &str = "myrelay/config/response";

    struct Harness {
        sink: NativeControlSink,
        config: Arc<ConfigStore>,
        mqtt: Arc<MqttShared>,
        signals: Signals,
        stop: tokio::sync::watch::Receiver<Option<crate::signals::StopReason>>,
        dir: PathBuf,
    }

    impl Harness {
        fn new(config: AppConfig) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "loxmqttrelay-control-{}",
                std::process::id() as u64 * 1000 + rand_suffix()
            ));
            let _ = std::fs::create_dir_all(&dir);
            let config = Arc::new(ConfigStore::new(dir.join("config.toml"), config));
            let mqtt = Arc::new(MqttShared::new());
            let (signals, stop) = Signals::new();
            Harness {
                sink: NativeControlSink::new(
                    Arc::clone(&config),
                    Arc::clone(&mqtt),
                    RESPONSE.to_owned(),
                    signals.clone(),
                ),
                config,
                mqtt,
                signals,
                stop,
                dir,
            }
        }

        fn dispatch(&self, kind: ControlTopic, payload: &[u8]) {
            self.sink.dispatch(kind, payload).expect("never reports");
        }

        /// What a `config/get` tried to publish.
        ///
        /// There is no broker here, so the publish is recorded as a drop - which
        /// keeps the topic and the payload, and is therefore exactly the
        /// observation this needs without a broker or a mock in sight.
        fn published(&self) -> Vec<(String, Vec<u8>)> {
            self.mqtt
                .take_undelivered()
                .into_iter()
                .map(|(topic, payload, _)| (topic, payload))
                .collect()
        }

        fn restart_requested(&self) -> bool {
            self.stop.borrow().as_ref().is_some_and(|stop| stop.restart)
        }

        fn stop_requested(&self) -> bool {
            self.stop.borrow().is_some()
        }

        /// Whether a resync was asked for, without blocking if it was not.
        fn resync_requested(&self) -> bool {
            futures_lite_now_or_never(self.signals.resync_requested())
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Poll a future once. Enough for a `Notify` that either has a permit or
    /// does not, and cheaper than pulling in a futures crate for it.
    fn futures_lite_now_or_never(future: impl Future<Output = ()>) -> bool {
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};
        matches!(
            pin!(future).poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(())
        )
    }

    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0)
    }

    // -- config/get ---------------------------------------------------------

    /// The response goes to the response topic, redacted, and is the same bytes
    /// `AppConfig::safe_json` produces - whose exact bytes the corpus pins.
    #[test]
    fn config_get_publishes_the_safe_config_to_the_response_topic() {
        let mut config = AppConfig::default();
        config.broker.user = Some("someone".to_owned());
        config.broker.password = Some("hunter2".to_owned());
        let harness = Harness::new(config);

        harness.dispatch(ControlTopic::ConfigGet, b"");

        let published = harness.published();
        assert_eq!(published.len(), 1);
        let (topic, payload) = &published[0];
        assert_eq!(topic, RESPONSE);
        assert_eq!(payload, &harness.config.safe_json());

        let text = String::from_utf8(payload.clone()).expect("utf-8");
        assert!(!text.contains("hunter2"), "the password was published: {text}");
        assert!(!text.contains("someone"), "the user was published: {text}");
        assert!(text.contains("\"host\":\"localhost\""), "{text}");
    }

    #[test]
    fn config_get_changes_nothing() {
        let harness = Harness::new(AppConfig::default());
        harness.dispatch(ControlTopic::ConfigGet, b"");
        assert_eq!(harness.config.snapshot(), AppConfig::default());
        assert!(!harness.stop_requested());
    }

    // -- config/set, add, remove --------------------------------------------

    #[test]
    fn a_usable_update_is_applied_and_asks_for_a_restart() {
        for (kind, payload, expected) in [
            (
                ControlTopic::ConfigSet,
                br#"{"subscriptions": ["only/#"]}"#.as_slice(),
                vec!["only/#"],
            ),
            (
                ControlTopic::ConfigAdd,
                br#"{"subscriptions": ["extra/#"]}"#.as_slice(),
                vec!["extra/#"],
            ),
        ] {
            let harness = Harness::new(AppConfig::default());
            harness.dispatch(kind, payload);
            assert_eq!(
                harness.config.snapshot().topics.subscriptions,
                expected,
                "{kind:?}"
            );
            assert!(harness.restart_requested(), "{kind:?} did not ask to restart");
        }
    }

    /// `remove` takes entries away rather than replacing them.
    #[test]
    fn a_remove_takes_the_named_entries_out() {
        let mut config = AppConfig::default();
        config.topics.subscriptions = vec!["a/#".to_owned(), "b/#".to_owned()];
        let harness = Harness::new(config);

        harness.dispatch(ControlTopic::ConfigRemove, br#"{"subscriptions": ["a/#"]}"#);

        assert_eq!(harness.config.snapshot().topics.subscriptions, ["b/#"]);
        assert!(harness.restart_requested());
    }

    /// A refused update must not restart: the relay would come back to the same
    /// configuration and the operator would have learned nothing.
    #[test]
    fn a_refused_update_changes_nothing_and_does_not_restart() {
        for payload in [
            // Protected: another host would be authenticated against with these
            // credentials after the restart.
            br#"{"host": "evil.example"}"#.as_slice(),
            br#"{"no_such_field": 1}"#.as_slice(),
            br#"{"udp_in_port": 0}"#.as_slice(),
            br#"{"cache_size": "not a number"}"#.as_slice(),
        ] {
            let harness = Harness::new(AppConfig::default());
            harness.dispatch(ControlTopic::ConfigSet, payload);
            assert_eq!(
                harness.config.snapshot(),
                AppConfig::default(),
                "{:?} changed the configuration",
                String::from_utf8_lossy(payload)
            );
            assert!(
                !harness.stop_requested(),
                "{:?} asked to restart",
                String::from_utf8_lossy(payload)
            );
        }
    }

    /// A payload that is not JSON never reaches the update at all.
    #[test]
    fn a_payload_that_is_not_json_does_not_restart() {
        for payload in [
            b"{oops".as_slice(),
            b"".as_slice(),
            // Valid JSON, but not an object, so it names no fields.
            b"[1, 2]".as_slice(),
            b"\"a string\"".as_slice(),
        ] {
            let harness = Harness::new(AppConfig::default());
            harness.dispatch(ControlTopic::ConfigSet, payload);
            assert_eq!(harness.config.snapshot(), AppConfig::default());
            assert!(
                !harness.stop_requested(),
                "{:?} asked to restart",
                String::from_utf8_lossy(payload)
            );
        }
    }

    // -- config/update and config/restart -----------------------------------

    /// Both only restart. Neither carries a payload, and neither may mutate.
    #[test]
    fn a_reload_restarts_without_touching_the_configuration() {
        let harness = Harness::new(AppConfig::default());
        harness.dispatch(ControlTopic::ConfigReload, br#"{"cache_size": 5}"#);
        assert_eq!(harness.config.snapshot(), AppConfig::default());
        assert!(harness.restart_requested());
    }

    // -- the Miniserver startup event ---------------------------------------

    #[test]
    fn a_miniserver_startup_asks_for_a_resync_when_sync_is_on() {
        let harness = Harness::new(AppConfig::default());
        assert!(harness.config.snapshot().miniserver.sync_with_miniserver);

        harness.dispatch(ControlTopic::MiniserverStartup, b"");

        assert!(harness.resync_requested());
        assert!(!harness.stop_requested(), "a startup event is not a restart");
    }

    #[test]
    fn a_miniserver_startup_is_ignored_when_sync_is_off() {
        let mut config = AppConfig::default();
        config.miniserver.sync_with_miniserver = false;
        let harness = Harness::new(config);

        harness.dispatch(ControlTopic::MiniserverStartup, b"");

        assert!(!harness.resync_requested());
    }

}

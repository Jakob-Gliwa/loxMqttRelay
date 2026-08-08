//! The Miniserver websocket: the relay's egress.
//!
//! Built like [`crate::mqtt`]: a `Shared` holding the connection that outlives
//! every reconnect, a handle the relay constructs and drives, and a diagnostic
//! ring for the values that were lost. The processing core reaches the
//! connection through [`LoxEgress`] and never sees this module.
//!
//! Lifecycle events leave the crate's reader task immediately, through a
//! channel to [`lifecycle_worker`]. That indirection was originally there to
//! keep the GIL off the reader; it stays because a resync after a reconnect
//! downloads the whole Miniserver configuration, and doing that from inside the
//! websocket's own callback would stall the connection it belongs to.

use std::collections::VecDeque;
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use log::{debug, info, warn};
use loxwebsocket::{ClientEvent, ConnState, ConnectConfig, Error as LoxError, LoxClient, LoxHandler, TlsMode};
use tokio::sync::mpsc;

use crate::config::AppConfig;
use crate::error::RelayError;
use crate::egress::{Egress, EgressError};
use crate::util::{lock_recover, loggable};

/// How many controls may be in flight towards the writer.
///
/// The crate defaults to 32, which is aimed at an application sending the odd
/// command. One MQTT message here expands to one control per JSON leaf, and a
/// boiler status message carries over fifty - so at the default a single
/// message would sit waiting on the channel for the writer to catch up.
const COMMAND_CHANNEL_DEPTH: usize = 1024;

/// How many lost values to remember for diagnostics.
const UNDELIVERED_RING: usize = 32;

/// Depth of the lifecycle bridge. Only reconnects travel it, so anything beyond
/// a handful is slack.
const EVENT_CHANNEL_DEPTH: usize = 16;

/// A value that never reached the Miniserver, kept for diagnostics.
struct Undelivered {
    normalized: String,
    value: String,
    reason: String,
}

/// The websocket session, shared between [`MiniserverClient`] and the
/// [`LoxEgress`] the processing core sends through.
pub(crate) struct MsShared {
    client: ArcSwapOption<LoxClient<RelayHandler>>,
    undelivered: std::sync::Mutex<VecDeque<Undelivered>>,
}

impl MsShared {
    pub(crate) fn new() -> Self {
        MsShared {
            client: ArcSwapOption::empty(),
            undelivered: std::sync::Mutex::new(VecDeque::with_capacity(UNDELIVERED_RING)),
        }
    }

    /// Whether a websocket session is up right now.
    pub(crate) fn connected(&self) -> bool {
        self.state() == ConnState::Connected
    }

    fn state(&self) -> ConnState {
        match self.client.load_full() {
            Some(client) => client.state(),
            None => ConnState::Closed,
        }
    }

    /// Logs and remembers a value that was lost.
    ///
    /// At QoS 0 with no outbox there is nothing to retry, so the only thing
    /// owed to the operator is a truthful account of what was lost and why -
    /// the same contract [`crate::mqtt::MqttShared::record_drop`] keeps.
    fn record_drop(&self, normalized: &str, value: &str, reason: &EgressError) {
        warn!(
            "Dropped Miniserver write '{}'='{}': {reason}",
            loggable(normalized),
            loggable(value)
        );
        let mut ring = lock_recover(&self.undelivered);
        if ring.len() >= UNDELIVERED_RING {
            ring.pop_front();
        }
        ring.push_back(Undelivered {
            normalized: normalized.to_owned(),
            value: value.to_owned(),
            reason: reason.to_string(),
        });
    }

    /// Take the writes that were lost, as `(input, value, reason)`.
    ///
    /// Draining rather than reading: these are diagnostics, and whoever looks
    /// at them is reporting them.
    pub(crate) fn take_undelivered(&self) -> Vec<(String, String, String)> {
        lock_recover(&self.undelivered)
            .drain(..)
            .map(|entry| (entry.normalized, entry.value, entry.reason))
            .collect()
    }

    /// Put one value on the wire, or report why it was lost.
    async fn write(&self, normalized: &str, value: &str) -> Result<(), EgressError> {
        let Some(client) = self.client.load_full() else {
            let error = EgressError::NotConnected;
            self.record_drop(normalized, value, &error);
            return Err(error);
        };
        debug!(
            "Sending '{}'='{}' to the Miniserver",
            loggable(normalized),
            loggable(value)
        );
        match client.send_control(normalized, value).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let error = classify(e);
                self.record_drop(normalized, value, &error);
                Err(error)
            }
        }
    }

    /// Wind the session down, killing its token on the way out.
    ///
    /// `LoxClient::stop` consumes the client, which is the only way the
    /// `killtoken` is awaited - so the handle has to come back out of the
    /// `ArcSwap` first. A second holder would leave only `Drop`, which requests
    /// the same shutdown but cannot wait for it.
    pub(crate) async fn shutdown(&self) {
        let Some(client) = self.client.swap(None) else {
            return;
        };
        match Arc::try_unwrap(client) {
            Ok(client) => {
                if let Err(e) = client.stop().await {
                    warn!("Error while closing the Miniserver websocket: {e}");
                }
            }
            Err(_) => warn!(
                "The Miniserver websocket is still in use elsewhere; its shutdown was requested \
                 but not awaited"
            ),
        }
    }
}

/// Translate the crate's error into what the relay does about it.
fn classify(error: LoxError) -> EgressError {
    match error {
        LoxError::NotConnected => EgressError::NotConnected,
        LoxError::Backpressure => EgressError::Backpressure,
        other => EgressError::Failed(other.to_string()),
    }
}

/// The processing core's route out.
pub(crate) struct LoxEgress {
    shared: Arc<MsShared>,
}

impl LoxEgress {
    pub(crate) fn new(shared: Arc<MsShared>) -> Self {
        LoxEgress { shared }
    }
}

impl Egress for LoxEgress {
    fn connected(&self) -> bool {
        self.shared.state() == ConnState::Connected
    }

    async fn send(&self, uuid: &str, value: &str) -> Result<(), EgressError> {
        self.shared.write(uuid, value).await
    }
}

/// Lifecycle events, handed off rather than acted on.
///
/// The crate calls this on its reader task, where an await is impossible. So it
/// only forwards, and [`lifecycle_worker`] does the work - which matters
/// because that work is a whole configuration download.
pub(crate) struct RelayHandler {
    events: mpsc::Sender<ClientEvent>,
}

impl LoxHandler for RelayHandler {
    fn on_event(&mut self, event: ClientEvent) {
        // Dropped rather than awaited: the reader task must not block, and a
        // full channel means a reconnect resync is already queued.
        if self.events.try_send(event).is_err() {
            debug!("Lifecycle event dropped; a resync is already pending");
        }
    }
}

/// Whether an event means the Miniserver may accept different inputs now.
///
/// Only a reconnect does. It means the Miniserver went away and came back,
/// which usually follows a configuration upload. The first `Connected` is not a
/// reconnect and the relay syncs on startup anyway, so acting on it too would
/// buy a duplicate config download on every start.
fn warrants_resync(event: &ClientEvent) -> bool {
    matches!(event, ClientEvent::Reconnected)
}

/// Turns a reconnect into a whitelist resync.
async fn lifecycle_worker(
    mut events: mpsc::Receiver<ClientEvent>,
    resync: Arc<dyn crate::signals::ResyncTrigger>,
) {
    while let Some(event) = events.recv().await {
        match event {
            ClientEvent::Connected => debug!("Miniserver websocket connected"),
            ClientEvent::Reconnected => info!("Miniserver websocket reconnected"),
            ClientEvent::ConnectionClosed { close_code } => warn!(
                "Miniserver websocket closed (code {close_code:?}); writes are dropped until it \
                 is back"
            ),
            ClientEvent::Closed => warn!("Miniserver websocket gave up reconnecting"),
        }
        if !warrants_resync(&event) {
            continue;
        }
        resync.request_resync();
    }
    debug!("Miniserver lifecycle worker stopped");
}

/// The base URL and TLS policy for a configured Miniserver.
///
/// HTTPS only on 443, and the port spelled out unless it is a default. The pin
/// mode matters because a
/// Miniserver reached by IP presents a CloudDNS certificate whose name never
/// matches the address dialled, so WebPKI validation cannot succeed.
fn endpoint(ip: &str, port: u16) -> (String, TlsMode) {
    if port == 443 {
        return (format!("https://{ip}"), TlsMode::PinOnFirstUse);
    }
    if port == 80 {
        return (format!("http://{ip}"), TlsMode::WebPki);
    }
    (format!("http://{ip}:{port}"), TlsMode::WebPki)
}

/// The relay's Miniserver websocket.
///
/// Holds what a connection is made from; the connection itself lives in
/// [`MsShared`], which [`LoxEgress`] and the relay's shutdown both reach
/// through.
pub(crate) struct MiniserverClient {
    shared: Arc<MsShared>,
    url: String,
    tls: TlsMode,
    user: String,
    password: String,
}

impl MiniserverClient {
    pub(crate) fn shared(&self) -> Arc<MsShared> {
        Arc::clone(&self.shared)
    }
}

/// Open the websocket and start the lifecycle worker.
pub(crate) async fn connect_with(
    shared: Arc<MsShared>,
    cfg: ConnectConfig,
    url: String,
    resync: Arc<dyn crate::signals::ResyncTrigger>,
) -> Result<(), RelayError> {
    let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_DEPTH);
    tokio::spawn(lifecycle_worker(events_rx, resync));

    info!("Connecting to the Miniserver websocket at {url}");
    let client = LoxClient::connect(cfg, RelayHandler { events: events_tx })
        .await
        .map_err(|e| RelayError::Miniserver(e.to_string()))?;
    shared.client.store(Some(Arc::new(client)));
    Ok(())
}

impl MiniserverClient {
    pub(crate) fn connect_config(&self) -> ConnectConfig {
        ConnectConfig {
            tls: self.tls.clone(),
            // The relay writes, it does not listen: without the event table
            // there is nothing to receive and no event slot to occupy on the
            // Miniserver.
            receive_updates: false,
            command_channel_depth: COMMAND_CHANNEL_DEPTH,
            ..ConnectConfig::new(self.url.clone(), self.user.clone(), self.password.clone())
        }
    }

    pub(crate) fn url(&self) -> String {
        self.url.clone()
    }

    /// The websocket client as the relay builds it, from the loaded configuration.
    pub(crate) fn build(config: &AppConfig) -> Self {
        let miniserver = &config.miniserver;
        // The configured address may carry a port of its own; the port field is
        // what decides the scheme, so only the host part is used here.
        let ip = miniserver
            .miniserver_ip
            .split(':')
            .next()
            .unwrap_or(&miniserver.miniserver_ip);
        let port = u16::try_from(miniserver.miniserver_port).unwrap_or(80);

        let (url, tls) = endpoint(ip, port);
        info!("Miniserver websocket configured for {url}");

        MiniserverClient {
            shared: Arc::new(MsShared::new()),
            url,
            tls,
            user: miniserver.miniserver_user.clone(),
            password: miniserver.miniserver_pass.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scheme has to follow the configured port, because only 443 is
    /// served over TLS - and an IP behind TLS needs the pin mode.
    #[test]
    fn the_endpoint_follows_the_configured_port() {
        assert_eq!(
            endpoint("10.0.0.1", 80),
            ("http://10.0.0.1".to_string(), TlsMode::WebPki)
        );
        assert_eq!(
            endpoint("10.0.0.1", 8080),
            ("http://10.0.0.1:8080".to_string(), TlsMode::WebPki)
        );
        let (url, tls) = endpoint("10.0.0.1", 443);
        assert_eq!(url, "https://10.0.0.1");
        assert!(matches!(tls, TlsMode::PinOnFirstUse));
    }

    /// Without a connection a write must be refused rather than queued, and it
    /// has to leave a trace: nothing retries it.
    #[tokio::test]
    async fn a_write_without_a_session_is_dropped_and_recorded() {
        let shared = Arc::new(MsShared::new());
        let egress = LoxEgress::new(Arc::clone(&shared));
        assert!(!egress.connected());
        assert_eq!(
            egress.send("dev_x", "1").await,
            Err(EgressError::NotConnected)
        );

        let lost = shared.take_undelivered();
        assert_eq!(lost.len(), 1);
        assert_eq!((lost[0].0.as_str(), lost[0].1.as_str()), ("dev_x", "1"));
        // Drained, so a second reader does not report the same loss again.
        assert!(shared.take_undelivered().is_empty());
    }

    /// The ring is a diagnostic, not a buffer: it must not grow without bound
    /// while the Miniserver is away.
    #[tokio::test]
    async fn the_diagnostic_ring_is_bounded() {
        let shared = Arc::new(MsShared::new());
        let egress = LoxEgress::new(Arc::clone(&shared));
        for i in 0..UNDELIVERED_RING * 3 {
            let _ = egress.send("dev_x", &i.to_string()).await;
        }
        // Bounded, and what is left is the most recent - which is what a report
        // needs.
        let lost = shared.take_undelivered();
        assert_eq!(lost.len(), UNDELIVERED_RING);
        assert_eq!(lost[lost.len() - 1].1, (UNDELIVERED_RING * 3 - 1).to_string());
    }

    /// The whitelist resync is not free - it downloads the whole Miniserver
    /// configuration - so only the event that can have changed it triggers one.
    #[test]
    fn only_a_reconnect_resyncs_the_whitelist() {
        assert!(warrants_resync(&ClientEvent::Reconnected));
        assert!(!warrants_resync(&ClientEvent::Connected));
        assert!(!warrants_resync(&ClientEvent::ConnectionClosed {
            close_code: Some(1006)
        }));
        assert!(!warrants_resync(&ClientEvent::Closed));
    }

    /// The reader task must never block, so a resync that is already queued
    /// swallows the next one rather than waiting for room.
    #[test]
    fn a_full_event_channel_does_not_stall_the_reader() {
        let (tx, _rx) = mpsc::channel(1);
        let mut handler = RelayHandler { events: tx };
        for _ in 0..10 {
            handler.on_event(ClientEvent::Reconnected);
        }
    }

    #[test]
    fn shutting_down_without_a_session_is_harmless() {
        let shared = Arc::new(MsShared::new());
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(shared.shutdown());
    }
}

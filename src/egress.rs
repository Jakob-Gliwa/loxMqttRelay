//! Where a forwarded value leaves the relay.
//!
//! The processing core in [`crate::process`] is generic over this trait for two
//! reasons. In production it is the Loxone websocket client
//! ([`crate::miniserver::LoxEgress`]); in the tests it is [`RecordingEgress`],
//! which is what lets the whole flattening and filtering path be exercised
//! without a Miniserver and without Python.

use std::fmt;
use std::future::Future;

/// Why a value did not reach the Miniserver.
///
/// At QoS 0 with no outbox there is nothing to retry, so the only thing owed to
/// the operator is a truthful account of what was lost and why - the same
/// contract [`crate::mqtt::MqttShared`] keeps for a publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EgressError {
    /// There is no websocket session, so nothing was sent and nothing queued.
    NotConnected,
    /// There is a session, but the writer is not keeping up.
    Backpressure,
    /// Anything else the client reported.
    Failed(String),
}

impl EgressError {
    /// Whether the rest of the batch is lost too.
    ///
    /// All values of one message share a connection, so they share its fate:
    /// when the connection is the problem, sending the remainder would only
    /// produce the same answer once per value. A value that failed on its own
    /// merits does not condemn the others.
    pub(crate) fn aborts_batch(&self) -> bool {
        matches!(self, EgressError::NotConnected | EgressError::Backpressure)
    }
}

impl fmt::Display for EgressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EgressError::NotConnected => f.write_str("no websocket connection to the Miniserver"),
            EgressError::Backpressure => f.write_str("the Miniserver writer is not keeping up"),
            EgressError::Failed(detail) => f.write_str(detail),
        }
    }
}

pub(crate) trait Egress: Send + Sync + 'static {
    /// Whether a value stands a chance right now. Only used for diagnostics -
    /// the send itself reports [`EgressError::NotConnected`] rather than relying
    /// on a check that could be stale by the time it is acted on.
    fn connected(&self) -> bool;

    /// Put one value on the wire under its Miniserver input name.
    fn send(
        &self,
        uuid: &str,
        value: &str,
    ) -> impl Future<Output = Result<(), EgressError>> + Send;
}

/// An egress that keeps what it was handed, for the tests.
#[cfg(test)]
pub(crate) struct RecordingEgress {
    sent: std::sync::Mutex<Vec<(String, String)>>,
    connected: std::sync::atomic::AtomicBool,
    /// Input names that fail on their own merits, to exercise the branch where
    /// one bad value must not take the rest of the message with it.
    failing: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
impl RecordingEgress {
    pub(crate) fn new() -> Self {
        RecordingEgress {
            sent: std::sync::Mutex::new(Vec::new()),
            connected: std::sync::atomic::AtomicBool::new(true),
            failing: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn disconnected() -> Self {
        let egress = Self::new();
        egress
            .connected
            .store(false, std::sync::atomic::Ordering::Relaxed);
        egress
    }

    pub(crate) fn fail_on(&self, uuid: &str) {
        crate::util::lock_recover(&self.failing).push(uuid.to_string());
    }

    /// What was sent, in order, since the last [`Self::drain`].
    pub(crate) fn drain(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *crate::util::lock_recover(&self.sent))
    }
}

#[cfg(test)]
impl Egress for RecordingEgress {
    fn connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }

    async fn send(&self, uuid: &str, value: &str) -> Result<(), EgressError> {
        if !self.connected() {
            return Err(EgressError::NotConnected);
        }
        if crate::util::lock_recover(&self.failing).iter().any(|f| f == uuid) {
            return Err(EgressError::Failed(format!("refused '{uuid}'")));
        }
        crate::util::lock_recover(&self.sent).push((uuid.to_string(), value.to_string()));
        Ok(())
    }
}

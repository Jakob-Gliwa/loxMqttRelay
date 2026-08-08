//! How the relay asks itself to do something.
//!
//! Two things need saying from wherever they are noticed: a whitelist resync,
//! which the websocket lifecycle worker and the Miniserver's startup topic both
//! ask for, and a shutdown, which a signal and a config update both ask for.
//!
//! [`Signals`] is cheap to clone and every method on it is callable from any
//! task without blocking or failing. That is the requirement, not a
//! convenience: a lifecycle hook runs on the websocket's reader task and cannot
//! wait, and a control topic must not be able to fail because nobody happened
//! to be listening yet.

use std::sync::Arc;

use log::{debug, info};
use tokio::sync::{Notify, watch};

/// Something that can ask for a whitelist resync.
///
/// A trait so [`crate::miniserver::lifecycle_worker`] can say "this connection
/// came back" without knowing that the answer is a whole configuration
/// download, or who will do it.
pub(crate) trait ResyncTrigger: Send + Sync {
    fn request_resync(&self);
}

/// Why the relay is stopping, and whether it should come back.
#[derive(Clone, Debug)]
pub struct StopReason {
    pub reason: String,
    pub restart: bool,
}

/// The relay's two internal signals.
#[derive(Clone)]
pub struct Signals {
    /// Wakes the resync worker.
    ///
    /// Coalescing by construction: a request that arrives while one is pending
    /// is swallowed, which is what the bounded lifecycle channel already did
    /// with its `try_send`. Two resyncs back to back would download the same
    /// configuration twice.
    resync: Arc<Notify>,
    /// Carries the shutdown decision, exactly once.
    stop: watch::Sender<Option<StopReason>>,
}

impl Signals {
    pub fn new() -> (Self, watch::Receiver<Option<StopReason>>) {
        let (stop, stop_rx) = watch::channel(None);
        (
            Signals {
                resync: Arc::new(Notify::new()),
                stop,
            },
            stop_rx,
        )
    }

    /// Ask the relay to shut down. Safe to call more than once.
    ///
    /// The first call wins, including its restart decision - so a second
    /// SIGTERM arriving during a config-change shutdown cannot turn a restart
    /// into a plain stop, or the other way round.
    pub(crate) fn request_stop(&self, reason: impl Into<String>, restart: bool) {
        let reason = reason.into();
        self.stop.send_if_modified(|slot| {
            if slot.is_some() {
                info!("Shutdown already under way, ignoring: {reason}");
                return false;
            }
            info!("Shutting down: {reason}");
            *slot = Some(StopReason { reason, restart });
            true
        });
    }

    /// Wait for the next resync request.
    pub(crate) async fn resync_requested(&self) {
        self.resync.notified().await;
    }
}

impl ResyncTrigger for Signals {
    fn request_resync(&self) {
        debug!("Whitelist resync requested");
        self.resync.notify_one();
    }
}

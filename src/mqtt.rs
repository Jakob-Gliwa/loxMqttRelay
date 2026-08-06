//! MQTT 5 ingress and egress on top of mqtt-glide.
//!
//! Inbound messages never cross into Python: the connection read task hands
//! them to a bounded channel and a worker feeds
//! [`MiniserverDataProcessor::handle_mqtt_message`] directly. The UDP path in
//! [`crate::udp`] publishes through [`MqttShared`] without the GIL either;
//! Python keeps only the egress to the Miniserver.

use std::collections::VecDeque;
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use log::{debug, error, info, warn};
use mqtt_glide::{
    AppError, ClientBuilder, ClientId, LifecycleHook, MqttClient as GlideClient, PublishMessage,
    QoS, SessionPhase, StandardReconnectPolicy, StaticCredentials, SubscribeAckReason, Subscription,
};
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};
use pyo3_async_runtimes::TaskLocals;
use pyo3_async_runtimes::tokio::{future_into_py, get_runtime};
use tokio::sync::{Notify, mpsc};

use crate::MiniserverDataProcessor;

/// Inbound queue depth. Also drives the MQTT Receive Maximum so the broker's
/// window and our handoff buffer stay aligned.
const INBOUND_CAPACITY: usize = 4096;

/// Application-to-write-task admission. Glide defaults to 32, which is aimed at
/// many light connections rather than one busy relay.
const COMMAND_CAPACITY: usize = 1024;

const KEEP_ALIVE: Duration = Duration::from_secs(60);
const RECONNECT_DELAY: Duration = Duration::from_secs(15);
const MAX_AUTH_BACKOFF: Duration = Duration::from_secs(300);

/// How long the "Disconnecting" status message may hold up the shutdown.
const FAREWELL_TIMEOUT: Duration = Duration::from_secs(1);

/// How many dropped publishes to remember for diagnostics.
const UNDELIVERED_RING: usize = 32;

/// Why a publish never reached the broker.
///
/// Everything here is a message that is gone: at QoS 0 with no local queue
/// there is nothing to retry, so the only thing owed to the operator is a
/// truthful account of what was lost and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// There was no broker connection when the publish was attempted.
    Disconnected,
    /// The connection existed, but the publish did not make it onto the wire.
    SendFailed,
}

impl DropReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DropReason::Disconnected => "broker not connected",
            DropReason::SendFailed => "publish failed",
        }
    }
}

/// A publish that was lost, kept for diagnostics.
struct Undelivered {
    topic: String,
    payload: Vec<u8>,
    reason: DropReason,
}

/// Connection state shared between the Python-facing [`MqttClient`] and
/// [`MiniserverDataProcessor`], which answers `config/get` without a detour
/// through Python.
pub struct MqttShared {
    client: ArcSwapOption<GlideClient>,
    /// Whether there is a live MQTT session, maintained by [`ConnectSignal`].
    ///
    /// The client handle alone does not say this: it outlives every reconnect,
    /// so it only records that [`MqttClient::connect`] once succeeded.
    session_live: AtomicBool,
    undelivered: Mutex<VecDeque<Undelivered>>,
}

impl MqttShared {
    /// A shared state with no connection yet - which is also what the UDP
    /// tests need to exercise the "the broker was not there" branch.
    pub(crate) fn new() -> Self {
        Self {
            client: ArcSwapOption::empty(),
            session_live: AtomicBool::new(false),
            undelivered: Mutex::new(VecDeque::with_capacity(UNDELIVERED_RING)),
        }
    }

    /// The client handle, but only while a session is actually up.
    ///
    /// Glide keeps accepting publishes into its command channel across a
    /// reconnect and lets them run into the acknowledgement timeout, which
    /// would report a message lost seconds later and under the wrong reason.
    /// At QoS 0 with no outbox there is nothing to gain from that wait, so a
    /// publish without a session is refused here and now.
    fn live_client(&self) -> Option<Arc<GlideClient>> {
        if self.session_live.load(Ordering::Acquire) {
            self.client.load_full()
        } else {
            None
        }
    }

    /// The client handle regardless of session state, for shutting it down.
    fn handle(&self) -> Option<Arc<GlideClient>> {
        self.client.load_full()
    }

    /// Logs and remembers a publish that was lost.
    ///
    /// `detail` carries the broker/transport error where there is one; the
    /// reason alone would not tell an operator whether to look at the network
    /// or at the broker.
    fn record_drop(&self, topic: &str, payload: &[u8], reason: DropReason, detail: Option<&str>) {
        match detail {
            Some(detail) => warn!(
                "Dropped MQTT publish to '{}' ({} bytes): {} - {}",
                topic,
                payload.len(),
                reason.as_str(),
                detail
            ),
            None => warn!(
                "Dropped MQTT publish to '{}' ({} bytes): {}",
                topic,
                payload.len(),
                reason.as_str()
            ),
        }

        let Ok(mut ring) = self.undelivered.lock() else {
            return;
        };
        if ring.len() >= UNDELIVERED_RING {
            ring.pop_front();
        }
        ring.push_back(Undelivered {
            topic: topic.to_owned(),
            payload: payload.to_vec(),
            reason,
        });
    }

    async fn publish_on(
        client: &GlideClient,
        topic: &str,
        payload: &[u8],
        retain: bool,
        user_properties: Vec<(String, String)>,
    ) -> Result<(), AppError> {
        let mut publish = client
            .publish_with()
            .topic(topic.to_owned())
            .payload(payload.to_vec())
            .qos(QoS::AtMostOnce)
            .retain(retain);
        for (key, value) in user_properties {
            publish = publish.user_property(key, value);
        }
        publish.send().await
    }

    /// Publishes, or reports why the message was lost. Never fails silently.
    pub(crate) async fn publish(
        &self,
        topic: String,
        payload: Vec<u8>,
        retain: bool,
        user_properties: Vec<(String, String)>,
    ) -> Option<DropReason> {
        let Some(client) = self.live_client() else {
            self.record_drop(&topic, &payload, DropReason::Disconnected, None);
            return Some(DropReason::Disconnected);
        };
        match Self::publish_on(&client, &topic, &payload, retain, user_properties).await {
            Ok(()) => None,
            Err(e) => {
                self.record_drop(&topic, &payload, DropReason::SendFailed, Some(&e.to_string()));
                Some(DropReason::SendFailed)
            }
        }
    }

    /// Fire-and-forget publish for callers that hold the GIL and must not await.
    ///
    /// The disconnected case is settled here rather than in the spawned task, so
    /// the drop is visible to the caller as soon as this returns.
    pub(crate) fn publish_detached(self: &Arc<Self>, topic: String, payload: Vec<u8>) {
        let Some(client) = self.live_client() else {
            self.record_drop(&topic, &payload, DropReason::Disconnected, None);
            return;
        };
        let shared = Arc::clone(self);
        get_runtime().spawn(async move {
            let result = Self::publish_on(&client, &topic, &payload, false, Vec::new()).await;
            if let Err(e) = result {
                shared.record_drop(&topic, &payload, DropReason::SendFailed, Some(&e.to_string()));
            }
        });
    }
}

/// Tracks whether a session is up, and wakes the resubscribe task on every
/// session start.
///
/// The hook runs on the driver task and must not await, so it only sets a flag
/// and signals; the actual SUBSCRIBE happens in [`resubscribe_loop`].
///
/// Glide emits `Connected` before it returns from the initial connect and
/// again after every reconnect, and `Disconnected` before it enters the
/// reconnect backoff - which is exactly the window in which a publish has
/// nowhere to go.
struct ConnectSignal {
    shared: Arc<MqttShared>,
    notify: Arc<Notify>,
}

impl LifecycleHook for ConnectSignal {
    fn on_lifecycle(&self, phase: &SessionPhase) {
        match phase {
            SessionPhase::Connected { generation, .. } => {
                self.shared.session_live.store(true, Ordering::Release);
                info!("MQTT connected (session {generation})");
                self.notify.notify_one();
            }
            SessionPhase::Disconnected(outcome) => {
                self.shared.session_live.store(false, Ordering::Release);
                warn!(
                    "MQTT disconnected: {outcome:?} - publishes are dropped while there is no \
                     connection"
                );
            }
            SessionPhase::NeverConnected => {}
        }
    }
}

fn is_granted(reason: SubscribeAckReason) -> bool {
    matches!(
        reason,
        SubscribeAckReason::GrantedQos0
            | SubscribeAckReason::GrantedQos1
            | SubscribeAckReason::GrantedQos2
    )
}

/// Subscribes and republishes the status topic after every (re)connect.
///
/// This is what gmqtt's `on_connect` callback used to do. Glide restores
/// subscriptions on a reconnect by itself, but only those its own registry
/// knows, and that registry is filled from the SUBACKs of application
/// subscribes - so the first pass here is what puts the filters in place at
/// all. The later passes repeat a SUBSCRIBE glide has already sent, which the
/// broker treats as a replacement; they are kept because this is also where
/// the status topic is republished and the reconnect policy is marked live.
async fn resubscribe_loop(
    shared: Arc<MqttShared>,
    topics: Vec<String>,
    status_topic: String,
    notify: Arc<Notify>,
    reconnect: StandardReconnectPolicy,
) {
    loop {
        notify.notified().await;

        let Some(client) = shared.live_client() else {
            debug!("Resubscribe skipped: the session was gone again already");
            continue;
        };

        let filters: Vec<Subscription> = topics
            .iter()
            .map(|topic| Subscription::new(topic.clone(), QoS::AtMostOnce))
            .collect();

        info!("Subscribing to {} topic(s)", filters.len());
        match client.subscribe(filters).await {
            Ok(reasons) => {
                for (topic, reason) in topics.iter().zip(reasons) {
                    if !is_granted(reason) {
                        // Nothing arriving on this filter is a silent hole
                        // otherwise: the relay looks healthy and simply never
                        // sees those messages.
                        error!(
                            "Broker rejected subscription to '{topic}': {reason:?} - no messages \
                             will be received on it"
                        );
                    }
                }
                // Only arm the fast reconnect retry once the session is really
                // usable, i.e. after the subscriptions are in place.
                reconnect.mark_live();
            }
            Err(e) => {
                error!(
                    "Subscribe failed: {e} - no messages will be received until the next reconnect"
                );
                continue;
            }
        }

        shared
            .publish(status_topic.clone(), b"Connected".to_vec(), false, Vec::new())
            .await;
    }
}

/// Drains the inbound channel and drives the Rust message handler.
///
/// One `Python::attach` per message. Batching attaches was measured at ~0.03 µs
/// saved per message against a ~0.28 µs handle path — not worth the complexity.
///
/// Runs inside a `pyo3_async_runtimes::tokio::scope` so the task locals of the
/// asyncio loop are available when the handler hands coroutines back to Python.
async fn ingress_worker(
    mut rx: mpsc::Receiver<PublishMessage>,
    processor: Py<MiniserverDataProcessor>,
) {
    while let Some(message) = rx.recv().await {
        // ByteString is refcounted, so keeping the topic for the error branch
        // costs an atomic rather than a second allocation per message.
        let received_on = message.topic.clone();
        let topic = String::from(message.topic);
        let payload = message.payload.to_vec();
        Python::attach(|py| {
            let bound = processor.bind(py);
            if let Err(e) = bound.borrow().handle_mqtt_message(py, topic, payload) {
                error!("Dropped inbound message on '{received_on}': {e}");
            }
        });
    }

    info!("MQTT ingress worker stopped");
}

fn config_value<'py>(
    config: &Bound<'py, PyAny>,
    section: &str,
    field: &str,
) -> PyResult<Bound<'py, PyAny>> {
    config.getattr(section)?.getattr(field)
}

/// MQTT 5 client handle exposed to Python.
///
/// Construct it before [`MiniserverDataProcessor`], which shares its connection
/// state, then call [`MqttClient::connect`] once the processor exists.
#[pyclass]
pub struct MqttClient {
    shared: Arc<MqttShared>,
    reconnect: StandardReconnectPolicy,
    broker_url: String,
    client_id_prefix: String,
    status_topic: String,
    credentials: Option<(String, String)>,
}

impl MqttClient {
    pub(crate) fn shared(&self) -> Arc<MqttShared> {
        Arc::clone(&self.shared)
    }
}

#[pymethods]
impl MqttClient {
    #[new]
    #[pyo3(text_signature = "(self, global_config)")]
    fn new(global_config: &Bound<'_, PyAny>) -> PyResult<Self> {
        let host: String = config_value(global_config, "broker", "host")?.extract()?;
        let port: u16 = config_value(global_config, "broker", "port")?.extract()?;
        let user: Option<String> = config_value(global_config, "broker", "user")?.extract()?;
        let password: Option<String> =
            config_value(global_config, "broker", "password")?.extract()?;
        let client_id: String = config_value(global_config, "broker", "client_id")?.extract()?;
        let base_topic: String = config_value(global_config, "general", "base_topic")?.extract()?;

        // Glide appends a UUID, so the configured id acts as a prefix. That keeps
        // ids unique across restarts, like the old timestamp suffix did.
        let credentials = user.filter(|u| !u.is_empty()).map(|u| (u, password.unwrap_or_default()));

        Ok(Self {
            shared: Arc::new(MqttShared::new()),
            reconnect: StandardReconnectPolicy::new(RECONNECT_DELAY, MAX_AUTH_BACKOFF),
            broker_url: format!("mqtt://{host}:{port}"),
            client_id_prefix: client_id,
            status_topic: format!("{base_topic}status"),
            credentials,
        })
    }

    /// Whether an MQTT session is up right now.
    ///
    /// False while glide is reconnecting, which is also when a publish is
    /// reported as dropped rather than queued.
    #[getter]
    fn connected(&self) -> bool {
        self.shared.live_client().is_some()
    }

    /// Connects, subscribes to `topics` and starts routing inbound messages
    /// into `processor`.
    #[pyo3(text_signature = "(self, topics, processor)")]
    fn connect<'py>(
        &self,
        py: Python<'py>,
        topics: Vec<String>,
        processor: Py<MiniserverDataProcessor>,
    ) -> PyResult<Bound<'py, PyAny>> {
        // Captured here, on the asyncio thread: the ingress worker runs on tokio
        // and would otherwise find no running loop to hand coroutines back to.
        let locals = TaskLocals::with_running_loop(py)?.copy_context(py)?;

        let shared = Arc::clone(&self.shared);
        let reconnect = self.reconnect.clone();
        let url = self.broker_url.clone();
        let status_topic = self.status_topic.clone();
        let client_id = ClientId::from_prefix(&self.client_id_prefix);
        let credentials = self.credentials.clone();

        future_into_py(py, async move {
            let (tx, rx) = mpsc::channel::<PublishMessage>(INBOUND_CAPACITY);
            let notify = Arc::new(Notify::new());

            let receive_maximum = NonZeroU16::new(
                u16::try_from(INBOUND_CAPACITY).unwrap_or(u16::MAX),
            )
            .expect("non-zero receive maximum");
            let command_capacity =
                NonZeroUsize::new(COMMAND_CAPACITY).expect("non-zero command capacity");

            let mut builder = ClientBuilder::new(&url)
                .map_err(|e| PyRuntimeError::new_err(format!("Invalid broker url '{url}': {e}")))?
                .inbound_sink(tx)
                .reconnect(reconnect.clone())
                .lifecycle(ConnectSignal {
                    shared: Arc::clone(&shared),
                    notify: Arc::clone(&notify),
                })
                .options(move |options| {
                    options
                        .client_id(client_id)
                        .keep_alive(KEEP_ALIVE)
                        .receive_maximum(receive_maximum)
                        .command_channel_capacity(command_capacity)
                        .tcp_nodelay(true)
                });

            if let Some((user, password)) = credentials {
                builder = builder.credentials(StaticCredentials::new(user, password));
            }

            info!("Connecting to MQTT broker at {url}");
            let client = builder
                .connect()
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("MQTT connect failed: {e}")))?;
            shared.client.store(Some(Arc::new(client)));

            get_runtime().spawn(pyo3_async_runtimes::tokio::scope(
                locals,
                ingress_worker(rx, processor),
            ));
            get_runtime().spawn(resubscribe_loop(
                Arc::clone(&shared),
                topics,
                status_topic,
                notify,
                reconnect,
            ));

            Ok(())
        })
    }

    /// Publishes at QoS 0 and reports the outcome.
    ///
    /// Returns `None` when the message was handed to the broker connection, or
    /// the reason it was dropped. Delivery problems are reported this way
    /// rather than raised: the UDP path publishes from a detached task, where
    /// an exception would only surface as an unretrieved task error.
    #[pyo3(signature = (topic, message, retain=false, user_properties=None))]
    #[pyo3(text_signature = "(self, topic, message, retain=False, user_properties=None)")]
    fn publish<'py>(
        &self,
        py: Python<'py>,
        topic: String,
        message: &Bound<'py, PyAny>,
        retain: bool,
        user_properties: Option<Vec<(String, String)>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        // Edition 2024 drops if-let temps before `else`; match keeps the borrow
        // scopes clear while we copy out owned bytes from either cast.
        let payload = match message.cast::<PyString>() {
            Ok(text) => text.to_cow()?.into_owned().into_bytes(),
            Err(_) => match message.cast::<PyBytes>() {
                Ok(raw) => raw.as_bytes().to_vec(),
                Err(_) => {
                    return Err(PyTypeError::new_err("message must be str or bytes"));
                }
            },
        };

        let shared = Arc::clone(&self.shared);
        let properties = user_properties.unwrap_or_default();

        future_into_py(py, async move {
            Ok(shared
                .publish(topic, payload, retain, properties)
                .await
                .map(DropReason::as_str))
        })
    }

    /// Says goodbye on the status topic, then closes the session.
    ///
    /// The farewell is given a deadline of its own. A socket can be dead
    /// without glide knowing yet - the keep-alive runs for a minute - and the
    /// publish would then sit out the acknowledgement timeout. `docker stop`
    /// allows ten seconds for the whole shutdown, and a status message nobody
    /// is left to receive must not eat into them.
    #[pyo3(text_signature = "(self)")]
    fn disconnect<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let shared = Arc::clone(&self.shared);
        let status_topic = self.status_topic.clone();

        future_into_py(py, async move {
            if let Some(client) = shared.handle() {
                let farewell =
                    shared.publish(status_topic, b"Disconnecting".to_vec(), false, Vec::new());
                let announced = tokio::time::timeout(FAREWELL_TIMEOUT, farewell).await;
                if announced.is_err() {
                    warn!("Timed out announcing the shutdown on the status topic");
                }
                if let Err(e) = client.shutdown().await {
                    warn!("Error during MQTT shutdown: {e}");
                }
            }
            shared.session_live.store(false, Ordering::Release);
            shared.client.store(None);
            Ok(())
        })
    }

    /// Drains the last dropped publishes as `(topic, payload, reason)`.
    #[pyo3(text_signature = "(self)")]
    fn take_undelivered(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyBytes>, &'static str)>> {
        let Ok(mut ring) = self.shared.undelivered.lock() else {
            return Ok(Vec::new());
        };
        Ok(ring
            .drain(..)
            .map(|entry| {
                (
                    entry.topic,
                    PyBytes::new(py, &entry.payload).unbind(),
                    entry.reason.as_str(),
                )
            })
            .collect())
    }
}

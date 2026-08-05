//! MQTT 5 ingress and egress on top of mqtt-glide.
//!
//! Inbound messages never cross into Python: the connection read task hands
//! them to a bounded channel and a worker feeds
//! [`MiniserverDataProcessor::handle_mqtt_message`] directly. Python keeps the
//! egress to the Miniserver and the UDP publish path.

use std::collections::VecDeque;
use std::num::{NonZeroU16, NonZeroUsize};
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

/// How many disconnected publishes to remember for diagnostics.
const UNDELIVERED_RING: usize = 32;

/// Connection state shared between the Python-facing [`MqttClient`] and
/// [`MiniserverDataProcessor`], which answers `config/get` without a detour
/// through Python.
pub struct MqttShared {
    client: ArcSwapOption<GlideClient>,
    undelivered: Mutex<VecDeque<(String, Vec<u8>)>>,
}

impl MqttShared {
    fn new() -> Self {
        Self {
            client: ArcSwapOption::empty(),
            undelivered: Mutex::new(VecDeque::with_capacity(UNDELIVERED_RING)),
        }
    }

    fn client(&self) -> Option<Arc<GlideClient>> {
        self.client.load_full()
    }

    /// Records a publish that had nowhere to go. The message is dropped either
    /// way, matching the previous gmqtt behaviour; the ring only makes the loss
    /// observable.
    fn record_undelivered(&self, topic: &str, payload: &[u8]) {
        let Ok(mut ring) = self.undelivered.lock() else {
            return;
        };
        if ring.len() >= UNDELIVERED_RING {
            ring.pop_front();
        }
        ring.push_back((topic.to_owned(), payload.to_vec()));
    }

    async fn publish_on(
        client: &GlideClient,
        topic: String,
        payload: Vec<u8>,
        retain: bool,
        user_properties: Vec<(String, String)>,
    ) -> Result<(), AppError> {
        let mut publish = client
            .publish_with()
            .topic(topic)
            .payload(payload)
            .qos(QoS::AtMostOnce)
            .retain(retain);
        for (key, value) in user_properties {
            publish = publish.user_property(key, value);
        }
        publish.send().await
    }

    async fn publish(
        &self,
        topic: String,
        payload: Vec<u8>,
        retain: bool,
        user_properties: Vec<(String, String)>,
    ) -> Result<(), AppError> {
        let Some(client) = self.client() else {
            warn!("MQTT publish without connection, dropping: {topic}");
            self.record_undelivered(&topic, &payload);
            return Ok(());
        };
        Self::publish_on(&client, topic, payload, retain, user_properties).await
    }

    /// Fire-and-forget publish for callers that hold the GIL and must not await.
    ///
    /// The disconnected case is settled here rather than in the spawned task, so
    /// the drop is visible to the caller as soon as this returns.
    pub(crate) fn publish_detached(&self, topic: String, payload: Vec<u8>) {
        let Some(client) = self.client() else {
            warn!("MQTT publish without connection, dropping: {topic}");
            self.record_undelivered(&topic, &payload);
            return;
        };
        get_runtime().spawn(async move {
            if let Err(e) = Self::publish_on(&client, topic, payload, false, Vec::new()).await {
                error!("MQTT publish failed: {e}");
            }
        });
    }
}

/// Wakes the resubscribe task on every session start.
///
/// The hook runs on the driver task and must not await, so it only signals;
/// the actual SUBSCRIBE happens in [`resubscribe_loop`].
struct ConnectSignal {
    notify: Arc<Notify>,
}

impl LifecycleHook for ConnectSignal {
    fn on_lifecycle(&self, phase: &SessionPhase) {
        match phase {
            SessionPhase::Connected { generation, .. } => {
                info!("MQTT connected (session {generation})");
                self.notify.notify_one();
            }
            SessionPhase::Disconnected(outcome) => {
                warn!("MQTT disconnected: {outcome:?}");
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

/// Re-subscribes and republishes the status topic after every (re)connect.
///
/// Glide reconnects transparently but does not restore subscriptions, so this
/// replaces what gmqtt did in its `on_connect` callback.
async fn resubscribe_loop(
    shared: Arc<MqttShared>,
    topics: Vec<String>,
    status_topic: String,
    notify: Arc<Notify>,
    reconnect: StandardReconnectPolicy,
) {
    loop {
        notify.notified().await;

        let Some(client) = shared.client() else {
            debug!("Resubscribe skipped: no client handle");
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
                        error!("Subscription to '{topic}' rejected: {reason:?}");
                    }
                }
                // Only arm the fast reconnect retry once the session is really
                // usable, i.e. after the subscriptions are in place.
                reconnect.mark_live();
            }
            Err(e) => {
                error!("Subscribe failed: {e}");
                continue;
            }
        }

        if let Err(e) = shared
            .publish(status_topic.clone(), b"Connected".to_vec(), false, Vec::new())
            .await
        {
            warn!("Failed to publish connection status: {e}");
        }
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
        let topic = String::from(message.topic);
        let payload = message.payload.to_vec();
        Python::attach(|py| {
            let bound = processor.bind(py);
            if let Err(e) = bound.borrow().handle_mqtt_message(py, topic, payload) {
                error!("handle_mqtt_message failed: {e}");
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

    #[getter]
    fn connected(&self) -> bool {
        self.shared.client().is_some()
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
        let payload = if let Ok(text) = message.cast::<PyString>() {
            text.to_cow()?.into_owned().into_bytes()
        } else if let Ok(raw) = message.cast::<PyBytes>() {
            raw.as_bytes().to_vec()
        } else {
            return Err(PyTypeError::new_err("message must be str or bytes"));
        };

        let shared = Arc::clone(&self.shared);
        let properties = user_properties.unwrap_or_default();

        future_into_py(py, async move {
            shared
                .publish(topic, payload, retain, properties)
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("MQTT publish failed: {e}")))
        })
    }

    #[pyo3(text_signature = "(self)")]
    fn disconnect<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let shared = Arc::clone(&self.shared);
        let status_topic = self.status_topic.clone();

        future_into_py(py, async move {
            if let Some(client) = shared.client() {
                if let Err(e) = shared
                    .publish(status_topic, b"Disconnecting".to_vec(), false, Vec::new())
                    .await
                {
                    warn!("Failed to publish disconnect status: {e}");
                }
                if let Err(e) = client.shutdown().await {
                    warn!("Error during MQTT shutdown: {e}");
                }
            }
            shared.client.store(None);
            Ok(())
        })
    }

    /// Drains publishes that were attempted while disconnected.
    #[pyo3(text_signature = "(self)")]
    fn take_undelivered(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyBytes>)>> {
        let Ok(mut ring) = self.shared.undelivered.lock() else {
            return Ok(Vec::new());
        };
        Ok(ring
            .drain(..)
            .map(|(topic, payload)| (topic, PyBytes::new(py, &payload).unbind()))
            .collect())
    }
}

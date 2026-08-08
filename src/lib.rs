//! The relay's Rust half.
//!
//! MQTT ingress ([`mqtt`]), the Miniserver websocket ([`miniserver`]), the UDP
//! listener ([`udp`]) and the processing core ([`process`]) all live here, and
//! the data path between them never touches Python. What is left in this file
//! is the boundary: the `#[pyclass]` Python constructs, the control topics that
//! genuinely have to reach `global_config` and `orjson`, and the module.
//!
//! [`MiniserverDataProcessor`] is deliberately thin. Everything about
//! flattening, filtering and forwarding sits in [`process::Core`], which knows
//! nothing about Python and is therefore also what the Rust tests exercise.

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::Arc;

use log::debug;
use pyo3::exceptions::PyValueError;
use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyDict;

mod control;
mod egress;
mod miniserver;
mod mqtt;
mod process;
mod udp;
mod util;

use control::PyControlSink;
use miniserver::{LoxEgress, MiniserverClient};
use mqtt::MqttClient;
use process::{Core, CoreConfig, FilterError, MqttTopics};
use udp::UdpServer;

/// `obj.a.b.c` with interned attribute names.
macro_rules! pyget {
    ($obj:expr, $py:expr, $($attr:expr),+) => {{
        let mut obj = $obj.bind($py).as_borrowed().to_owned();
        $( obj = obj.getattr(intern!($py, $attr))?; )*
        obj
    }};
}

impl From<FilterError> for PyErr {
    fn from(error: FilterError) -> PyErr {
        PyValueError::new_err(error.0)
    }
}

/// The Python-facing handle on the processing core.
///
/// Holds only what has to be reached across the language boundary. The data
/// path goes straight to [`Core`] from [`mqtt::ingress_worker`] and never
/// arrives here.
#[pyclass]
pub struct MiniserverDataProcessor {
    #[pyo3(get)]
    global_config: Py<PyAny>,
    /// Everything the control topics still need from Python. Behind a trait so
    /// [`mqtt::ingress_worker`] does not know a Python object is on the other
    /// side - see [`control`].
    sink: Arc<PyControlSink>,
    core: Arc<Core<LoxEgress>>,
}

#[pymethods]
impl MiniserverDataProcessor {
    #[new]
    #[pyo3(
        text_signature = "(self, topic_ns, global_config_py, relay_main_obj, mqtt_client, miniserver_client, orjson_obj)"
    )]
    fn new(
        py: Python,
        topic_ns: Py<PyAny>,
        global_config_py: Py<PyAny>,
        relay_main_obj: Py<PyAny>,
        mqtt_client: PyRef<'_, MqttClient>,
        miniserver_client: PyRef<'_, MiniserverClient>,
        orjson_obj: Py<PyAny>,
    ) -> PyResult<Self> {
        let cache_size: usize = pyget!(global_config_py, py, "general", "cache_size").extract()?;
        debug!("Initializing MiniserverDataProcessor with cache_size={cache_size}");

        // topic_whitelist may be a Python set, frozenset or list depending on how
        // config built it. pyo3's Vec extraction rejects sets ("not a Sequence"),
        // so iterate any iterable instead.
        let mut topic_whitelist = HashSet::new();
        for item in pyget!(global_config_py, py, "topics", "topic_whitelist").try_iter()? {
            topic_whitelist.insert(item?.extract::<String>()?);
        }

        let bound_ns = topic_ns.bind(py);
        let topics = MqttTopics {
            miniserver_startup: bound_ns
                .getattr(intern!(py, "MINISERVER_STARTUP_EVENT"))?
                .extract()?,
            config_get: bound_ns.getattr(intern!(py, "CONFIG_GET"))?.extract()?,
            config_response: bound_ns.getattr(intern!(py, "CONFIG_RESPONSE"))?.extract()?,
            config_set: bound_ns.getattr(intern!(py, "CONFIG_SET"))?.extract()?,
            config_add: bound_ns.getattr(intern!(py, "CONFIG_ADD"))?.extract()?,
            config_remove: bound_ns.getattr(intern!(py, "CONFIG_REMOVE"))?.extract()?,
            config_update: bound_ns.getattr(intern!(py, "CONFIG_UPDATE"))?.extract()?,
            config_restart: bound_ns.getattr(intern!(py, "CONFIG_RESTART"))?.extract()?,
        };

        let config = CoreConfig {
            topics,
            subscription_filters: pyget!(global_config_py, py, "topics", "subscription_filters")
                .extract()?,
            do_not_forward: pyget!(global_config_py, py, "topics", "do_not_forward").extract()?,
            topic_whitelist,
            // Sync on ⇒ the Miniserver's virtual inputs are the allow-list.
            // An empty set must then block, not open the floodgates.
            whitelist_required: pyget!(
                global_config_py,
                py,
                "miniserver",
                "sync_with_miniserver"
            )
            .extract()?,
            cache_size,
            expand_json: pyget!(global_config_py, py, "processing", "expand_json").extract()?,
            convert_booleans: pyget!(global_config_py, py, "processing", "convert_booleans")
                .extract()?,
        };

        let core = Core::new(config, LoxEgress::new(miniserver_client.shared()))?;
        let sink = PyControlSink::new(
            global_config_py.clone_ref(py),
            relay_main_obj,
            mqtt_client.shared(),
            orjson_obj,
            core.topics().config_response.clone(),
        );
        debug!("MiniserverDataProcessor initialization complete");
        Ok(MiniserverDataProcessor {
            global_config: global_config_py,
            sink: Arc::new(sink),
            core: Arc::new(core),
        })
    }

    #[getter]
    fn topic_whitelist(&self) -> HashSet<String> {
        self.core.topic_whitelist()
    }

    #[getter]
    fn convert_booleans(&self) -> bool {
        self.core.convert_booleans()
    }

    #[pyo3(text_signature = "(self, filters)")]
    fn update_subscription_filters(&self, filters: Vec<String>) -> PyResult<()> {
        Ok(self.core.update_subscription_filters(filters)?)
    }

    #[pyo3(text_signature = "(self, whitelist)")]
    fn update_topic_whitelist(&self, whitelist: Vec<String>) {
        self.core.update_topic_whitelist(whitelist);
    }

    #[pyo3(text_signature = "(self, filters)")]
    fn update_do_not_forward(&self, filters: Vec<String>) -> PyResult<()> {
        Ok(self.core.update_do_not_forward(filters)?)
    }

    #[pyo3(text_signature = "(self)")]
    fn get_subscription_filters(&self) -> Vec<String> {
        self.core.subscription_filters()
    }

    #[pyo3(text_signature = "(self)")]
    fn get_do_not_forward_patterns(&self) -> Vec<String> {
        self.core.do_not_forward_patterns()
    }

    /// The keyword mapping itself, always applied.
    ///
    /// `processing.convert_booleans` decides whether the forwarding path calls
    /// the mapping at all; it stays available on its own so both settings can be
    /// described against the same reference.
    #[pyo3(text_signature = "(self, val)")]
    fn _convert_boolean(&self, val: &str) -> String {
        self.core.convert_boolean(val)
    }

    /// The Miniserver input name for a topic.
    #[pyo3(text_signature = "(self, topic)")]
    fn normalize_topic(&self, topic: &str) -> String {
        self.core.normalize_topic(topic)
    }

    #[pyo3(text_signature = "(self, topic)")]
    fn is_in_whitelist(&self, topic: &str) -> bool {
        self.core.is_in_whitelist(topic)
    }

    /// Handle `topic` if it is one of the relay's control topics.
    ///
    /// Reports whether it was. The data path does not come through here: it is
    /// recognised in Rust by [`Core::control_kind`] and stays there, which is
    /// what keeps the GIL out of it.
    #[pyo3(text_signature = "(self, topic, message)")]
    fn handle_control(&self, py: Python<'_>, topic: &str, message: &[u8]) -> PyResult<bool> {
        let Some(kind) = self.core.control_kind(topic) else {
            return Ok(false);
        };
        self.sink.dispatch_py(py, kind, message)?;
        Ok(true)
    }

    /// Route every message through the DOM path, bypassing learned plans.
    ///
    /// A kill switch: the differential tests that used to need it now run
    /// against [`Core`] directly.
    #[pyo3(text_signature = "(self, enabled)")]
    fn set_shape_cache_enabled(&self, enabled: bool) {
        self.core.set_shape_cache_enabled(enabled);
    }

    /// `(cached_plans, plans_built)` - how many topics currently hold a plan,
    /// and how many plans were built since construction.
    ///
    /// A plan is only stored once it has flattened a document end to end, so a
    /// message that neither raises the build count nor leaves a plan behind
    /// went down the DOM route. A relay forwarding steady JSON builds one plan
    /// per topic and then stops counting; a build count that keeps climbing
    /// means the payloads carry something the scanner refuses - escapes,
    /// duplicate keys, numbers that would not render identically.
    #[pyo3(text_signature = "(self)")]
    fn get_shape_stats(&self) -> (usize, u64) {
        self.core.shape_stats()
    }

    /// Everything the shape cache counts, as a dict.
    ///
    /// - `plans`, `learns` - as in `get_shape_stats`
    /// - `hits` - messages a stored plan carried end to end
    /// - `learn_failures` - documents no plan could be built for
    /// - `dom_fallbacks` - messages that reached the plan route and left it
    /// - `negative_skips` - of those, the ones held back without even trying
    /// - `unplannable` - topics currently held back
    ///
    /// All of them count only messages the plan route was offered at all, so
    /// `hits + learns + dom_fallbacks` is that population; a message with
    /// `expand_json` off, one whose payload is not a JSON object, or anything at
    /// all while the cache is switched off never appears here.
    #[pyo3(text_signature = "(self)")]
    fn get_shape_metrics<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let m = self.core.shape_metrics();
        let metrics = PyDict::new(py);
        metrics.set_item(intern!(py, "plans"), m.plans)?;
        metrics.set_item(intern!(py, "learns"), m.learns)?;
        metrics.set_item(intern!(py, "hits"), m.hits)?;
        metrics.set_item(intern!(py, "learn_failures"), m.learn_failures)?;
        metrics.set_item(intern!(py, "dom_fallbacks"), m.dom_fallbacks)?;
        metrics.set_item(intern!(py, "negative_skips"), m.negative_skips)?;
        metrics.set_item(intern!(py, "unplannable"), m.unplannable)?;
        Ok(metrics)
    }
}

/// Internals that are not part of the Python surface.
impl MiniserverDataProcessor {
    pub(crate) fn core(&self) -> Arc<Core<LoxEgress>> {
        Arc::clone(&self.core)
    }

    /// The control-topic handler, for [`mqtt::ingress_worker`] to hold directly.
    ///
    /// Handed over as the trait object rather than the concrete type: the worker
    /// has no business knowing that acting on a control topic currently means
    /// taking the GIL.
    pub(crate) fn sink(&self) -> Arc<dyn control::ControlSink> {
        Arc::clone(&self.sink) as Arc<dyn control::ControlSink>
    }
}

/// Route Rust's `log` output through env_logger, at the level Python configured.
///
/// Without this the whole Rust half is silent - including the only warning the
/// relay emits about dropped messages. `LOG_LEVEL` now reaches the Rust side
/// too, and `RUST_LOG` still overrides it for anyone who wants to turn a single
/// module up.
///
/// Returns whether this call installed the logger. `False` means one was
/// already in place and the level handed in here had no effect at all - worth
/// saying out loud rather than discarding, because the symptom otherwise is a
/// Rust half that logs at a level nobody asked for.
#[pyfunction]
#[pyo3(signature = (level = "INFO"))]
#[pyo3(text_signature = "(level='INFO')")]
fn init_rust_logger(level: &str) -> bool {
    let default = match level.to_ascii_uppercase().as_str() {
        "DEBUG" => "debug",
        "INFO" => "info",
        "WARNING" | "WARN" => "warn",
        // `log` has no level above error, so CRITICAL lands there as well.
        "ERROR" | "CRITICAL" => "error",
        _ => "info",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default))
        // Mirrors the Python format ('%(asctime)s %(levelname)s [%(name)s] ...'),
        // so both halves of the relay read as one log.
        .format(|buf, record| {
            writeln!(
                buf,
                "{} {} [{}] {}",
                buf.timestamp(),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .try_init()
        .is_ok()
}

#[pymodule]
fn _loxmqttrelay(_py: Python, m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Initialize the Tokio runtime for pyo3-asyncio.
    Python::initialize();
    let mut builder = pyo3_async_runtimes::tokio::re_exports::runtime::Builder::new_multi_thread();
    builder.enable_all();
    pyo3_async_runtimes::tokio::init(builder);
    m.add_class::<MiniserverDataProcessor>()?;
    m.add_class::<MiniserverClient>()?;
    m.add_class::<MqttClient>()?;
    m.add_class::<UdpServer>()?;
    m.add_function(wrap_pyfunction!(init_rust_logger, m)?)?;
    Ok(())
}

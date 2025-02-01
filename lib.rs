use pyo3::prelude::*;
use pyo3::types::{PyFrozenSet, PyTuple, IntoPyDict};
use regex::Regex;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

// For caching
use lru::LruCache;
use std::num::NonZeroUsize;

// For JSON flattening
use serde_json::Value;

// For logging
use log::{debug, error, info};

/// A struct representing some global config values
#[pyclass]
#[derive(Clone)]
pub struct GeneralConfig {
    #[pyo3(get, set)]
    pub cache_size: usize,
    #[pyo3(get, set)]
    pub base_topic: String,
}

#[pymethods]
impl GeneralConfig {
    #[new]
    fn new(cache_size: usize, base_topic: String) -> Self {
        GeneralConfig { cache_size, base_topic }
    }
}

#[pyclass]
#[derive(Clone)]
pub struct TopicsConfig {
    #[pyo3(get, set)]
    pub subscription_filters: Vec<String>,
    #[pyo3(get, set)]
    pub topic_whitelist: HashSet<String>,
}

#[pymethods]
impl TopicsConfig {
    #[new]
    fn new(subscription_filters: Vec<String>, topic_whitelist: HashSet<String>) -> Self {
        TopicsConfig { subscription_filters, topic_whitelist }
    }
}

#[pyclass]
#[derive(Clone)]
pub struct ProcessingConfig {
    #[pyo3(get, set)]
    pub expand_json: bool,
}

#[pymethods]
impl ProcessingConfig {
    #[new]
    fn new(expand_json: bool) -> Self {
        ProcessingConfig { expand_json }
    }
}

#[pyclass]
#[derive(Clone)]
pub struct DebugConfig {
    #[pyo3(get, set)]
    pub publish_processed_topics: bool,
}

#[pymethods]
impl DebugConfig {
    #[new]
    fn new(publish_processed_topics: bool) -> Self {
        DebugConfig { publish_processed_topics }
    }
}

#[pyclass]
#[derive(Clone)]
pub struct GlobalConfig {
    #[pyo3(get, set)]
    pub general: GeneralConfig,
    #[pyo3(get, set)]
    pub topics: TopicsConfig,
    #[pyo3(get, set)]
    pub processing: ProcessingConfig,
    #[pyo3(get, set)]
    pub debug: DebugConfig,
}

#[pymethods]
impl GlobalConfig {
    #[new]
    fn new(
        general: GeneralConfig,
        topics: TopicsConfig,
        processing: ProcessingConfig,
        debug: DebugConfig,
    ) -> Self {
        GlobalConfig {
            general,
            topics,
            processing,
            debug,
        }
    }
}

/// Convert a known boolean string to "1"/"0", or None if unrecognized.
fn convert_boolean_str(input: &str) -> Option<&'static str> {
    match input {
        "true" | "yes" | "on" | "enabled" | "enable" | "1"
        | "check" | "checked" | "select" | "selected" => Some("1"),
        "false" | "no" | "off" | "disabled" | "disable" | "0" => Some("0"),
        _ => None,
    }
}

/// Flatten a serde_json `Value` into `key/value` pairs using '/' as separator.
fn flatten_json(obj: &Value, prefix: &str, acc: &mut Vec<(String, String)>) {
    match obj {
        Value::Object(map) => {
            for (k, v) in map {
                let new_key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}/{}", prefix, k)
                };
                match v {
                    Value::Object(_) | Value::Array(_) => {
                        flatten_json(v, &new_key, acc);
                    }
                    Value::String(s) => {
                        acc.push((new_key, s.clone()));
                    }
                    Value::Number(num) => {
                        acc.push((new_key, num.to_string()));
                    }
                    Value::Bool(b) => {
                        acc.push((new_key, b.to_string()));
                    }
                    Value::Null => {
                        acc.push((new_key, "null".to_string()));
                    }
                }
            }
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                let new_key = if prefix.is_empty() {
                    i.to_string()
                } else {
                    format!("{}/{}", prefix, i)
                };
                match item {
                    Value::Object(_) | Value::Array(_) => {
                        flatten_json(item, &new_key, acc);
                    }
                    Value::String(s) => {
                        acc.push((new_key, s.clone()));
                    }
                    Value::Number(num) => {
                        acc.push((new_key, num.to_string()));
                    }
                    Value::Bool(b) => {
                        acc.push((new_key, b.to_string()));
                    }
                    Value::Null => {
                        acc.push((new_key, "null".to_string()));
                    }
                }
            }
        }
        _ => {}
    }
}

#[pyclass]
pub struct MiniserverDataProcessor {
    #[pyo3(get, set)]
    pub global_config: GlobalConfig,

    compiled_subscription_filter: Option<String>,
    do_not_forward_patterns: Option<String>,
    topic_whitelist: HashSet<String>,
    convert_bool_cache: Mutex<LruCache<String, String>>,
    normalize_topic_cache: Mutex<LruCache<String, String>>,
}

#[pymethods]
impl MiniserverDataProcessor {
    #[new]
    fn new(global_config: GlobalConfig) -> PyResult<Self> {
        debug!(
            "Initializing MiniserverDataProcessor with cache_size={}",
            global_config.general.cache_size
        );

        let compiled = Self::compile_filters(global_config.topics.subscription_filters.clone());
        let cache_size = if global_config.general.cache_size == 0 {
            64
        } else {
            global_config.general.cache_size
        };
        let lru_size = NonZeroUsize::new(cache_size).unwrap();

        let processor = MiniserverDataProcessor {
            compiled_subscription_filter: compiled,
            do_not_forward_patterns: None,
            topic_whitelist: global_config.topics.topic_whitelist.clone(),
            convert_bool_cache: Mutex::new(LruCache::new(lru_size)),
            normalize_topic_cache: Mutex::new(LruCache::new(lru_size)),
            global_config,
        };

        debug!("MiniserverDataProcessor initialization complete");
        Ok(processor)
    }

    #[staticmethod]
    fn compile_filters(filters: Vec<String>) -> Option<String> {
        if filters.is_empty() {
            debug!("No filters provided.");
            return None;
        }
        let mut valid_filters = Vec::new();
        for flt in filters {
            match Regex::new(&flt) {
                Ok(_) => {
                    debug!("Filter '{}' is valid", flt);
                    valid_filters.push(flt);
                }
                Err(e) => {
                    error!("Invalid filter '{}': {}", flt, e);
                }
            }
        }
        if valid_filters.is_empty() {
            debug!("No valid filters found.");
            return None;
        }
        let pattern = format!("({})", valid_filters.join("|"));
        Some(pattern)
    }

    #[pyo3(text_signature = "(self, filters)")]
    fn update_subscription_filters(&mut self, filters: Vec<String>) {
        debug!("Updating subscription filters: {:?}", filters);
        self.compiled_subscription_filter = Self::compile_filters(filters);
    }

    #[pyo3(text_signature = "(self, whitelist)")]
    fn update_topic_whitelist(&mut self, whitelist: Vec<String>) {
        let set: HashSet<String> = whitelist.into_iter().collect();
        debug!("Updating topic whitelist: {:?}", set);
        self.topic_whitelist = set;
    }

    #[pyo3(text_signature = "(self, filters)")]
    fn update_do_not_forward(&mut self, filters: Vec<String>) {
        debug!("Updating do_not_forward filters: {:?}", filters);
        self.do_not_forward_patterns = Self::compile_filters(filters);
    }

    #[pyo3(text_signature = "(self, val)")]
    fn _convert_boolean(&self, val: &str) -> PyResult<Option<String>> {
        let mut cache = self.convert_bool_cache.lock().unwrap();
        if let Some(cached) = cache.get(val) {
            return Ok(Some(cached.clone()));
        }
        if val.is_empty() {
            return Ok(Some(val.to_string()));
        }
        let normalized = val.trim().to_lowercase();
        if let Some(mapped) = convert_boolean_str(&normalized) {
            cache.put(val.to_string(), mapped.to_string());
            Ok(Some(mapped.to_string()))
        } else {
            cache.put(val.to_string(), val.to_string());
            Ok(Some(val.to_string()))
        }
    }

    #[pyo3(text_signature = "(self, topic)")]
    fn normalize_topic(&self, topic: &str) -> PyResult<String> {
        let mut cache = self.normalize_topic_cache.lock().unwrap();
        if let Some(cached) = cache.get(topic) {
            return Ok(cached.clone());
        }
        if !topic.contains('/') && !topic.contains('%') {
            cache.put(topic.to_string(), topic.to_string());
            return Ok(topic.to_string());
        }
        let normalized = topic.replace('/', "_").replace('%', "_");
        cache.put(topic.to_string(), normalized.clone());
        Ok(normalized)
    }

    #[pyo3(text_signature = "(self, topic, val)")]
    fn expand_json<'py>(&self, py: Python<'py>, topic: &str, val: &str) -> PyResult<&'py PyFrozenSet> {
        if val.is_empty() || ((!val.starts_with('{')) && (!val.starts_with('['))) {
            let tuple = (topic.to_string(), val.to_string());
            let set = PyFrozenSet::new(py, &[tuple])?;
            return Ok(set);
        }
        match serde_json::from_str::<Value>(val) {
            Ok(json_val) => {
                if !json_val.is_object() {
                    let tuple = (topic.to_string(), val.to_string());
                    let set = PyFrozenSet::new(py, &[tuple])?;
                    return Ok(set);
                }
                let mut flattened = Vec::new();
                flatten_json(&json_val, "", &mut flattened);
                let results: Vec<(String, String)> = flattened
                    .into_iter()
                    .map(|(k, v)| (format!("{}/{}", topic, k), v))
                    .collect();
                let set = PyFrozenSet::new(py, &results)?;
                Ok(set)
            }
            Err(_) => {
                let tuple = (topic.to_string(), val.to_string());
                let set = PyFrozenSet::new(py, &[tuple])?;
                Ok(set)
            }
        }
    }

    #[pyo3(text_signature = "(self, topic)")]
    fn is_in_whitelist(&self, topic: &str) -> PyResult<bool> {
        let normalized = self.normalize_topic(topic)?;
        Ok(self.topic_whitelist.contains(&normalized))
    }

    #[pyo3(text_signature = "(self, topic, message, mqtt_publish_callback)")]
    fn process_data(
        &self,
        py: Python,
        topic: &str,
        message: &str,
        mqtt_publish_callback: Option<PyObject>,
    ) -> PyResult<Vec<(String, Option<String>)>> {
        debug!("Processing data - topic: {}, message: {}", topic, message);

        // subscription filter
        if let Some(ref pattern) = self.compiled_subscription_filter {
            let regex = Regex::new(pattern).unwrap();
            if regex.is_match(topic) {
                // Filtered => no data
                return Ok(vec![]);
            }
        }

        let expand = self.global_config.processing.expand_json;
        debug!("Transforming data with expand_json={}", expand);

        let flattened: Vec<(String, String)> = if expand {
            match serde_json::from_str::<Value>(message) {
                Ok(json_val) => {
                    if !json_val.is_object() {
                        vec![(topic.to_string(), message.to_string())]
                    } else {
                        let mut flat_vec = Vec::new();
                        flatten_json(&json_val, "", &mut flat_vec);
                        flat_vec.into_iter().map(|(k, v)| (format!("{}/{}", topic, k), v)).collect()
                    }
                }
                Err(_) => vec![(topic.to_string(), message.to_string())],
            }
        } else {
            vec![(topic.to_string(), message.to_string())]
        };
        debug!("Data after flattening: {:?}", flattened);

        // debug publish
        if self.global_config.debug.publish_processed_topics {
            if let Some(ref callback) = mqtt_publish_callback {
                for (dbg_topic, val) in &flattened {
                    let normalized_dbg_topic = self.normalize_topic(dbg_topic)?;
                    let final_dbg_topic = format!(
                        "{}/processedtopics/{}",
                        self.global_config.general.base_topic, normalized_dbg_topic
                    );
                    // Synchronously call the Python publish callback
                    let args = PyTuple::new(py, &[
                        final_dbg_topic.into_py(py),
                        val.clone().into_py(py),
                        false.to_string().into_py(py),
                    ]);
                    let _ = callback.call1(py, args);
                }
            }
        }

        let mut results = Vec::new();
        for (t, v) in flattened {
            if !self.topic_whitelist.is_empty() {
                if !self.is_in_whitelist(&t)? {
                    debug!("Topic '{}' not in whitelist", t);
                    continue;
                }
            }
            // second pass subscription filter
            if let Some(ref pattern) = self.compiled_subscription_filter {
                let regex = Regex::new(pattern).unwrap();
                if regex.is_match(&t) {
                    debug!("Topic '{}' filtered by second pass", t);
                    continue;
                }
            }
            // do_not_forward
            if let Some(ref pattern) = self.do_not_forward_patterns {
                let regex = Regex::new(pattern).unwrap();
                if regex.is_match(&t) {
                    debug!("Topic '{}' filtered by do_not_forward", t);
                    continue;
                }
            }
            debug!("Topic '{}' passed all filters", t);

            let converted = self._convert_boolean(&v)?;
            results.push((t, converted));
        }

        Ok(results)
    }

    #[pyo3(text_signature = "(self, topic, value, http_code, mqtt_publish_callback)")]
    fn publish_forwarded_topic(
        &self,
        py: Python,
        topic: &str,
        value: &str,
        http_code: i32,
        mqtt_publish_callback: PyObject,
    ) -> PyResult<()> {
        debug!(
            "Publishing forwarded topic: {} with value={}, http_code={}",
            topic, value, http_code
        );
        let normalized = self.normalize_topic(topic)?;
        let mqtt_topic = format!(
            "{}/forwardedtopics/{}",
            self.global_config.general.base_topic, normalized
        );
        let payload = serde_json::json!({
            "value": value,
            "http_code": http_code
        });
        let payload_str = payload.to_string();

        debug!("Publishing to MQTT topic '{}' => {}", mqtt_topic, payload_str);

        let args = PyTuple::new(py, &[
            mqtt_topic.into_py(py),
            payload_str.into_py(py),
            false.to_string().into_py(py),
        ]);
        mqtt_publish_callback.call1(py, args)?;
        Ok(())
    }
}

/// Initialize the Rust logger
#[pyfunction]
fn init_rust_logger() {
    let _ = env_logger::try_init();
}

/// The **new** Rust function that replaces `received_mqtt_message`.
/// It is designed to be called from Python (via `partial(...)`) as an async callback.
/// However, here we show a simple approach: the entire function is `async` but we
/// mostly do synchronous calls into Python. You can adapt it with `pyo3-asyncio` if
/// you wish to truly `await` Python coroutines inside Rust.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
pub fn handle_mqtt_message(
    py: Python<'_>,
    // Bound from partial(...) in Python:
    relay_obj: PyObject,              // The "self" of MQTTRelay
    mqtt_client_obj: PyObject,        // The python 'mqtt_client' instance
    global_config_obj: PyObject,      // The python 'global_config'
    http_handler_obj: PyObject,       // The python 'http_miniserver_handler' module
    topic_ns_obj: PyObject,           // The python 'self.TOPIC' namespace
    orjson_obj: PyObject,             // The python 'orjson' module
    // Actual data from the MQTT callback:
    topic: String,
    message: String
) -> PyResult<()> {
    // We can do Rust-side logging here
    debug!("MQTT IN from Rust callback: topic='{}', message='{}'", topic, message);

    // We'll fetch the actual topic constants from topic_ns_obj
    let start_ui_topic: String = topic_ns_obj.getattr(py, "START_UI")?.extract(py)?;
    let stop_ui_topic: String = topic_ns_obj.getattr(py, "STOP_UI")?.extract(py)?;
    let miniserver_startup_topic: String = topic_ns_obj.getattr(py, "MINISERVER_STARTUP_EVENT")?.extract(py)?;
    let config_get_topic: String = topic_ns_obj.getattr(py, "CONFIG_GET")?.extract(py)?;
    let config_response_topic: String = topic_ns_obj.getattr(py, "CONFIG_RESPONSE")?.extract(py)?;
    let config_set_topic: String = topic_ns_obj.getattr(py, "CONFIG_SET")?.extract(py)?;
    let config_add_topic: String = topic_ns_obj.getattr(py, "CONFIG_ADD")?.extract(py)?;
    let config_remove_topic: String = topic_ns_obj.getattr(py, "CONFIG_REMOVE")?.extract(py)?;
    let config_update_topic: String = topic_ns_obj.getattr(py, "CONFIG_UPDATE")?.extract(py)?;
    let config_restart_topic: String = topic_ns_obj.getattr(py, "CONFIG_RESTART")?.extract(py)?;

    // We will also get `miniserver_sync_enabled` from the python global_config
    let miniserver_sync_enabled: bool = global_config_obj
        .getattr(py, "miniserver")?
        .getattr(py, "sync_with_miniserver")?
        .extract(py)?;

    // We'll do an if/else chain to replicate the match/case in Python
    if topic == start_ui_topic {
        // call `await self.start_ui()` => done synchronously
        let _ = relay_obj.call_method0(py, "start_ui")?;
    }
    else if topic == stop_ui_topic {
        let _ = relay_obj.call_method0(py, "stop_ui")?;
    }
    else if topic == miniserver_startup_topic {
        if miniserver_sync_enabled {
            // logger
            info!("Miniserver startup detected, resyncing whitelist (from Rust)");
            // call `await self.handle_miniserver_sync()`
            let _ = relay_obj.call_method0(py, "handle_miniserver_sync")?;
        }
    }
    else if topic == config_get_topic {
        // config -> publish -> orjson.dumps(global_config.get_safe_config())
        let safe_cfg = global_config_obj.call_method0(py, "get_safe_config")?;
        let serialized = orjson_obj.call_method1(py, "dumps", (safe_cfg,))?;
        // then publish
        let publish_method = mqtt_client_obj.getattr(py, "publish")?;
        let _ = publish_method.call(py, (config_response_topic, serialized), None);
    }
    else if topic == config_set_topic || topic == config_add_topic || topic == config_remove_topic {
        // parse JSON
        let update_mode = if topic == config_set_topic {
            "set"
        } else if topic == config_add_topic {
            "add"
        } else {
            "remove"
        };
        // orjson.loads(message)
        let load_res = orjson_obj.call_method1(py, "loads", (message.as_str(),));
        match load_res {
            Ok(py_obj) => {
                // call global_config.update_fields(...)
                let update_res = global_config_obj.call_method(
                    py,
                    "update_fields",
                    (py_obj, update_mode),
                    None
                );
                if let Err(e) = update_res {
                    error!("Error updating configuration: {:?}", e);
                } else {
                    info!("Configuration updated via MQTT. Restarting program (from Rust).");
                    // self.restart_relay_incl_ui()
                    let _ = relay_obj.call_method0(py, "restart_relay_incl_ui")?;
                }
            },
            Err(e) => {
                error!("Invalid JSON format in MQTT message: {:?}", e);
            }
        }
    }
    else if topic == config_update_topic || topic == config_restart_topic {
        info!("Reloading configuration. Restarting program (from Rust).");
        let _ = relay_obj.call_method0(py, "restart_relay_incl_ui")?;
    }
    else {
        // default => process_data in Rust and call `send_to_miniserver`
        let processor_obj = relay_obj.getattr(py, "miniserver_data_processor")?;
        // process_data(self, topic, message, callback)
        let debug_publish_callback = if processor_obj
            .getattr(py, "global_config")?
            .getattr(py, "debug")?
            .getattr(py, "publish_processed_topics")?
            .extract::<bool>(py)? {
                // pass mqtt_client.publish as callback
                Some(mqtt_client_obj.getattr(py, "publish")?)
            } else {
                None
            };

        let process_data_res = processor_obj.call_method(
            py,
            "process_data",
            (topic.clone(), message.clone(), debug_publish_callback),
            None
        );

        match process_data_res {
            Ok(results_py) => {
                // results_py is a list of (topic, Option<String>)
                let results: Vec<(String, Option<String>)> = results_py.extract(py)?;
                for (t, maybe_val) in results {
                    if let Some(val) = maybe_val {
                        // call "http_miniserver_handler.send_to_miniserver(...)"
                        // We do the same thing Python did with `asyncio.create_task(...)`.
                        // For simplicity, we just call it synchronously here.
                        let _ = http_handler_obj.call_method(
                            py,
                            "send_to_miniserver",
                            (t, val),
                            // we also pass 'mqtt_publish_callback=mqtt_client.publish'
                            Some(vec![
                                ("mqtt_publish_callback", mqtt_client_obj.getattr(py, "publish")?)
                            ].into_py_dict(py))
                        );
                    }
                }
            },
            Err(e) => {
                error!("Error processing and sending to Miniserver: {:?}", e);
            }
        }
    }
    Ok(())
}

#[pymodule]
fn _loxmqttrelay(py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<MiniserverDataProcessor>()?;
    m.add_class::<GlobalConfig>()?;
    m.add_class::<GeneralConfig>()?;
    m.add_class::<TopicsConfig>()?;
    m.add_class::<ProcessingConfig>()?;
    m.add_class::<DebugConfig>()?;
    m.add_function(wrap_pyfunction!(init_rust_logger, m)?)?;
    // Our new function
    m.add_function(wrap_pyfunction!(handle_mqtt_message, m)?)?;
    Ok(())
}
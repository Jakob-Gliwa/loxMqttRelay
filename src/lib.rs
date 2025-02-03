use pyo3::{prelude::*, BoundObject};
use pyo3::types::{PyFrozenSet, PyTuple, IntoPyDict, PyBool};
use regex::Regex;
use pyo3::intern;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

// For caching
use lru::LruCache;
use std::num::NonZeroUsize;

// For JSON flattening
use serde_json::Value;

// For logging
use log::{debug, error, info};

// Import `into_future` from pyo3_async_runtimes and `spawn` from tokio
use pyo3_async_runtimes::tokio::into_future;
use tokio::spawn;
use tokio::runtime::Builder;
/// A small struct to store all relevant MQTT topics in Rust, so we don't fetch them repeatedly
#[derive(Clone, Debug)]
struct MqttTopics {
    start_ui_topic: String,
    stop_ui_topic: String,
    miniserver_startup_topic: String,
    config_get_topic: String,
    config_response_topic: String,
    config_set_topic: String,
    config_add_topic: String,
    config_remove_topic: String,
    config_update_topic: String,
    config_restart_topic: String,
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

macro_rules! pyget {
    ($obj:expr, $py:expr, $($attr:expr),+) => {{
        let mut obj = $obj.to_object($py);
        $( obj = obj.getattr($py, intern!($py, $attr))?; )*
        obj
    }};
}


#[pyclass]
pub struct MiniserverDataProcessor {
    #[pyo3(get)]
    global_config: PyObject,

    // Expose the compiled filter to Python so tests can see it
    #[pyo3(get)]
    compiled_subscription_filter: Option<String>,
    #[pyo3(get)]
    do_not_forward_patterns: Option<String>,
    #[pyo3(get)]
    topic_whitelist: HashSet<String>,
    convert_bool_cache: Mutex<LruCache<String, String>>,
    normalize_topic_cache: Mutex<LruCache<String, String>>,

    relay_main_obj: PyObject,
    mqtt_client_obj: PyObject,
    #[pyo3(get)]
    http_handler_obj: PyObject,
    orjson_obj: PyObject,
    mqtt_topics: Option<MqttTopics>,
}

#[pymethods]
impl MiniserverDataProcessor {

    #[new]
    #[pyo3(text_signature = "(self, global_config_py, relay_main_obj, mqtt_client_obj, http_handler_obj, orjson_obj)")]
    fn new(py: Python, topic_ns: PyObject, global_config_py: PyObject, relay_main_obj: PyObject, mqtt_client_obj: PyObject, http_handler_obj: PyObject, orjson_obj: PyObject) -> PyResult<Self> {
        debug!(
            "Initializing MiniserverDataProcessor with cache_size={}",
            pyget!(global_config_py, py, "general", "cache_size").extract::<i32>(py)?
        );

        let compiled = Self::compile_filters(pyget!(global_config_py, py, "topics", "subscription_filters").extract(py)?);
        let cache_size = if pyget!(global_config_py, py, "general", "cache_size").extract::<i32>(py)? == 0 {
            64
        } else {
            pyget!(global_config_py, py, "general", "cache_size").extract(py)?
        };
        let lru_size = NonZeroUsize::new(cache_size).unwrap();

        let start_ui_topic: String = topic_ns.getattr(py, "START_UI")?.extract(py)?;
        let stop_ui_topic: String = topic_ns.getattr(py, "STOP_UI")?.extract(py)?;
        let miniserver_startup_topic: String = topic_ns.getattr(py, "MINISERVER_STARTUP_EVENT")?.extract(py)?;
        let config_get_topic: String = topic_ns.getattr(py, "CONFIG_GET")?.extract(py)?;
        let config_response_topic: String = topic_ns.getattr(py, "CONFIG_RESPONSE")?.extract(py)?;
        let config_set_topic: String = topic_ns.getattr(py, "CONFIG_SET")?.extract(py)?;
        let config_add_topic: String = topic_ns.getattr(py, "CONFIG_ADD")?.extract(py)?;
        let config_remove_topic: String = topic_ns.getattr(py, "CONFIG_REMOVE")?.extract(py)?;
        let config_update_topic: String = topic_ns.getattr(py, "CONFIG_UPDATE")?.extract(py)?;
        let config_restart_topic: String = topic_ns.getattr(py, "CONFIG_RESTART")?.extract(py)?;

        let topics = MqttTopics {
            start_ui_topic,
            stop_ui_topic,
            miniserver_startup_topic,
            config_get_topic,
            config_response_topic,
            config_set_topic,
            config_add_topic,
            config_remove_topic,
            config_update_topic,
            config_restart_topic,
        };
        // processor.mqtt_topics = Some(topics);


        let processor = MiniserverDataProcessor {
            compiled_subscription_filter: compiled,
            do_not_forward_patterns: None,
            topic_whitelist: pyget!(global_config_py, py, "topics", "topic_whitelist")
                .extract::<Vec<String>>(py)?
                .into_iter()
                .collect(),
            convert_bool_cache: Mutex::new(LruCache::new(lru_size)),
            normalize_topic_cache: Mutex::new(LruCache::new(lru_size)),
            global_config: global_config_py,
            mqtt_topics: Some(topics),
            relay_main_obj,
            mqtt_client_obj,
            http_handler_obj,
            orjson_obj,
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
    fn expand_json(&self, py: Python, topic: &str, val: &str) -> PyResult<Py<PyFrozenSet>> {
        if val.is_empty() || ((!val.starts_with('{')) && (!val.starts_with('['))) {
            let tuple = (topic.to_string(), val.to_string());
            let set = PyFrozenSet::new(py, &[tuple])?;
            return Ok(set.into());
        }
        match serde_json::from_str::<Value>(val) {
            Ok(json_val) => {
                if !json_val.is_object() {
                    let tuple = (topic.to_string(), val.to_string());
                    let set = PyFrozenSet::new(py, &[tuple])?;
                    return Ok(set.into());
                }
                let mut flattened = Vec::new();
                flatten_json(&json_val, "", &mut flattened);
                let results: Vec<(String, String)> = flattened
                    .into_iter()
                    .map(|(k, v)| (format!("{}/{}", topic, k), v))
                    .collect();
                let set = PyFrozenSet::new(py, &results)?;
                Ok(set.into())
            }
            Err(_) => {
                let tuple = (topic.to_string(), val.to_string());
                let set = PyFrozenSet::new(py, &[tuple])?;
                Ok(set.into())
            }
        }
    }

    #[pyo3(text_signature = "(self, topic)")]
    fn is_in_whitelist(&self, topic: &str) -> PyResult<bool> {
        let normalized = self.normalize_topic(topic)?;
        Ok(self.topic_whitelist.contains(&normalized))
    }

    #[pyo3(text_signature = "(self, topic, message, mqtt_publish_callback=None)")]
    fn process_data(
        &self,
        py: Python,
        topic: &str,
        message: &str,
        mqtt_publish_callback: Option<PyObject>,
    ) -> PyResult<()> {
        debug!("Processing data - topic: {}, message: {}", topic, message);

        // Normalize topic for whitelist comparison right away
        let normalized_topic = self.normalize_topic(topic)?;
        debug!("Normalized topic for processing: '{}'", normalized_topic);

        // subscription filter (on original topic)
        if let Some(ref pattern) = self.compiled_subscription_filter {
            let regex = Regex::new(pattern).unwrap();
            if regex.is_match(topic) {
                debug!("Topic '{}' filtered by subscription filter", topic);
                return Ok(());
            }
        }

        let expand = pyget!(self.global_config, py, "processing", "expand_json").extract(py)?;
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
        if pyget!(self.global_config, py, "debug", "publish_processed_topics").extract(py)? {
            if let Some(ref callback) = mqtt_publish_callback {
                for (dbg_topic, val) in &flattened {
                    let normalized_dbg_topic = self.normalize_topic(dbg_topic)?;
                    let final_dbg_topic = format!(
                        "{}processedtopics/{}",
                        pyget!(self.global_config, py, "general", "base_topic").extract::<String>(py)?, normalized_dbg_topic
                    );
                    let args = PyTuple::new(py, &[
                        final_dbg_topic.into_py(py),
                        val.clone().into_py(py),
                        false.to_string().into_py(py),
                    ])?;
                    let _ = callback.call1(py, args);
                }
            }
        }

        // Loop for sending topics to the miniserver asynchronously
        for (t, v) in flattened {
            // Check whitelist first (using normalized topic)
            if !self.topic_whitelist.is_empty() {
                let curr_normalized = self.normalize_topic(&t)?;
                debug!("Checking whitelist for topic '{}' (normalized: '{}') against whitelist: {:?}", 
                       t, curr_normalized, self.topic_whitelist);
                
                if !self.topic_whitelist.contains(&curr_normalized) {
                    debug!("Topic '{}' (normalized: '{}') not in whitelist", t, curr_normalized);
                    continue;
                }
                debug!("Topic '{}' (normalized: '{}') found in whitelist", t, curr_normalized);
            }
            
            // second pass subscription filter (on original topic)
            if let Some(ref pattern) = self.compiled_subscription_filter {
                let regex = Regex::new(pattern).unwrap();
                if regex.is_match(&t) {
                    debug!("Topic '{}' filtered by second pass", t);
                    continue;
                }
            }
            
            // do_not_forward (on original topic)
            if let Some(ref pattern) = self.do_not_forward_patterns {
                let regex = Regex::new(pattern).unwrap();
                if regex.is_match(&t) {
                    debug!("Topic '{}' filtered by do_not_forward", t);
                    continue;
                }
            }
            
            debug!("Topic '{}' passed all filters, sending to miniserver", t);
            let converted = self._convert_boolean(&v)?;
            if let Some(val) = converted {
                // Instead of the synchronous call, call the async method and spawn it.
                let callback_obj = match &mqtt_publish_callback {
                    Some(cb) => cb,
                    None => &py.None(),
                };
                let coro = self.http_handler_obj.call_method(py, "send_to_miniserver", (t, val, &callback_obj), None)?;
                let coro_bound = coro.bind(py);
                let fut = into_future(coro_bound.clone())?;
                pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
                    if let Err(e) = fut.await {
                        error!("Error in send_to_miniserver async call: {:?}", e);
                    }
                });
            }
        }

        Ok(())
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
            "{}forwardedtopics/{}",
            pyget!(self.global_config, py, "general", "base_topic").extract::<String>(py)?, normalized
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
            PyBool::new(py, false).into_py(py),
        ])?;
        mqtt_publish_callback.call1(py, args)?;
        Ok(())
    }

    /// Equivalent of the old `received_mqtt_message`, but now inside MiniserverDataProcessor.
    /// Because we already stored all topic strings in `mqtt_topics`, we do not repeatedly
    /// fetch them from Python on every call. Much more efficient.
    ///
    /// Called in Python via partial:
    ///    callback = partial(
    ///       self.miniserver_data_processor.handle_mqtt_message,
    ///       self,  # MQTTRelay instance
    ///       mqtt_client,
    ///       http_handler_obj,
    ///       orjson,
    ///    )
    ///    ...
    ///    asyncio.create_task(callback(topic, message))
    #[pyo3(text_signature = "(self,topic, message)")]
    #[allow(clippy::too_many_arguments)]
    fn handle_mqtt_message(
        &self,
        py: Python<'_>,
        topic: String,
        message: String
    ) -> PyResult<()> {
        debug!("(Rust) handle_mqtt_message: {} => {}", topic, message);

        let Some(ref topics) = self.mqtt_topics else {
            error!("mqtt_topics was never initialized!");
            return Ok(()); 
        };

        // Match the topic to whichever action it needs
        if topic == topics.miniserver_startup_topic {
            if pyget!(self.global_config, py, "miniserver", "sync_with_miniserver").extract::<bool>(py)? {
                info!("Miniserver startup detected, resyncing whitelist (from Rust)");
                let _ = self.relay_main_obj.call_method0(py, "schedule_miniserver_sync")?;
            }
        }
        else if topic == topics.start_ui_topic {
            let _ = self.relay_main_obj.call_method0(py, "start_ui");
        }
        else if topic == topics.stop_ui_topic {
            let _ = self.relay_main_obj.call_method0(py, "stop_ui");
        }
        else if topic == topics.config_get_topic {
            // global_config.get_safe_config -> orjson.dumps -> publish
            let global_config_py = self.relay_main_obj.getattr(py, "miniserver_data_processor")?
                .getattr(py, "global_config")?;
            let safe_cfg = global_config_py.call_method0(py, "get_safe_config")?;
            let serialized = self.orjson_obj.call_method1(py, "dumps", (safe_cfg,))?;
            let publish_fn = self.mqtt_client_obj.getattr(py, "publish")?;
            let _ = publish_fn.call(py, (topics.config_response_topic.clone(), serialized), None);
        }
        else if topic == topics.config_set_topic || topic == topics.config_add_topic || topic == topics.config_remove_topic {
            let update_mode = if topic == topics.config_set_topic {
                "set"
            } else if topic == topics.config_add_topic {
                "add"
            } else {
                "remove"
            };
            let load_res = self.orjson_obj.call_method1(py, "loads", (message.as_str(),));
            match load_res {
                Ok(py_obj) => {
                    let global_config_py = self.relay_main_obj.getattr(py, "miniserver_data_processor")?
                        .getattr(py, "global_config")?;
                    let update_res = global_config_py.call_method(py, "update_fields", (py_obj, update_mode), None);
                    if let Err(e) = update_res {
                        error!("Error updating configuration: {:?}", e);
                    } else {
                        info!("Configuration updated via MQTT. Restarting program (from Rust).");
                        let _ = self.relay_main_obj.call_method0(py, "restart_relay_incl_ui");
                    }
                },
                Err(e) => {
                    error!("Invalid JSON format in MQTT message: {:?}", e);
                }
            }
        }
        else if topic == topics.config_update_topic || topic == topics.config_restart_topic {
            info!("Reloading configuration. Restarting program (from Rust).");
            let _ = self.relay_main_obj.call_method0(py, "restart_relay_incl_ui");
        }
        else {
            // The "normal" data path => process_data => forward to miniserver
            let debug_publish_callback = {
                let dbg_flag: bool = pyget!(self.global_config, py, "debug", "publish_processed_topics").extract::<bool>(py)?;
                if dbg_flag {
                    Some(self.mqtt_client_obj.getattr(py, "publish")?)
                } else {
                    None
                }
            };

            // process_data(...) returns Vec<(String, Option<String>)>
            let _ = self.process_data(
                py,
                &topic,
                &message,
                debug_publish_callback
            );
        }

        Ok(())
    }   

}

/// Initialize the Rust logger
#[pyfunction]
fn init_rust_logger() {
    let _ = env_logger::try_init();
}

#[pymodule]
fn _loxmqttrelay(py: Python, m: &Bound<'_, PyModule>) -> PyResult<()>{
    // Initialize the Tokio runtime for pyo3-asyncio.
    pyo3::prepare_freethreaded_python();
    let mut builder = pyo3_async_runtimes::tokio::re_exports::runtime::Builder::new_multi_thread();
    builder.enable_all();
    pyo3_async_runtimes::tokio::init(builder);
    m.add_class::<MiniserverDataProcessor>()?;
    m.add_function(wrap_pyfunction!(init_rust_logger, m.to_owned())?)?;
    Ok(())
}
use pyo3::prelude::*;
use pyo3::types::{PyFrozenSet, PyTuple};
use regex::Regex;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

// For caching
use lru::LruCache;
use std::num::NonZeroUsize;

// For JSON flattening
use serde_json::Value;

// For logging
use log::{debug, error};

/// A struct representing some global config values mentioned in Python code.
///
/// In your real setup, you'd likely load this from a file or pass it in from Python.
/// Shown here for completeness.
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

/// A dictionary mapping known true/false strings to "1" or "0".
/// We use a match approach for speed, but you could also do a HashMap lookup.
fn convert_boolean_str(input: &str) -> Option<&'static str> {
    match input {
        "true" | "yes" | "on" | "enabled" | "enable" | "1"
        | "check" | "checked" | "select" | "selected" => Some("1"),
        "false" | "no" | "off" | "disabled" | "disable" | "0" => Some("0"),
        _ => None,
    }
}

/// Flatten a serde_json `Value` into a set of `(key, value)` tuples,
/// using `'/'` to separate nested keys (like the Python version).
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
                    // Flatten scalars
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
        _ => {
            // No-op for direct scalars; we normally don't call flatten_json on top-level scalars
        }
    }
}

/// Our main data processor struct, analogous to the Python `MiniserverDataProcessor`.
#[pyclass]
pub struct MiniserverDataProcessor {
    #[pyo3(get, set)]
    pub global_config: GlobalConfig,

    // Remove pyo3 visibility for these fields
    compiled_subscription_filter: Option<String>,
    do_not_forward_patterns: Option<String>,
    topic_whitelist: HashSet<String>,
    convert_bool_cache: Mutex<LruCache<String, String>>,
    normalize_topic_cache: Mutex<LruCache<String, String>>,
}

#[pymethods]
impl MiniserverDataProcessor {
    /// Constructor
    #[new]
    fn new(global_config: GlobalConfig) -> PyResult<Self> {
        debug!(
            "Initializing MiniserverDataProcessor with cache_size={}",
            global_config.general.cache_size
        );

        // Compile initial subscription filters
        let compiled = Self::compile_filters(global_config.topics.subscription_filters.clone());

        // Create caches with user-defined size
        let cache_size = if global_config.general.cache_size == 0 {
            64 // default something if 0 is passed
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

    /// Internal method to compile a list of regex filters into one pattern.
    #[staticmethod]
    fn compile_filters(filters: Vec<String>) -> Option<String> {
        if filters.is_empty() {
            debug!("No filters provided.");
            return None;
        }
        let mut valid_filters = Vec::new();
        for flt in filters {
            match Regex::new(flt.as_str()) {
                Ok(_) => {
                    debug!("Filter '{}' is valid", flt);
                    valid_filters.push(flt.clone());
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
        // Create a single big pattern, e.g. (flt1|flt2|...)
        let pattern = format!("({})", valid_filters.join("|"));
        Some(pattern)
    }

    /// Updates the subscription filters
    #[pyo3(text_signature = "(self, filters)")]
    fn update_subscription_filters(&mut self, filters: Vec<String>) {
        debug!("Updating subscription filters: {:?}", filters);
        self.compiled_subscription_filter = Self::compile_filters(filters);
    }

    /// Updates the topic whitelist, clearing the cache for is_in_whitelist
    #[pyo3(text_signature = "(self, whitelist)")]
    fn update_topic_whitelist(&mut self, whitelist: Vec<String>) {
        let set: HashSet<String> = whitelist.into_iter().collect();
        debug!("Updating topic whitelist: {:?}", set);
        self.topic_whitelist = set;
        // No separate cache for is_in_whitelist in this version,
        // but if you had one, you'd clear it here.
    }

    /// Updates the do_not_forward filters
    #[pyo3(text_signature = "(self, filters)")]
    fn update_do_not_forward(&mut self, filters: Vec<String>) {
        debug!("Updating do_not_forward filters: {:?}", filters);
        self.do_not_forward_patterns = Self::compile_filters(filters);
    }

    /// Convert a value to "1" or "0" based on known boolean strings.
    /// Returns the original value if no mapping is found.
    ///
    /// This is cached by an LRU. For maximum performance, we do direct matching.
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
            // Return original
            cache.put(val.to_string(), val.to_string());
            Ok(Some(val.to_string()))
        }
    }

    /// Normalize a topic by replacing '/' and '%' with '_'.
    /// Returns the same string if no replacements are needed.
    ///
    /// This is cached by an LRU.
    #[pyo3(text_signature = "(self, topic)")]
    fn normalize_topic(&self, topic: &str) -> PyResult<String> {
        let mut cache = self.normalize_topic_cache.lock().unwrap();
        if let Some(cached) = cache.get(topic) {
            return Ok(cached.clone());
        }
        if !topic.contains('/') && !topic.contains('%') {
            // Save it as-is in cache
            cache.put(topic.to_string(), topic.to_string());
            return Ok(topic.to_string());
        }
        let normalized = topic.replace('/', "_").replace('%', "_");
        cache.put(topic.to_string(), normalized.clone());
        Ok(normalized)
    }

    /// Expands a JSON string, returning a frozenset of `(topic/key, value)` pairs.
    /// Short-circuits for non-JSON or invalid JSON.
    #[pyo3(text_signature = "(self, topic, val)")]
    fn expand_json<'py>(&self, py: Python<'py>, topic: &str, val: &str) -> PyResult<&'py PyFrozenSet> {
        // If it doesn't begin with '{' or '[', short-circuit
        if val.is_empty() || (!val.starts_with('{') && !val.starts_with('[')) {
            let tuple = (topic.to_string(), val.to_string());
            let set = PyFrozenSet::new(py, &[tuple])?;
            return Ok(set);
        }
        match serde_json::from_str::<Value>(val) {
            Ok(json_val) => {
                if !json_val.is_object() {
                    // If top-level is not an object, short-circuit
                    let tuple = (topic.to_string(), val.to_string());
                    let set = PyFrozenSet::new(py, &[tuple])?;
                    return Ok(set);
                }
                let mut flattened = Vec::new();
                flatten_json(&json_val, "", &mut flattened);
                // Build a new set with (topic/key, value)
                let results: Vec<(String, String)> = flattened
                    .into_iter()
                    .map(|(k, v)| (format!("{}/{}", topic, k), v))
                    .collect();
                let set = PyFrozenSet::new(py, &results)?;
                Ok(set)
            }
            Err(_) => {
                // Invalid JSON
                let tuple = (topic.to_string(), val.to_string());
                let set = PyFrozenSet::new(py, &[tuple])?;
                Ok(set)
            }
        }
    }

    /// Checks if the topic is in the whitelist
    #[pyo3(text_signature = "(self, topic)")]
    fn is_in_whitelist(&self, topic: &str) -> PyResult<bool> {
        let normalized = self.normalize_topic(topic)?;
        Ok(self.topic_whitelist.contains(&normalized))
    }

    /// Process data through filters and transformations.
    /// This method returns a list of (final_topic, final_value) pairs (instead of a generator).
    ///
    /// If `debug.publish_processed_topics` is set, calls the `mqtt_publish_callback` (if provided)
    /// with debug topics for each processed tuple. The callback is optional.
    ///
    /// In Python, this was a generator. For simplicity and performance, we return a Vec here.
    /// If you need a lazy generator, you can wrap this logic in a streaming iterator.
    #[pyo3(text_signature = "(self, topic, message, mqtt_publish_callback)")]
    #[allow(clippy::type_complexity)]
    fn process_data(
        &self,
        py: Python,
        topic: &str,
        message: &str,
        mqtt_publish_callback: Option<PyObject>,
    ) -> PyResult<Vec<(String, Option<String>)>> {
        debug!("Processing data - topic: {}, message: {}", topic, message);

        // 1) First pass subscription filter
        if let Some(ref pattern) = self.compiled_subscription_filter {
            let regex = Regex::new(pattern.as_str()).unwrap();
            if regex.is_match(topic) {
                // Filtered out => returns empty list
                return Ok(vec![]);
            }
        }

        // 2) Flatten data if configured
        let expand = self.global_config.processing.expand_json;
        debug!("Transforming data with expand_json={}", expand);

        let flattened: Vec<(String, String)> = if expand {
            // Reuse expand_json logic in native form
            match serde_json::from_str::<Value>(message) {
                Ok(json_val) => {
                    // Must check if object
                    if !json_val.is_object() {
                        // Not an object => single pair
                        vec![(topic.to_string(), message.to_string())]
                    } else {
                        let mut flat_vec = Vec::new();
                        flatten_json(&json_val, "", &mut flat_vec);
                        flat_vec
                            .into_iter()
                            .map(|(k, v)| (format!("{}/{}", topic, k), v))
                            .collect()
                    }
                }
                Err(_) => {
                    // Not valid JSON => single pair
                    vec![(topic.to_string(), message.to_string())]
                }
            }
        } else {
            // No expansion
            vec![(topic.to_string(), message.to_string())]
        };
        debug!("Data after flattening: {:?}", flattened);

        // 2a) Optional debug-publish
        if self.global_config.debug.publish_processed_topics {
            if let Some(ref callback) = mqtt_publish_callback {
                for (dbg_topic, val) in &flattened {
                    let normalized_dbg_topic = self.normalize_topic(dbg_topic)?;
                    let final_dbg_topic = format!(
                        "{}/processedtopics/{}",
                        self.global_config.general.base_topic, normalized_dbg_topic
                    );
                    // We'll just do a direct call, synchronous style
                    // If you need real async, use pyo3-asyncio to spawn a future
                    let args = PyTuple::new(py, &[final_dbg_topic.clone(), val.clone(), false.to_string()]);
                    let _ = callback.call1(py, args);
                }
            }
        }

        // 3) Final filtering
        let mut results = Vec::new();
        for (t, v) in flattened {
            // Whitelist check
            if !self.topic_whitelist.is_empty() {
                if !self.is_in_whitelist(&t)? {
                    debug!("Topic '{}' not in whitelist", t);
                    continue;
                }
            }
            // Subscription filter (2nd pass)
            if let Some(ref pattern) = self.compiled_subscription_filter {
                let regex = Regex::new(pattern.as_str()).unwrap();
                if regex.is_match(&t) {
                    debug!("Topic '{}' filtered by second pass (subscription_filter)", t);
                    continue;
                }
            }
            // do_not_forward
            if let Some(ref pattern) = self.do_not_forward_patterns {
                let regex = Regex::new(pattern.as_str()).unwrap();
                if regex.is_match(&t) {
                    debug!("Topic '{}' filtered by do_not_forward", t);
                    continue;
                }
            }
            debug!("Topic '{}' passed all filters", t);

            // Convert boolean
            let converted = self._convert_boolean(&v)?;
            results.push((t, converted));
        }

        Ok(results)
    }

    /// Publishes a forwarded topic with value and HTTP code to MQTT.
    /// The callback is an async Python function that we can `.call` or `.await`.
    ///
    /// For production, use `pyo3-asyncio` to properly `await` the Python function in Rust.
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
        // If no callback is provided in your real code, you'd handle that scenario.
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

        let args = PyTuple::new(py, &[mqtt_topic, payload_str, false.to_string()]);
        let _ = mqtt_publish_callback.call1(py, args)?;
        Ok(())
    }
}

/// A convenience function to initialize the logger if you want logs displayed.
///
/// In real usage, you might do this in your Python environment or main().
#[pyfunction]
fn init_rust_logger() {
    let _ = env_logger::try_init();
}

/// PyO3 module definition
#[pymodule]
fn _loxmqttrelay(py: Python, m: &PyModule) -> PyResult<()> {
    // Register your classes and functions here
    m.add_class::<MiniserverDataProcessor>()?;
    m.add_class::<GlobalConfig>()?;
    m.add_class::<GeneralConfig>()?;
    m.add_class::<TopicsConfig>()?;
    m.add_class::<ProcessingConfig>()?;
    m.add_class::<DebugConfig>()?;
    m.add_function(wrap_pyfunction!(init_rust_logger, m)?)?;
    Ok(())
}
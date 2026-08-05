use pyo3::{prelude::*, types::{PyFrozenSet, PyList, PyString}};
use regex::Regex;
use pyo3::intern;

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

// For caching
use lru::LruCache;
use std::num::NonZeroUsize;

// For JSON flattening
use serde_json::Value;

// For logging
use log::{debug, error, info, warn};
use base64::{Engine, engine::general_purpose};

// Import `into_future` from pyo3_async_runtimes and `spawn` from tokio
use pyo3_async_runtimes::tokio::into_future;

/// A small struct to store all relevant MQTT topics in Rust, so we don't fetch them repeatedly
#[derive(Clone, Debug)]
struct MqttTopics {
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

/// Apply the boolean mapping without the LRU cache `_convert_boolean` uses.
///
/// Same contract - trim, lowercase, look up the keyword table, hand back the
/// original when nothing matches - but the plan path calls this per leaf, where
/// a mutex and two allocations per value would eat most of what the plan saves.
///
/// Short ASCII values lowercase into a stack buffer; everything else falls
/// through to the allocating route. The buffer size is a speed knob only: an
/// oversized value takes the slow path, it is never declared a non-match, so
/// the constant cannot silently disagree with the keyword table.
fn convert_bool_value(val: &str) -> Cow<'_, str> {
    if val.is_empty() {
        return Cow::Borrowed(val);
    }
    let trimmed = val.trim();
    const BUF: usize = 16;
    if trimmed.is_ascii() && trimmed.len() <= BUF {
        let mut buf = [0u8; BUF];
        let bytes = trimmed.as_bytes();
        for (slot, b) in buf.iter_mut().zip(bytes) {
            *slot = b.to_ascii_lowercase();
        }
        if let Some(mapped) = std::str::from_utf8(&buf[..bytes.len()])
            .ok()
            .and_then(convert_boolean_str)
        {
            return Cow::Borrowed(mapped);
        }
        return Cow::Borrowed(val);
    }
    match convert_boolean_str(&trimmed.to_lowercase()) {
        Some(mapped) => Cow::Borrowed(mapped),
        None => Cow::Borrowed(val),
    }
}

/// Render a JSON number the way `serde_json::Value` would.
///
/// Returns `None` for anything whose canonical form is not obvious from the
/// source text (leading zeros, `-0`, integers too wide for i64/u64). The caller
/// then falls back to the DOM path, so a miss costs speed but never accuracy.
fn number_value(text: &str) -> Option<Cow<'_, str>> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let is_integer = !bytes
        .iter()
        .any(|&c| c == b'.' || c == b'e' || c == b'E');
    if is_integer {
        let (negative, digits) = match bytes.split_first() {
            Some((b'-', rest)) => (true, rest),
            _ => (false, bytes),
        };
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        // Leading zeros are invalid JSON and "-0" round-trips as "-0.0".
        if (digits.len() > 1 && digits[0] == b'0') || (negative && digits == b"0") {
            return None;
        }
        let fits = if negative {
            text.parse::<i64>().is_ok()
        } else {
            text.parse::<u64>().is_ok()
        };
        if fits {
            return Some(Cow::Borrowed(text));
        }
        // Too wide for an integer: serde_json widens to f64, so do the same.
    }
    let parsed: f64 = text.parse().ok()?;
    // Number::from_f64 rejects inf/NaN and is the very serializer the DOM path
    // uses, so the rendering cannot drift apart.
    Some(Cow::Owned(serde_json::Number::from_f64(parsed)?.to_string()))
}

/// `topic.replace('/', "_").replace('%', "_")`, allocation-free when unchanged.
fn normalize_topic_str(topic: &str) -> String {
    if !topic.as_bytes().iter().any(|&c| c == b'/' || c == b'%') {
        return topic.to_string();
    }
    topic
        .chars()
        .map(|c| if c == '/' || c == '%' { '_' } else { c })
        .collect()
}

// ---------------------------------------------------------------------------
// Shape plan
// ---------------------------------------------------------------------------
//
// Devices repeat the same JSON layout message after message. Instead of
// building a DOM every time, the first message of a topic is turned into a
// plan: the tree of keys, and per leaf the finished target topic plus the
// verdict of every filter. Later messages are matched against that plan with a
// byte scanner that only reads the values.
//
// The moment the document deviates from the plan - a renamed key, an extra
// field, an escaped string, a number that would not round-trip - the scanner
// bails out and the DOM path takes over, so output can never silently drift.

/// Number of topics that keep a learned plan. Plans hold interned Python
/// strings per leaf, so this trades memory for a bounded working set.
const SHAPE_CACHE_ENTRIES: usize = 512;
/// Matches serde_json's own recursion limit; deeper documents are rejected
/// there anyway, and it keeps the recursive scanner off the stack cliff.
const MAX_PLAN_DEPTH: u32 = 128;

enum PlanNode {
    /// A value that passed every filter when the plan was learned.
    Emit {
        topic: Py<PyString>,
        normalized: Py<PyString>,
        /// Position in the output, so emission order matches the DOM path.
        slot: usize,
    },
    /// A value the filters drop - still has to be consumed by the scanner.
    Drop,
    Object(Vec<(Box<str>, PlanNode)>),
    Array(Vec<PlanNode>),
}

struct Shape {
    root: PlanNode,
    emits: usize,
}

/// The learned plans, plus how many had to be built.
///
/// The counter sits inside the cache's own mutex and is only touched when a
/// plan is stored, so the path that matters - a message replaying a cached
/// plan - never writes to it. It exists because the two flattening routes are
/// output-identical by design: without it, a fast path that silently stopped
/// engaging would leave every test comparing the slow route against itself.
struct ShapeStore {
    plans: LruCache<String, Arc<Shape>>,
    learns: u64,
}

/// One extracted value, borrowed from the plan and the message where possible.
type Emitted<'p, 'a> = (&'p Py<PyString>, &'p Py<PyString>, Cow<'a, str>);

enum Scalar<'a> {
    Number(&'a str),
    True,
    False,
    Null,
}

struct Scan<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Scan<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Scan { bytes, pos: 0 }
    }

    #[inline]
    fn skip_ws(&mut self) {
        while let Some(&c) = self.bytes.get(self.pos) {
            match c {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    #[inline]
    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.bytes.get(self.pos).copied()
    }

    #[inline]
    fn bump(&mut self) {
        self.pos += 1;
    }

    #[inline]
    fn eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    #[inline]
    fn at_end(&mut self) -> bool {
        self.skip_ws();
        self.pos >= self.bytes.len()
    }

    /// Read a string that needs no unescaping.
    ///
    /// Returns `None` on escapes and on raw control characters - the first
    /// because the plan hands out borrowed slices, the second because
    /// serde_json rejects them and the DOM path must decide what happens.
    fn string(&mut self) -> Option<&'a str> {
        self.skip_ws();
        if self.bytes.get(self.pos) != Some(&b'"') {
            return None;
        }
        let start = self.pos + 1;
        let mut i = start;
        while let Some(&c) = self.bytes.get(i) {
            match c {
                b'"' => {
                    self.pos = i + 1;
                    // '"' and '\\' never occur inside a multi-byte sequence, so
                    // the slice is always on a char boundary.
                    return std::str::from_utf8(&self.bytes[start..i]).ok();
                }
                b'\\' => return None,
                0x00..=0x1F => return None,
                _ => i += 1,
            }
        }
        None
    }

    fn scalar(&mut self) -> Option<Scalar<'a>> {
        self.skip_ws();
        let rest = self.bytes.get(self.pos..)?;
        match rest.first()? {
            b't' if rest.starts_with(b"true") => {
                self.pos += 4;
                Some(Scalar::True)
            }
            b'f' if rest.starts_with(b"false") => {
                self.pos += 5;
                Some(Scalar::False)
            }
            b'n' if rest.starts_with(b"null") => {
                self.pos += 4;
                Some(Scalar::Null)
            }
            b'-' | b'0'..=b'9' => {
                let start = self.pos;
                let mut i = self.pos;
                while let Some(&c) = self.bytes.get(i) {
                    match c {
                        b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E' => i += 1,
                        _ => break,
                    }
                }
                self.pos = i;
                std::str::from_utf8(&self.bytes[start..i])
                    .ok()
                    .map(Scalar::Number)
            }
            _ => None,
        }
    }
}

/// Read one leaf value and convert it exactly like the DOM path would.
fn read_leaf<'a>(sc: &mut Scan<'a>) -> Option<Cow<'a, str>> {
    match sc.peek()? {
        b'"' => sc.string().map(convert_bool_value),
        // The plan expected a scalar but the document grew a container here.
        b'{' | b'[' => None,
        _ => match sc.scalar()? {
            // Numbers render as "1"/"0"/"12.5"; the boolean table maps those to
            // themselves, so it can be skipped.
            Scalar::Number(text) => number_value(text),
            Scalar::True => Some(Cow::Borrowed("1")),
            Scalar::False => Some(Cow::Borrowed("0")),
            Scalar::Null => Some(Cow::Borrowed("null")),
        },
    }
}

/// Hand every emitting leaf the slot it occupies in the DOM path's output.
///
/// `flatten_json` walks a `BTreeMap`, so within an object it visits keys in
/// sorted order while the scanner has to follow document order. Numbering the
/// leaves here keeps both orders in sync at zero per-message cost - which
/// matters whenever two paths normalize onto the same miniserver target and the
/// later write is the one that sticks.
fn assign_slots(node: &mut PlanNode, next: &mut usize) {
    match node {
        PlanNode::Emit { slot, .. } => {
            *slot = *next;
            *next += 1;
        }
        PlanNode::Drop => {}
        PlanNode::Object(fields) => {
            let mut order: Vec<usize> = (0..fields.len()).collect();
            order.sort_by(|&a, &b| fields[a].0.cmp(&fields[b].0));
            for i in order {
                assign_slots(&mut fields[i].1, next);
            }
        }
        PlanNode::Array(items) => {
            for item in items {
                assign_slots(item, next);
            }
        }
    }
}

/// Walk a plan against a message, collecting the values of emitting leaves.
///
/// Returns false as soon as the document stops matching; the caller then
/// discards whatever was gathered and falls back to the DOM path.
fn exec_node<'p, 'a>(
    node: &'p PlanNode,
    sc: &mut Scan<'a>,
    out: &mut [Option<Emitted<'p, 'a>>],
) -> bool {
    match node {
        PlanNode::Emit {
            topic,
            normalized,
            slot,
        } => {
            let Some(value) = read_leaf(sc) else {
                return false;
            };
            let Some(cell) = out.get_mut(*slot) else {
                return false;
            };
            *cell = Some((topic, normalized, value));
            true
        }
        PlanNode::Drop => read_leaf(sc).is_some(),
        PlanNode::Object(fields) => {
            if !sc.eat(b'{') {
                return false;
            }
            for (i, (key, child)) in fields.iter().enumerate() {
                if i > 0 && !sc.eat(b',') {
                    return false;
                }
                if sc.string() != Some(&**key) || !sc.eat(b':') {
                    return false;
                }
                if !exec_node(child, sc, out) {
                    return false;
                }
            }
            sc.eat(b'}')
        }
        PlanNode::Array(items) => {
            if !sc.eat(b'[') {
                return false;
            }
            for (i, child) in items.iter().enumerate() {
                if i > 0 && !sc.eat(b',') {
                    return false;
                }
                if !exec_node(child, sc, out) {
                    return false;
                }
            }
            sc.eat(b']')
        }
    }
}

macro_rules! pyget {
    ($obj:expr, $py:expr, $($attr:expr),+) => {{
        let mut obj = $obj.bind($py).as_borrowed().to_owned();
        $( obj = obj.getattr(intern!($py, $attr))?; )*
        obj
    }};
}

/// Private helper function to compile regex filters
fn compile_filters(filters: Vec<String>) -> Option<Regex> {
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
    match Regex::new(&pattern) {
        Ok(compiled_regex) => Some(compiled_regex),
        Err(e) => {
            error!("Failed to compile combined regex '{}': {}", pattern, e);
            None
        }
    }
}

#[pyclass]
pub struct MiniserverDataProcessor {
    #[pyo3(get)]
    global_config: Py<PyAny>,

    compiled_subscription_filter: Option<Regex>,
    
    do_not_forward_patterns: Option<Regex>,

    #[pyo3(get)]
    topic_whitelist: HashSet<String>,
    convert_bool_cache: Mutex<LruCache<String, String>>,
    normalize_topic_cache: Mutex<LruCache<String, String>>,
    // Learned JSON layout per topic. Plans bake in the filter verdicts, so every
    // mutation of a filter or the whitelist has to drop them.
    shape_cache: Mutex<ShapeStore>,
    shape_cache_enabled: bool,

    relay_main_obj: Py<PyAny>,
    mqtt_client_obj: Py<PyAny>,
    #[pyo3(get)]
    http_handler_obj: Py<PyAny>,
    orjson_obj: Py<PyAny>,
    mqtt_topics: Option<MqttTopics>,
    base_topic: String,
    // Cached once at construction. Config is immutable between restarts (a config
    // change re-execs the process), so the per-message hot path needs no getattr
    // back into Python for this flag.
    expand_json: bool,
}

#[pymethods]
impl MiniserverDataProcessor {

    #[new]
    #[pyo3(text_signature = "(self, global_config_py, relay_main_obj, mqtt_client_obj, http_handler_obj, orjson_obj)")]
    fn new(py: Python, topic_ns: Py<PyAny>, global_config_py: Py<PyAny>, relay_main_obj: Py<PyAny>, mqtt_client_obj: Py<PyAny>, http_handler_obj: Py<PyAny>, orjson_obj: Py<PyAny>) -> PyResult<Self> {
        debug!(
            "Initializing MiniserverDataProcessor with cache_size={}",
            pyget!(global_config_py, py, "general", "cache_size").extract::<i32>()?
        );

        let compiled = compile_filters(pyget!(global_config_py, py, "topics", "subscription_filters").extract()?);
        let cache_size = if pyget!(global_config_py, py, "general", "cache_size").extract::<i32>()? == 0 {
            64
        } else {
            pyget!(global_config_py, py, "general", "cache_size").extract()? 
        };
        let lru_size = NonZeroUsize::new(cache_size).unwrap();
        let base_topic: String = pyget!(global_config_py, py, "general", "base_topic").extract()?;
        let expand_json: bool = pyget!(global_config_py, py, "processing", "expand_json").extract()?;
        let miniserver_startup_topic: String = topic_ns.bind(py).getattr(intern!(py, "MINISERVER_STARTUP_EVENT"))?.extract()?;
        let config_get_topic: String = topic_ns.bind(py).getattr(intern!(py, "CONFIG_GET"))?.extract()?;
        let config_response_topic: String = topic_ns.bind(py).getattr(intern!(py, "CONFIG_RESPONSE"))?.extract()?;
        let config_set_topic: String = topic_ns.bind(py).getattr(intern!(py, "CONFIG_SET"))?.extract()?;
        let config_add_topic: String = topic_ns.bind(py).getattr(intern!(py, "CONFIG_ADD"))?.extract()?;
        let config_remove_topic: String = topic_ns.bind(py).getattr(intern!(py, "CONFIG_REMOVE"))?.extract()?;
        let config_update_topic: String = topic_ns.bind(py).getattr(intern!(py, "CONFIG_UPDATE"))?.extract()?;
        let config_restart_topic: String = topic_ns.bind(py).getattr(intern!(py, "CONFIG_RESTART"))?.extract()?;

        let topics = MqttTopics {
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


        // topic_whitelist may be a Python set, frozenset or list depending on how
        // config built it. pyo3's Vec extraction rejects sets ("not a Sequence"),
        // so iterate any iterable and collect into the HashSet field.
        let mut topic_whitelist = HashSet::new();
        for item in pyget!(global_config_py, py, "topics", "topic_whitelist").try_iter()? {
            topic_whitelist.insert(item?.extract::<String>()?);
        }

        let processor = MiniserverDataProcessor {
            compiled_subscription_filter: compiled,
            do_not_forward_patterns: None,
            topic_whitelist,
            convert_bool_cache: Mutex::new(LruCache::new(lru_size)),
            normalize_topic_cache: Mutex::new(LruCache::new(lru_size)),
            shape_cache: Mutex::new(ShapeStore {
                plans: LruCache::new(NonZeroUsize::new(SHAPE_CACHE_ENTRIES).unwrap()),
                learns: 0,
            }),
            shape_cache_enabled: true,
            global_config: global_config_py,
            mqtt_topics: Some(topics),
            relay_main_obj,
            mqtt_client_obj,
            http_handler_obj,
            orjson_obj,
            base_topic:base_topic,
            expand_json,
        };

  
        debug!("MiniserverDataProcessor initialization complete");
        Ok(processor)
    }

    #[pyo3(text_signature = "(self, filters)")]
    fn update_subscription_filters(&mut self, filters: Vec<String>) {
        debug!("Updating subscription filters: {:?}", filters);
        self.compiled_subscription_filter = compile_filters(filters);
        self.invalidate_shapes();
    }

    #[pyo3(text_signature = "(self, whitelist)")]
    fn update_topic_whitelist(&mut self, whitelist: Vec<String>) {
        let set: HashSet<String> = whitelist.into_iter().collect();
        debug!("Updating topic whitelist: {:?}", set);
        self.topic_whitelist = set;
        self.invalidate_shapes();
    }

    #[pyo3(text_signature = "(self, filters)")]
    fn update_do_not_forward(&mut self, filters: Vec<String>) {
        debug!("Updating do_not_forward filters: {:?}", filters);
        self.do_not_forward_patterns = compile_filters(filters);
        self.invalidate_shapes();
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

    #[pyo3(text_signature = "(self, topic, message)")]
    fn process_data(
        &self,
        py: Python,
        topic: &str,
        message: &str,
    ) -> PyResult<()> {
        debug!("Processing data - topic: {}, message: {}", topic, message);

        // subscription filter (on original topic)
        if let Some(ref regex) = self.compiled_subscription_filter {
            if regex.is_match(topic) {
                debug!("Topic '{}' filtered by subscription filter", topic);
                return Ok(());
            }
        }

        let batch = self.build_batch(py, topic, message)?;
        if batch.is_empty() {
            return Ok(());
        }

        // One handover for the whole message instead of one per JSON leaf: the
        // cross-language call and the task spawn dominate the per-value cost.
        debug!("Handing {} value(s) over to the miniserver handler", batch.len());
        let coro = self
            .http_handler_obj
            .bind(py)
            .call_method1(intern!(py, "send_batch_to_miniserver"), (batch,))?;
        let fut = into_future(coro)?;
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            if let Err(e) = fut.await {
                error!("Error in send_batch_to_miniserver async call: {:?}", e);
            }
        });

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
        message_in: Vec<u8>
    ) -> PyResult<()> {
        // Try UTF-8 conversion, but don't crash on failure
        let message = match String::from_utf8(message_in) {
            Ok(s) => s,
            Err(e) => {
                // e.into_bytes() gives us the original bytes back
                let original_bytes = e.into_bytes();
                warn!("Received binary MQTT message on topic '{}': {} bytes. Encoding as base64 for exact preservation.", topic, original_bytes.len());
                
                // Encode binary data as base64 to preserve exact data
                format!("[base64:{}]", general_purpose::STANDARD.encode(original_bytes))
            }
        };

        debug!("(Rust) handle_mqtt_message: {} => {}", topic, message);

        let Some(ref topics) = self.mqtt_topics else {
            error!("mqtt_topics was never initialized!");
            return Ok(()); 
        };
        if topic.starts_with(&self.base_topic) {
        // Match the topic to whichever action it needs
            if topic == topics.miniserver_startup_topic {
                if pyget!(self.global_config, py, "miniserver", "sync_with_miniserver").extract::<bool>()? {
                    info!("Miniserver startup detected, resyncing whitelist (from Rust)");
                    let _ = self.relay_main_obj.bind(py).call_method0("schedule_miniserver_sync")?;
                }
            }
            else if topic == topics.config_get_topic {
                // global_config.get_safe_config -> orjson.dumps -> publish
                let global_config_py = self
                    .relay_main_obj
                    .bind(py)
                    .getattr(intern!(py, "miniserver_data_processor"))?
                    .getattr(intern!(py, "global_config"))?;
                let safe_cfg = global_config_py.call_method0("get_safe_config")?;
                let serialized = self.orjson_obj.bind(py).call_method1("dumps", (safe_cfg,))?;
                let coro = self
                    .mqtt_client_obj
                    .bind(py)
                    .call_method1("publish", (topics.config_response_topic.clone(), serialized))?;
                let fut = into_future(coro.clone())?;
                pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
                    if let Err(e) = fut.await {
                        error!("Error publishing config response: {:?}", e);
                    }
                });
            }
            else if topic == topics.config_set_topic || topic == topics.config_add_topic || topic == topics.config_remove_topic {
                let update_mode = if topic == topics.config_set_topic {
                    "set"
                } else if topic == topics.config_add_topic {
                    "add"
                } else {
                    "remove"
                };
                let load_res = self.orjson_obj.bind(py).call_method1("loads", (message.as_str(),));
                match load_res {
                    Ok(py_obj) => {
                        let global_config_py = self
                            .relay_main_obj
                            .bind(py)
                            .getattr(intern!(py, "miniserver_data_processor"))?
                            .getattr(intern!(py, "global_config"))?;
                        let update_res = global_config_py.call_method1("update_fields", (py_obj, update_mode));
                        if let Err(e) = update_res {
                            error!("Error updating configuration: {:?}", e);
                        } else {
                            info!("Configuration updated via MQTT. Restarting program (from Rust).");
                            let _ = self.relay_main_obj.bind(py).call_method0("restart_relay");
                        }
                    },
                    Err(e) => {
                        error!("Invalid JSON format in MQTT message: {:?}", e);
                    }
                }
            }
            else if topic == topics.config_update_topic || topic == topics.config_restart_topic {
                info!("Reloading configuration. Restarting program (from Rust).");
                let _ = self.relay_main_obj.bind(py).call_method0("restart_relay");
            }
        }
        else {

            // process_data(...) returns Vec<(String, Option<String>)>
            let _ = self.process_data(
                py,
                &topic,
                &message
            );
        }

        Ok(())
    }   

    #[pyo3(text_signature = "(self)")]
    fn get_do_not_forward_patterns(&self) -> Vec<String> {
        if let Some(ref regex) = self.do_not_forward_patterns {
            // Convert the regex pattern back to individual patterns by:
            // 1. Remove the outer parentheses
            // 2. Split on the '|' character
            let pattern = regex.as_str();
            if pattern.starts_with('(') && pattern.ends_with(')') {
                pattern[1..pattern.len()-1]
                    .split('|')
                    .map(String::from)
                    .collect()
            } else {
                vec![pattern.to_string()]
            }
        } else {
            Vec::new()
        }
    }

    /// Route every message through the DOM path, bypassing learned plans.
    ///
    /// The regression suite runs one processor with plans and one without and
    /// requires identical output, which is what keeps the two implementations
    /// honest without freezing expectations into a checked-in file. In
    /// production it doubles as a kill switch.
    ///
    /// Cached plans are left in place: they stay valid (a filter change clears
    /// them anyway), so switching back does not force a relearn.
    #[pyo3(text_signature = "(self, enabled)")]
    fn set_shape_cache_enabled(&mut self, enabled: bool) {
        debug!("Shape cache {}", if enabled { "enabled" } else { "disabled" });
        self.shape_cache_enabled = enabled;
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
        let store = self.shape_cache.lock().unwrap();
        (store.plans.len(), store.learns)
    }

    #[pyo3(text_signature = "(self)")]
    fn get_subscription_filters(&self) -> Vec<String> {
        if let Some(ref regex) = self.compiled_subscription_filter {
            // Convert the regex pattern back to individual patterns by:
            // 1. Remove the outer parentheses
            // 2. Split on the '|' character
            let pattern = regex.as_str();
            if pattern.starts_with('(') && pattern.ends_with(')') {
                pattern[1..pattern.len()-1]
                    .split('|')
                    .map(String::from)
                    .collect()
            } else {
                vec![pattern.to_string()]
            }
        } else {
            Vec::new()
        }
    }

}

/// Internals that are not part of the Python surface.
impl MiniserverDataProcessor {
    /// Plans cache the verdict of every filter, so a filter change voids them.
    fn invalidate_shapes(&mut self) {
        if let Ok(store) = self.shape_cache.get_mut() {
            store.plans.clear();
        }
    }

    /// Everything this message should hand over, in the order the DOM path
    /// would have produced it.
    fn build_batch<'py>(
        &self,
        py: Python<'py>,
        topic: &str,
        message: &str,
    ) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        if self.shape_cache_enabled
            && self.expand_json
            && message.trim_ascii_start().starts_with('{')
        {
            // Appends nothing unless the plan matched the whole document.
            if self.shape_batch(py, topic, message, &list)? {
                return Ok(list);
            }
        }
        self.generic_batch(topic, message, &list)?;
        Ok(list)
    }

    /// Try the learned layout for this topic, learning a fresh one if the
    /// document moved on. Returns false when the plan route gave up entirely.
    fn shape_batch<'py>(
        &self,
        py: Python<'py>,
        topic: &str,
        message: &str,
        list: &Bound<'py, PyList>,
    ) -> PyResult<bool> {
        let cached = {
            let mut store = self.shape_cache.lock().unwrap();
            store.plans.get(topic).map(Arc::clone)
        };
        if let Some(shape) = cached {
            if self.emit_shape(py, &shape, message, list)? {
                return Ok(true);
            }
            debug!("Shape plan for '{}' no longer matches, relearning", topic);
        }

        let Some(shape) = self.learn(py, topic, message) else {
            return Ok(false);
        };
        // Only cache once the plan has carried a whole document, so a stored
        // plan is always one that works.
        if !self.emit_shape(py, &shape, message, list)? {
            return Ok(false);
        }
        let mut store = self.shape_cache.lock().unwrap();
        store.learns += 1;
        store.plans.put(topic.to_string(), Arc::new(shape));
        Ok(true)
    }

    /// Run a plan and append its values. Nothing is appended unless the whole
    /// document matched, so a caller can safely fall back afterwards.
    fn emit_shape<'py>(
        &self,
        py: Python<'py>,
        shape: &Shape,
        message: &str,
        list: &Bound<'py, PyList>,
    ) -> PyResult<bool> {
        let mut out: Vec<Option<Emitted<'_, '_>>> = Vec::new();
        out.resize_with(shape.emits, || None);

        let mut sc = Scan::new(message.as_bytes());
        if !exec_node(&shape.root, &mut sc, &mut out) || !sc.at_end() {
            return Ok(false);
        }
        if out.iter().any(|slot| slot.is_none()) {
            return Ok(false);
        }

        for slot in &out {
            let (topic, normalized, value) = slot.as_ref().expect("all slots filled");
            list.append((topic.bind(py), normalized.bind(py), &**value))?;
        }
        Ok(true)
    }

    /// Derive a plan from one message, or `None` if this document cannot be
    /// replayed faithfully.
    fn learn(&self, py: Python<'_>, topic: &str, message: &str) -> Option<Shape> {
        let mut sc = Scan::new(message.as_bytes());
        if sc.peek() != Some(b'{') {
            return None;
        }
        let mut path = String::with_capacity(topic.len() + 64);
        path.push_str(topic);
        let mut root = self.learn_value(py, &mut sc, &mut path, 0)?;
        if !sc.at_end() {
            return None;
        }
        let mut emits = 0;
        assign_slots(&mut root, &mut emits);
        debug!("Learned shape for '{}' with {} target(s)", topic, emits);
        Some(Shape { root, emits })
    }

    fn learn_value<'a>(
        &self,
        py: Python<'_>,
        sc: &mut Scan<'a>,
        path: &mut String,
        depth: u32,
    ) -> Option<PlanNode> {
        if depth > MAX_PLAN_DEPTH {
            return None;
        }
        match sc.peek()? {
            b'{' => {
                sc.bump();
                let mut fields: Vec<(Box<str>, PlanNode)> = Vec::new();
                if sc.eat(b'}') {
                    return Some(PlanNode::Object(fields));
                }
                loop {
                    let key = sc.string()?;
                    // serde_json keeps the last of duplicated keys; a plan that
                    // emits both would produce a different result.
                    if fields.iter().any(|(known, _)| &**known == key) {
                        return None;
                    }
                    if !sc.eat(b':') {
                        return None;
                    }
                    let base = path.len();
                    path.push('/');
                    path.push_str(key);
                    let child = self.learn_value(py, sc, path, depth + 1)?;
                    path.truncate(base);
                    fields.push((Box::from(key), child));
                    if sc.eat(b',') {
                        continue;
                    }
                    if !sc.eat(b'}') {
                        return None;
                    }
                    break;
                }
                Some(PlanNode::Object(fields))
            }
            b'[' => {
                sc.bump();
                let mut items = Vec::new();
                if sc.eat(b']') {
                    return Some(PlanNode::Array(items));
                }
                let mut index = 0usize;
                loop {
                    let base = path.len();
                    path.push('/');
                    let _ = write!(path, "{}", index);
                    let child = self.learn_value(py, sc, path, depth + 1)?;
                    path.truncate(base);
                    items.push(child);
                    index += 1;
                    if sc.eat(b',') {
                        continue;
                    }
                    if !sc.eat(b']') {
                        return None;
                    }
                    break;
                }
                Some(PlanNode::Array(items))
            }
            b'"' => {
                sc.string()?;
                Some(self.make_leaf(py, path))
            }
            _ => {
                if let Scalar::Number(text) = sc.scalar()? {
                    // Refuse the plan rather than learn a leaf whose value we
                    // could not render identically later on.
                    number_value(text)?;
                }
                Some(self.make_leaf(py, path))
            }
        }
    }

    /// Resolve target topic and filter verdict once, at learn time.
    fn make_leaf(&self, py: Python<'_>, full_topic: &str) -> PlanNode {
        let normalized = normalize_topic_str(full_topic);
        if !self.topic_whitelist.is_empty() && !self.topic_whitelist.contains(&normalized) {
            return PlanNode::Drop;
        }
        if let Some(ref regex) = self.compiled_subscription_filter {
            if regex.is_match(full_topic) {
                return PlanNode::Drop;
            }
        }
        if let Some(ref regex) = self.do_not_forward_patterns {
            if regex.is_match(full_topic) {
                return PlanNode::Drop;
            }
        }
        PlanNode::Emit {
            topic: PyString::new(py, full_topic).unbind(),
            normalized: PyString::new(py, &normalized).unbind(),
            slot: 0,
        }
    }

    /// The DOM route: build a `serde_json::Value`, flatten it, filter it. Slow,
    /// but it is the definition of correct behaviour and the fallback for every
    /// document the plan route refuses.
    fn generic_batch(
        &self,
        topic: &str,
        message: &str,
        list: &Bound<'_, PyList>,
    ) -> PyResult<()> {
        let flattened: Vec<(String, String)> = if self.expand_json {
            match serde_json::from_str::<Value>(message) {
                Ok(json_val) if json_val.is_object() => {
                    let mut flat_vec = Vec::new();
                    flatten_json(&json_val, "", &mut flat_vec);
                    flat_vec
                        .into_iter()
                        .map(|(k, v)| (format!("{}/{}", topic, k), v))
                        .collect()
                }
                _ => vec![(topic.to_string(), message.to_string())],
            }
        } else {
            vec![(topic.to_string(), message.to_string())]
        };

        for (t, v) in flattened {
            let normalized = self.normalize_topic(&t)?;
            if !self.topic_whitelist.is_empty() && !self.topic_whitelist.contains(&normalized) {
                debug!("Topic '{}' (normalized: '{}') not in whitelist", t, normalized);
                continue;
            }
            if let Some(ref regex) = self.compiled_subscription_filter {
                if regex.is_match(&t) {
                    debug!("Topic '{}' filtered by second pass", t);
                    continue;
                }
            }
            if let Some(ref regex) = self.do_not_forward_patterns {
                if regex.is_match(&t) {
                    debug!("Topic '{}' filtered by do_not_forward", t);
                    continue;
                }
            }
            // Deliberately the cached, allocating variant and not the plan
            // path's `convert_bool_value`: this route is the reference the
            // regression suite diffs against, so it has to stay the code that
            // shipped before the shape plans existed.
            if let Some(value) = self._convert_boolean(&v)? {
                list.append((t.as_str(), normalized.as_str(), value.as_str()))?;
            }
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
fn _loxmqttrelay(_py: Python, m: &Bound<'_, PyModule>) -> PyResult<()>{
    // Initialize the Tokio runtime for pyo3-asyncio.
    Python::initialize();
    let mut builder = pyo3_async_runtimes::tokio::re_exports::runtime::Builder::new_multi_thread();
    builder.enable_all();
    pyo3_async_runtimes::tokio::init(builder);
    m.add_class::<MiniserverDataProcessor>()?;
    m.add_function(wrap_pyfunction!(init_rust_logger, m)?)?;
    Ok(())
}
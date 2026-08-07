use pyo3::{prelude::*, types::{PyDict, PyList, PyString}};
use regex::{Regex, RegexSet};
use pyo3::exceptions::PyValueError;
use pyo3::intern;

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Write as _;
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

mod mqtt;
mod udp;
mod util;
use mqtt::{MqttClient, MqttShared};
use udp::UdpServer;
use util::{lock_recover, loggable};

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
        if let Ok(s) = std::str::from_utf8(&buf[..bytes.len()])
            && let Some(mapped) = convert_boolean_str(s)
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

/// `topic.replace('/', "_").replace('%', "_")` in one pass and one allocation.
///
/// Both separators and their replacement are ASCII, so the result is exactly as
/// long as the input and the segments between them can be copied wholesale. The
/// obvious `chars().map().collect()` cannot know that: `Chars::size_hint` only
/// promises a quarter of the byte length, so it reserves too little and grows
/// the string again while filling it.
fn normalize_topic_str(topic: &str) -> String {
    if !topic.as_bytes().iter().any(|&c| c == b'/' || c == b'%') {
        return topic.to_string();
    }
    let mut out = String::with_capacity(topic.len());
    let mut rest = topic;
    while let Some(at) = rest.find(['/', '%']) {
        out.push_str(&rest[..at]);
        out.push('_');
        rest = &rest[at + 1..];
    }
    out.push_str(rest);
    out
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

/// How many documents in a row a topic has to have refused before its plan
/// attempts are skipped.
///
/// The two mistakes here are not the same size. Attempting a plan that fails
/// wastes one scan and then costs the DOM route that was going to run anyway -
/// well under half the message. Skipping a plan that would have worked costs
/// the whole difference between the two routes, which is a factor of three.
/// So skipping needs real evidence that this topic cannot be planned at all,
/// and a single refusal is not it: a topic that only occasionally sends an
/// escaped string would lose its fast path for everything else.
///
/// Measured on the fuzz corpus, which is exactly that mixed case: at a
/// threshold of one, plans built and replayed dropped from 150 to 17.
const NEGATIVE_STRIKES: u32 = 8;

/// Once skipping, how many messages pass before the plan route is offered again.
///
/// Bounded rather than permanent: a publisher whose payloads become plannable
/// again - a firmware update that stops escaping a string, a value that leaves
/// the range where it renders as `1e3` - must not be stuck on the slow route
/// for the rest of the process.
const NEGATIVE_RETRY_EVERY: u32 = 64;
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
/// A topic's record of refusals, kept only while it has one.
#[derive(Default)]
struct Refusals {
    /// Documents refused in a row. Reset by any plan this topic manages to
    /// build, so a topic that only fails now and then never arms the skip.
    strikes: u32,
    /// Messages waved through since the skip armed, for the periodic retry.
    skipped: u32,
}

struct ShapeStore {
    plans: LruCache<String, Arc<Shape>>,
    /// Topics that have refused a document recently. Bounded like `plans`, so a
    /// flood of one-off topics cannot grow it.
    unplannable: LruCache<String, Refusals>,
    learns: u64,
    hits: u64,
    learn_failures: u64,
    dom_fallbacks: u64,
    negative_skips: u64,
}

impl ShapeStore {
    /// Whether this topic's plan attempt is skipped this time round.
    ///
    /// Only asked for topics without a plan. Counting the skipped messages here
    /// is what makes the skip expire on its own.
    fn hold_back(&mut self, topic: &str) -> bool {
        let Some(record) = self.unplannable.get_mut(topic) else {
            return false;
        };
        if record.strikes < NEGATIVE_STRIKES {
            return false;
        }
        record.skipped += 1;
        record.skipped % NEGATIVE_RETRY_EVERY != 0
    }

    /// Note that no plan could be built for this topic.
    fn refused(&mut self, topic: &str) {
        if let Some(record) = self.unplannable.get_mut(topic) {
            record.strikes = record.strikes.saturating_add(1);
            return;
        }
        self.unplannable
            .put(topic.to_string(), Refusals { strikes: 1, skipped: 0 });
    }
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
///
/// `convert` mirrors `processing.convert_booleans`: with it off, strings are
/// passed through untouched and JSON literals render the way `flatten_json`
/// writes them, which is what the DOM path hands over when the flag is off.
fn read_leaf<'a>(sc: &mut Scan<'a>, convert: bool) -> Option<Cow<'a, str>> {
    match sc.peek()? {
        b'"' => sc.string().map(|s| {
            if convert {
                convert_bool_value(s)
            } else {
                Cow::Borrowed(s)
            }
        }),
        // The plan expected a scalar but the document grew a container here.
        b'{' | b'[' => None,
        _ => match sc.scalar()? {
            // Numbers render as "1"/"0"/"12.5"; the boolean table maps those to
            // themselves, so it can be skipped.
            Scalar::Number(text) => number_value(text),
            Scalar::True => Some(Cow::Borrowed(if convert { "1" } else { "true" })),
            Scalar::False => Some(Cow::Borrowed(if convert { "0" } else { "false" })),
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
    convert: bool,
) -> bool {
    match node {
        PlanNode::Emit {
            topic,
            normalized,
            slot,
        } => {
            let Some(value) = read_leaf(sc, convert) else {
                return false;
            };
            let Some(cell) = out.get_mut(*slot) else {
                return false;
            };
            *cell = Some((topic, normalized, value));
            true
        }
        PlanNode::Drop => read_leaf(sc, convert).is_some(),
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
                if !exec_node(child, sc, out, convert) {
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
                if !exec_node(child, sc, out, convert) {
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

/// A configured filter list: the patterns as they were written, and the machine
/// that matches them.
///
/// A `RegexSet` rather than one regex built from `patterns.join("|")`. Joining
/// looked cheaper but made the list unrecoverable - a pattern containing `|`,
/// an escaped `\|` or a `[|]` class could not be split apart again for the
/// getters - and it let the patterns interfere with each other: `(?i)` in one
/// of them applies to everything that follows in the enclosing group, so it
/// silently turned every later filter case-insensitive, and two patterns using
/// the same capture name refused to compile together although each was fine.
struct FilterSet {
    patterns: Vec<String>,
    set: RegexSet,
}

impl FilterSet {
    #[inline]
    fn is_match(&self, text: &str) -> bool {
        self.set.is_match(text)
    }

    fn patterns(&self) -> Vec<String> {
        self.patterns.clone()
    }
}

/// Compile regex filters, refusing the whole set if one pattern is unusable.
///
/// Used wherever a filter list comes straight from the configuration: silently
/// dropping a pattern there would hand the user a relay that forwards what they
/// told it to hold back, so a typo has to surface as a startup error instead.
fn compile_filters_strict(kind: &str, filters: &[String]) -> PyResult<Option<FilterSet>> {
    if filters.is_empty() {
        debug!("No {} filters configured.", kind);
        return Ok(None);
    }
    for flt in filters {
        // An empty expression matches every topic, so a stray "" in the list
        // would filter away everything instead of the one thing it names.
        if flt.trim().is_empty() {
            return Err(PyValueError::new_err(format!(
                "Empty '{kind}' pattern: an empty expression matches every topic"
            )));
        }
        // Compiled one by one first, so the error names the offending pattern
        // rather than the whole set.
        if let Err(e) = Regex::new(flt) {
            return Err(PyValueError::new_err(format!(
                "Invalid '{}' pattern '{}': {}",
                kind, flt, e
            )));
        }
    }
    let set = RegexSet::new(filters).map_err(|e| {
        PyValueError::new_err(format!("Failed to compile the '{kind}' filters: {e}"))
    })?;
    Ok(Some(FilterSet {
        patterns: filters.to_vec(),
        set,
    }))
}

#[pyclass]
pub struct MiniserverDataProcessor {
    #[pyo3(get)]
    global_config: Py<PyAny>,

    compiled_subscription_filter: Option<FilterSet>,

    do_not_forward_patterns: Option<FilterSet>,

    #[pyo3(get)]
    topic_whitelist: HashSet<String>,
    convert_bool_cache: Mutex<LruCache<String, String>>,
    normalize_topic_cache: Mutex<LruCache<String, String>>,
    // Learned JSON layout per topic. Plans bake in the filter verdicts, so every
    // mutation of a filter or the whitelist has to drop them.
    shape_cache: Mutex<ShapeStore>,
    shape_cache_enabled: bool,

    relay_main_obj: Py<PyAny>,
    // Shared with the Python-facing MqttClient so config responses publish
    // straight from Rust instead of calling back into Python.
    mqtt_shared: Arc<MqttShared>,
    #[pyo3(get)]
    http_handler_obj: Py<PyAny>,
    orjson_obj: Py<PyAny>,
    mqtt_topics: MqttTopics,
    // Cached once at construction. Config is immutable between restarts (a config
    // change re-execs the process), so the per-message hot path needs no getattr
    // back into Python for this flag.
    expand_json: bool,
    #[pyo3(get)]
    convert_booleans: bool,
}

#[pymethods]
impl MiniserverDataProcessor {

    #[new]
    #[pyo3(text_signature = "(self, topic_ns, global_config_py, relay_main_obj, mqtt_client, http_handler_obj, orjson_obj)")]
    fn new(py: Python, topic_ns: Py<PyAny>, global_config_py: Py<PyAny>, relay_main_obj: Py<PyAny>, mqtt_client: PyRef<'_, MqttClient>, http_handler_obj: Py<PyAny>, orjson_obj: Py<PyAny>) -> PyResult<Self> {
        debug!(
            "Initializing MiniserverDataProcessor with cache_size={}",
            pyget!(global_config_py, py, "general", "cache_size").extract::<i32>()?
        );

        // Strict, like do_not_forward: a filter that cannot be compiled used to
        // be skipped with a log line, so a typo silently forwarded everything
        // the filter was meant to hold back.
        let subscription_filters: Vec<String> =
            pyget!(global_config_py, py, "topics", "subscription_filters").extract()?;
        let compiled = compile_filters_strict("subscription_filters", &subscription_filters)?;
        let cache_size = if pyget!(global_config_py, py, "general", "cache_size").extract::<i32>()? == 0 {
            64
        } else {
            pyget!(global_config_py, py, "general", "cache_size").extract()? 
        };
        let lru_size = NonZeroUsize::new(cache_size).unwrap();
        let expand_json: bool = pyget!(global_config_py, py, "processing", "expand_json").extract()?;
        let convert_booleans: bool = pyget!(global_config_py, py, "processing", "convert_booleans").extract()?;
        // Configured filtering has to be live from the first message on: nothing
        // else installs it later, and a topic that leaks once is already on a
        // miniserver input.
        let do_not_forward: Vec<String> = pyget!(global_config_py, py, "topics", "do_not_forward").extract()?;
        let compiled_do_not_forward = compile_filters_strict("do_not_forward", &do_not_forward)?;
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

        // topic_whitelist may be a Python set, frozenset or list depending on how
        // config built it. pyo3's Vec extraction rejects sets ("not a Sequence"),
        // so iterate any iterable and collect into the HashSet field.
        let mut topic_whitelist = HashSet::new();
        for item in pyget!(global_config_py, py, "topics", "topic_whitelist").try_iter()? {
            topic_whitelist.insert(item?.extract::<String>()?);
        }

        let processor = MiniserverDataProcessor {
            compiled_subscription_filter: compiled,
            do_not_forward_patterns: compiled_do_not_forward,
            topic_whitelist,
            convert_bool_cache: Mutex::new(LruCache::new(lru_size)),
            normalize_topic_cache: Mutex::new(LruCache::new(lru_size)),
            shape_cache: Mutex::new(ShapeStore {
                plans: LruCache::new(NonZeroUsize::new(SHAPE_CACHE_ENTRIES).unwrap()),
                unplannable: LruCache::new(NonZeroUsize::new(SHAPE_CACHE_ENTRIES).unwrap()),
                learns: 0,
                hits: 0,
                learn_failures: 0,
                dom_fallbacks: 0,
                negative_skips: 0,
            }),
            shape_cache_enabled: true,
            global_config: global_config_py,
            mqtt_topics: topics,
            relay_main_obj,
            mqtt_shared: mqtt_client.shared(),
            http_handler_obj,
            orjson_obj,
            expand_json,
            convert_booleans,
        };

        info!(
            "Processing configuration: expand_json={}, convert_booleans={}, do_not_forward={} pattern(s), topic_whitelist={} entry/entries",
            processor.expand_json,
            processor.convert_booleans,
            do_not_forward.len(),
            processor.topic_whitelist.len()
        );
        debug!("MiniserverDataProcessor initialization complete");
        Ok(processor)
    }

    #[pyo3(text_signature = "(self, filters)")]
    fn update_subscription_filters(&mut self, filters: Vec<String>) -> PyResult<()> {
        debug!("Updating subscription filters: {:?}", filters);
        self.compiled_subscription_filter =
            compile_filters_strict("subscription_filters", &filters)?;
        self.invalidate_shapes();
        Ok(())
    }

    #[pyo3(text_signature = "(self, whitelist)")]
    fn update_topic_whitelist(&mut self, whitelist: Vec<String>) {
        let set: HashSet<String> = whitelist.into_iter().collect();
        debug!("Updating topic whitelist: {:?}", set);
        self.topic_whitelist = set;
        self.invalidate_shapes();
    }

    #[pyo3(text_signature = "(self, filters)")]
    fn update_do_not_forward(&mut self, filters: Vec<String>) -> PyResult<()> {
        debug!("Updating do_not_forward filters: {:?}", filters);
        self.do_not_forward_patterns = compile_filters_strict("do_not_forward", &filters)?;
        self.invalidate_shapes();
        Ok(())
    }

    

    /// The keyword mapping itself, always applied.
    ///
    /// `processing.convert_booleans` decides whether the forwarding path calls
    /// the mapping at all; it stays available on its own so both settings can be
    /// described against the same reference.
    ///
    /// Neither flattening route goes through here any more - both use
    /// [`convert_bool_value`], which reaches the same answer without a cache.
    /// This is now the second, independent implementation that
    /// `test_boolean_mapping_is_pinned` diffs the shared one against over the
    /// whole keyword table, which is what keeps the mapping pinned once the two
    /// routes no longer disagree by construction.
    #[pyo3(text_signature = "(self, val)")]
    fn _convert_boolean(&self, val: &str) -> PyResult<Option<String>> {
        let mut cache = lock_recover(&self.convert_bool_cache);
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

    /// The Miniserver input name for a topic.
    ///
    /// The cache pays off here because callers ask about the same handful of
    /// subscribed topics repeatedly. The flattening routes do not: every leaf
    /// they normalize is a freshly built `topic/key`, so they use
    /// [`normalize_topic_str`] and this stays the reference a normalization test
    /// pins them against.
    #[pyo3(text_signature = "(self, topic)")]
    fn normalize_topic(&self, topic: &str) -> PyResult<String> {
        let mut cache = lock_recover(&self.normalize_topic_cache);
        if let Some(cached) = cache.get(topic) {
            return Ok(cached.clone());
        }
        if !topic.contains('/') && !topic.contains('%') {
            cache.put(topic.to_string(), topic.to_string());
            return Ok(topic.to_string());
        }
        let normalized = topic.replace(['/', '%'], "_");
        cache.put(topic.to_string(), normalized.clone());
        Ok(normalized)
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
        debug!(
            "Processing data - topic: {}, message: {}",
            loggable(topic),
            loggable(message)
        );

        // subscription filter (on original topic)
        if let Some(ref filters) = self.compiled_subscription_filter
            && filters.is_match(topic)
        {
            debug!("Topic '{}' filtered by subscription filter", loggable(topic));
            return Ok(());
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

    /// Route one inbound MQTT message: a control topic, or the data path.
    ///
    /// Called from [`crate::mqtt`]'s ingress worker, which holds the GIL for the
    /// duration. The control topics are compared against the strings resolved
    /// once at construction rather than fetched from Python per message.
    ///
    /// Both arguments are borrowed. Owning them meant copying every topic and
    /// every payload once per message, on the one path every message takes -
    /// the caller's buffers outlive the call, and UTF-8 payloads are handed on
    /// without a copy at all.
    #[pyo3(text_signature = "(self,topic, message)")]
    pub(crate) fn handle_mqtt_message(
        &self,
        py: Python<'_>,
        topic: &str,
        message_in: &[u8]
    ) -> PyResult<()> {
        // Try UTF-8 conversion, but don't crash on failure
        let message = match std::str::from_utf8(message_in) {
            Ok(s) => Cow::Borrowed(s),
            Err(_) => {
                warn!(
                    "Received binary MQTT message on topic '{}': {} bytes. Encoding as base64 for exact preservation.",
                    loggable(topic),
                    message_in.len()
                );

                // Encode binary data as base64 to preserve exact data
                Cow::Owned(format!(
                    "[base64:{}]",
                    general_purpose::STANDARD.encode(message_in)
                ))
            }
        };

        debug!(
            "(Rust) handle_mqtt_message: {} => {}",
            loggable(topic),
            loggable(&message)
        );

        let topics = &self.mqtt_topics;
        // Matched exactly against the known control topics rather than gated by
        // a `starts_with(base_topic)` prefix check: the prefix alone does not
        // identify a control topic, so anything under base_topic that is not
        // one of the cases below used to be silently dropped instead of
        // reaching process_data - a real risk with an empty/short base_topic
        // or data subscriptions that happen to live under it.
        if topic == topics.miniserver_startup_topic {
            if pyget!(self.global_config, py, "miniserver", "sync_with_miniserver").extract::<bool>()? {
                info!("Miniserver startup detected, resyncing whitelist (from Rust)");
                let _ = self.relay_main_obj.bind(py).call_method0("schedule_miniserver_sync")?;
            }
        }
        else if topic == topics.config_get_topic {
            // global_config.get_safe_config -> orjson.dumps -> publish. Straight
            // off the field: the same object used to be looked up the long way
            // round, through the relay and back into this very processor.
            let safe_cfg = self
                .global_config
                .bind(py)
                .call_method0(intern!(py, "get_safe_config"))?;
            let serialized = self.orjson_obj.bind(py).call_method1("dumps", (safe_cfg,))?;
            self.mqtt_shared.publish_detached(
                topics.config_response_topic.clone(),
                serialized.extract::<Vec<u8>>()?,
            );
        }
        else if topic == topics.config_set_topic || topic == topics.config_add_topic || topic == topics.config_remove_topic {
            let update_mode = if topic == topics.config_set_topic {
                "set"
            } else if topic == topics.config_add_topic {
                "add"
            } else {
                "remove"
            };
            let load_res = self.orjson_obj.bind(py).call_method1("loads", (&*message,));
            match load_res {
                Ok(py_obj) => {
                    let update_res = self
                        .global_config
                        .bind(py)
                        .call_method1(intern!(py, "update_fields"), (py_obj, update_mode));
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
        else {
            // Everything else takes the normal data path, whether or not it
            // happens to live under base_topic. Propagated so ingress_worker
            // can log the topic and reason; a discarded Err here used to leave
            // those failures silent.
            self.process_data(py, topic, &message)?;
        }

        Ok(())
    }   

    #[pyo3(text_signature = "(self)")]
    fn get_do_not_forward_patterns(&self) -> Vec<String> {
        self.do_not_forward_patterns
            .as_ref()
            .map(FilterSet::patterns)
            .unwrap_or_default()
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
        let store = lock_recover(&self.shape_cache);
        (store.plans.len(), store.learns)
    }

    /// Everything the shape cache counts, as a dict.
    ///
    /// `get_shape_stats` stays a 2-tuple because it is unpacked positionally all
    /// over the regression suite; this is where the rest lives:
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
    ///
    /// A relay whose `dom_fallbacks` keeps climbing alongside `hits` is
    /// forwarding something the scanner refuses on some of its topics.
    #[pyo3(text_signature = "(self)")]
    fn get_shape_metrics<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let store = lock_recover(&self.shape_cache);
        let metrics = PyDict::new(py);
        metrics.set_item(intern!(py, "plans"), store.plans.len())?;
        metrics.set_item(intern!(py, "learns"), store.learns)?;
        metrics.set_item(intern!(py, "hits"), store.hits)?;
        metrics.set_item(intern!(py, "learn_failures"), store.learn_failures)?;
        metrics.set_item(intern!(py, "dom_fallbacks"), store.dom_fallbacks)?;
        metrics.set_item(intern!(py, "negative_skips"), store.negative_skips)?;
        metrics.set_item(intern!(py, "unplannable"), store.unplannable.len())?;
        Ok(metrics)
    }

    #[pyo3(text_signature = "(self)")]
    fn get_subscription_filters(&self) -> Vec<String> {
        self.compiled_subscription_filter
            .as_ref()
            .map(FilterSet::patterns)
            .unwrap_or_default()
    }

}

/// Internals that are not part of the Python surface.
impl MiniserverDataProcessor {
    /// Plans cache the verdict of every filter, so a filter change voids them.
    ///
    /// A poisoned lock must not skip this: a plan that survives a filter change
    /// keeps forwarding what the new filter was meant to hold back.
    fn invalidate_shapes(&mut self) {
        let mut store = lock_recover(&self.shape_cache);
        store.plans.clear();
        // The hold-backs go with them: whether a document is plannable does not
        // depend on the filters, but a topic held back would keep paying for a
        // refusal that has nothing to do with the change just made.
        store.unplannable.clear();
    }

    /// Everything this message should hand over, in the order the DOM path
    /// would have produced it.
    fn build_batch<'py>(
        &self,
        py: Python<'py>,
        topic: &str,
        message: &str,
    ) -> PyResult<Bound<'py, PyList>> {
        if self.shape_cache_enabled
            && self.expand_json
            && message.trim_ascii_start().starts_with('{')
            && let Some(list) = self.shape_batch(py, topic, message)?
        {
            return Ok(list);
        }
        self.generic_batch(py, topic, message)
    }

    /// Try the learned layout for this topic, learning a fresh one if the
    /// document moved on. `None` when the plan route gave up entirely.
    ///
    /// One lock on the path that matters. A message replaying a cached plan
    /// takes it once, to fetch the plan and count the hit; everything else -
    /// removing a plan the document outgrew, recording a refusal, storing a
    /// fresh plan - happens on paths that were already going to lock.
    fn shape_batch<'py>(
        &self,
        py: Python<'py>,
        topic: &str,
        message: &str,
    ) -> PyResult<Option<Bound<'py, PyList>>> {
        let cached = {
            let mut store = lock_recover(&self.shape_cache);
            let cached = store.plans.get(topic).map(Arc::clone);
            match cached {
                // Counted here rather than after the plan ran, so the hit costs
                // no second lock; a plan that then turns out not to fit takes it
                // back below, on a path that locks anyway.
                Some(_) => store.hits += 1,
                // No plan, and this topic has refused everything for a while:
                // the scan would walk the document only to give up again.
                None if store.hold_back(topic) => {
                    store.negative_skips += 1;
                    store.dom_fallbacks += 1;
                    return Ok(None);
                }
                None => {}
            }
            cached
        };

        if let Some(ref shape) = cached {
            if let Some(list) = self.emit_shape(py, shape, message)? {
                return Ok(Some(list));
            }
            debug!(
                "Shape plan for '{}' no longer matches, relearning",
                loggable(topic)
            );
            let mut store = lock_recover(&self.shape_cache);
            store.hits -= 1;
            // Dropped rather than left for the LRU to evict: a plan that does
            // not fit the documents arriving now would be tried, and fail,
            // on every one of them.
            store.plans.pop(topic);
        }

        // Only cached once the plan has carried a whole document, so a stored
        // plan is always one that works.
        let learned = match self.learn(py, topic, message) {
            Some(shape) => self.emit_shape(py, &shape, message)?.map(|list| (shape, list)),
            None => None,
        };

        let mut store = lock_recover(&self.shape_cache);
        match learned {
            Some((shape, list)) => {
                store.learns += 1;
                store.plans.put(topic.to_string(), Arc::new(shape));
                store.unplannable.pop(topic);
                Ok(Some(list))
            }
            None => {
                store.learn_failures += 1;
                store.dom_fallbacks += 1;
                store.refused(topic);
                Ok(None)
            }
        }
    }

    /// Run a plan and hand back its values, or `None` if the plan did not carry
    /// the whole document.
    ///
    /// The list is built at its final length from slots the plan already sized,
    /// rather than grown one `append` at a time. That also makes "a miss leaves
    /// nothing behind" structural: there is no list yet for a partial run to
    /// have written into, so the caller cannot fall back onto a half-filled one.
    fn emit_shape<'py>(
        &self,
        py: Python<'py>,
        shape: &Shape,
        message: &str,
    ) -> PyResult<Option<Bound<'py, PyList>>> {
        let mut out: Vec<Option<Emitted<'_, '_>>> = Vec::new();
        out.resize_with(shape.emits, || None);

        let mut sc = Scan::new(message.as_bytes());
        if !exec_node(&shape.root, &mut sc, &mut out, self.convert_booleans) || !sc.at_end() {
            return Ok(None);
        }
        if out.iter().any(|slot| slot.is_none()) {
            return Ok(None);
        }

        let list = PyList::new(
            py,
            out.iter().map(|slot| {
                let (topic, normalized, value) = slot.as_ref().expect("all slots filled");
                (topic.bind(py), normalized.bind(py), &**value)
            }),
        )?;
        Ok(Some(list))
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
        debug!(
            "Learned shape for '{}' with {} target(s)",
            loggable(topic),
            emits
        );
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
        if let Some(ref filters) = self.compiled_subscription_filter
            && filters.is_match(full_topic)
        {
            return PlanNode::Drop;
        }
        if let Some(ref filters) = self.do_not_forward_patterns
            && filters.is_match(full_topic)
        {
            return PlanNode::Drop;
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
    ///
    /// It shares the leaf helpers with the plan route, so the differential suite
    /// no longer covers those two by comparing the routes to each other; the
    /// dedicated pinning tests do that against [`MiniserverDataProcessor::_convert_boolean`]
    /// and [`MiniserverDataProcessor::normalize_topic`]. What the route
    /// comparison still establishes on its own is the part that actually differs
    /// between them: which leaves exist, in which order, and which survive the
    /// filters.
    fn generic_batch<'py>(
        &self,
        py: Python<'py>,
        topic: &str,
        message: &str,
    ) -> PyResult<Bound<'py, PyList>> {
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

        // Borrowed from `flattened`, which outlives the list being built, so a
        // value the converter passed through does not have to be copied to
        // survive until then.
        let mut kept: Vec<(&str, String, Cow<'_, str>)> = Vec::with_capacity(flattened.len());
        for (t, v) in &flattened {
            // The plan path's helpers, not the `#[pymethods]` wrappers around
            // them: those consult an LRU cache behind a mutex, and a cache that
            // does hit still costs a lock, a hash and a copy of the answer -
            // more than recomputing it from a topic that is already at hand.
            let normalized = normalize_topic_str(t);
            if !self.topic_whitelist.is_empty() && !self.topic_whitelist.contains(&normalized) {
                debug!(
                    "Topic '{}' (normalized: '{}') not in whitelist",
                    loggable(t),
                    loggable(&normalized)
                );
                continue;
            }
            if let Some(ref filters) = self.compiled_subscription_filter
                && filters.is_match(t)
            {
                debug!("Topic '{}' filtered by second pass", loggable(t));
                continue;
            }
            if let Some(ref filters) = self.do_not_forward_patterns
                && filters.is_match(t)
            {
                debug!("Topic '{}' filtered by do_not_forward", loggable(t));
                continue;
            }
            let value = if self.convert_booleans {
                convert_bool_value(v)
            } else {
                Cow::Borrowed(v.as_str())
            };
            kept.push((t.as_str(), normalized, value));
        }

        PyList::new(
            py,
            kept.iter()
                .map(|(t, normalized, value)| (*t, normalized.as_str(), &**value)),
        )
    }
}

/// Initialize the Rust logger at the level Python resolved.
///
/// Without a level this used to be `env_logger::try_init()`, whose default
/// filter with no `RUST_LOG` set is `error` - which quietly swallowed every
/// warning the relay emits about dropped messages. `LOG_LEVEL` now reaches the
/// Rust side too, and `RUST_LOG` still overrides it for anyone who wants to
/// turn a single module up.
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
fn _loxmqttrelay(_py: Python, m: &Bound<'_, PyModule>) -> PyResult<()>{
    // Initialize the Tokio runtime for pyo3-asyncio.
    Python::initialize();
    let mut builder = pyo3_async_runtimes::tokio::re_exports::runtime::Builder::new_multi_thread();
    builder.enable_all();
    pyo3_async_runtimes::tokio::init(builder);
    m.add_class::<MiniserverDataProcessor>()?;
    m.add_class::<MqttClient>()?;
    m.add_class::<UdpServer>()?;
    m.add_function(wrap_pyfunction!(init_rust_logger, m)?)?;
    Ok(())
}
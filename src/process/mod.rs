//! Everything an inbound MQTT message goes through.
//!
//! [`Core`] owns the filters, the whitelist, the learned layouts and the route
//! out. It is generic over [`Egress`] so the production path and the tests
//! share one implementation of the part that matters.
//!
//! Nothing here allocates a configuration or reaches for a lock it did not
//! need: the per-message path reads the filters and the whitelist through an
//! `ArcSwap`, and the two caches below are read by the reference helpers rather
//! than by the message itself. The control topics never arrive here at all -
//! [`crate::mqtt::ingress_worker`] tells them apart first.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::{ArcSwap, ArcSwapOption};
use base64::{Engine, engine::general_purpose};
use log::{debug, error, info, warn};
use lru::LruCache;
use regex::{Regex, RegexSet};

use crate::egress::Egress;
use crate::util::{lock_recover, loggable};

pub(crate) mod flatten;
#[cfg(test)]
mod regression;
pub(crate) mod shape;

use shape::{LeafPolicy, PlanNode, Route, ShapeStore};

/// One value on its way out.
///
/// Everything is a [`Cow`] because the plan route borrows all three from the
/// plan and the message - a replayed message allocates nothing at all - while
/// the DOM route has to build the topic and its normalized form.
pub(crate) struct Outgoing<'a> {
    /// The source topic. Only used for logs; the Miniserver never sees it.
    pub(crate) topic: Cow<'a, str>,
    /// The Miniserver input name this value is written to.
    pub(crate) normalized: Cow<'a, str>,
    pub(crate) value: Cow<'a, str>,
}

/// The values one message produced, in the order the DOM route would emit them.
///
/// `None` slots only occur while the plan route is filling the vector: it
/// writes each leaf into the slot the plan assigned it, and a plan that leaves
/// one behind is rejected before anything is delivered.
pub(crate) type Batch<'a> = Vec<Option<Outgoing<'a>>>;

/// The relay's own topics, derived once from `general.base_topic` so the
/// per-message path only ever compares strings.
#[derive(Clone, Debug)]
pub(crate) struct MqttTopics {
    pub(crate) miniserver_startup: String,
    pub(crate) config_get: String,
    pub(crate) config_response: String,
    pub(crate) config_set: String,
    pub(crate) config_add: String,
    pub(crate) config_remove: String,
    pub(crate) config_update: String,
    pub(crate) config_restart: String,
}

impl MqttTopics {
    /// The eight control topics a `base_topic` implies.
    ///
    /// Derived in one place, because they are also the topics the relay
    /// subscribes to and the order it subscribes in matters - `review_suback`
    /// zips the SUBACK reasons against that list.
    pub(crate) fn from_base(base: &str) -> Self {
        MqttTopics {
            miniserver_startup: format!("{base}miniserverevent/startup"),
            config_get: format!("{base}config/get"),
            config_response: format!("{base}config/response"),
            config_set: format!("{base}config/set"),
            config_add: format!("{base}config/add"),
            config_remove: format!("{base}config/remove"),
            config_update: format!("{base}config/update"),
            config_restart: format!("{base}config/restart"),
        }
    }

    /// What the relay subscribes to besides the configured subscriptions.
    ///
    /// The order matters: it is the order the SUBACK reasons come back in, and
    /// `review_suback` reads them against this list. `config_response` is not
    /// here - the relay publishes it, it does not listen for it.
    pub(crate) fn subscriptions(&self) -> [String; 7] {
        [
            self.config_set.clone(),
            self.config_add.clone(),
            self.config_remove.clone(),
            self.config_update.clone(),
            self.config_restart.clone(),
            self.config_get.clone(),
            self.miniserver_startup.clone(),
        ]
    }
}

/// A control topic, identified without taking the GIL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlTopic {
    MiniserverStartup,
    ConfigGet,
    ConfigSet,
    ConfigAdd,
    ConfigRemove,
    /// `config/update` and `config/restart`, which do the same thing.
    ConfigReload,
}

impl ControlTopic {
    /// The mode `AppConfig.update_fields` is called with, for the three topics
    /// that carry a payload.
    pub(crate) fn update_mode(self) -> Option<&'static str> {
        match self {
            ControlTopic::ConfigSet => Some("set"),
            ControlTopic::ConfigAdd => Some("add"),
            ControlTopic::ConfigRemove => Some("remove"),
            _ => None,
        }
    }
}

/// A filter list that could not be used as written.
///
/// Carries the finished message: the wording is what an operator has to act on,
/// and it reaches them as the reason the relay refused to start.
#[derive(Debug)]
pub(crate) struct FilterError(pub(crate) String);

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
pub(crate) struct FilterSet {
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
/// Silently dropping a pattern would hand the user a relay that forwards what
/// they told it to hold back, so a typo has to surface as an error instead.
pub(crate) fn compile_filters(
    kind: &str,
    filters: &[String],
) -> Result<Option<FilterSet>, FilterError> {
    if filters.is_empty() {
        debug!("No {kind} filters configured.");
        return Ok(None);
    }
    for flt in filters {
        // An empty expression matches every topic, so a stray "" in the list
        // would filter away everything instead of the one thing it names.
        if flt.trim().is_empty() {
            return Err(FilterError(format!(
                "Empty '{kind}' pattern: an empty expression matches every topic"
            )));
        }
        // Compiled one by one first, so the error names the offending pattern
        // rather than the whole set.
        if let Err(e) = Regex::new(flt) {
            return Err(FilterError(format!(
                "Invalid '{kind}' pattern '{flt}': {e}"
            )));
        }
    }
    let set = RegexSet::new(filters)
        .map_err(|e| FilterError(format!("Failed to compile the '{kind}' filters: {e}")))?;
    Ok(Some(FilterSet {
        patterns: filters.to_vec(),
        set,
    }))
}

/// Everything [`Core`] needs at construction.
pub(crate) struct CoreConfig {
    pub(crate) topics: MqttTopics,
    pub(crate) subscription_filters: Vec<String>,
    pub(crate) do_not_forward: Vec<String>,
    pub(crate) topic_whitelist: HashSet<String>,
    /// When true, an empty whitelist forwards nothing.
    ///
    /// Set from `sync_with_miniserver`: the user who turns sync on is relying
    /// on the Miniserver's virtual inputs as the only allowed targets, so an
    /// empty list must not fall open to "forward everything".
    pub(crate) whitelist_required: bool,
    pub(crate) cache_size: usize,
    pub(crate) expand_json: bool,
    pub(crate) convert_booleans: bool,
}

/// Everything the shape cache counts.
pub(crate) struct ShapeMetrics {
    pub(crate) plans: usize,
    pub(crate) learns: u64,
    pub(crate) hits: u64,
    pub(crate) learn_failures: u64,
    pub(crate) dom_fallbacks: u64,
    pub(crate) negative_skips: u64,
    pub(crate) unplannable: usize,
}

/// A binary payload turned into text, exactly as the relay forwards it.
///
/// Base64 rather than a lossy decode: the value ends up on a Miniserver input,
/// and a replacement character there is indistinguishable from one the sender
/// meant.
pub(crate) fn decode_payload<'a>(topic: &str, payload: &'a [u8]) -> Cow<'a, str> {
    match std::str::from_utf8(payload) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => {
            warn!(
                "Received binary MQTT message on topic '{}': {} bytes. Encoding as base64 for \
                 exact preservation.",
                loggable(topic),
                payload.len()
            );
            Cow::Owned(format!(
                "[base64:{}]",
                general_purpose::STANDARD.encode(payload)
            ))
        }
    }
}

pub(crate) struct Core<E: Egress> {
    egress: E,
    // Swapped rather than locked: every message reads these and only a config
    // change writes them, so the read side must not pay for a mutex.
    subscription_filters: ArcSwapOption<FilterSet>,
    do_not_forward: ArcSwapOption<FilterSet>,
    topic_whitelist: ArcSwap<HashSet<String>>,
    // Immutable for the life of the process: a config change re-execs.
    whitelist_required: bool,
    /// Sized from `general.cache_size`, and read only by the reference helpers
    /// below - see the note there.
    #[allow(dead_code, reason = "read by the reference helpers, not by the message path")]
    convert_bool_cache: Mutex<LruCache<String, String>>,
    #[allow(dead_code, reason = "read by the reference helpers, not by the message path")]
    normalize_topic_cache: Mutex<LruCache<String, String>>,
    // Learned JSON layout per topic. Plans bake in the filter verdicts, so every
    // mutation of a filter or the whitelist has to drop them.
    shape_cache: Mutex<ShapeStore>,
    shape_cache_enabled: AtomicBool,
    topics: MqttTopics,
    // Cached once at construction. Config is immutable between restarts (a
    // config change re-execs the process), so the per-message path needs no
    // rebuilt per message.
    expand_json: bool,
    convert_booleans: bool,
}

impl<E: Egress> Core<E> {
    pub(crate) fn new(config: CoreConfig, egress: E) -> Result<Self, FilterError> {
        let subscription_filters =
            compile_filters("subscription_filters", &config.subscription_filters)?;
        // Configured filtering has to be live from the first message on: nothing
        // else installs it later, and a topic that leaks once is already on a
        // miniserver input.
        let do_not_forward = compile_filters("do_not_forward", &config.do_not_forward)?;

        let cache_size = std::num::NonZeroUsize::new(config.cache_size)
            .unwrap_or_else(|| std::num::NonZeroUsize::new(64).expect("non-zero fallback"));

        info!(
            "Processing configuration: expand_json={}, convert_booleans={}, do_not_forward={} \
             pattern(s), topic_whitelist={} entry/entries, whitelist_required={}",
            config.expand_json,
            config.convert_booleans,
            config.do_not_forward.len(),
            config.topic_whitelist.len(),
            config.whitelist_required
        );
        if config.whitelist_required && config.topic_whitelist.is_empty() {
            warn!(
                "topic_whitelist is empty while sync_with_miniserver is on: nothing will be \
                 forwarded until the sync fills it"
            );
        }

        Ok(Core {
            egress,
            subscription_filters: ArcSwapOption::from(subscription_filters.map(Arc::new)),
            do_not_forward: ArcSwapOption::from(do_not_forward.map(Arc::new)),
            topic_whitelist: ArcSwap::from_pointee(config.topic_whitelist),
            whitelist_required: config.whitelist_required,
            convert_bool_cache: Mutex::new(LruCache::new(cache_size)),
            normalize_topic_cache: Mutex::new(LruCache::new(cache_size)),
            shape_cache: Mutex::new(ShapeStore::new()),
            shape_cache_enabled: AtomicBool::new(true),
            topics: config.topics,
            expand_json: config.expand_json,
            convert_booleans: config.convert_booleans,
        })
    }

    /// What the core was built with, and the levers a test pulls on it.
    ///
    /// Nothing on the message path calls these. They are here because the
    /// alternative is tests that reach into private fields, and because the
    /// configuration is immutable for the process lifetime - a config change
    /// re-execs - so `update_*` has exactly one production caller between them:
    /// the whitelist sync, through `update_topic_whitelist`.
    #[allow(dead_code, reason = "the readable surface of state the message path reaches inline")]
    pub(crate) fn convert_booleans(&self) -> bool {
        self.convert_booleans
    }

    /// Which control topic this is, or `None` for the data path.
    ///
    /// Matched exactly against the known control topics rather than gated by a
    /// `starts_with(base_topic)` prefix check: the prefix alone does not
    /// identify a control topic, so anything under `base_topic` that is not one
    /// of the cases below used to be silently dropped instead of being
    /// forwarded.
    pub(crate) fn control_kind(&self, topic: &str) -> Option<ControlTopic> {
        let t = &self.topics;
        if topic == t.miniserver_startup {
            Some(ControlTopic::MiniserverStartup)
        } else if topic == t.config_get {
            Some(ControlTopic::ConfigGet)
        } else if topic == t.config_set {
            Some(ControlTopic::ConfigSet)
        } else if topic == t.config_add {
            Some(ControlTopic::ConfigAdd)
        } else if topic == t.config_remove {
            Some(ControlTopic::ConfigRemove)
        } else if topic == t.config_update || topic == t.config_restart {
            Some(ControlTopic::ConfigReload)
        } else {
            None
        }
    }

    // -- configuration ------------------------------------------------------

    #[allow(dead_code, reason = "a config change re-execs, so only the whitelist changes at runtime")]
    pub(crate) fn update_subscription_filters(
        &self,
        filters: Vec<String>,
    ) -> Result<(), FilterError> {
        debug!("Updating subscription filters: {filters:?}");
        let compiled = compile_filters("subscription_filters", &filters)?;
        self.subscription_filters.store(compiled.map(Arc::new));
        self.invalidate_shapes();
        Ok(())
    }

    #[allow(dead_code, reason = "a config change re-execs, so only the whitelist changes at runtime")]
    pub(crate) fn update_do_not_forward(&self, filters: Vec<String>) -> Result<(), FilterError> {
        debug!("Updating do_not_forward filters: {filters:?}");
        let compiled = compile_filters("do_not_forward", &filters)?;
        self.do_not_forward.store(compiled.map(Arc::new));
        self.invalidate_shapes();
        Ok(())
    }

    pub(crate) fn update_topic_whitelist(&self, whitelist: Vec<String>) {
        let set: HashSet<String> = whitelist.into_iter().collect();
        debug!("Updating topic whitelist: {set:?}");
        self.topic_whitelist.store(Arc::new(set));
        self.invalidate_shapes();
    }

    #[allow(dead_code, reason = "the reference implementation the pinning tests diff against")]
    pub(crate) fn subscription_filters(&self) -> Vec<String> {
        self.subscription_filters
            .load()
            .as_ref()
            .map(|f| f.patterns())
            .unwrap_or_default()
    }

    #[allow(dead_code, reason = "the reference implementation the pinning tests diff against")]
    pub(crate) fn do_not_forward_patterns(&self) -> Vec<String> {
        self.do_not_forward
            .load()
            .as_ref()
            .map(|f| f.patterns())
            .unwrap_or_default()
    }

    #[allow(dead_code, reason = "the reference implementation the pinning tests diff against")]
    pub(crate) fn topic_whitelist(&self) -> HashSet<String> {
        self.topic_whitelist.load().as_ref().clone()
    }

    /// Plans cache the verdict of every filter, so a filter change voids them.
    ///
    /// A poisoned lock must not skip this: a plan that survives a filter change
    /// keeps forwarding what the new filter was meant to hold back.
    fn invalidate_shapes(&self) {
        lock_recover(&self.shape_cache).clear();
    }

    /// Route every message through the DOM path, bypassing learned plans.
    ///
    /// Cached plans are left in place: they stay valid (a filter change clears
    /// them anyway), so switching back does not force a relearn.
#[allow(dead_code, reason = "the kill switch the differential suite flips to compare both routes")]
    pub(crate) fn set_shape_cache_enabled(&self, enabled: bool) {
        debug!(
            "Shape cache {}",
            if enabled { "enabled" } else { "disabled" }
        );
        self.shape_cache_enabled.store(enabled, Ordering::Relaxed);
    }

    #[allow(dead_code, reason = "the reference implementation the pinning tests diff against")]
    pub(crate) fn shape_stats(&self) -> (usize, u64) {
        let store = lock_recover(&self.shape_cache);
        (store.plan_count(), store.learns)
    }

    pub(crate) fn shape_metrics(&self) -> ShapeMetrics {
        let store = lock_recover(&self.shape_cache);
        ShapeMetrics {
            plans: store.plan_count(),
            learns: store.learns,
            hits: store.hits,
            learn_failures: store.learn_failures,
            dom_fallbacks: store.dom_fallbacks,
            negative_skips: store.negative_skips,
            unplannable: store.unplannable_count(),
        }
    }

    // -- the cached single-topic helpers ------------------------------------
    //
    // NOTE: neither flattening route goes through these any more - both use the
    // uncached functions in `flatten`, which reach the same answers without a
    // lock. What is left is their role as the second, independent
    // implementation that the pinning tests diff the shared ones against, plus
    // `is_in_whitelist`, which is the readable form of a check the message path
    // does inline against an already-normalized name.
    //
    // That leaves `general.cache_size` sizing two caches nothing on the hot path
    // reads. Removing them would make the setting a no-op and removing the
    // setting is a config change, so both are left alone here and flagged.

    /// The Miniserver input name for a topic.
    ///
    /// The cache pays off here because callers ask about the same handful of
    /// subscribed topics repeatedly. The flattening routes do not: every leaf
    /// they normalize is a freshly built `topic/key`, so they use
    /// [`flatten::normalize_topic_str`] and this stays the reference a
    /// normalization test pins them against.
    #[allow(dead_code, reason = "the reference implementation the pinning tests diff against")]
    pub(crate) fn normalize_topic(&self, topic: &str) -> String {
        let mut cache = lock_recover(&self.normalize_topic_cache);
        if let Some(cached) = cache.get(topic) {
            return cached.clone();
        }
        let normalized = flatten::normalize_topic_str(topic);
        cache.put(topic.to_string(), normalized.clone());
        normalized
    }

    #[allow(dead_code, reason = "the reference implementation the pinning tests diff against")]
    pub(crate) fn is_in_whitelist(&self, topic: &str) -> bool {
        let normalized = self.normalize_topic(topic);
        self.topic_whitelist.load().contains(&normalized)
    }

    /// Whether a normalized Miniserver input name may leave.
    ///
    /// Empty whitelist + `whitelist_required` (sync on) is fail-closed. Empty
    /// whitelist with sync off keeps the historical "forward everything"
    /// meaning: the user who left the list blank is not relying on it.
    fn allowed_by_whitelist(&self, whitelist: &HashSet<String>, normalized: &str) -> bool {
        if whitelist.is_empty() {
            return !self.whitelist_required;
        }
        whitelist.contains(normalized)
    }

    /// The keyword mapping itself, always applied.
    ///
    /// Neither flattening route goes through here any more - both use
    /// [`flatten::convert_bool_value`], which reaches the same answer without a
    /// cache. This is the second, independent implementation that the pinning
    /// test diffs the shared one against over the whole keyword table, which is
    /// what keeps the mapping pinned once the two routes no longer disagree by
    /// construction.
    #[allow(dead_code, reason = "the reference implementation the pinning tests diff against")]
    pub(crate) fn convert_boolean(&self, val: &str) -> String {
        let mut cache = lock_recover(&self.convert_bool_cache);
        if let Some(cached) = cache.get(val) {
            return cached.clone();
        }
        if val.is_empty() {
            return val.to_string();
        }
        let normalized = val.trim().to_lowercase();
        let mapped = match flatten::convert_boolean_str(&normalized) {
            Some(mapped) => mapped.to_string(),
            None => val.to_string(),
        };
        cache.put(val.to_string(), mapped.clone());
        mapped
    }

    // -- the data path ------------------------------------------------------

    /// One inbound message on a data topic, payload still raw.
    pub(crate) async fn handle_data(&self, topic: &str, payload: &[u8]) {
        let message = decode_payload(topic, payload);
        self.process_data(topic, &message).await;
    }

    /// Flatten, filter and forward one message.
    pub(crate) async fn process_data(&self, topic: &str, message: &str) {
        debug!(
            "Processing data - topic: {}, message: {}",
            loggable(topic),
            loggable(message)
        );

        // Fail-closed before any other work: with sync on, an empty list means
        // "no inputs known yet", not "forward everything". Settled here so a
        // retained dump cannot even reach the connection check.
        if self.whitelist_required && self.topic_whitelist.load().is_empty() {
            debug!(
                "Dropping '{}': whitelist empty while sync_with_miniserver requires one",
                loggable(topic)
            );
            return;
        }

        // Nothing to gain from flattening a message that cannot leave. There is
        // no outbox, so during an outage this is the whole message's fate, and
        // saying so once beats a per-value account of the same connection.
        if !self.egress.connected() {
            warn!(
                "Dropping '{}': no connection to the Miniserver",
                loggable(topic)
            );
            return;
        }

        // Subscription filter on the original topic, before any flattening.
        {
            let filters = self.subscription_filters.load();
            if let Some(filters) = filters.as_ref()
                && filters.is_match(topic)
            {
                debug!("Topic '{}' filtered by subscription filter", loggable(topic));
                return;
            }
        }

        if self.plan_route_offered(message) {
            let route = lock_recover(&self.shape_cache).route(topic);
            match route {
                // This topic has refused everything for a while, so the scan
                // would only walk the document to give up again.
                Route::Skip => {}
                Route::Cached(shape) => {
                    let mut batch: Batch<'_> = Vec::with_capacity(shape.emits);
                    if shape.emit(message, self.convert_booleans, &mut batch) {
                        self.deliver(topic, &batch).await;
                        return;
                    }
                    // The borrow of `shape` ends here, so the plan can be dropped.
                    drop(batch);
                    debug!(
                        "Shape plan for '{}' no longer matches, relearning",
                        loggable(topic)
                    );
                    lock_recover(&self.shape_cache).unfit(topic);
                    if self.learn_and_deliver(topic, message).await {
                        return;
                    }
                }
                Route::Learn => {
                    if self.learn_and_deliver(topic, message).await {
                        return;
                    }
                }
            }
        }

        self.dom_deliver(topic, message).await;
    }

    /// Whether a plan is worth attempting for this payload at all.
    fn plan_route_offered(&self, message: &str) -> bool {
        self.shape_cache_enabled.load(Ordering::Relaxed)
            && self.expand_json
            && message.trim_ascii_start().starts_with('{')
    }

    /// Build a plan for this message and deliver through it.
    ///
    /// Reports whether that worked; on `false` the caller falls back to the DOM
    /// route. A plan is only stored once it has carried a whole document, so a
    /// stored plan is always one that works.
    async fn learn_and_deliver(&self, topic: &str, message: &str) -> bool {
        let Some(shape) = shape::learn(self, topic, message) else {
            lock_recover(&self.shape_cache).refused(topic);
            return false;
        };
        let shape = Arc::new(shape);
        let mut batch: Batch<'_> = Vec::with_capacity(shape.emits);
        if !shape.emit(message, self.convert_booleans, &mut batch) {
            drop(batch);
            lock_recover(&self.shape_cache).refused(topic);
            return false;
        }
        lock_recover(&self.shape_cache).store(topic, Arc::clone(&shape));
        self.deliver(topic, &batch).await;
        true
    }

    /// The DOM route: build a `serde_json::Value`, flatten it, filter it. Slow,
    /// but it is the definition of correct behaviour and the fallback for every
    /// document the plan route refuses.
    async fn dom_deliver(&self, topic: &str, message: &str) {
        let flattened = flatten::dom_targets(topic, message, self.expand_json);
        let mut batch: Batch<'_> = Vec::with_capacity(flattened.len());
        {
            // Loaded once for the whole message rather than per leaf, and
            // dropped before the first await so the future stays `Send`.
            let subscription = self.subscription_filters.load();
            let do_not_forward = self.do_not_forward.load();
            let whitelist = self.topic_whitelist.load();
            for (target, value) in &flattened {
                // The plan route's helper, not the cached `normalize_topic`:
                // that consults an LRU behind a mutex, and a cache that does hit
                // still costs a lock, a hash and a copy of the answer - more
                // than recomputing it from a topic that is already at hand.
                let normalized = flatten::normalize_topic_str(target);
                if !self.allowed_by_whitelist(&whitelist, &normalized) {
                    debug!(
                        "Topic '{}' (normalized: '{}') not in whitelist",
                        loggable(target),
                        loggable(&normalized)
                    );
                    continue;
                }
                if let Some(filters) = subscription.as_ref()
                    && filters.is_match(target)
                {
                    debug!("Topic '{}' filtered by second pass", loggable(target));
                    continue;
                }
                if let Some(filters) = do_not_forward.as_ref()
                    && filters.is_match(target)
                {
                    debug!("Topic '{}' filtered by do_not_forward", loggable(target));
                    continue;
                }
                let value = if self.convert_booleans {
                    flatten::convert_bool_value(value)
                } else {
                    Cow::Borrowed(value.as_str())
                };
                batch.push(Some(Outgoing {
                    topic: Cow::Borrowed(target),
                    normalized: Cow::Owned(normalized),
                    value,
                }));
            }
        }
        self.deliver(topic, &batch).await;
    }

    /// Put one message's values on the wire, in order.
    ///
    /// Sequential rather than concurrent: all values of a message share one
    /// connection, so sending them in order is also what keeps the Miniserver
    /// seeing them in the order the DOM route defined.
    async fn deliver(&self, topic: &str, batch: &Batch<'_>) {
        if batch.is_empty() {
            return;
        }
        debug!(
            "Handing {} value(s) over to the Miniserver from '{}'",
            batch.len(),
            loggable(topic)
        );
        for (index, slot) in batch.iter().enumerate() {
            let Some(item) = slot else {
                continue;
            };
            let Err(error) = self.egress.send(&item.normalized, &item.value).await else {
                continue;
            };
            if error.aborts_batch() {
                // Nothing retries these, so name what was lost. One line for the
                // message instead of one per value - they share the connection,
                // so they share its fate.
                warn!(
                    "Dropped {} value(s) from '{}': {error}",
                    batch.len() - index,
                    loggable(topic)
                );
                return;
            }
            error!(
                "Error sending {} (as {})={} to the Miniserver: {error}",
                loggable(&item.topic),
                loggable(&item.normalized),
                loggable(&item.value)
            );
        }
    }
}

/// Resolve target topic and filter verdict once, at learn time.
impl<E: Egress> LeafPolicy for Core<E> {
    fn leaf(&self, full_topic: &str) -> PlanNode {
        let normalized = flatten::normalize_topic_str(full_topic);
        let whitelist = self.topic_whitelist.load();
        if !self.allowed_by_whitelist(&whitelist, &normalized) {
            return PlanNode::Drop;
        }
        let subscription = self.subscription_filters.load();
        if let Some(filters) = subscription.as_ref()
            && filters.is_match(full_topic)
        {
            return PlanNode::Drop;
        }
        let do_not_forward = self.do_not_forward.load();
        if let Some(filters) = do_not_forward.as_ref()
            && filters.is_match(full_topic)
        {
            return PlanNode::Drop;
        }
        PlanNode::Emit {
            topic: Box::from(full_topic),
            normalized: Box::from(normalized),
            slot: 0,
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Building a [`Core`] for the tests, and driving it without a Miniserver.

    use super::*;
    use crate::egress::RecordingEgress;

    pub(crate) const BASE_TOPIC: &str = "myrelay/";

    pub(crate) fn topics() -> MqttTopics {
        MqttTopics {
            miniserver_startup: format!("{BASE_TOPIC}miniserverevent/startup"),
            config_get: format!("{BASE_TOPIC}config/get"),
            config_response: format!("{BASE_TOPIC}config/response"),
            config_set: format!("{BASE_TOPIC}config/set"),
            config_add: format!("{BASE_TOPIC}config/add"),
            config_remove: format!("{BASE_TOPIC}config/remove"),
            config_update: format!("{BASE_TOPIC}config/update"),
            config_restart: format!("{BASE_TOPIC}config/restart"),
        }
    }

    pub(crate) fn config() -> CoreConfig {
        CoreConfig {
            topics: topics(),
            subscription_filters: Vec::new(),
            do_not_forward: Vec::new(),
            topic_whitelist: HashSet::new(),
            // Tests that need the sync-on fail-closed gate set this themselves.
            whitelist_required: false,
            cache_size: 512,
            expand_json: true,
            convert_booleans: true,
        }
    }

    /// A core with a recording egress, plans on.
    pub(crate) fn core(config: CoreConfig) -> Arc<Core<RecordingEgress>> {
        Arc::new(Core::new(config, RecordingEgress::new()).expect("valid filters"))
    }

    impl Core<RecordingEgress> {
        /// Feed one message and hand back the `(input name, value)` pairs it
        /// produced.
        pub(crate) async fn run(&self, topic: &str, message: &str) -> Vec<(String, String)> {
            self.egress.drain();
            self.process_data(topic, message).await;
            self.egress.drain()
        }

        pub(crate) fn egress(&self) -> &RecordingEgress {
            &self.egress
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[tokio::test]
    async fn json_is_flattened_into_one_value_per_leaf() {
        let core = core(config());
        assert_eq!(
            core.run("dev/x", r#"{"a":1,"b":2}"#).await,
            vec![
                ("dev_x_a".to_string(), "1".to_string()),
                ("dev_x_b".to_string(), "2".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn a_payload_that_is_not_json_is_forwarded_as_it_stands() {
        let core = core(config());
        assert_eq!(
            core.run("dev/x", "23.5").await,
            vec![("dev_x".to_string(), "23.5".to_string())]
        );
    }

    /// The two routes are output-identical by design, and this is what keeps
    /// them that way: the same message through a core with plans and one
    /// pinned to the DOM route.
    #[tokio::test]
    async fn the_plan_and_the_dom_route_agree() {
        let plan = core(config());
        let dom = core(config());
        dom.set_shape_cache_enabled(false);

        let payloads = [
            r#"{"a":1,"b":"on"}"#,
            r#"{"a":2,"b":"off"}"#,
            r#"{"nested":{"deep":[1,2,{"x":"yes"}]}}"#,
            r#"{"esc":"a\nb"}"#,
            r#"{"wide":123456789012345678901234567890}"#,
            "not json at all",
            r#"{"a":1,"b":"on"}"#,
        ];
        for payload in payloads {
            assert_eq!(
                plan.run("dev/x", payload).await,
                dom.run("dev/x", payload).await,
                "{payload}"
            );
        }
        // The fast path really did engage, or the two would be comparing the
        // DOM route against itself.
        assert!(plan.shape_metrics().hits > 0);
        assert_eq!(dom.shape_metrics().learns, 0);
    }

    #[tokio::test]
    async fn a_whitelist_keeps_only_what_it_names() {
        let core = core(config());
        core.update_topic_whitelist(vec!["dev_x_a".to_string()]);
        assert_eq!(
            core.run("dev/x", r#"{"a":1,"b":2}"#).await,
            vec![("dev_x_a".to_string(), "1".to_string())]
        );
    }

    /// Sync-on means the whitelist is the sole source of allowed inputs: empty
    /// must not fall open to "forward everything".
    #[tokio::test]
    async fn an_empty_required_whitelist_forwards_nothing() {
        let mut cfg = config();
        cfg.whitelist_required = true;
        let core = core(cfg);
        assert!(core.run("dev/x", r#"{"a":1,"b":2}"#).await.is_empty());
        assert!(core.run("dev/x", "23.5").await.is_empty());
    }

    /// Sync-off keeps the historical meaning: an empty list is "no filter".
    #[tokio::test]
    async fn an_empty_optional_whitelist_still_forwards() {
        let core = core(config());
        assert_eq!(
            core.run("dev/x", r#"{"a":1}"#).await,
            vec![("dev_x_a".to_string(), "1".to_string())]
        );
    }

    #[tokio::test]
    async fn do_not_forward_drops_matching_leaves() {
        let core = core(config());
        core.update_do_not_forward(vec!["/b$".to_string()]).unwrap();
        assert_eq!(
            core.run("dev/x", r#"{"a":1,"b":2}"#).await,
            vec![("dev_x_a".to_string(), "1".to_string())]
        );
    }

    /// The subscription filter runs on the original topic, so a match takes the
    /// whole message rather than a single leaf.
    #[tokio::test]
    async fn a_subscription_filter_drops_the_whole_message() {
        let core = core(config());
        core.update_subscription_filters(vec!["^dev/x$".to_string()])
            .unwrap();
        assert!(core.run("dev/x", r#"{"a":1}"#).await.is_empty());
        assert_eq!(core.run("dev/y", r#"{"a":1}"#).await.len(), 1);
    }

    /// A filter that arrives after a plan was learned has to void that plan,
    /// or it would keep forwarding what the filter was meant to hold back.
    #[tokio::test]
    async fn a_filter_change_voids_learned_plans() {
        let core = core(config());
        assert_eq!(core.run("dev/x", r#"{"a":1,"b":2}"#).await.len(), 2);
        assert_eq!(core.shape_stats().0, 1, "a plan was cached");

        core.update_do_not_forward(vec!["/b$".to_string()]).unwrap();
        assert_eq!(core.shape_stats().0, 0, "and dropped again");
        assert_eq!(
            core.run("dev/x", r#"{"a":1,"b":2}"#).await,
            vec![("dev_x_a".to_string(), "1".to_string())]
        );
    }

    #[tokio::test]
    async fn convert_booleans_can_be_turned_off() {
        let mut cfg = config();
        cfg.convert_booleans = false;
        let core = core(cfg);
        assert_eq!(
            core.run("dev/x", r#"{"a":"on","b":true}"#).await,
            vec![
                ("dev_x_a".to_string(), "on".to_string()),
                ("dev_x_b".to_string(), "true".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn expand_json_off_forwards_the_raw_message() {
        let mut cfg = config();
        cfg.expand_json = false;
        let core = core(cfg);
        let raw = r#"{"a":1}"#;
        assert_eq!(
            core.run("dev/x", raw).await,
            vec![("dev_x".to_string(), raw.to_string())]
        );
    }

    /// Values change constantly, the layout does not - so a new value must not
    /// cost a relearn.
    #[tokio::test]
    async fn value_changes_do_not_need_a_relearn() {
        let core = core(config());
        for value in 0..20 {
            core.run("dev/x", &format!(r#"{{"a":{value}}}"#)).await;
        }
        let (plans, learns) = core.shape_stats();
        assert_eq!((plans, learns), (1, 1));
        assert_eq!(core.shape_metrics().hits, 19);
    }

    /// A topic whose payloads the scanner keeps refusing must stop paying for
    /// the attempt, but not forever.
    #[tokio::test]
    async fn a_topic_that_refuses_everything_stops_being_offered_a_plan() {
        let core = core(config());
        // An escaped string is never plannable.
        for _ in 0..shape::NEGATIVE_STRIKES {
            core.run("dev/x", r#"{"a":"a\nb"}"#).await;
        }
        let before = core.shape_metrics().negative_skips;
        core.run("dev/x", r#"{"a":"a\nb"}"#).await;
        assert!(core.shape_metrics().negative_skips > before);
    }

    /// Nothing is handed to the egress when every leaf was filtered away, so a
    /// fully filtered message costs no send at all.
    #[tokio::test]
    async fn nothing_is_delivered_when_everything_is_filtered() {
        let core = core(config());
        core.update_do_not_forward(vec![".".to_string()]).unwrap();
        assert!(core.run("dev/x", r#"{"a":1,"b":2}"#).await.is_empty());
    }

    #[tokio::test]
    async fn a_disconnected_egress_drops_the_whole_batch() {
        let core = Arc::new(
            Core::new(config(), crate::egress::RecordingEgress::disconnected())
                .expect("valid filters"),
        );
        core.process_data("dev/x", r#"{"a":1,"b":2}"#).await;
        assert!(core.egress().drain().is_empty());
    }

    /// One value failing on its own merits must not take the rest of the
    /// message with it.
    #[tokio::test]
    async fn one_bad_value_does_not_abort_the_batch() {
        let core = core(config());
        core.egress().fail_on("dev_x_a");
        assert_eq!(
            core.run("dev/x", r#"{"a":1,"b":2}"#).await,
            vec![("dev_x_b".to_string(), "2".to_string())]
        );
    }

    /// Two leaves that normalize onto the same input mean the later write wins,
    /// so both routes have to agree on which one is later.
    #[tokio::test]
    async fn colliding_targets_keep_the_dom_ordering() {
        let plan = core(config());
        let dom = core(config());
        dom.set_shape_cache_enabled(false);
        // "b/c" and "b%c" both normalize to dev_x_b_c.
        let payload = r#"{"b/c":1,"b%c":2}"#;
        assert_eq!(plan.run("dev/x", payload).await, dom.run("dev/x", payload).await);
    }

    #[test]
    fn control_topics_are_told_apart_from_data() {
        let core = core(config());
        assert_eq!(
            core.control_kind("myrelay/config/get"),
            Some(ControlTopic::ConfigGet)
        );
        assert_eq!(
            core.control_kind("myrelay/config/update"),
            Some(ControlTopic::ConfigReload)
        );
        assert_eq!(
            core.control_kind("myrelay/config/restart"),
            Some(ControlTopic::ConfigReload)
        );
        assert_eq!(
            core.control_kind("myrelay/miniserverevent/startup"),
            Some(ControlTopic::MiniserverStartup)
        );
        // Under base_topic but not a control topic, so it takes the data path.
        assert_eq!(core.control_kind("myrelay/something/else"), None);
        assert_eq!(core.control_kind("dev/x"), None);
    }

    /// A binary payload has to survive exactly, because the value ends up on a
    /// Miniserver input where a replacement character is indistinguishable from
    /// one the sender meant.
    #[tokio::test]
    async fn binary_payloads_are_forwarded_as_base64() {
        let core = core(config());
        core.handle_data("dev/x", b"\xff\xfe").await;
        assert_eq!(
            core.egress().drain(),
            vec![("dev_x".to_string(), "[base64:／／4=]".replace('／', "/"))]
        );
    }

    #[test]
    fn an_empty_filter_pattern_is_refused() {
        let mut cfg = config();
        cfg.do_not_forward = vec![String::new()];
        assert!(Core::new(cfg, crate::egress::RecordingEgress::new()).is_err());
    }

    #[test]
    fn an_invalid_filter_pattern_names_itself() {
        let core = core(config());
        let err = core
            .update_subscription_filters(vec!["[unclosed".to_string()])
            .expect_err("invalid");
        assert!(err.0.contains("[unclosed"), "{}", err.0);
    }

    /// The cached helpers are the reference the flattening routes are pinned
    /// against, so they have to agree with them over the whole keyword table.
    #[test]
    fn the_cached_helpers_agree_with_the_shared_ones() {
        let core = core(config());
        for value in [
            "true", "false", "on", "off", "yes", "no", "1", "0", "enabled", "disabled", "enable",
            "disable", "check", "checked", "select", "selected", "23.5", "Rollo", "", " TRUE ",
        ] {
            assert_eq!(
                core.convert_boolean(value),
                flatten::convert_bool_value(value),
                "{value}"
            );
        }
        for topic in ["dev/a%b/c", "plain", "a//b", "%", ""] {
            assert_eq!(
                core.normalize_topic(topic),
                flatten::normalize_topic_str(topic),
                "{topic}"
            );
        }
    }
    // -- the accessors the relay and the operator read ----------------------

    /// The getters hand back exactly what was configured, pattern for pattern.
    ///
    /// They once rebuilt the list by splitting one joined expression at every
    /// '|', which mangled any pattern containing a pipe - escaped, in a
    /// character class, or as a real alternation.
    #[test]
    fn the_filter_getters_return_what_was_configured() {
        for filters in [
            vec![r"^foo\|bar$".to_string()],
            vec![r"bar\\|baz".to_string()],
            vec![r"[|]".to_string()],
            vec![r"^a$".to_string(), r"^b$".to_string()],
        ] {
            let core = core(config());
            core.update_subscription_filters(filters.clone())
                .expect("usable patterns");
            assert_eq!(core.subscription_filters(), filters);

            core.update_do_not_forward(filters.clone())
                .expect("usable patterns");
            assert_eq!(core.do_not_forward_patterns(), filters);
        }
    }

    #[test]
    fn the_whitelist_getter_returns_what_was_set() {
        let core = core(config());
        core.update_topic_whitelist(vec![
            "some_allowed_topic".to_string(),
            "another_allowed_topic".to_string(),
        ]);
        assert_eq!(
            core.topic_whitelist(),
            ["some_allowed_topic".to_string(), "another_allowed_topic".to_string()].into()
        );
    }

    /// The whitelist holds Miniserver input names, not MQTT topics, so the
    /// question is asked about the normalized name.
    #[test]
    fn is_in_whitelist_asks_about_the_normalized_name() {
        let core = core(config());
        core.update_topic_whitelist(vec!["a_b_c".to_string()]);
        assert!(core.is_in_whitelist("a/b/c"));
        assert!(!core.is_in_whitelist("a/b/d"));
    }

    /// `shape_stats` is the pair that was there first; `shape_metrics` is the
    /// same two numbers plus the rest. They must not drift apart.
    #[tokio::test]
    async fn the_shape_stats_and_metrics_agree() {
        let core = core(config());
        core.run("dev/x", r#"{"a":1}"#).await;
        core.run("dev/x", r#"{"a":2}"#).await;

        let (plans, learns) = core.shape_stats();
        let metrics = core.shape_metrics();
        assert_eq!(metrics.plans, plans);
        assert_eq!(metrics.learns, learns);
        assert!(metrics.hits > 0, "the second message should have replayed");
    }

    /// `convert_booleans` reports what the core was built with, so the banner
    /// and a bug report agree about it.
    #[test]
    fn convert_booleans_reports_what_was_configured() {
        assert!(core(config()).convert_booleans());
        let mut off = config();
        off.convert_booleans = false;
        assert!(!core(off).convert_booleans());
    }

}

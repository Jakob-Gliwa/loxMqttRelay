//! Learned JSON layouts, replayed by a byte scanner.
//!
//! Devices repeat the same JSON layout message after message. Instead of
//! building a DOM every time, the first message of a topic is turned into a
//! plan: the tree of keys, and per leaf the finished target topic plus the
//! verdict of every filter. Later messages are matched against that plan with a
//! byte scanner that only reads the values.
//!
//! The moment the document deviates from the plan - a renamed key, an extra
//! field, an escaped string, a number that would not round-trip - the scanner
//! bails out and the DOM path in [`super::flatten`] takes over, so output can
//! never silently drift.
//!
//! Nothing here touches Python. The plans hold plain `Box<str>`, which is what
//! lets the whole data path run without the GIL.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::num::NonZeroUsize;

use log::debug;
use lru::LruCache;

use super::Outgoing;
use crate::util::loggable;

/// Number of topics that keep a learned plan. Trades memory for a bounded
/// working set.
pub(crate) const SHAPE_CACHE_ENTRIES: usize = 512;

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
pub(crate) const NEGATIVE_STRIKES: u32 = 8;

/// Once skipping, how many messages pass before the plan route is offered again.
///
/// Bounded rather than permanent: a publisher whose payloads become plannable
/// again - a firmware update that stops escaping a string, a value that leaves
/// the range where it renders as `1e3` - must not be stuck on the slow route
/// for the rest of the process.
pub(crate) const NEGATIVE_RETRY_EVERY: u32 = 64;

/// Matches serde_json's own recursion limit; deeper documents are rejected
/// there anyway, and it keeps the recursive scanner off the stack cliff.
const MAX_PLAN_DEPTH: u32 = 128;

/// What the learner needs from its owner: the target topic of a leaf and
/// whether the filters let it through.
///
/// A trait rather than a closure so the filter verdicts stay where the filters
/// live, while the scanner below knows nothing about them.
pub(crate) trait LeafPolicy {
    /// Resolve target topic and filter verdict for one leaf, once, at learn
    /// time.
    fn leaf(&self, full_topic: &str) -> PlanNode;
}

pub(crate) enum PlanNode {
    /// A value that passed every filter when the plan was learned.
    Emit {
        topic: Box<str>,
        normalized: Box<str>,
        /// Position in the output, so emission order matches the DOM path.
        slot: usize,
    },
    /// A value the filters drop - still has to be consumed by the scanner.
    Drop,
    Object(Vec<(Box<str>, PlanNode)>),
    Array(Vec<PlanNode>),
}

pub(crate) struct Shape {
    root: PlanNode,
    pub(crate) emits: usize,
}

/// A topic's record of refusals, kept only while it has one.
#[derive(Default)]
struct Refusals {
    /// Documents refused in a row. Reset by any plan this topic manages to
    /// build, so a topic that only fails now and then never arms the skip.
    strikes: u32,
    /// Messages waved through since the skip armed, for the periodic retry.
    skipped: u32,
}

/// The learned plans, plus the counters that say which route messages took.
///
/// The counters exist because the two flattening routes are output-identical by
/// design: without them, a fast path that silently stopped engaging would leave
/// every test comparing the slow route against itself.
pub(crate) struct ShapeStore {
    plans: LruCache<String, std::sync::Arc<Shape>>,
    /// Topics that have refused a document recently. Bounded like `plans`, so a
    /// flood of one-off topics cannot grow it.
    unplannable: LruCache<String, Refusals>,
    pub(crate) learns: u64,
    pub(crate) hits: u64,
    pub(crate) learn_failures: u64,
    pub(crate) dom_fallbacks: u64,
    pub(crate) negative_skips: u64,
}

/// Which route a message takes, decided under one lock.
pub(crate) enum Route {
    /// Replay this plan.
    Cached(std::sync::Arc<Shape>),
    /// No plan yet - try to build one.
    Learn,
    /// This topic has refused everything for a while; go straight to the DOM.
    Skip,
}

impl ShapeStore {
    pub(crate) fn new() -> Self {
        let entries = NonZeroUsize::new(SHAPE_CACHE_ENTRIES).expect("non-zero cache size");
        ShapeStore {
            plans: LruCache::new(entries),
            unplannable: LruCache::new(entries),
            learns: 0,
            hits: 0,
            learn_failures: 0,
            dom_fallbacks: 0,
            negative_skips: 0,
        }
    }

    pub(crate) fn plan_count(&self) -> usize {
        self.plans.len()
    }

    pub(crate) fn unplannable_count(&self) -> usize {
        self.unplannable.len()
    }

    /// Plans cache the verdict of every filter, so a filter change voids them.
    pub(crate) fn clear(&mut self) {
        self.plans.clear();
        // The hold-backs go with them: whether a document is plannable does not
        // depend on the filters, but a topic held back would keep paying for a
        // refusal that has nothing to do with the change just made.
        self.unplannable.clear();
    }

    /// Pick the route for one message, counting what that choice was.
    ///
    /// A hit is counted here rather than after the plan ran, so it costs no
    /// second lock; a plan that then turns out not to fit takes it back through
    /// [`Self::unfit`], on a path that locks anyway.
    pub(crate) fn route(&mut self, topic: &str) -> Route {
        match self.plans.get(topic).map(std::sync::Arc::clone) {
            Some(shape) => {
                self.hits += 1;
                Route::Cached(shape)
            }
            None if self.hold_back(topic) => {
                self.negative_skips += 1;
                self.dom_fallbacks += 1;
                Route::Skip
            }
            None => Route::Learn,
        }
    }

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

    /// Take back the hit counted for a plan the document had outgrown, and drop
    /// the plan.
    ///
    /// Dropped rather than left for the LRU to evict: a plan that does not fit
    /// the documents arriving now would be tried, and fail, on every one of
    /// them.
    pub(crate) fn unfit(&mut self, topic: &str) {
        self.hits -= 1;
        self.plans.pop(topic);
    }

    /// A plan that carried a whole document end to end.
    pub(crate) fn store(&mut self, topic: &str, shape: std::sync::Arc<Shape>) {
        self.learns += 1;
        self.plans.put(topic.to_string(), shape);
        self.unplannable.pop(topic);
    }

    /// Note that no plan could be built for this topic.
    pub(crate) fn refused(&mut self, topic: &str) {
        self.learn_failures += 1;
        self.dom_fallbacks += 1;
        if let Some(record) = self.unplannable.get_mut(topic) {
            record.strikes = record.strikes.saturating_add(1);
            return;
        }
        self.unplannable
            .put(topic.to_string(), Refusals { strikes: 1, skipped: 0 });
    }
}

enum Scalar<'a> {
    Number(&'a str),
    True,
    False,
    Null,
}

pub(crate) struct Scan<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Scan<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
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
    pub(crate) fn at_end(&mut self) -> bool {
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
/// passed through untouched and JSON literals render the way the DOM route
/// writes them.
fn read_leaf<'a>(sc: &mut Scan<'a>, convert: bool) -> Option<Cow<'a, str>> {
    match sc.peek()? {
        b'"' => sc.string().map(|s| {
            if convert {
                super::flatten::convert_bool_value(s)
            } else {
                Cow::Borrowed(s)
            }
        }),
        // The plan expected a scalar but the document grew a container here.
        b'{' | b'[' => None,
        _ => match sc.scalar()? {
            // Numbers render as "1"/"0"/"12.5"; the boolean table maps those to
            // themselves, so it can be skipped.
            Scalar::Number(text) => super::flatten::number_value(text),
            Scalar::True => Some(Cow::Borrowed(if convert { "1" } else { "true" })),
            Scalar::False => Some(Cow::Borrowed(if convert { "0" } else { "false" })),
            Scalar::Null => Some(Cow::Borrowed("null")),
        },
    }
}

/// Hand every emitting leaf the slot it occupies in the DOM path's output.
///
/// The DOM route walks a `BTreeMap`, so within an object it visits keys in
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
fn exec_node<'a>(
    node: &'a PlanNode,
    sc: &mut Scan<'a>,
    out: &mut [Option<Outgoing<'a>>],
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
            *cell = Some(Outgoing {
                topic: Cow::Borrowed(topic),
                normalized: Cow::Borrowed(normalized),
                value,
            });
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

impl Shape {
    /// Run this plan and fill `out`, or report that it did not carry the whole
    /// document.
    ///
    /// `out` is sized to the plan's slot count up front, which also makes "a
    /// miss leaves nothing usable behind" structural: a partial run leaves
    /// empty slots, and an empty slot is a miss.
    pub(crate) fn emit<'a>(
        &'a self,
        message: &'a str,
        convert_booleans: bool,
        out: &mut Vec<Option<Outgoing<'a>>>,
    ) -> bool {
        out.clear();
        out.resize_with(self.emits, || None);

        let mut sc = Scan::new(message.as_bytes());
        if !exec_node(&self.root, &mut sc, out, convert_booleans) || !sc.at_end() {
            return false;
        }
        out.iter().all(Option::is_some)
    }
}

/// Derive a plan from one message, or `None` if this document cannot be
/// replayed faithfully.
pub(crate) fn learn(policy: &impl LeafPolicy, topic: &str, message: &str) -> Option<Shape> {
    let mut sc = Scan::new(message.as_bytes());
    if sc.peek() != Some(b'{') {
        return None;
    }
    let mut path = String::with_capacity(topic.len() + 64);
    path.push_str(topic);
    let mut root = learn_value(policy, &mut sc, &mut path, 0)?;
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

fn learn_value(
    policy: &impl LeafPolicy,
    sc: &mut Scan<'_>,
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
                let child = learn_value(policy, sc, path, depth + 1)?;
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
                let _ = write!(path, "{index}");
                let child = learn_value(policy, sc, path, depth + 1)?;
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
            Some(policy.leaf(path))
        }
        _ => {
            if let Scalar::Number(text) = sc.scalar()? {
                // Refuse the plan rather than learn a leaf whose value we
                // could not render identically later on.
                super::flatten::number_value(text)?;
            }
            Some(policy.leaf(path))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Emits everything, so the tests below exercise the scanner rather than
    /// the filters.
    struct EmitAll;

    impl LeafPolicy for EmitAll {
        fn leaf(&self, full_topic: &str) -> PlanNode {
            PlanNode::Emit {
                topic: Box::from(full_topic),
                normalized: Box::from(super::super::flatten::normalize_topic_str(full_topic)),
                slot: 0,
            }
        }
    }

    fn replay(shape: &Shape, message: &str) -> Option<Vec<(String, String)>> {
        let mut out = Vec::new();
        if !shape.emit(message, true, &mut out) {
            return None;
        }
        Some(
            out.into_iter()
                .map(|slot| {
                    let item = slot.expect("checked filled");
                    (item.normalized.into_owned(), item.value.into_owned())
                })
                .collect(),
        )
    }

    #[test]
    fn a_plan_replays_a_document_of_the_same_shape() {
        let shape = learn(&EmitAll, "dev/x", r#"{"a":1,"b":"on"}"#).expect("plannable");
        assert_eq!(
            replay(&shape, r#"{"a":7,"b":"off"}"#),
            Some(vec![
                ("dev_x_a".to_string(), "7".to_string()),
                ("dev_x_b".to_string(), "0".to_string()),
            ])
        );
    }

    /// The scanner follows document order, but the output has to match the
    /// DOM route, which visits an object's keys sorted.
    #[test]
    fn slots_restore_the_dom_ordering() {
        let shape = learn(&EmitAll, "dev/x", r#"{"b":1,"a":2}"#).expect("plannable");
        let out = replay(&shape, r#"{"b":10,"a":20}"#).expect("replayable");
        assert_eq!(
            out,
            vec![
                ("dev_x_a".to_string(), "20".to_string()),
                ("dev_x_b".to_string(), "10".to_string()),
            ]
        );
    }

    #[test]
    fn a_renamed_key_makes_the_plan_refuse() {
        let shape = learn(&EmitAll, "dev/x", r#"{"a":1}"#).expect("plannable");
        assert!(replay(&shape, r#"{"z":1}"#).is_none());
    }

    #[test]
    fn an_extra_field_makes_the_plan_refuse() {
        let shape = learn(&EmitAll, "dev/x", r#"{"a":1}"#).expect("plannable");
        assert!(replay(&shape, r#"{"a":1,"b":2}"#).is_none());
    }

    #[test]
    fn a_scalar_that_became_a_container_makes_the_plan_refuse() {
        let shape = learn(&EmitAll, "dev/x", r#"{"a":1}"#).expect("plannable");
        assert!(replay(&shape, r#"{"a":{"b":1}}"#).is_none());
    }

    /// The plan hands out slices borrowed from the message, so an escape it
    /// would have to expand cannot be replayed.
    #[test]
    fn escapes_are_left_to_the_dom_route() {
        assert!(learn(&EmitAll, "dev/x", r#"{"a":"a\nb"}"#).is_none());
        let shape = learn(&EmitAll, "dev/x", r#"{"a":"plain"}"#).expect("plannable");
        assert!(replay(&shape, r#"{"a":"a\nb"}"#).is_none());
    }

    #[test]
    fn duplicate_keys_are_left_to_the_dom_route() {
        assert!(learn(&EmitAll, "dev/x", r#"{"a":1,"a":2}"#).is_none());
    }

    #[test]
    fn a_number_that_would_not_round_trip_is_left_to_the_dom_route() {
        assert!(learn(&EmitAll, "dev/x", r#"{"a":007}"#).is_none());
        let shape = learn(&EmitAll, "dev/x", r#"{"a":1}"#).expect("plannable");
        assert!(replay(&shape, r#"{"a":-0}"#).is_none());
    }

    #[test]
    fn trailing_content_is_refused() {
        assert!(learn(&EmitAll, "dev/x", r#"{"a":1} junk"#).is_none());
        let shape = learn(&EmitAll, "dev/x", r#"{"a":1}"#).expect("plannable");
        assert!(replay(&shape, r#"{"a":1} junk"#).is_none());
    }

    #[test]
    fn only_objects_are_planned() {
        assert!(learn(&EmitAll, "dev/x", "[1,2]").is_none());
        assert!(learn(&EmitAll, "dev/x", "\"text\"").is_none());
    }

    #[test]
    fn nested_containers_are_planned_through() {
        let shape =
            learn(&EmitAll, "dev/x", r#"{"a":{"b":[1,2]}}"#).expect("plannable");
        assert_eq!(
            replay(&shape, r#"{"a":{"b":[3,4]}}"#),
            Some(vec![
                ("dev_x_a_b_0".to_string(), "3".to_string()),
                ("dev_x_a_b_1".to_string(), "4".to_string()),
            ])
        );
    }

    /// A dropped leaf still has to be consumed, or every value after it would
    /// land in the wrong slot.
    #[test]
    fn a_dropped_leaf_is_still_read_past() {
        struct DropFirst;
        impl LeafPolicy for DropFirst {
            fn leaf(&self, full_topic: &str) -> PlanNode {
                if full_topic.ends_with("/a") {
                    return PlanNode::Drop;
                }
                PlanNode::Emit {
                    topic: Box::from(full_topic),
                    normalized: Box::from(super::super::flatten::normalize_topic_str(full_topic)),
                    slot: 0,
                }
            }
        }
        let shape = learn(&DropFirst, "dev/x", r#"{"a":1,"b":2}"#).expect("plannable");
        let mut out = Vec::new();
        assert!(shape.emit(r#"{"a":9,"b":8}"#, true, &mut out));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_ref().unwrap().value, "8");
    }

    #[test]
    fn a_hold_back_arms_only_after_repeated_refusals() {
        let mut store = ShapeStore::new();
        for _ in 0..NEGATIVE_STRIKES - 1 {
            store.refused("dev/x");
            assert!(
                matches!(store.route("dev/x"), Route::Learn),
                "still worth a try"
            );
        }
        store.refused("dev/x");
        assert!(matches!(store.route("dev/x"), Route::Skip));
    }

    /// A skip has to expire on its own, or a publisher whose payloads become
    /// plannable again would never get its fast path back.
    #[test]
    fn a_hold_back_expires_periodically() {
        let mut store = ShapeStore::new();
        for _ in 0..NEGATIVE_STRIKES {
            store.refused("dev/x");
        }
        let mut skipped = 0;
        for _ in 0..NEGATIVE_RETRY_EVERY {
            match store.route("dev/x") {
                Route::Skip => skipped += 1,
                Route::Learn => break,
                Route::Cached(_) => unreachable!("nothing was stored"),
            }
        }
        assert_eq!(skipped, NEGATIVE_RETRY_EVERY as usize - 1);
        assert!(matches!(store.route("dev/x"), Route::Skip), "and re-arms");
    }

    #[test]
    fn a_stored_plan_clears_the_refusals() {
        let mut store = ShapeStore::new();
        for _ in 0..NEGATIVE_STRIKES {
            store.refused("dev/x");
        }
        let shape = learn(&EmitAll, "dev/x", r#"{"a":1}"#).expect("plannable");
        store.store("dev/x", std::sync::Arc::new(shape));
        assert_eq!(store.unplannable_count(), 0);
        assert!(matches!(store.route("dev/x"), Route::Cached(_)));
    }

    #[test]
    fn a_filter_change_clears_plans_and_hold_backs() {
        let mut store = ShapeStore::new();
        let shape = learn(&EmitAll, "dev/x", r#"{"a":1}"#).expect("plannable");
        store.store("dev/x", std::sync::Arc::new(shape));
        store.refused("dev/y");
        store.clear();
        assert_eq!(store.plan_count(), 0);
        assert_eq!(store.unplannable_count(), 0);
    }
}

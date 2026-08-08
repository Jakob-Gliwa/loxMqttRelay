//! The differential suite for the two flattening routes.
//!
//! The flattener has two routes. The fast one learns a topic's JSON layout once
//! and afterwards only reads values with a byte scanner. The slow one builds a
//! `serde_json` DOM and stays in the binary as the fallback for every document
//! the scanner refuses.
//!
//! Every test here drives both routes with the same message and demands
//! identical output - same values, same order, including which value wins when
//! two JSON paths normalize onto the same Miniserver input.
//! [`Core::shape_metrics`] backs that up: without it a fast path that silently
//! stopped engaging would compare the slow route against itself and pass.
//!
//! What the route comparison covers is therefore the part where the two
//! genuinely differ: which leaves a document yields, in which order, and which
//! of them survive the filters. The two per-leaf transformations are shared
//! code, so they are pinned separately against the cached
//! [`Core::convert_boolean`] and [`Core::normalize_topic`], which remain
//! independent implementations of the same two answers.
//!
//! The corpus - anonymized real device payloads, handcrafted oddities and
//! deterministic synthetics - is frozen into `corpus/`. It was generated once
//! from a seeded generator and checked in rather than rebuilt here: it was
//! always fully determined by its seed, so freezing it costs nothing and saves
//! reimplementing a JSON encoder byte for byte.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock};

use super::testing::{config, core};
use super::*;
use crate::egress::RecordingEgress;

/// Named payloads: real devices, oddities, synthetics.
static PAYLOADS: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("corpus/payloads.json")).expect("readable payload corpus")
});

/// 400 arbitrary documents, in the order the generator produced them.
static FUZZ: LazyLock<Vec<String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("corpus/fuzz.json")).expect("readable fuzz corpus")
});

fn payload(name: &str) -> &'static str {
    PAYLOADS
        .get(name)
        .unwrap_or_else(|| panic!("no payload named '{name}'"))
}

/// Filters and whitelist for one run.
///
/// `whitelist` is a step: the entries are picked out of the targets a payload
/// actually produces, so the filter bites instead of matching nothing.
struct Scenario {
    name: &'static str,
    expand_json: bool,
    subscription_filters: &'static [&'static str],
    do_not_forward: &'static [&'static str],
    whitelist_step: Option<usize>,
}

const PLAIN: Scenario = Scenario {
    name: "plain",
    expand_json: true,
    subscription_filters: &[],
    do_not_forward: &[],
    whitelist_step: None,
};

const SCENARIOS: &[Scenario] = &[
    PLAIN,
    Scenario {
        name: "no_expand",
        expand_json: false,
        subscription_filters: &[],
        do_not_forward: &[],
        whitelist_step: None,
    },
    Scenario {
        name: "whitelist_half",
        expand_json: true,
        subscription_filters: &[],
        do_not_forward: &[],
        whitelist_step: Some(2),
    },
    Scenario {
        name: "whitelist_sparse",
        expand_json: true,
        subscription_filters: &[],
        do_not_forward: &[],
        whitelist_step: Some(5),
    },
    Scenario {
        name: "do_not_forward",
        expand_json: true,
        subscription_filters: &[],
        do_not_forward: &["temp", "/k1", "/0$", "mode$", "status"],
        whitelist_step: None,
    },
    Scenario {
        name: "subscription_filter",
        expand_json: true,
        subscription_filters: &["hc1", "/k2", "sensorData/0/co2", "dhw"],
        do_not_forward: &[],
        whitelist_step: None,
    },
    Scenario {
        name: "combined",
        expand_json: true,
        subscription_filters: &["battery"],
        do_not_forward: &["co2", "unit$"],
        whitelist_step: Some(3),
    },
];

/// One core using shape plans, one pinned to the DOM route.
///
/// Both see the same messages in the same order, so the plan side builds up a
/// realistic cache history (learn, hit, relearn) while the DOM side stays the
/// unchanged reference.
struct Pair {
    plan: Arc<Core<RecordingEgress>>,
    dom: Arc<Core<RecordingEgress>>,
}

impl Pair {
    fn new(expand_json: bool, convert_booleans: bool) -> Self {
        let build = || {
            let mut cfg = config();
            cfg.expand_json = expand_json;
            cfg.convert_booleans = convert_booleans;
            core(cfg)
        };
        let pair = Pair {
            plan: build(),
            dom: build(),
        };
        pair.dom.set_shape_cache_enabled(false);
        pair
    }

    fn apply(&self, scenario: &Scenario, whitelist: Vec<String>) {
        for core in [&self.plan, &self.dom] {
            core.update_subscription_filters(strings(scenario.subscription_filters))
                .expect("valid filters");
            core.update_do_not_forward(strings(scenario.do_not_forward))
                .expect("valid filters");
            core.update_topic_whitelist(whitelist.clone());
        }
    }

    /// Feed both and hand back `(plan output, dom output)`.
    async fn run(&self, topic: &str, message: &str) -> (Vec<(String, String)>, Vec<(String, String)>) {
        (
            self.plan.run(topic, message).await,
            self.dom.run(topic, message).await,
        )
    }

    /// The values both routes agreed on, or a panic naming where they did not.
    async fn agree(&self, topic: &str, message: &str, what: &str) -> Vec<(String, String)> {
        let (plan, dom) = self.run(topic, message).await;
        assert_eq!(plan, dom, "{what} diverged");
        plan
    }

    /// The inputs a payload reaches unfiltered - the whitelist source.
    async fn targets(&self, name: &str) -> Vec<String> {
        self.apply(&PLAIN, Vec::new());
        let (_, dom) = self.run(&topic_for(name), payload(name)).await;
        dom.into_iter().map(|(input, _)| input).collect()
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Some payloads arrive on a topic that itself needs normalizing.
fn topic_for(name: &str) -> String {
    if name.bytes().map(u32::from).sum::<u32>() % 7 == 0 {
        format!("dev/a%b/{name}")
    } else {
        format!("dev/{name}")
    }
}

fn derive_whitelist(step: Option<usize>, targets: &[String]) -> Vec<String> {
    let Some(step) = step else {
        return Vec::new();
    };
    let sorted: BTreeSet<&String> = targets.iter().collect();
    sorted.into_iter().step_by(step).cloned().collect()
}

/// Scenarios beyond "plain" run on a representative slice: real and handcrafted
/// payloads always, synthetics every fourth.
fn in_slice(name: &str) -> bool {
    !name.starts_with("synth_") || name.bytes().map(u32::from).sum::<u32>() % 4 == 0
}

#[tokio::test]
async fn the_two_routes_agree_over_the_whole_corpus() {
    // One pair per expand_json setting, reused across every case: plans stay
    // warm, so eviction and invalidation get exercised the way a running relay
    // would.
    let expanding = Pair::new(true, true);
    let raw = Pair::new(false, true);

    for name in PAYLOADS.keys() {
        let topic = topic_for(name);
        let targets = expanding.targets(name).await;

        for scenario in SCENARIOS {
            if scenario.name != "plain" && !in_slice(name) {
                continue;
            }
            let pair = if scenario.expand_json {
                &expanding
            } else {
                &raw
            };
            pair.apply(scenario, derive_whitelist(scenario.whitelist_step, &targets));

            // First message learns the layout, second replays it from the cache.
            for pass in ["cold", "warm"] {
                pair.agree(&topic, payload(name), &format!("{name}/{}/{pass}", scenario.name))
                    .await;
            }
        }
    }

    // A permanently dead fast path would keep every assertion above green.
    assert!(expanding.plan.shape_metrics().hits > 0);
    assert_eq!(raw.plan.shape_metrics().learns, 0, "expand_json was off");
}

/// Random payloads on recycled topics, each sent twice.
///
/// The repeat is what makes a plan get replayed from the cache; the next
/// iteration's different layout on the same topic then forces a relearn. Both
/// are where the plan route is most likely to break, together with payloads
/// carrying escapes or numbers like `1e3` that make it bail out to the DOM
/// route part-way through a document.
#[tokio::test]
async fn the_two_routes_agree_across_changing_schemas() {
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());

    let mut compared = 0usize;
    let mut planned = 0u64;
    for (i, message) in FUZZ.iter().enumerate() {
        let topic = format!("dev/fuzz{}", i % 20);
        let before = pair.plan.shape_stats().1;
        for repeat in 0..2 {
            let out = pair
                .agree(&topic, message, &format!("iteration {i} pass {repeat} on {topic}"))
                .await;
            compared += out.len();
        }
        let built = pair.plan.shape_stats().1 - before;
        assert!(
            built <= 1,
            "iteration {i} rebuilt its plan for the identical repeat on {topic}"
        );
        planned += built;
    }

    assert!(compared > 5000, "the fuzz corpus produced almost no values");
    // Every built plan was replayed once by the repeat that followed it.
    assert!(planned > 100, "plans were hardly ever built and replayed");
    assert!(planned < 350, "the DOM fallback was hardly ever taken");
}

#[tokio::test]
async fn real_payloads_take_the_plan_route() {
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());

    let (cached_before, built_before) = pair.plan.shape_stats();
    for _ in 0..5 {
        for name in ["real_heating", "real_solar", "real_sensor"] {
            pair.run(&format!("dev/stats_{name}"), payload(name)).await;
        }
    }
    let (cached, built) = pair.plan.shape_stats();

    // Three plans for fifteen messages: the other twelve replayed a cached one,
    // and none of them fell back, or the payload would have left no plan behind.
    assert_eq!(built - built_before, 3, "each topic should be planned once");
    assert_eq!(cached - cached_before, 3, "every topic should hold a plan");
}

#[tokio::test]
async fn unplannable_payloads_fall_back_to_the_dom_route() {
    let cases = [
        ("odd_escaped_quote", "escapes need unescaping the scanner cannot do"),
        ("odd_escaped_unicode", "\\u sequences need decoding"),
        ("odd_duplicate_key", "serde_json keeps the last duplicate"),
        ("odd_scientific_numbers", "1e3 does not render as written"),
        ("odd_truncated", "invalid JSON is forwarded verbatim"),
        ("odd_trailing_garbage", "trailing bytes make it invalid JSON"),
        ("odd_root_array", "non-object roots are forwarded verbatim"),
        ("odd_plain_text", "not JSON at all"),
    ];
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());

    for (name, reason) in cases {
        let before = pair.plan.shape_stats();
        pair.agree(&format!("dev/fb_{name}"), payload(name), name).await;
        let after = pair.plan.shape_stats();
        assert_eq!(after.1, before.1, "{name}: {reason}");
        assert_eq!(after.0, before.0, "{name} must not leave a plan behind");
    }
}

/// A plan that stops fitting must not sit in the cache being retried.
///
/// Left behind, it would be fetched and run against every following message,
/// fail, and only leave once the LRU evicted it.
#[tokio::test]
async fn a_plan_the_document_outgrew_is_dropped() {
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());
    let topic = "dev/outgrown";

    let before = pair.plan.shape_metrics();
    for _ in 0..2 {
        pair.agree(topic, payload("real_heating"), "outgrown").await;
    }
    let planned = pair.plan.shape_metrics();
    assert_eq!(planned.plans, before.plans + 1, "the payload should have planned");
    assert_eq!(planned.hits, before.hits + 1, "the second message replayed it");

    // Same topic, a document no plan can be built for at all.
    pair.agree(topic, payload("odd_escaped_quote"), "outgrown relearn")
        .await;
    let after = pair.plan.shape_metrics();

    assert_eq!(after.plans, before.plans, "the plan that no longer fits must go");
    assert_eq!(after.learn_failures, before.learn_failures + 1);
    // Fetching the plan counted a hit; it was taken back when the plan turned
    // out not to carry the document.
    assert_eq!(after.hits, planned.hits, "a plan that did not fit is not a hit");
}

/// Nothing is gained by scanning a document that will be refused again.
#[tokio::test]
async fn a_topic_that_refuses_everything_stops_being_offered_a_plan() {
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());
    let topic = "dev/never_plannable";

    let before = pair.plan.shape_metrics();
    for _ in 0..shape::NEGATIVE_STRIKES {
        pair.agree(topic, payload("odd_escaped_quote"), "arming").await;
    }
    let armed = pair.plan.shape_metrics();

    assert_eq!(
        armed.learn_failures,
        before.learn_failures + u64::from(shape::NEGATIVE_STRIKES)
    );
    assert_eq!(armed.negative_skips, before.negative_skips, "not held back yet");
    assert_eq!(armed.unplannable, before.unplannable + 1);

    for _ in 0..20 {
        pair.agree(topic, payload("odd_escaped_quote"), "held back")
            .await;
    }
    let after = pair.plan.shape_metrics();

    assert_eq!(
        after.learn_failures, armed.learn_failures,
        "a held-back topic must stop paying for a scan that cannot succeed"
    );
    assert_eq!(after.negative_skips, armed.negative_skips + 20);
    assert_eq!(after.plans, before.plans, "nothing was ever planned here");
}

/// The expensive mistake is skipping a plan that would have worked.
///
/// A topic that sends one unplannable document among plannable ones must keep
/// its fast path; at a threshold of one refusal it lost it for 64 messages.
#[tokio::test]
async fn an_occasional_refusal_does_not_cost_the_fast_path() {
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());
    let topic = "dev/mostly_plannable";

    let before = pair.plan.shape_metrics();
    for i in 0..30 {
        let name = if i % 5 == 4 {
            "odd_escaped_quote"
        } else {
            "real_solar"
        };
        pair.agree(topic, payload(name), name).await;
    }
    let after = pair.plan.shape_metrics();

    assert_eq!(
        after.negative_skips, before.negative_skips,
        "an occasional refusal must not arm the skip"
    );
    // Six refusals, each dropping the plan, so the next plannable message
    // rebuilds it - and the 24 plannable ones all got the plan route.
    assert_eq!(after.learn_failures, before.learn_failures + 6);
    assert!(after.learns >= before.learns + 6);
}

/// Held back is not given up on: the plan route is offered again.
#[tokio::test]
async fn a_topic_that_becomes_plannable_again_recovers() {
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());
    let topic = "dev/recovers";

    for _ in 0..shape::NEGATIVE_STRIKES {
        pair.run(topic, payload("odd_escaped_quote")).await;
    }
    let armed = pair.plan.shape_metrics();

    // Plannable from here on, but the topic is held back for the moment.
    for _ in 0..shape::NEGATIVE_RETRY_EVERY - 1 {
        pair.agree(topic, payload("real_sensor"), "waiting").await;
    }
    let waiting = pair.plan.shape_metrics();
    assert_eq!(waiting.plans, armed.plans, "still on the DOM route");
    assert_eq!(
        waiting.negative_skips,
        armed.negative_skips + u64::from(shape::NEGATIVE_RETRY_EVERY) - 1
    );

    pair.agree(topic, payload("real_sensor"), "recovering").await;
    let recovered = pair.plan.shape_metrics();

    assert_eq!(
        recovered.plans,
        armed.plans + 1,
        "a plannable topic must recover within {} messages",
        shape::NEGATIVE_RETRY_EVERY
    );
    assert_eq!(recovered.unplannable, armed.unplannable - 1);

    // And from now on it is a plain cache hit again.
    pair.agree(topic, payload("real_sensor"), "recovered").await;
    assert_eq!(pair.plan.shape_metrics().hits, recovered.hits + 1);
}

/// A plan is void after a filter change, and so is a refusal record.
///
/// The record has nothing to do with the filters, but keeping it would make a
/// topic go on paying for a refusal from before the change.
#[tokio::test]
async fn a_filter_change_clears_the_hold_backs() {
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());
    let topic = "dev/cleared";

    for _ in 0..shape::NEGATIVE_STRIKES {
        pair.run(topic, payload("odd_escaped_quote")).await;
    }
    assert!(pair.plan.shape_metrics().unplannable > 0);

    let do_not_forward = SCENARIOS
        .iter()
        .find(|s| s.name == "do_not_forward")
        .expect("scenario exists");
    pair.apply(do_not_forward, Vec::new());
    assert_eq!(pair.plan.shape_metrics().unplannable, 0);

    // Offered the plan route again straight away, not in 64 messages.
    let before = pair.plan.shape_metrics();
    pair.agree(topic, payload("real_solar"), "cleared").await;
    assert_eq!(pair.plan.shape_metrics().plans, before.plans + 1);
}

/// The whole keyword table, diffed against the cached implementation.
///
/// Both flattening routes share one uncached converter, so comparing them to
/// each other no longer says anything about the mapping - this is what pins it
/// instead. [`Core::convert_boolean`] stays in the binary as a second,
/// independent implementation of the same table and is the expectation here.
///
/// Every keyword in every spelling, plus what the payload corpus has no reason
/// to produce: surrounding whitespace, near-misses, non-ASCII, and values on
/// both sides of the short-ASCII buffer the shared converter lowercases into.
#[tokio::test]
async fn the_boolean_mapping_is_pinned() {
    let keywords = [
        "true", "yes", "on", "enabled", "enable", "1", "check", "checked", "select", "selected",
        "false", "no", "off", "disabled", "disable", "0",
    ];
    let mut raw: Vec<String> = Vec::new();
    for word in keywords {
        raw.push(word.to_string());
        raw.push(word.to_uppercase());
        let mut chars = word.chars();
        let capitalized = match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        };
        raw.push(capitalized);
    }
    raw.extend(
        [
            // Whitespace is trimmed before the lookup but must not reach the output.
            " true ",
            "\ton\t",
            "        true",
            "on\n",
            "\r\nOFF",
            // Near-misses: a keyword must be the whole value, not a part of it.
            "tru",
            "truthy",
            "offline",
            "yesplease",
            "disabledx",
            "on off",
            "1.0",
            // Nothing to map.
            "",
            "   ",
            "\u{b0}C",
            "W\u{e4}rme",
            "-",
            "null",
            // Non-ASCII takes the Unicode-lowercasing route, and U+212A
            // lowercases into a keyword there - so that route has to stay
            // reachable.
            "CHEC\u{212a}",
            "\u{c4}NABLED",
            "TrUe",
            // Either side of the 16-byte buffer the fast route uses.
            "aaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaaa",
            "enabled_but_not_quite",
        ]
        .map(str::to_string),
    );

    // Keys sort in the order they were built, so the DOM route emits the values
    // in exactly the order `raw` lists them.
    let document: serde_json::Map<String, serde_json::Value> = raw
        .iter()
        .enumerate()
        .map(|(i, value)| (format!("k{i:03}"), serde_json::Value::String(value.clone())))
        .collect();
    let message = serde_json::to_string(&document).expect("serializable");

    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());
    let out = pair.agree("dev/booleans", &message, "booleans").await;

    let expected: Vec<String> = raw.iter().map(|v| pair.plan.convert_boolean(v)).collect();
    assert_eq!(
        out.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>(),
        expected
    );
    // The table is only pinned if the inputs actually reach both verdicts.
    let mapped: BTreeSet<String> = expected.into_iter().collect();
    assert!(mapped.contains("1") && mapped.contains("0"));
}

/// Every target the flattener builds, diffed against
/// [`Core::normalize_topic`].
///
/// Same reason as the boolean mapping: both routes normalize through one
/// uncached helper now, so the cached implementation is what the result is held
/// against. It covers the separators the relay replaces, doubled and adjacent
/// ones, and non-ASCII around them.
#[tokio::test]
async fn normalization_is_pinned() {
    let keys = [
        "plain",
        "with/slash",
        "with%percent",
        "with/both%kinds",
        "double//slash",
        "double%%percent",
        "mixed/%adjacent",
        "/leading",
        "trailing/",
        "%",
        "/",
        "//",
        "s p a c e s",
        "W\u{e4}rme%Z\u{e4}hler",
        "\u{b5}/unit",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ];
    let document: serde_json::Map<String, serde_json::Value> = keys
        .iter()
        .map(|k| (k.to_string(), serde_json::Value::String("v".into())))
        .collect();
    let message = serde_json::to_string(&document).expect("serializable");

    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());

    for topic in ["dev/norm", "dev/a%b/norm", "dev%norm"] {
        let out = pair.agree(topic, &message, topic).await;
        assert!(!out.is_empty(), "the payload must produce targets to compare");

        // Every input name the flattener produced has to be the one the cached
        // normalizer gives for the target that produced it.
        let expected: BTreeSet<String> = keys
            .iter()
            .map(|key| pair.plan.normalize_topic(&format!("{topic}/{key}")))
            .collect();
        let produced: BTreeSet<String> = out.iter().map(|(input, _)| input.clone()).collect();
        assert_eq!(produced, expected);
        // A normalized target may not carry a separator the relay replaces.
        assert!(!produced.iter().any(|n| n.contains('/') || n.contains('%')));
    }
}

/// Zigbee2MQTT-style `on`/`off` and JSON literals stay as sent.
#[tokio::test]
async fn boolean_conversion_can_be_turned_off() {
    let pair = Pair::new(true, false);
    pair.apply(&PLAIN, Vec::new());
    let message = r#"{"a":"on","b":"OFF","c":true,"d":false,"e":"true","f":1,"g":"x"}"#;

    let out = pair.agree("dev/raw_booleans", message, "raw booleans").await;
    assert_eq!(
        out.iter().map(|(_, v)| v.as_str()).collect::<Vec<_>>(),
        ["on", "OFF", "true", "false", "true", "1", "x"]
    );
}

/// The whole corpus again with the converter off: the setting is read once per
/// route at construction and gates the converter in two different places, so
/// whether it is honoured needs its own parity check.
#[tokio::test]
async fn the_two_routes_agree_without_boolean_conversion() {
    let pair = Pair::new(true, false);
    pair.apply(&PLAIN, Vec::new());

    for name in PAYLOADS.keys() {
        let topic = topic_for(name);
        for pass in ["cold", "warm"] {
            pair.agree(&topic, payload(name), &format!("{name}/{pass}"))
                .await;
        }
    }
    assert!(pair.plan.shape_metrics().hits > 0);
}

/// One message, one value per JSON leaf - and the count is what the corpus
/// says, so a flattener that started dropping or duplicating leaves shows up
/// here rather than only in a diff against itself.
#[tokio::test]
async fn a_real_message_yields_one_value_per_leaf() {
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());
    let out = pair.agree("dev/batching", payload("real_heating"), "heating").await;
    assert_eq!(out.len(), 56);
}

#[tokio::test]
async fn nothing_is_handed_over_when_everything_is_filtered() {
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, vec!["no_such_target".to_string()]);
    assert!(
        pair.agree("dev/allfiltered", r#"{"a":1,"b":2}"#, "all filtered")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn value_changes_do_not_need_a_relearn() {
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());
    let topic = "dev/values";

    pair.run(topic, r#"{"t":20.5,"s":"on"}"#).await;
    let before = pair.plan.shape_stats().1;
    let out = pair
        .agree(topic, r#"{"t":-3.25,"s":"OFF"}"#, "value change")
        .await;

    assert_eq!(
        out,
        vec![
            ("dev_values_s".to_string(), "0".to_string()),
            ("dev_values_t".to_string(), "-3.25".to_string()),
        ]
    );
    assert_eq!(
        pair.plan.shape_stats().1,
        before,
        "different values in the same layout must replay the cached plan"
    );
}

/// Plans carry baked-in filter verdicts, so a config change must void them.
#[tokio::test]
async fn filter_updates_invalidate_learned_plans() {
    for mutation in ["whitelist", "subscription_filters", "do_not_forward"] {
        let pair = Pair::new(true, true);
        pair.apply(&PLAIN, Vec::new());
        let topic = "dev/invalidate";
        let message = r#"{"keep":1,"drop":2}"#;

        assert_eq!(pair.agree(topic, message, mutation).await.len(), 2);

        for core in [&pair.plan, &pair.dom] {
            match mutation {
                "whitelist" => {
                    core.update_topic_whitelist(vec!["dev_invalidate_keep".to_string()])
                }
                "subscription_filters" => core
                    .update_subscription_filters(vec!["/drop$".to_string()])
                    .expect("valid"),
                _ => core
                    .update_do_not_forward(vec!["/drop$".to_string()])
                    .expect("valid"),
            }
        }

        assert_eq!(
            pair.agree(topic, message, mutation).await,
            vec![("dev_invalidate_keep".to_string(), "1".to_string())],
            "{mutation}"
        );
    }
}

/// `a_b` and `a/b` share one input - the DOM route writes `a/b` last.
///
/// Document order would reverse that and leave a different value on the
/// Miniserver input, so the plan renumbers its leaves to match.
#[tokio::test]
async fn colliding_targets_keep_the_dom_ordering() {
    let pair = Pair::new(true, true);
    pair.apply(&PLAIN, Vec::new());

    for _ in 0..3 {
        let out = pair
            .agree("dev/collide", payload("odd_colliding_targets"), "collision")
            .await;
        assert_eq!(
            out,
            vec![
                ("dev_collide_a_b".to_string(), "2".to_string()),
                ("dev_collide_a_b".to_string(), "1".to_string()),
            ]
        );
    }
}

#[tokio::test]
async fn expand_json_disabled_forwards_the_raw_message() {
    let pair = Pair::new(false, true);
    let no_expand = SCENARIOS
        .iter()
        .find(|s| s.name == "no_expand")
        .expect("scenario exists");
    pair.apply(no_expand, Vec::new());

    let message = r#"{"a":1,"b":2}"#;
    assert_eq!(
        pair.agree("dev/raw", message, "no expand").await,
        vec![("dev_raw".to_string(), message.to_string())]
    );
}

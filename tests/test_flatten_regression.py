"""
Regression suite for the shape-plan flattener.

The flattener has two routes. The fast one learns a topic's JSON layout once
and afterwards only reads values with a byte scanner. The slow one builds a
``serde_json`` DOM and stays in the binary as the fallback for every document
the scanner refuses.

Every test here drives both routes with the same message and demands identical
output - same values, same order, including which value wins when two JSON
paths normalize onto the same miniserver target. ``get_shape_stats()`` backs
that up: without it a fast path that silently stopped engaging would compare
the slow route against itself and pass. A plan is only cached once it has
flattened a whole document, so a message that builds no plan and leaves none
behind is a message that went down the DOM route.

What the route comparison covers is therefore the part where the two genuinely
differ: which leaves a document yields, in which order, and which of them
survive the filters. The two per-leaf transformations are shared code, so they
are pinned separately against the cached ``_convert_boolean`` and
``normalize_topic``, which remain independent implementations of the same two
answers.

Corpus: anonymized real device payloads, deterministic synthetics and a pile of
handcrafted oddities, each under seven filter/whitelist scenarios, plus a seeded
fuzz run over changing schemas.
"""

from __future__ import annotations

import json

import pytest

from loxmqttrelay.config import global_config
from tests import flatten_corpus as fc
from tests.flatten_payloads import PAYLOADS, Lcg, random_payload

# Scenarios beyond "plain" run on a representative slice: real and handcrafted
# payloads always, synthetics every fourth.
SLICE_SCENARIOS = tuple(k for k in fc.SCENARIOS if k != "plain")


def _in_slice(name: str) -> bool:
    if not name.startswith("synth_"):
        return True
    return sum(ord(c) for c in name) % 4 == 0


@pytest.fixture(scope="module")
def pairs():
    """One plan/DOM pair per expand_json setting, reused across all cases.

    Reusing them is deliberate: plans stay warm between cases, so eviction and
    invalidation get exercised the way a running relay would.
    """
    return {flag: fc.Pair(global_config, flag) for flag in (True, False)}


@pytest.fixture(scope="module")
def raw_pair():
    """A plan/DOM pair built with ``processing.convert_booleans`` turned off.

    The setting is read once per route at construction and gates the converter
    in two different places, so whether it is honoured needs its own parity
    check rather than riding along on the pairs above.
    """
    return fc.Pair(global_config, True, convert_booleans=False)


@pytest.mark.parametrize("payload_name", sorted(PAYLOADS))
@pytest.mark.asyncio
async def test_plan_and_dom_agree(pairs, payload_name):
    payload = PAYLOADS[payload_name]
    topic = fc.topic_for(payload_name)
    targets = fc.plan_targets(pairs[True], payload_name)

    scenarios = ["plain"]
    if _in_slice(payload_name):
        scenarios += list(SLICE_SCENARIOS)

    for scenario_name in scenarios:
        scenario = fc.SCENARIOS[scenario_name]
        pair = pairs[scenario["expand_json"]]
        pair.apply(scenario, fc.derive_whitelist(scenario["whitelist"], targets))

        # First message learns the layout, second replays it from the cache.
        for pass_name in ("cold", "warm"):
            plan, dom = pair.run(topic, payload)
            assert plan == dom, f"{payload_name}/{scenario_name}/{pass_name} diverged"


@pytest.mark.asyncio
async def test_fuzz_agreement_across_changing_schemas(pairs):
    """Random payloads on recycled topics, each sent twice.

    The repeat is what makes a plan get replayed from the cache; the next
    iteration's different layout on the same topic then forces a relearn. Both
    are where the plan route is most likely to break, together with payloads
    carrying escapes or numbers like ``1e3`` that make it bail out to the DOM
    route part-way through a document.

    Which route each payload took is asserted alongside, because a comparison
    that never reaches the fast path would agree with itself and prove nothing.
    """
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)
    rng = Lcg(0xC0FFEE)

    compared = 0
    planned = 0
    for i in range(400):
        payload = random_payload(rng)
        topic = f"dev/fuzz{i % 20}"
        built_before = pair.plan.get_shape_stats()[1]
        for repeat in range(2):
            plan, dom = pair.run(topic, payload)
            assert plan == dom, (
                f"iteration {i} pass {repeat} on {topic} diverged for {payload}"
            )
            compared += len(plan)
        built = pair.plan.get_shape_stats()[1] - built_before
        assert built <= 1, (
            f"iteration {i} rebuilt its plan for the identical repeat on {topic}"
        )
        planned += built

    assert compared > 5000, "the fuzz corpus produced almost no values"
    # Every built plan was replayed once by the repeat that followed it.
    assert planned > 100, "plans were hardly ever built and replayed"
    assert planned < 350, "the DOM fallback was hardly ever taken"


@pytest.mark.asyncio
async def test_real_payloads_use_the_plan_path(pairs):
    """A permanently dead fast path would keep every assertion above green."""
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)

    cached_before, built_before = pair.plan.get_shape_stats()
    for _ in range(5):
        for name in ("real_heating", "real_solar", "real_sensor"):
            pair.run(f"dev/stats_{name}", PAYLOADS[name])
    cached, built = pair.plan.get_shape_stats()

    # Three plans for fifteen messages: the other twelve replayed a cached one,
    # and none of them fell back, or the payload would have left no plan behind.
    assert built - built_before == 3, "each topic should be planned exactly once"
    assert cached - cached_before == 3, "every topic should hold a plan"


@pytest.mark.parametrize(
    "payload_name,reason",
    [
        ("odd_escaped_quote", "escapes need unescaping the scanner cannot do"),
        ("odd_escaped_unicode", "\\u sequences need decoding"),
        ("odd_duplicate_key", "serde_json keeps the last duplicate"),
        ("odd_scientific_numbers", "1e3 does not render as written"),
        ("odd_truncated", "invalid JSON is forwarded verbatim"),
        ("odd_trailing_garbage", "trailing bytes make it invalid JSON"),
        ("odd_root_array", "non-object roots are forwarded verbatim"),
        ("odd_plain_text", "not JSON at all"),
    ],
)
@pytest.mark.asyncio
async def test_unplannable_payloads_fall_back(pairs, payload_name, reason):
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)

    before = pair.plan.get_shape_stats()
    pair.run(f"dev/fb_{payload_name}", PAYLOADS[payload_name])
    cached, built = (after - prev for after, prev in zip(pair.plan.get_shape_stats(), before))

    assert built == 0, reason
    assert cached == 0, "an unplannable payload must not leave a plan behind"


@pytest.mark.asyncio
async def test_a_plan_the_document_outgrew_is_dropped(pairs):
    """A plan that stops fitting must not sit in the cache being retried.

    Left behind, it would be fetched and run against every following message,
    fail, and only leave once the LRU evicted it.
    """
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)
    topic = "dev/outgrown"

    before = pair.plan.get_shape_metrics()
    for _ in range(2):
        plan, dom = pair.run(topic, PAYLOADS["real_heating"])
        assert plan == dom
    planned = pair.plan.get_shape_metrics()
    assert planned["plans"] == before["plans"] + 1, "the payload should have planned"
    assert planned["hits"] == before["hits"] + 1, "the second message replayed it"

    # Same topic, a document no plan can be built for at all.
    plan, dom = pair.run(topic, PAYLOADS["odd_escaped_quote"])
    assert plan == dom
    after = pair.plan.get_shape_metrics()

    assert after["plans"] == before["plans"], "the plan that no longer fits must go"
    assert after["learn_failures"] == before["learn_failures"] + 1
    # Fetching the plan counted a hit; it was taken back when the plan turned
    # out not to carry the document.
    assert after["hits"] == planned["hits"], "a plan that did not fit is not a hit"


@pytest.mark.asyncio
async def test_a_topic_that_refuses_everything_stops_being_offered_a_plan(pairs):
    """Nothing is gained by scanning a document that will be refused again."""
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)
    topic = "dev/never_plannable"
    payload = PAYLOADS["odd_escaped_quote"]

    before = pair.plan.get_shape_metrics()
    for _ in range(fc.NEGATIVE_STRIKES):
        plan, dom = pair.run(topic, payload)
        assert plan == dom
    armed = pair.plan.get_shape_metrics()

    assert armed["learn_failures"] == before["learn_failures"] + fc.NEGATIVE_STRIKES
    assert armed["negative_skips"] == before["negative_skips"], "not held back yet"
    assert armed["unplannable"] == before["unplannable"] + 1

    for _ in range(20):
        plan, dom = pair.run(topic, payload)
        assert plan == dom, "being held back must not change what is forwarded"
    after = pair.plan.get_shape_metrics()

    assert after["learn_failures"] == armed["learn_failures"], (
        "a held-back topic must stop paying for a scan that cannot succeed"
    )
    assert after["negative_skips"] == armed["negative_skips"] + 20
    assert after["plans"] == before["plans"], "nothing was ever planned here"


@pytest.mark.asyncio
async def test_an_occasional_refusal_does_not_cost_the_fast_path(pairs):
    """The expensive mistake is skipping a plan that would have worked.

    A topic that sends one unplannable document among plannable ones must keep
    its fast path; at a threshold of one refusal it lost it for 64 messages.
    """
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)
    topic = "dev/mostly_plannable"

    before = pair.plan.get_shape_metrics()
    for i in range(30):
        payload = PAYLOADS["odd_escaped_quote" if i % 5 == 4 else "real_solar"]
        plan, dom = pair.run(topic, payload)
        assert plan == dom
    after = pair.plan.get_shape_metrics()

    assert after["negative_skips"] == before["negative_skips"], (
        "an occasional refusal must not arm the skip"
    )
    # Six refusals, each dropping the plan, so the next plannable message
    # rebuilds it - and the 24 plannable ones all got the plan route.
    assert after["learn_failures"] == before["learn_failures"] + 6
    assert after["learns"] >= before["learns"] + 6


@pytest.mark.asyncio
async def test_a_topic_that_becomes_plannable_again_recovers(pairs):
    """Held back is not given up on: the plan route is offered again."""
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)
    topic = "dev/recovers"

    for _ in range(fc.NEGATIVE_STRIKES):
        pair.run(topic, PAYLOADS["odd_escaped_quote"])
    armed = pair.plan.get_shape_metrics()

    # Plannable from here on, but the topic is held back for the moment.
    for _ in range(fc.NEGATIVE_RETRY_EVERY - 1):
        plan, dom = pair.run(topic, PAYLOADS["real_sensor"])
        assert plan == dom
    waiting = pair.plan.get_shape_metrics()
    assert waiting["plans"] == armed["plans"], "still on the DOM route"
    assert waiting["negative_skips"] == armed["negative_skips"] + fc.NEGATIVE_RETRY_EVERY - 1

    plan, dom = pair.run(topic, PAYLOADS["real_sensor"])
    assert plan == dom
    recovered = pair.plan.get_shape_metrics()

    assert recovered["plans"] == armed["plans"] + 1, (
        f"a plannable topic must recover within {fc.NEGATIVE_RETRY_EVERY} messages"
    )
    assert recovered["unplannable"] == armed["unplannable"] - 1

    # And from now on it is a plain cache hit again.
    plan, dom = pair.run(topic, PAYLOADS["real_sensor"])
    assert plan == dom
    assert pair.plan.get_shape_metrics()["hits"] == recovered["hits"] + 1


@pytest.mark.asyncio
async def test_shape_metrics_and_shape_stats_agree(pairs):
    """The 2-tuple stays what it was; the dict is the same numbers plus more."""
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)
    pair.run("dev/metrics", PAYLOADS["real_heating"])

    cached, built = pair.plan.get_shape_stats()
    metrics = pair.plan.get_shape_metrics()

    assert metrics["plans"] == cached
    assert metrics["learns"] == built
    assert set(metrics) == {
        "plans", "learns", "hits", "learn_failures",
        "dom_fallbacks", "negative_skips", "unplannable",
    }


@pytest.mark.asyncio
async def test_a_filter_change_clears_the_hold_backs(pairs):
    """A plan is void after a filter change, and so is a refusal record.

    The record has nothing to do with the filters, but keeping it would make a
    topic go on paying for a refusal from before the change.
    """
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)
    topic = "dev/cleared"

    for _ in range(fc.NEGATIVE_STRIKES):
        pair.run(topic, PAYLOADS["odd_escaped_quote"])
    assert pair.plan.get_shape_metrics()["unplannable"] > 0

    pair.apply(fc.SCENARIOS["do_not_forward"], None)
    assert pair.plan.get_shape_metrics()["unplannable"] == 0

    # Offered the plan route again straight away, not in 64 messages.
    before = pair.plan.get_shape_metrics()
    plan, dom = pair.run(topic, PAYLOADS["real_solar"])
    assert plan == dom
    assert pair.plan.get_shape_metrics()["plans"] == before["plans"] + 1


@pytest.mark.asyncio
async def test_boolean_mapping_is_pinned(pairs):
    """The whole keyword table, diffed against the cached implementation.

    Both flattening routes share one uncached converter, so comparing them to
    each other no longer says anything about the mapping - this is what pins it
    instead. ``_convert_boolean`` stays in the binary as a second, independent
    implementation of the same table and is the expectation here.

    Every keyword in every spelling, plus what the payload corpus has no reason
    to produce: surrounding whitespace, near-misses, non-ASCII, and values on
    both sides of the short-ASCII buffer the shared converter lowercases into.
    """
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)

    keywords = [
        "true", "yes", "on", "enabled", "enable", "1",
        "check", "checked", "select", "selected",
        "false", "no", "off", "disabled", "disable", "0",
    ]
    raw = [
        spelling
        for word in keywords
        for spelling in (word, word.upper(), word.capitalize())
    ]
    raw += [
        # Whitespace is trimmed before the lookup but must not reach the output.
        " true ", "\ton\t", "        true", "on\n", "\r\nOFF",
        # Near-misses: a keyword must be the whole value, not a part of it.
        "tru", "truthy", "offline", "yesplease", "disabledx", "on off", "1.0",
        # Nothing to map.
        "", "   ", "\u00b0C", "W\u00e4rme", "-", "null",
        # Non-ASCII takes the Unicode-lowercasing route, and U+212A lowercases
        # into a keyword there - so that route has to stay reachable.
        "CHEC\u212a", "\u00c4NABLED", "TrUe",
        # Either side of the 16-byte buffer the fast route uses.
        "a" * 16, "a" * 17, "enabled_but_not_quite",
    ]
    payload = json.dumps({f"k{i:03d}": v for i, v in enumerate(raw)})

    plan, dom = pair.run("dev/booleans", payload)

    assert plan == dom
    assert [value for _, _, value in plan] == [
        pair.plan._convert_boolean(v) for v in raw
    ]
    # The table is only pinned if the inputs actually reach both verdicts.
    mapped = {pair.plan._convert_boolean(v) for v in raw}
    assert {"1", "0"} <= mapped


@pytest.mark.asyncio
async def test_normalization_is_pinned(pairs):
    """Every target the flattener builds, diffed against ``normalize_topic``.

    Same reason as the boolean mapping: both routes normalize through one
    uncached helper now, so the cached ``normalize_topic`` is what the result is
    held against. It covers the separators the relay replaces, doubled and
    adjacent ones, and non-ASCII around them.
    """
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)

    keys = [
        "plain", "with/slash", "with%percent", "with/both%kinds",
        "double//slash", "double%%percent", "mixed/%adjacent",
        "/leading", "trailing/", "%", "/", "//", "s p a c e s",
        "W\u00e4rme%Z\u00e4hler", "\u00b5/unit", "a" * 40,
    ]
    payload = json.dumps({k: "v" for k in keys})

    for topic in ("dev/norm", "dev/a%b/norm", "dev%norm"):
        plan, dom = pair.run(topic, payload)

        assert plan == dom
        assert plan, "the payload must produce targets to compare"
        for full_topic, normalized, _ in plan:
            assert normalized == pair.plan.normalize_topic(full_topic)
        # A normalized target may not carry a separator the relay replaces.
        assert not any("/" in n or "%" in n for _, n, _ in plan)


@pytest.mark.parametrize("payload_name", sorted(PAYLOADS))
@pytest.mark.asyncio
async def test_plan_and_dom_agree_without_boolean_conversion(raw_pair, payload_name):
    raw_pair.apply(fc.SCENARIOS["plain"], None)
    topic = fc.topic_for(payload_name)

    for _ in ("cold", "warm"):
        plan, dom = raw_pair.run(topic, PAYLOADS[payload_name])
        assert plan == dom, f"{payload_name} diverged with conversion disabled"


@pytest.mark.asyncio
async def test_boolean_conversion_can_be_turned_off(raw_pair):
    """Zigbee2MQTT-style ``on``/``off`` and JSON literals stay as sent."""
    raw_pair.apply(fc.SCENARIOS["plain"], None)
    payload = json.dumps(
        {"a": "on", "b": "OFF", "c": True, "d": False, "e": "true", "f": 1, "g": "x"}
    )

    plan, dom = raw_pair.run("dev/raw_booleans", payload)

    assert plan == dom
    assert [value for _, _, value in plan] == [
        "on", "OFF", "true", "false", "true", "1", "x",
    ]


@pytest.mark.asyncio
async def test_handover_is_batched(pairs):
    """One call per MQTT message, not one per JSON leaf."""
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)
    pair.plan_rec.batch_calls = 0
    pair.plan_rec.single_calls = 0

    plan, dom = pair.run("dev/batching", PAYLOADS["real_heating"])

    assert len(plan) == 56
    assert plan == dom
    assert pair.plan_rec.batch_calls == 1
    assert pair.plan_rec.single_calls == 0


@pytest.mark.asyncio
async def test_nothing_is_handed_over_when_everything_is_filtered(pairs):
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], ["no_such_target"])
    pair.plan_rec.batch_calls = 0

    plan, dom = pair.run("dev/allfiltered", '{"a":1,"b":2}')

    assert plan == dom == []
    assert pair.plan_rec.batch_calls == 0


@pytest.mark.asyncio
async def test_value_changes_do_not_need_a_relearn(pairs):
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)
    topic = "dev/values"

    pair.run(topic, '{"t":20.5,"s":"on"}')
    built_before = pair.plan.get_shape_stats()[1]
    plan, dom = pair.run(topic, '{"t":-3.25,"s":"OFF"}')

    assert plan == dom
    assert plan == [
        ("dev/values/s", "dev_values_s", "0"),
        ("dev/values/t", "dev_values_t", "-3.25"),
    ]
    assert pair.plan.get_shape_stats()[1] == built_before, (
        "different values in the same layout must replay the cached plan"
    )


@pytest.mark.parametrize("mutate", ["whitelist", "subscription_filters", "do_not_forward"])
@pytest.mark.asyncio
async def test_filter_updates_invalidate_learned_plans(pairs, mutate):
    """Plans carry baked-in filter verdicts, so a config change must void them."""
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)
    topic = "dev/invalidate"
    payload = '{"keep":1,"drop":2}'

    assert len(pair.run(topic, payload)[0]) == 2

    for processor in (pair.plan, pair.dom):
        if mutate == "whitelist":
            processor.update_topic_whitelist(["dev_invalidate_keep"])
        elif mutate == "subscription_filters":
            processor.update_subscription_filters([r"/drop$"])
        else:
            processor.update_do_not_forward([r"/drop$"])

    plan, dom = pair.run(topic, payload)
    assert plan == dom
    assert plan == [("dev/invalidate/keep", "dev_invalidate_keep", "1")]


@pytest.mark.asyncio
async def test_colliding_targets_keep_the_dom_ordering(pairs):
    """``a_b`` and ``a/b`` share one target - the DOM path writes ``a/b`` last.

    Document order would reverse that and leave a different value on the
    miniserver input, so the plan renumbers its leaves to match.
    """
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)

    for _ in range(3):
        plan, dom = pair.run("dev/collide", PAYLOADS["odd_colliding_targets"])
        assert plan == dom
        assert plan == [
            ("dev/collide/a/b", "dev_collide_a_b", "2"),
            ("dev/collide/a_b", "dev_collide_a_b", "1"),
        ]


@pytest.mark.asyncio
async def test_expand_json_disabled_forwards_the_raw_message(pairs):
    pair = pairs[False]
    pair.apply(fc.SCENARIOS["no_expand"], None)

    plan, dom = pair.run("dev/raw", '{"a":1,"b":2}')

    assert plan == dom
    assert plan == [("dev/raw", "dev_raw", '{"a":1,"b":2}')]

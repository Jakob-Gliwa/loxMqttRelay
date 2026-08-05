"""
Regression suite for the shape-plan flattener.

The flattener has two routes. The fast one learns a topic's JSON layout once
and afterwards only reads values with a byte scanner. The slow one builds a
``serde_json`` DOM and is, unchanged, the implementation from before the
optimization; it stays in the binary as the fallback for every document the
scanner refuses.

Every test here drives both routes with the same message and demands identical
output - same values, same order, including which value wins when two JSON
paths normalize onto the same miniserver target. ``get_shape_stats()`` backs
that up: without it a fast path that silently stopped engaging would compare
the slow route against itself and pass. A plan is only cached once it has
flattened a whole document, so a message that builds no plan and leaves none
behind is a message that went down the DOM route.

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
async def test_boolean_mapping_is_pinned(pairs):
    """Edge cases around the mapping that the payload corpus does not contain.

    The routes no longer share their boolean conversion - the DOM path uses the
    original ``_convert_boolean`` - so every value in the corpus already gets
    compared. This adds the inputs the corpus has no reason to produce:
    surrounding whitespace, mixed case, near-misses and non-ASCII.
    """
    pair = pairs[True]
    pair.apply(fc.SCENARIOS["plain"], None)

    raw = [
        "true", "TRUE", "True", "yes", "on", "ON", "enabled", "enable", "1",
        "check", "checked", "select", "selected",
        "false", "FALSE", "no", "off", "OFF", "disabled", "disable", "0",
        " true ", "\ton\t", "        true", "tru", "truthy", "offline",
        "yesplease", "", "   ", "TrUe", "\u00b0C", "W\u00e4rme", "disabledx",
    ]
    payload = json.dumps({f"k{i:03d}": v for i, v in enumerate(raw)})

    plan, dom = pair.run("dev/booleans", payload)

    assert plan == dom
    assert [value for _, _, value in plan] == [
        pair.plan._convert_boolean(v) for v in raw
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

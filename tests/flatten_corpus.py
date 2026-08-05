"""
Shared driver for the flatten regression suite.

The suite is differential: every message goes through two processors that are
configured identically except that one uses the learned shape plans and the
other is pinned to the DOM path (``set_shape_cache_enabled(False)``). The DOM
path is the pre-optimization implementation and stays in the binary as the
correctness fallback, so no expectations have to be frozen into a file.

The recorder accepts the per-value handover (``send_to_miniserver``) and the
batched one (``send_batch_to_miniserver``) and flattens both into the same
ordered list of ``(topic, normalized_topic, value)`` triples.
"""

from __future__ import annotations

import asyncio
from typing import Any
from unittest.mock import AsyncMock, MagicMock

from tests.flatten_payloads import PAYLOADS

BASE_TOPIC = "myrelay/"


class TopicNS:
    """Control topics kept under BASE_TOPIC so data topics never collide."""

    MINISERVER_STARTUP_EVENT = "myrelay/miniserverevent/startup"
    CONFIG_GET = "myrelay/config/get"
    CONFIG_RESPONSE = "myrelay/config/response"
    CONFIG_SET = "myrelay/config/set"
    CONFIG_ADD = "myrelay/config/add"
    CONFIG_REMOVE = "myrelay/config/remove"
    CONFIG_UPDATE = "myrelay/config/update"
    CONFIG_RESTART = "myrelay/config/restart"


class Recorder:
    """Stub miniserver handler that records what the processor hands over.

    Returns an already-resolved future because the Rust side pushes whatever it
    gets into ``asyncio.ensure_future``.
    """

    def __init__(self) -> None:
        self.calls: list[tuple[str, str, str]] = []
        self.batch_calls = 0
        self.single_calls = 0

    @staticmethod
    def _resolved():
        fut = asyncio.get_running_loop().create_future()
        fut.set_result(None)
        return fut

    def send_to_miniserver(self, topic, normalized_topic, value):
        self.single_calls += 1
        self.calls.append((topic, normalized_topic, value))
        return self._resolved()

    def send_batch_to_miniserver(self, items):
        self.batch_calls += 1
        for topic, normalized_topic, value in items:
            self.calls.append((topic, normalized_topic, value))
        return self._resolved()


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------

# ``whitelist`` picks entries out of the targets a payload actually produces, so
# the filter bites instead of matching nothing.
SCENARIOS: dict[str, dict[str, Any]] = {
    "plain": {
        "expand_json": True,
        "subscription_filters": [],
        "do_not_forward": [],
        "whitelist": None,
    },
    "no_expand": {
        "expand_json": False,
        "subscription_filters": [],
        "do_not_forward": [],
        "whitelist": None,
    },
    "whitelist_half": {
        "expand_json": True,
        "subscription_filters": [],
        "do_not_forward": [],
        "whitelist": "every_2nd",
    },
    "whitelist_sparse": {
        "expand_json": True,
        "subscription_filters": [],
        "do_not_forward": [],
        "whitelist": "every_5th",
    },
    "do_not_forward": {
        "expand_json": True,
        "subscription_filters": [],
        "do_not_forward": [r"temp", r"/k1", r"/0$", r"mode$", r"status"],
        "whitelist": None,
    },
    "subscription_filter": {
        "expand_json": True,
        "subscription_filters": [r"hc1", r"/k2", r"sensorData/0/co2", r"dhw"],
        "do_not_forward": [],
        "whitelist": None,
    },
    "combined": {
        "expand_json": True,
        "subscription_filters": [r"battery"],
        "do_not_forward": [r"co2", r"unit$"],
        "whitelist": "every_3rd",
    },
}

TOPICS = {
    "default": "dev/{name}",
    "percent": "dev/a%b/{name}",
}


def topic_for(name: str) -> str:
    variant = "percent" if sum(ord(c) for c in name) % 7 == 0 else "default"
    return TOPICS[variant].format(name=name)


def derive_whitelist(mode: str | None, targets: list[str]) -> list[str] | None:
    if mode is None:
        return None
    step = {"every_2nd": 2, "every_3rd": 3, "every_5th": 5}[mode]
    return sorted(set(targets))[::step]


# ---------------------------------------------------------------------------
# Driving the processor
# ---------------------------------------------------------------------------


def make_processor(config, expand_json: bool, shape_cache: bool = True):
    """Build a processor with the config knobs the Rust side reads once."""
    from loxmqttrelay.compatible._loxmqttrelay import MiniserverDataProcessor, MqttClient

    config.processing.expand_json = expand_json
    config.general.base_topic = BASE_TOPIC
    config.general.cache_size = 512
    config.topics.subscription_filters = []
    config.topics.topic_whitelist = set()
    config.topics.do_not_forward = []

    recorder = Recorder()
    processor = MiniserverDataProcessor(
        TopicNS(),
        config,
        AsyncMock(),
        MqttClient(config),
        recorder,
        MagicMock(),
    )
    if not shape_cache:
        processor.set_shape_cache_enabled(False)
    return processor, recorder


class Pair:
    """One processor using shape plans, one pinned to the DOM path.

    Both see the same messages in the same order, so the plan side builds up a
    realistic cache history (learn, hit, relearn) while the DOM side stays the
    unchanged reference.
    """

    def __init__(self, config, expand_json: bool):
        self.plan, self.plan_rec = make_processor(config, expand_json, shape_cache=True)
        self.dom, self.dom_rec = make_processor(config, expand_json, shape_cache=False)

    def apply(self, scenario: dict[str, Any], whitelist: list[str] | None):
        for processor in (self.plan, self.dom):
            processor.update_subscription_filters(list(scenario["subscription_filters"]))
            processor.update_do_not_forward(list(scenario["do_not_forward"]))
            processor.update_topic_whitelist(list(whitelist) if whitelist else [])

    def run(self, topic: str, payload: str):
        """Feed both processors and return ``(plan_output, dom_output)``."""
        return (
            _drive(self.plan, self.plan_rec, topic, payload),
            _drive(self.dom, self.dom_rec, topic, payload),
        )


def _drive(processor, recorder: Recorder, topic: str, payload: str):
    recorder.calls.clear()
    processor.process_data(topic, payload)
    return list(recorder.calls)


def run_case(processor, recorder: Recorder, topic: str, payload: str):
    """Feed one message through a single processor."""
    return _drive(processor, recorder, topic, payload)


def plan_targets(pair: Pair, name: str) -> list[str]:
    """Normalized targets a payload yields unfiltered - the whitelist source."""
    pair.apply(SCENARIOS["plain"], None)
    _, dom = pair.run(topic_for(name), PAYLOADS[name])
    return [normalized for _, normalized, _ in dom]

"""The processor's Python surface.

Flattening, filtering and forwarding live entirely in Rust now and are tested
there, against the processing core rather than through this class - see
``src/process/regression.rs``. What is left here is the boundary itself: what
the constructor reads out of the config, the getters and mutators the whitelist
sync and the config tests use, and the control topics, which are the one part of
the message path that still calls back into Python.
"""

import json
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock

import pytest
import pytest_asyncio

from loxmqttrelay.compatible._loxmqttrelay import (
    MiniserverClient,
    MiniserverDataProcessor,
    MqttClient,
)
from loxmqttrelay.config import ConfigError, global_config


@pytest.fixture(scope="function")
def temp_config_file(tmp_path):
    """Create a temporary config file"""
    config_file = tmp_path / "config.json"
    config_data = {
        "topics": {
            "subscription_filters": [],
            "topic_whitelist": [],
            "do_not_forward": []
        },
        "processing": {
            "expand_json": False,
            "convert_booleans": False
        },
        "general": {
            "base_topic": "myrelay/",
            "cache_size": 100
        }
    }
    config_file.write_text(json.dumps(config_data))
    return str(config_file)

@pytest.fixture(scope="function")
def config_instance(temp_config_file):
    """Create and configure a Config instance"""
    with open(temp_config_file, 'r') as f:
        config_dict = json.load(f)

    # Update global config for the test
    global_config.topics.subscription_filters = config_dict["topics"]["subscription_filters"]
    global_config.topics.topic_whitelist = config_dict["topics"]["topic_whitelist"]
    global_config.topics.do_not_forward = config_dict["topics"]["do_not_forward"]
    global_config.processing.expand_json = config_dict["processing"]["expand_json"]
    global_config.processing.convert_booleans = config_dict["processing"]["convert_booleans"]
    global_config.general.base_topic = config_dict["general"]["base_topic"]
    global_config.general.cache_size = config_dict["general"]["cache_size"]

    return global_config


class DummyTopicNS:
    MINISERVER_STARTUP_EVENT = "dummy_startup"
    CONFIG_GET = "dummy_config_get"
    CONFIG_RESPONSE = "dummy_config_response"
    CONFIG_SET = "dummy_config_set"
    CONFIG_ADD = "dummy_config_add"
    CONFIG_REMOVE = "dummy_config_remove"
    CONFIG_UPDATE = "dummy_config_update"
    CONFIG_RESTART = "dummy_config_restart"


def build_processor(config, topics=None, relay_main=None, orjson=None, mqtt_client=None):
    """Build a processor the way production does.

    Both native clients are real: the processor shares their connection state,
    so a mock in either place would leave it with nothing to share.
    """
    return MiniserverDataProcessor(
        topics or DummyTopicNS(),
        config,
        relay_main or AsyncMock(),
        mqtt_client or MqttClient(config),
        MiniserverClient(config),
        orjson or MagicMock(),
    )


@pytest_asyncio.fixture(scope="function")
async def processor(config_instance):
    return build_processor(config_instance)


@pytest.mark.parametrize("whitelist", [
    {"set/topic/a", "set/topic/b"},  # real default config type (Set[str])
    set(),                            # empty set
    ["list/topic"],                   # list still works
])
def test_construct_accepts_set_or_list_whitelist(config_instance, whitelist):
    """Regression: topic_whitelist is a Python set in the default config.

    pyo3 0.28 rejects ``Vec`` extraction from a set ("not a Sequence"), so the
    Rust constructor must accept any iterable. Tests elsewhere only ever pass
    lists, which is why this crashed at runtime but not in CI.
    """
    config_instance.topics.topic_whitelist = whitelist
    assert build_processor(config_instance).topic_whitelist == set(whitelist)


def test_construct_refuses_an_unusable_filter(config_instance):
    """A typo in a configured filter has to surface as a startup error.

    Skipped with a log line, it would hand the operator a relay that forwards
    exactly what the filter was meant to hold back.
    """
    config_instance.topics.do_not_forward = ["[unclosed"]
    with pytest.raises(ValueError, match=r"\[unclosed"):
        build_processor(config_instance)


@pytest.mark.parametrize("input_val,expected", [
    ("true", "1"),
    ("TRUE", "1"),
    ("yes", "1"),
    ("on", "1"),
    ("enabled", "1"),
    ("enable", "1"),
    ("1", "1"),
    ("check", "1"),
    ("checked", "1"),
    ("select", "1"),
    ("selected", "1"),
    ("false", "0"),
    ("FALSE", "0"),
    ("no", "0"),
    ("off", "0"),
    ("disabled", "0"),
    ("disable", "0"),
    ("0", "0"),
    ("invalid", "invalid"),
    ("", ""),
])
def test_convert_boolean(processor, input_val, expected):
    assert processor._convert_boolean(input_val) == expected


def test_normalize_topic(processor):
    assert processor.normalize_topic("a/b/c") == "a_b_c"
    assert processor.normalize_topic("a_b_c") == "a_b_c"  # Already normalized
    assert processor.normalize_topic("test/topic") == "test_topic"
    assert processor.normalize_topic("test%topic") == "test_topic"
    assert processor.normalize_topic("test/topic%with/both") == "test_topic_with_both"
    assert processor.normalize_topic("test/topic/%with/both") == "test_topic__with_both"


def test_cache_behavior(processor):
    # Both answers are cached, so asking twice has to give the same answer.
    assert processor.normalize_topic("a/b/c") == "a_b_c"
    assert processor.normalize_topic("a/b/c") == "a_b_c"
    assert processor._convert_boolean("true") == "1"
    assert processor._convert_boolean("true") == "1"


def test_is_in_whitelist_asks_about_the_normalized_name(processor):
    """The whitelist holds Miniserver input names, not MQTT topics."""
    processor.update_topic_whitelist(["a_b_c"])
    assert processor.is_in_whitelist("a/b/c")
    assert not processor.is_in_whitelist("a/b/d")


def test_update_topic_whitelist(processor):
    whitelist = ["some_allowed_topic", "another_allowed_topic"]
    processor.update_topic_whitelist(whitelist)
    assert processor.topic_whitelist == set(whitelist)


def test_update_subscription_filters(processor):
    filters = [r"^ignore_.*", r"^skip_.*"]
    processor.update_subscription_filters(filters)
    assert processor.get_subscription_filters() == filters


def test_update_do_not_forward(processor):
    do_not_forward = [r"^debug_.*", r"private_topic"]
    processor.update_do_not_forward(do_not_forward)
    assert processor.get_do_not_forward_patterns() == do_not_forward


@pytest.mark.parametrize("filters", [
    [r"^foo\/(a|b)$"],   # an alternation of its own
    [r"bar\|baz"],       # an escaped pipe
    [r"[|]"],            # a pipe in a character class
    [r"^a$", r"^b$"],    # several patterns, unchanged
])
def test_the_filter_getters_return_what_was_configured(processor, filters):
    """The getters used to rebuild the list by splitting one joined pattern at
    every '|', which mangled every pattern that contained a pipe itself."""
    processor.update_subscription_filters(filters)
    assert processor.get_subscription_filters() == filters

    processor.update_do_not_forward(filters)
    assert processor.get_do_not_forward_patterns() == filters


@pytest.mark.parametrize("filters", [[""], ["   "], [r"^ok\/", ""]])
def test_an_empty_filter_pattern_is_refused(processor, filters):
    """An empty expression matches every topic, so a stray "" in the list would
    silently filter away everything instead of the one thing it names."""
    with pytest.raises(ValueError):
        processor.update_subscription_filters(filters)
    with pytest.raises(ValueError):
        processor.update_do_not_forward(filters)


def test_shape_metrics_and_shape_stats_agree(processor):
    """The 2-tuple stays what it was; the dict is the same numbers plus more."""
    cached, built = processor.get_shape_stats()
    metrics = processor.get_shape_metrics()

    assert metrics["plans"] == cached
    assert metrics["learns"] == built
    assert set(metrics) == {
        "plans", "learns", "hits", "learn_failures",
        "dom_fallbacks", "negative_skips", "unplannable",
    }


class ControlTopicNS:
    MINISERVER_STARTUP_EVENT = "myrelay/miniserverevent/startup"
    CONFIG_GET = "myrelay/config/get"
    CONFIG_RESPONSE = "myrelay/config/response"
    CONFIG_SET = "myrelay/config/set"
    CONFIG_ADD = "myrelay/config/add"
    CONFIG_REMOVE = "myrelay/config/remove"
    CONFIG_UPDATE = "myrelay/config/update"
    CONFIG_RESTART = "myrelay/config/restart"


class TestConfigControlTopics:
    """The control topics, which are the only part of the message path that
    still crosses into Python.

    The mocks are kept so the Python callbacks the Rust code triggers
    (relay_main, orjson) can be asserted on. Publishing no longer crosses into
    Python, so it is observed through the MQTT client's undelivered ring.
    """

    @pytest.fixture
    def ctx(self, config_instance, monkeypatch):
        mock_mqtt_client = MqttClient(config_instance)
        mock_relay_main = MagicMock()
        mock_orjson = MagicMock()
        topics = ControlTopicNS()
        # The config actions work on the object the processor was handed, not on
        # one reached back through the relay - so that is where they are observed.
        monkeypatch.setattr(config_instance, "get_safe_config", MagicMock(return_value={}))
        monkeypatch.setattr(config_instance, "update_fields", MagicMock())
        processor = build_processor(
            config_instance,
            topics=topics,
            relay_main=mock_relay_main,
            orjson=mock_orjson,
            mqtt_client=mock_mqtt_client,
        )
        return SimpleNamespace(
            processor=processor,
            topics=topics,
            relay_main=mock_relay_main,
            mqtt_client=mock_mqtt_client,
            orjson=mock_orjson,
            global_config=config_instance,
        )

    def test_config_get_serializes_and_publishes_safe_config(self, ctx):
        ctx.orjson.dumps.return_value = b'{"general": {}}'
        assert ctx.processor.handle_control(ctx.topics.CONFIG_GET, b"") is True

        # safe config is fetched, serialized and published to the response topic
        ctx.global_config.get_safe_config.assert_called_once()
        ctx.orjson.dumps.assert_called_once()
        # The client is not connected, so the publish lands in the undelivered
        # ring - which is where the target topic becomes observable, together
        # with the reason the response never went out.
        assert ctx.mqtt_client.take_undelivered() == [
            (ctx.topics.CONFIG_RESPONSE, b'{"general": {}}', "broker not connected")
        ]
        # config/get must never restart the relay
        ctx.relay_main.restart_relay.assert_not_called()

    @pytest.mark.parametrize("topic_attr,expected_mode", [
        ("CONFIG_SET", "set"),
        ("CONFIG_ADD", "add"),
        ("CONFIG_REMOVE", "remove"),
    ])
    def test_config_modify_updates_fields_and_restarts(self, ctx, topic_attr, expected_mode):
        topic = getattr(ctx.topics, topic_attr)
        assert ctx.processor.handle_control(topic, b'{"general": {"cache_size": 50}}') is True

        ctx.orjson.loads.assert_called_once()
        ctx.global_config.update_fields.assert_called_once()
        # second positional arg is the update mode ("set"/"add"/"remove")
        assert ctx.global_config.update_fields.call_args[0][1] == expected_mode
        # a successful update restarts the relay
        ctx.relay_main.restart_relay.assert_called_once()

    def test_rejected_config_update_does_not_restart(self, ctx):
        """A refused update must not send the relay through os.execv.

        The config would be unchanged, so the restart would achieve nothing but
        a dropped MQTT session - and a publisher could trigger it at will.
        """
        ctx.global_config.update_fields.side_effect = ConfigError("refused")

        ctx.processor.handle_control(ctx.topics.CONFIG_SET, b'{"miniserver_ip": "203.0.113.5"}')

        ctx.global_config.update_fields.assert_called_once()
        ctx.relay_main.restart_relay.assert_not_called()

    def test_invalid_json_does_not_restart(self, ctx):
        """An unparseable payload is a publisher's problem, not a reason to
        restart - and must not reach update_fields at all."""
        ctx.orjson.loads.side_effect = ValueError("not json")

        assert ctx.processor.handle_control(ctx.topics.CONFIG_SET, b"{oops") is True

        ctx.global_config.update_fields.assert_not_called()
        ctx.relay_main.restart_relay.assert_not_called()

    @pytest.mark.parametrize("topic_attr", ["CONFIG_UPDATE", "CONFIG_RESTART"])
    def test_config_update_and_restart_only_restart(self, ctx, topic_attr):
        topic = getattr(ctx.topics, topic_attr)
        assert ctx.processor.handle_control(topic, b"") is True

        ctx.relay_main.restart_relay.assert_called_once()
        # plain update/restart must not mutate the config
        ctx.global_config.update_fields.assert_not_called()

    def test_miniserver_startup_triggers_sync_when_enabled(self, ctx):
        global_config.miniserver.sync_with_miniserver = True
        assert ctx.processor.handle_control(ctx.topics.MINISERVER_STARTUP_EVENT, b"") is True
        ctx.relay_main.schedule_miniserver_sync.assert_called_once()

    def test_miniserver_startup_skips_sync_when_disabled(self, ctx):
        global_config.miniserver.sync_with_miniserver = False
        ctx.processor.handle_control(ctx.topics.MINISERVER_STARTUP_EVENT, b"")
        ctx.relay_main.schedule_miniserver_sync.assert_not_called()

    @pytest.mark.parametrize("topic", [
        "some/data/topic",
        # Under base_topic but not a control topic. Routing used to be gated by
        # a plain `starts_with(base_topic)` check, so this matched the prefix,
        # hit no branch, and fell through to nothing at all.
        "myrelay/sensor/temperature",
    ])
    def test_a_data_topic_is_not_treated_as_control(self, ctx, topic):
        """Reported as not-control, so the caller takes the data path.

        Where that path leads is Rust's business now; what matters here is that
        none of the control actions fired.
        """
        assert ctx.processor.handle_control(topic, b"21.5") is False

        ctx.relay_main.restart_relay.assert_not_called()
        ctx.relay_main.schedule_miniserver_sync.assert_not_called()
        assert ctx.mqtt_client.take_undelivered() == []

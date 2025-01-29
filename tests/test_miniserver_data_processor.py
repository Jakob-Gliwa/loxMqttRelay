import pytest
import pytest_asyncio
import json
from unittest.mock import AsyncMock, patch
from loxmqttrelay.config import Config, AppConfig, global_config
from loxmqttrelay.miniserver_data_processor import MiniserverDataProcessor

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
        },
        "debug": {
            "publish_processed_topics": False
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
    global_config.debug.publish_processed_topics = config_dict["debug"]["publish_processed_topics"]
    
    return global_config

@pytest_asyncio.fixture(scope="function")
async def processor(config_instance):
    """Create processor instance"""
    return MiniserverDataProcessor()

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
    (None, None),
])
def test_convert_boolean(processor, input_val, expected):
    assert processor._convert_boolean(input_val) == expected

def test_flatten_dict(processor):
    input_dict = {
        "a": 1,
        "b": {
            "c": 2,
            "d": {
                "e": 3
            }
        },
        "f": [1, 2, 3]
    }
    expected = {
        ("a", 1),
        ("b/c", 2),
        ("b/d/e", 3),
        ("f/0", 1),
        ("f/1", 2),
        ("f/2", 3)
    }
    assert processor.flatten_dict(input_dict) == expected

def test_normalize_topic(processor):
    assert processor.normalize_topic("a/b/c") == "a_b_c"
    assert processor.normalize_topic("a_b_c") == "a_b_c"  # Already normalized
    assert processor.normalize_topic("test/topic") == "test_topic"
    assert processor.normalize_topic("test%topic") == "test_topic"  # Test percent sign
    assert processor.normalize_topic("test/topic%with/both") == "test_topic_with_both"  # Test both / and %
    assert processor.normalize_topic("test/topic/%with/both") == "test_topic__with_both"  # Test both / and %

def test_expand_json(processor):
    result = processor.expand_json("test", '{"key1": "val1", "key2": {"nested": "val2"}}')
    expected = {
        ("test/key1", "val1"),
        ("test/key2/nested", "val2")
    }
    assert result == expected

    # Test with non-JSON value
    result = processor.expand_json("test", "normal_value")
    assert result == {("test", "normal_value")}


def test_cache_behavior(processor):
    # Test that cache is working for normalize_topic
    assert processor.normalize_topic("a/b/c") == "a_b_c"
    assert processor.normalize_topic("a/b/c") == "a_b_c"  # Should hit cache
    
    # Test convert_boolean cache
    assert processor._convert_boolean("true") == "1"
    assert processor._convert_boolean("true") == "1"  # Should hit cache

@pytest.mark.asyncio
async def test_update_subscription_filters_single(processor):
    """Test setting subscription filters."""
    filters = [r"^ignore_.*", r"^skip_.*"]
    processor.update_subscription_filters(filters)
    assert processor.compiled_subscription_filter is not None

@pytest.mark.asyncio
async def test_update_topic_whitelist(processor):
    whitelist = ["some_allowed_topic", "another_allowed_topic"]
    processor.update_topic_whitelist(whitelist)
    assert processor.topic_whitelist == whitelist

@pytest.mark.asyncio
async def test_update_do_not_forward(processor):
    do_not_forward = [r"^debug_.*", r"private_topic"]
    processor.update_do_not_forward(do_not_forward)
    assert processor.do_not_forward_patterns is not None

@pytest.mark.parametrize("filters,topic,message,should_stay", [
    ([r"^ignore\/.*"], "ignore/something", "value", False),
    ([r"^ignore\/.*"], "normal/topic", "value", True),
])
@pytest.mark.asyncio
async def test_process_data_single_filter_pass(processor, filters, topic, message, should_stay):
    """Test if subscription filter works correctly in first pass."""
    processor.update_subscription_filters(filters)
    
    results = []
    async for result in processor.process_data(topic, message):
        results.append(result)
    
    if should_stay:
        assert len(results) > 0, f"Topic '{topic}' should remain after filtering"
        assert isinstance(results[0], tuple) and len(results[0]) == 2, "Result should be a topic-value tuple"
        topic_result, _ = results[0]
        assert topic_result == topic
    else:
        assert len(results) == 0, f"Topic '{topic}' should be filtered out"

@pytest.mark.asyncio
async def test_process_data_filter_second_pass_after_flatten(processor, monkeypatch):
    """Test that filter works after JSON flattening."""
    topic = "original/topic"
    message = '{"key1": "val1", "ignore": {"nested": "val2"}}'

    processor.update_subscription_filters([r"ignore\/.*"])
    monkeypatch.setattr(global_config.processing, 'expand_json', True)

    results = []
    async for result in processor.process_data(topic, message):
        results.append(result)

    result_topics = [t for t, _ in results]

    assert "original/topic/ignore/nested" not in result_topics
    assert "original/topic/key1" in result_topics

@pytest.mark.parametrize("whitelist,topic,message,should_stay", [
    (["some_allowed_topic"], "not/whitelisted", "value", False),
    (["some_allowed_topic"], "some/allowed/topic", "value", True),
    ([], "whatever/topic", "value", True),
])
@pytest.mark.asyncio
async def test_process_data_with_whitelist(processor, whitelist, topic, message, should_stay):
    """Test whitelist functionality."""
    processor.update_topic_whitelist(whitelist)
    
    results = []
    async for result in processor.process_data(topic, message):
        results.append(result)

    if should_stay:
        assert len(results) > 0, f"Topic '{topic}' should remain in whitelist"
        assert isinstance(results[0], tuple) and len(results[0]) == 2, "Result should be a topic-value tuple"
        topic_result, _ = results[0]
        assert topic_result == topic
    else:
        assert len(results) == 0, f"Topic '{topic}' should be filtered by whitelist"

@pytest.mark.parametrize("dnf_filter,topic,message,should_stay", [
    ([r"^debug\/.*"], "debug/sensor", "value", False),
    ([r"^debug\/.*"], "normal/sensor", "value", True),
    ([r"private\/topic"], "private/topic", "value", False),
    ([], "private/topic", "value", True),
])
@pytest.mark.asyncio
async def test_process_data_with_do_not_forward(processor, dnf_filter, topic, message, should_stay):
    processor.update_do_not_forward(dnf_filter)
    
    results = []
    async for result in processor.process_data(topic, message):
        results.append(result)

    if should_stay:
        assert len(results) > 0, f"Topic '{topic}' should not be filtered"
        assert isinstance(results[0], tuple) and len(results[0]) == 2, "Result should be a topic-value tuple"
        topic_result, _ = results[0]
        assert topic_result == topic
    else:
        assert len(results) == 0, f"Topic '{topic}' should be filtered by do_not_forward"

@pytest.mark.asyncio
async def test_process_data_order_of_filters(processor, monkeypatch):
    """Test filter order: 1) SubscriptionFilter, 2) Flatten, 3) SubscriptionFilter, 4) Whitelist, 5) doNotForward"""
    topic_messages = [
        ("ignore/before/foo", "val1"),
        ("json/topic", '{"ignore":{"after":{"bar":"val2"}}}'),
        ("whitelisted/foo", "val4"),
        ("dnf/bar", "val5"),
        ("normal/publish", "val6")
    ]

    processor.update_subscription_filters([r"^ignore\/before\/.*", r"^ignore\/after\/.*"])
    processor.update_topic_whitelist(["whitelisted_foo", "normal_publish"])
    processor.update_do_not_forward([r"^dnf\/.*"])
    monkeypatch.setattr(global_config.processing, 'expand_json', True)

    results = []
    for topic, message in topic_messages:
        async for result in processor.process_data(topic, message):
            results.append(result)

    result_topics = [t for t, _ in results]

    assert "ignore/before/foo" not in result_topics
    assert "json/topic/ignore/after/bar" not in result_topics
    assert "dnf/bar" not in result_topics
    assert "whitelisted/foo" in result_topics
    assert "normal/publish" in result_topics

@pytest.mark.asyncio
async def test_process_data_with_debug_publish(processor, monkeypatch):
    """Test debug publishing functionality."""
    mock_publish = AsyncMock()
    topic_messages = [
        ("topic/one", "1"),
        ("ignore/before/two", "2"),
        ("topic/three", "3")
    ]

    processor.update_subscription_filters([r"^ignore\/before\/.*"])
    monkeypatch.setattr(global_config.debug, 'publish_processed_topics', True)
    monkeypatch.setattr(global_config.general, 'base_topic', "myrelay/")

    results = []
    for topic, message in topic_messages:
        async for result in processor.process_data(topic, message, mqtt_publish_callback=mock_publish):
            results.append(result)

    result_topics = [t for t, _ in results]
    assert "topic/one" in result_topics
    assert "topic/three" in result_topics
    assert "ignore/before/two" not in result_topics

    assert mock_publish.call_count == 2
    published_topics = [call.args[0] for call in mock_publish.call_args_list]
    assert "myrelay/processedtopics/topic_one" in published_topics
    assert "myrelay/processedtopics/topic_three" in published_topics

@pytest.mark.asyncio
async def test_publish_forwarded_topic(processor):
    """Test forwarded topic publishing."""
    mock_publish = AsyncMock()
    await processor.publish_forwarded_topic("test/topic", "value", 200, mock_publish)
    
    mock_publish.assert_called_once()
    call_args = mock_publish.call_args[0]
    assert call_args[0] == "myrelay/forwardedtopics/test_topic"
    assert "value" in call_args[1]
    assert "200" in call_args[1]
    assert call_args[2] is False

# Rest of the existing tests...

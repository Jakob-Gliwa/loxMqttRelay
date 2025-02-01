import pytest
import pytest_asyncio
import json
from unittest.mock import AsyncMock, patch, MagicMock
from loxmqttrelay.config import Config, AppConfig, global_config
import asyncio
from loxmqttrelay._loxmqttrelay import MiniserverDataProcessor  # Assuming 'librs' is the compiled Rust module

TOPIC = 'mock/topic'  # Define a mock or placeholder for the TOPIC variable

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

class DummyTopicNS:
    START_UI = "dummy_start_ui"
    STOP_UI = "dummy_stop_ui"
    MINISERVER_STARTUP_EVENT = "dummy_startup"
    CONFIG_GET = "dummy_config_get"
    CONFIG_RESPONSE = "dummy_config_response"
    CONFIG_SET = "dummy_config_set"
    CONFIG_ADD = "dummy_config_add"
    CONFIG_REMOVE = "dummy_config_remove"
    CONFIG_UPDATE = "dummy_config_update"
    CONFIG_RESTART = "dummy_config_restart"

class TestMiniserverDataProcessor:
    def __init__(self, config_instance):
        """Initialize required mocks and processor instance."""
        self.mock_http_handler = MagicMock()
        self.mock_mqtt_client = MagicMock()
        self.mock_relay_main = AsyncMock()
        self.mock_orjson = MagicMock()

        self.dummy_topic_ns = DummyTopicNS()
        self.config_instance = config_instance
        
        # Initialize the processor with the Rust implementation
        self.processor = MiniserverDataProcessor(
            self.dummy_topic_ns, 
            self.config_instance, 
            self.mock_relay_main, 
            self.mock_mqtt_client, 
            self.mock_http_handler, 
            self.mock_orjson
        )

@pytest_asyncio.fixture(scope="function")
async def processor(config_instance):
    """Return the processor directly for easier testing"""
    test_processor = TestMiniserverDataProcessor(config_instance)
    return test_processor.processor

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
    (None, "")
])
def test_convert_boolean(processor, input_val, expected):
    # If input_val is None, pass an empty string
    in_val = input_val if input_val is not None else ""
    assert processor._convert_boolean(in_val) == expected

def test_flatten_dict(processor):
    import json
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
    json_str = json.dumps(input_dict)
    # Using topic prefix 'test'
    result = processor.expand_json("test", json_str)
    expected = {
        ("test/a", "1"),
        ("test/b/c", "2"),
        ("test/b/d/e", "3"),
        ("test/f/0", "1"),
        ("test/f/1", "2"),
        ("test/f/2", "3")
    }
    assert set(result) == expected

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

def test_update_subscription_filters_single(processor):
    """Test setting subscription filters."""
    filters = [r"^ignore_.*", r"^skip_.*"]
    processor.update_subscription_filters(filters)
    assert processor.compiled_subscription_filter is not None

def test_update_topic_whitelist(processor):
    whitelist = ["some_allowed_topic", "another_allowed_topic"]
    processor.update_topic_whitelist(whitelist)
    assert processor.topic_whitelist == set(whitelist)

def test_update_do_not_forward(processor):
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
    processor.process_data(topic, message)
    
    if should_stay:
        processor.http_handler_obj.send_to_miniserver_sync.assert_called()
    else:
        processor.http_handler_obj.send_to_miniserver_sync.assert_not_called()

@pytest.mark.asyncio
async def test_process_data_filter_second_pass_after_flatten(processor, monkeypatch):
    """Test that filter works after JSON flattening."""
    topic = "original/topic"
    message = '{"key1": "val1", "ignore": {"nested": "val2"}}'

    processor.update_subscription_filters([r"ignore\/.*"])
    monkeypatch.setattr(global_config.processing, 'expand_json', True)

    processor.process_data(topic, message)
    calls = processor.http_handler_obj.send_to_miniserver_sync.call_args_list
    processed_topics = [call[0][0] for call in calls]

    assert "original/topic/ignore/nested" not in processed_topics
    assert "original/topic/key1" in processed_topics

@pytest.mark.asyncio
async def test_process_data_with_whitelist(processor):
    # Test non-whitelisted case
    whitelist = ["not_whitelisted"]  # Using normalized format
    topic = "is/whitelisted"
    message = "value"
    processor.update_topic_whitelist(whitelist)
    processor.process_data(topic, message)
    processor.http_handler_obj.send_to_miniserver_sync.assert_not_called()
    
    # Test passing case - reset mock first
    processor.http_handler_obj.send_to_miniserver_sync.reset_mock()
    
    # Get the normalized version of the topic we'll send
    test_topic = "some/allowed/topic"
    normalized_topic = processor.normalize_topic(test_topic)
    processor.update_topic_whitelist([normalized_topic])  # Use the normalized version in whitelist
    
    # Debug prints before processing
    print(f"Test topic: {test_topic}")
    print(f"Normalized topic: {normalized_topic}")
    print(f"Whitelist before: {processor.topic_whitelist}")
    
    # Process with the non-normalized topic (it should be normalized internally)
    processor.process_data(test_topic, "value")
    
    # Debug prints after processing
    print(f"Whitelist after: {processor.topic_whitelist}")
    print(f"Mock calls: {processor.http_handler_obj.send_to_miniserver_sync.mock_calls}")
    
    processor.http_handler_obj.send_to_miniserver_sync.assert_called()

@pytest.mark.asyncio
async def test_process_data_with_do_not_forward(processor):
    dnf_filter = [r"^debug\/.*"]
    topic = "debug/sensor"
    message = "value"
    processor.update_do_not_forward(dnf_filter)
    processor.process_data(topic, message)
    processor.http_handler_obj.send_to_miniserver_sync.assert_not_called()

@pytest.mark.asyncio
async def test_process_data_order_of_filters(processor, monkeypatch):
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

    for topic, message in topic_messages:
        processor.process_data(topic, message)
        
    # Reset call list to ensure we start fresh
    processor.http_handler_obj.send_to_miniserver_sync.reset_mock()
    
    # Process messages again to ensure clean state
    for topic, message in topic_messages:
        processor.process_data(topic, message)

    expected_topics = ["whitelisted/foo", "normal/publish"]
    actual_calls = [call[0][0] for call in processor.http_handler_obj.send_to_miniserver_sync.call_args_list]
    print(f"Actual calls: {actual_calls}")  # Debug print
    print(f"Expected topics: {expected_topics}")  # Debug print
    assert set(actual_calls) == set(expected_topics), "Only whitelisted and normal topics should be processed"

@pytest.mark.asyncio
async def test_process_data_with_debug_publish(processor, monkeypatch):
    mock_publish = AsyncMock()
    topic_messages = [
        ("topic/one", "1"),
        ("ignore/before/two", "2"),
        ("topic/three", "3")
    ]

    processor.update_subscription_filters([r"^ignore\/before\/.*"])
    monkeypatch.setattr(global_config.debug, 'publish_processed_topics', True)
    monkeypatch.setattr(global_config.general, 'base_topic', "myrelay/")

    for topic, message in topic_messages:
        processor.process_data(topic, message, mqtt_publish_callback=mock_publish)

    await asyncio.sleep(0.1)

    expected_debug_topics = [
        "myrelay/processedtopics/topic_one",
        "myrelay/processedtopics/topic_three"
    ]
    
    actual_publish_topics = [call[0][0] for call in mock_publish.call_args_list if len(call[0]) > 0]
    assert set(actual_publish_topics) == set(expected_debug_topics), "Debug topics should be published correctly"

@pytest.mark.asyncio
async def test_publish_forwarded_topic(processor):
    mock_publish = AsyncMock()
    processor.publish_forwarded_topic("test/topic", "value", 200, mock_publish)
    
    mock_publish.assert_called_once()
    call_args = mock_publish.call_args[0]
    assert call_args[0] == "myrelay/forwardedtopics/test_topic"
    assert "value" in call_args[1]
    assert "200" in call_args[1]
    assert call_args[2] is False

# Rest of the existing tests...

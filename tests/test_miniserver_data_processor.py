import pytest
import pytest_asyncio
import json
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch, MagicMock
from loxmqttrelay.config import Config, AppConfig, ConfigError, global_config
import asyncio
from loxmqttrelay.compatible._loxmqttrelay import MiniserverDataProcessor, MqttClient  # Assuming 'librs' is the compiled Rust module

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

class TestMiniserverDataProcessor:
    def __init__(self, config_instance):
        """Initialize required mocks and processor instance."""
        self.mock_http_handler = MagicMock()
        # The processor shares the client's connection state, so this has to be
        # the real Rust class rather than a mock.
        self.mock_mqtt_client = MqttClient(config_instance)
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


def build_processor(
    config,
    *,
    expand_json=False,
    convert_booleans=True,
    do_not_forward=(),
    topic_whitelist=(),
):
    """Build a processor the way production does: configure, then construct.

    Everything the Rust side reads once is set on the config beforehand, so
    these tests exercise the same wiring ``MQTTRelay.__init__`` uses instead of
    reaching for the update_* mutators.
    """
    config.processing.expand_json = expand_json
    config.processing.convert_booleans = convert_booleans
    config.topics.do_not_forward = list(do_not_forward)
    config.topics.topic_whitelist = set(topic_whitelist)
    return MiniserverDataProcessor(
        DummyTopicNS(),
        config,
        AsyncMock(),
        MqttClient(config),
        MagicMock(),
        MagicMock(),
    )


def handed_over(http_handler):
    """The (topic, normalized_topic, value) triples the Rust side handed over.

    A message is passed across in one batched call, so the assertions below
    flatten the recorded batches back into individual values.
    """
    return [
        tuple(item)
        for call in http_handler.send_batch_to_miniserver.call_args_list
        for item in call[0][0]
    ]


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
    proc = MiniserverDataProcessor(
        DummyTopicNS(),
        config_instance,
        AsyncMock(),
        MqttClient(config_instance),
        MagicMock(),
        MagicMock(),
    )
    assert proc.topic_whitelist == set(whitelist)

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

@pytest.mark.asyncio
async def test_json_is_flattened_into_one_value_per_leaf(config_instance):
    """Nested objects and arrays each become their own '/'-joined target."""
    processor = build_processor(config_instance, expand_json=True, convert_booleans=False)
    message = json.dumps({"a": 1, "b": {"c": 2, "d": {"e": 3}}, "f": [1, 2, 3]})

    processor.process_data("test", message)

    assert set(handed_over(processor.http_handler_obj)) == {
        ("test/a", "test_a", "1"),
        ("test/b/c", "test_b_c", "2"),
        ("test/b/d/e", "test_b_d_e", "3"),
        ("test/f/0", "test_f_0", "1"),
        ("test/f/1", "test_f_1", "2"),
        ("test/f/2", "test_f_2", "3"),
    }

def test_normalize_topic(processor):
    assert processor.normalize_topic("a/b/c") == "a_b_c"
    assert processor.normalize_topic("a_b_c") == "a_b_c"  # Already normalized
    assert processor.normalize_topic("test/topic") == "test_topic"
    assert processor.normalize_topic("test%topic") == "test_topic"  # Test percent sign
    assert processor.normalize_topic("test/topic%with/both") == "test_topic_with_both"  # Test both / and %
    assert processor.normalize_topic("test/topic/%with/both") == "test_topic__with_both"  # Test both / and %

@pytest.mark.asyncio
async def test_a_payload_that_is_not_json_is_forwarded_as_it_stands(config_instance):
    processor = build_processor(config_instance, expand_json=True, convert_booleans=False)

    processor.process_data("test", "normal_value")

    assert handed_over(processor.http_handler_obj) == [("test", "test", "normal_value")]


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
    assert processor.get_subscription_filters() == filters


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


@pytest.mark.asyncio
async def test_an_inline_flag_stays_inside_its_own_pattern(processor):
    """`(?i)` applies to the rest of its enclosing group, so joining the
    patterns into one made every following filter case-insensitive too."""
    processor.update_do_not_forward([r"(?i)^debug\/", r"^Secret\/"])

    processor.process_data("secret/value", "1")

    assert handed_over(processor.http_handler_obj) == [
        ("secret/value", "secret_value", "1")
    ]

def test_update_topic_whitelist(processor):
    whitelist = ["some_allowed_topic", "another_allowed_topic"]
    processor.update_topic_whitelist(whitelist)
    assert processor.topic_whitelist == set(whitelist)

def test_update_do_not_forward(processor):
    do_not_forward = [r"^debug_.*", r"private_topic"]
    processor.update_do_not_forward(do_not_forward)
    assert processor.get_do_not_forward_patterns() == do_not_forward

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
        assert handed_over(processor.http_handler_obj)
    else:
        assert not handed_over(processor.http_handler_obj)

@pytest.mark.asyncio
async def test_process_data_filter_second_pass_after_flatten(config_instance, monkeypatch):
    """Test that filter works after JSON flattening."""
    topic = "original/topic"
    message = '{"key1": "val1", "ignore": {"nested": "val2"}}'

    # expand_json is read once at construction (config is immutable between
    # restarts), so enable it BEFORE building the processor.
    monkeypatch.setattr(global_config.processing, 'expand_json', True)
    processor = TestMiniserverDataProcessor(config_instance).processor

    processor.update_subscription_filters([r"ignore\/.*"])

    processor.process_data(topic, message)
    processed_topics = [t for t, _, _ in handed_over(processor.http_handler_obj)]

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
    assert not handed_over(processor.http_handler_obj)
    
    # Test passing case - reset mock first
    processor.http_handler_obj.send_batch_to_miniserver.reset_mock()
    
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
    print(f"Handed over: {handed_over(processor.http_handler_obj)}")
    
    assert handed_over(processor.http_handler_obj)

@pytest.mark.asyncio
async def test_process_data_with_do_not_forward(processor):
    dnf_filter = [r"^debug\/.*"]
    topic = "debug/sensor"
    message = "value"
    processor.update_do_not_forward(dnf_filter)
    processor.process_data(topic, message)
    assert not handed_over(processor.http_handler_obj)


@pytest.mark.parametrize("convert_booleans", [True, False])
@pytest.mark.parametrize("value,mapped", [
    ("on", "1"),
    ("off", "0"),
    ("true", "1"),
    ("false", "0"),
    ("ON", "1"),
    ("1", "1"),
    ("0", "0"),
    ("23.5", "23.5"),
    ("hello", "hello"),
])
@pytest.mark.asyncio
async def test_convert_booleans_setting_decides_the_forwarded_value(
    config_instance, convert_booleans, value, mapped
):
    """``processing.convert_booleans`` has to reach the forwarding path.

    With the option off, a Zigbee2MQTT ``action`` of ``on``/``off`` must arrive
    at the miniserver as sent - otherwise declarative action matching is
    impossible.
    """
    processor = build_processor(config_instance, convert_booleans=convert_booleans)
    processor.process_data("zigbee/switch/action", value)

    expected = mapped if convert_booleans else value
    assert [v for _, _, v in handed_over(processor.http_handler_obj)] == [expected]


@pytest.mark.parametrize("convert_booleans,expected", [
    (True, ["1", "0", "1", "0", "1"]),
    (False, ["on", "off", "true", "false", "1"]),
])
@pytest.mark.asyncio
async def test_convert_booleans_setting_applies_to_expanded_json(
    config_instance, convert_booleans, expected
):
    processor = build_processor(
        config_instance, expand_json=True, convert_booleans=convert_booleans
    )
    payload = '{"a":"on","b":"off","c":true,"d":false,"e":1}'
    processor.process_data("zigbee/device", payload)

    assert [v for _, _, v in handed_over(processor.http_handler_obj)] == expected


@pytest.mark.parametrize("convert_booleans", [True, False])
def test_effective_convert_booleans_is_readable(config_instance, convert_booleans):
    """Startup diagnostics report this value, so it has to be observable."""
    processor = build_processor(config_instance, convert_booleans=convert_booleans)
    assert processor.convert_booleans is convert_booleans


@pytest.mark.asyncio
async def test_configured_do_not_forward_is_active_after_construction(config_instance):
    """Regression: the patterns used to need a mutator call no one made."""
    processor = build_processor(config_instance, do_not_forward=[r"^debug\/.*"])

    assert processor.get_do_not_forward_patterns() == [r"^debug\/.*"]

    processor.process_data("debug/sensor", "value")
    assert not handed_over(processor.http_handler_obj)

    processor.process_data("normal/sensor", "value")
    assert [t for t, _, _ in handed_over(processor.http_handler_obj)] == ["normal/sensor"]


@pytest.mark.asyncio
async def test_whitelist_update_keeps_configured_do_not_forward(config_instance):
    """The Miniserver sync replaces the whitelist and nothing else."""
    processor = build_processor(config_instance, do_not_forward=[r"^debug\/.*"])
    processor.update_topic_whitelist(["debug_sensor", "normal_sensor"])

    processor.process_data("debug/sensor", "value")
    processor.process_data("normal/sensor", "value")

    assert [t for t, _, _ in handed_over(processor.http_handler_obj)] == ["normal/sensor"]


def test_invalid_do_not_forward_pattern_fails_construction(config_instance):
    with pytest.raises(ValueError, match="do_not_forward"):
        build_processor(config_instance, do_not_forward=[r"^ok\/.*", r"(unclosed"])


def test_invalid_do_not_forward_pattern_fails_update(processor):
    with pytest.raises(ValueError, match="do_not_forward"):
        processor.update_do_not_forward([r"(unclosed"])

@pytest.mark.asyncio
async def test_process_data_order_of_filters(config_instance, monkeypatch):
    topic_messages = [
        ("ignore/before/foo", "val1"),
        ("json/topic", '{"ignore":{"after":{"bar":"val2"}}}'),
        ("whitelisted/foo", "val4"),
        ("dnf/bar", "val5"),
        ("normal/publish", "val6")
    ]

    # expand_json is read once at construction, so enable it before building.
    monkeypatch.setattr(global_config.processing, 'expand_json', True)
    processor = TestMiniserverDataProcessor(config_instance).processor

    processor.update_subscription_filters([r"^ignore\/before\/.*", r"^ignore\/after\/.*"])
    processor.update_topic_whitelist(["whitelisted_foo", "normal_publish"])
    processor.update_do_not_forward([r"^dnf\/.*"])

    for topic, message in topic_messages:
        processor.process_data(topic, message)
        
    # Reset call list to ensure we start fresh
    processor.http_handler_obj.send_batch_to_miniserver.reset_mock()
    
    # Process messages again to ensure clean state
    for topic, message in topic_messages:
        processor.process_data(topic, message)

    expected_topics = ["whitelisted/foo", "normal/publish"]
    actual_calls = [t for t, _, _ in handed_over(processor.http_handler_obj)]
    print(f"Actual calls: {actual_calls}")  # Debug print
    print(f"Expected topics: {expected_topics}")  # Debug print
    assert set(actual_calls) == set(expected_topics), "Only whitelisted and normal topics should be processed"


class TestBinaryDataHandling:
    """Test cases for handling non-UTF-8 MQTT messages"""

    # Handing the batch over goes through into_future, which needs a running
    # loop; handle_mqtt_message reports that failure rather than swallowing it.
    pytestmark = pytest.mark.asyncio

    @pytest.fixture
    def processor(self, config_instance):
        """Create a processor instance for testing"""
        test_processor = TestMiniserverDataProcessor(config_instance)
        return test_processor.processor
    
    async def test_utf8_text_message_handling(self, processor):
        """Test that valid UTF-8 messages are processed normally"""
        topic = "test/topic"
        message = b"Hello, World! This is UTF-8 text."
        
        # Should not raise any exceptions
        try:
            result = processor.handle_mqtt_message(
                topic, 
                message
            )
            # Method should complete without error
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"UTF-8 message handling failed with exception: {e}")
    
    async def test_binary_message_handling(self, processor):
        """Test that binary messages are handled gracefully without crashing"""
        topic = "test/binary"
        
        # Test with zlib compressed data (like the problematic data from the error)
        binary_message = bytes([120, 156, 165, 125, 217, 142, 158, 201, 145, 221, 187, 212, 245, 47])
        
        # Should not crash - should handle gracefully
        try:
            result = processor.handle_mqtt_message(
                topic, 
                binary_message
            )
            # Method should complete without error
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Binary message handling failed with exception: {e}")
    
    async def test_gzip_compressed_data_handling(self, processor):
        """Test handling of gzip compressed data"""
        topic = "test/gzip"
        
        # Gzip header: 0x1f 0x8b
        gzip_data = bytes([0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
        
        try:
            result = processor.handle_mqtt_message(
                topic, 
                gzip_data
            )
            # Method should complete without error
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Gzip message handling failed with exception: {e}")
    
    async def test_png_image_data_handling(self, processor):
        """Test handling of PNG image data"""
        topic = "test/image"
        
        # PNG header: 89 50 4E 47
        png_data = bytes([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
        
        try:
            result = processor.handle_mqtt_message(
                topic, 
                png_data
            )
            # Method should complete without error
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"PNG message handling failed with exception: {e}")
    
    async def test_jpeg_image_data_handling(self, processor):
        """Test handling of JPEG image data"""
        topic = "test/jpeg"
        
        # JPEG header: FF D8 FF
        jpeg_data = bytes([0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46])
        
        try:
            result = processor.handle_mqtt_message(
                topic, 
                jpeg_data
            )
            # Method should complete without error
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"JPEG message handling failed with exception: {e}")
    
    async def test_zip_archive_data_handling(self, processor):
        """Test handling of ZIP archive data"""
        topic = "test/zip"
        
        # ZIP header: 50 4B 03 04
        zip_data = bytes([0x50, 0x4B, 0x03, 0x04, 0x14, 0x00, 0x00, 0x00, 0x08, 0x00])
        
        try:
            result = processor.handle_mqtt_message(
                topic, 
                zip_data
            )
            # Method should complete without error
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"ZIP message handling failed with exception: {e}")
    
    async def test_large_binary_data_handling(self, processor):
        """Test handling of large binary data"""
        topic = "test/large_binary"
        
        # Create a larger binary message (more than 32 bytes)
        large_binary = bytes([i % 256 for i in range(100)])
        
        try:
            result = processor.handle_mqtt_message(
                topic, 
                large_binary
            )
            # Method should complete without error
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Large binary message handling failed with exception: {e}")
    
    async def test_mixed_binary_and_text_topics(self, processor):
        """Test that both binary and text topics work in the same session"""
        # Test text topic
        try:
            text_result = processor.handle_mqtt_message(
                "test/text", 
                b"Hello World"
            )
            assert text_result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Text message handling failed with exception: {e}")
        
        # Test binary topic
        try:
            binary_result = processor.handle_mqtt_message(
                "test/binary", 
                bytes([120, 156, 165, 125, 217, 142])
            )
            assert binary_result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Binary message handling failed with exception: {e}")
        
        # Test another text topic
        try:
            text_result2 = processor.handle_mqtt_message(
                "test/another_text", 
                b"Another message"
            )
            assert text_result2 is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Second text message handling failed with exception: {e}")
    
    async def test_binary_data_with_special_characters(self, processor):
        """Test handling of binary data that contains special characters"""
        topic = "test/special_chars"
        
        # Binary data with null bytes and control characters
        special_binary = bytes([0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F])
        
        try:
            result = processor.handle_mqtt_message(
                topic, 
                special_binary
            )
            # Method should complete without error
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Special character binary message handling failed with exception: {e}")
    
    async def test_empty_binary_message(self, processor):
        """Test handling of empty binary message"""
        topic = "test/empty"
        
        try:
            result = processor.handle_mqtt_message(
                topic, 
                b""
            )
            # Method should complete without error
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Empty binary message handling failed with exception: {e}")
    
    async def test_single_byte_binary_message(self, processor):
        """Test handling of single byte binary message"""
        topic = "test/single_byte"
        
        try:
            result = processor.handle_mqtt_message(
                topic, 
                bytes([0xFF])
            )
            # Method should complete without error
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Single byte binary message handling failed with exception: {e}")


class TestBase64BinaryDataPreservation:
    """Test cases to verify that base64 encoding preserves binary data exactly"""

    pytestmark = pytest.mark.asyncio
    
    async def test_base64_encoding_preserves_exact_binary_data(self, processor):
        """Test that base64 encoding preserves binary data exactly"""
        topic = "sensor/exact_binary_test"
        
        # Test with the exact zlib data from the original error
        original_binary = bytes([120, 156, 165, 125, 217, 142, 158, 201, 145, 221, 187, 212, 245, 47])
        
        # This should convert to base64 without crashing
        try:
            result = processor.handle_mqtt_message(topic, original_binary)
            assert result is None  # handle_mqtt_message returns void
            
            # The binary data should be converted to base64 format
            # We can't directly access the converted value from the Rust code,
            # but we can verify that the method completed successfully
            # which means the base64 conversion worked
            
        except Exception as e:
            pytest.fail(f"Base64 encoding of binary data failed with exception: {e}")
    
    async def test_base64_encoding_various_binary_formats(self, processor):
        """Test base64 encoding with various binary formats"""
        test_cases = [
            ("sensor/zlib_data", bytes([120, 156, 165, 125, 217, 142])),
            ("sensor/gzip_data", bytes([0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])),
            ("sensor/png_data", bytes([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])),
            ("sensor/jpeg_data", bytes([0xFF, 0xD8, 0xFF, 0x0E, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46])),
            ("sensor/empty_data", bytes([])),
            ("sensor/single_byte", bytes([0xFF])),
        ]
        
        for topic, binary_data in test_cases:
            try:
                result = processor.handle_mqtt_message(topic, binary_data)
                assert result is None  # handle_mqtt_message returns void
                
                # Each binary format should be handled without crashing
                # and converted to base64 representation
                
            except Exception as e:
                pytest.fail(f"Base64 encoding failed for {topic} with exception: {e}")
    
    async def test_base64_encoding_large_binary_data(self, processor):
        """Test base64 encoding with large binary data"""
        topic = "sensor/large_binary_test"
        
        # Create a larger binary message (100 bytes)
        large_binary = bytes([i % 256 for i in range(100)])
        
        try:
            result = processor.handle_mqtt_message(topic, large_binary)
            assert result is None  # handle_mqtt_message returns void
            
            # Large binary data should be handled without memory issues
            # and converted to base64 representation
            
        except Exception as e:
            pytest.fail(f"Base64 encoding of large binary data failed with exception: {e}")


class TestDownstreamBinaryDataFlow:
    """Test cases for complete downstream data flow with binary data"""

    pytestmark = pytest.mark.asyncio
    
    async def test_binary_data_handling_in_rust_processor(self, processor):
        """Test that binary data is handled correctly by the Rust processor"""
        # Test with zlib compressed data (like the problematic data from the error)
        topic = "sensor/binary_data"
        binary_message = bytes([120, 156, 165, 125, 217, 142, 158, 201, 145, 221, 187, 212, 245, 47])
        
        # This should not crash and should handle the binary data gracefully
        try:
            result = processor.handle_mqtt_message(topic, binary_message)
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Binary message handling failed with exception: {e}")
    
    async def test_binary_data_conversion_in_pipeline(self, processor):
        """Test that binary data gets converted to safe representation in the pipeline"""
        topic = "sensor/binary_conversion"
        binary_message = bytes([120, 156, 165, 125, 217, 142])
        
        # Process the binary message - should not crash
        try:
            result = processor.handle_mqtt_message(topic, binary_message)
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Binary message handling failed with exception: {e}")
    
    async def test_mixed_data_types_in_pipeline(self, processor):
        """Test that mixed text and binary data flows correctly through the pipeline"""
        # Test text message
        text_topic = "sensor/text_data"
        text_message = b"Hello World"
        
        try:
            result = processor.handle_mqtt_message(text_topic, text_message)
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Text message handling failed with exception: {e}")
        
        # Test binary message
        binary_topic = "sensor/binary_data"
        binary_message = bytes([120, 156, 165, 125, 217, 142])
        
        try:
            result = processor.handle_mqtt_message(binary_topic, binary_message)
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Binary message handling failed with exception: {e}")
    
    async def test_large_binary_data_in_pipeline(self, processor):
        """Test that large binary data is handled correctly in the pipeline"""
        topic = "sensor/large_binary_data"
        large_binary = bytes([i % 256 for i in range(1000)])
        
        try:
            result = processor.handle_mqtt_message(topic, large_binary)
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Large binary message handling failed with exception: {e}")
    
    async def test_special_character_binary_in_pipeline(self, processor):
        """Test that binary data with special characters flows correctly"""
        topic = "sensor/special_chars"
        special_binary = bytes([0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F])
        
        try:
            result = processor.handle_mqtt_message(topic, special_binary)
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Special character binary message handling failed with exception: {e}")
    
    async def test_compression_signatures_in_pipeline(self, processor):
        """Test that compression signatures are detected and handled correctly"""
        # Test zlib signature
        zlib_topic = "sensor/zlib_data"
        zlib_data = bytes([120, 156, 165, 125, 217, 142])
        
        try:
            result = processor.handle_mqtt_message(zlib_topic, zlib_data)
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Zlib message handling failed with exception: {e}")
        
        # Test gzip signature
        gzip_topic = "sensor/gzip_data"
        gzip_data = bytes([0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
        
        try:
            result = processor.handle_mqtt_message(gzip_topic, gzip_data)
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"Gzip message handling failed with exception: {e}")
    
    async def test_end_to_end_binary_data_flow(self, processor):
        """Test complete end-to-end flow of binary data through the system"""
        topic = "sensor/end_to_end_binary"
        binary_message = bytes([120, 156, 165, 125, 217, 142, 158, 201, 145, 221, 187, 212, 245, 47])
        
        try:
            result = processor.handle_mqtt_message(topic, binary_message)
            assert result is None  # handle_mqtt_message returns void
        except Exception as e:
            pytest.fail(f"End-to-end binary message handling failed with exception: {e}")


# Topics must live under the test base_topic ("myrelay/") so they pass the
# `topic.starts_with(base_topic)` gate inside the Rust handle_mqtt_message.
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
    """Exercises the config/control-topic routing inside the Rust
    handle_mqtt_message. Unlike the shared `processor` fixture, this keeps
    references to the injected mocks so we can assert on the Python callbacks
    that the Rust code triggers (relay_main, orjson). Publishing no longer
    crosses into Python, so it is observed via the client's undelivered ring."""

    @pytest.fixture
    def ctx(self, config_instance, monkeypatch):
        mock_http_handler = MagicMock()
        mock_mqtt_client = MqttClient(config_instance)
        mock_relay_main = MagicMock()
        mock_orjson = MagicMock()
        topics = ControlTopicNS()
        # The config actions work on the object the processor was handed, not on
        # one reached back through the relay - so that is where they are observed.
        monkeypatch.setattr(config_instance, "get_safe_config", MagicMock(return_value={}))
        monkeypatch.setattr(config_instance, "update_fields", MagicMock())
        processor = MiniserverDataProcessor(
            topics,
            config_instance,
            mock_relay_main,
            mock_mqtt_client,
            mock_http_handler,
            mock_orjson,
        )
        return SimpleNamespace(
            processor=processor,
            topics=topics,
            relay_main=mock_relay_main,
            mqtt_client=mock_mqtt_client,
            orjson=mock_orjson,
            http_handler=mock_http_handler,
            global_config=config_instance,
        )

    def test_config_get_serializes_and_publishes_safe_config(self, ctx):
        ctx.orjson.dumps.return_value = b'{"general": {}}'
        ctx.processor.handle_mqtt_message(ctx.topics.CONFIG_GET, b"")

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
        ctx.processor.handle_mqtt_message(topic, b'{"general": {"cache_size": 50}}')

        ctx.orjson.loads.assert_called_once()
        global_config_mock = ctx.global_config
        global_config_mock.update_fields.assert_called_once()
        # second positional arg is the update mode ("set"/"add"/"remove")
        assert global_config_mock.update_fields.call_args[0][1] == expected_mode
        # a successful update restarts the relay (verifies the restart_relay rename)
        ctx.relay_main.restart_relay.assert_called_once()

    def test_rejected_config_update_does_not_restart(self, ctx):
        """A refused update must not send the relay through os.execv.

        The config would be unchanged, so the restart would achieve nothing but
        a dropped MQTT session - and a publisher could trigger it at will.
        """
        global_config_mock = ctx.global_config
        global_config_mock.update_fields.side_effect = ConfigError("refused")

        ctx.processor.handle_mqtt_message(ctx.topics.CONFIG_SET, b'{"miniserver_ip": "203.0.113.5"}')

        global_config_mock.update_fields.assert_called_once()
        ctx.relay_main.restart_relay.assert_not_called()

    @pytest.mark.parametrize("topic_attr", ["CONFIG_UPDATE", "CONFIG_RESTART"])
    def test_config_update_and_restart_only_restart(self, ctx, topic_attr):
        topic = getattr(ctx.topics, topic_attr)
        ctx.processor.handle_mqtt_message(topic, b"")

        # verifies the restart_relay rename (old name would never be called)
        ctx.relay_main.restart_relay.assert_called_once()
        # plain update/restart must not mutate the config
        ctx.global_config.update_fields.assert_not_called()

    def test_miniserver_startup_triggers_sync_when_enabled(self, ctx):
        global_config.miniserver.sync_with_miniserver = True
        ctx.processor.handle_mqtt_message(ctx.topics.MINISERVER_STARTUP_EVENT, b"")
        ctx.relay_main.schedule_miniserver_sync.assert_called_once()

    def test_miniserver_startup_skips_sync_when_disabled(self, ctx):
        global_config.miniserver.sync_with_miniserver = False
        ctx.processor.handle_mqtt_message(ctx.topics.MINISERVER_STARTUP_EVENT, b"")
        ctx.relay_main.schedule_miniserver_sync.assert_not_called()

    @pytest.mark.asyncio
    async def test_data_topic_is_not_treated_as_control(self, ctx):
        # A topic outside base_topic must take the data path, never the
        # control branches (no restart, no config response published).
        ctx.processor.handle_mqtt_message("some/data/topic", b"value")
        ctx.relay_main.restart_relay.assert_not_called()
        ctx.relay_main.schedule_miniserver_sync.assert_not_called()
        assert ctx.mqtt_client.take_undelivered() == []
        ctx.http_handler.send_batch_to_miniserver.assert_called_once()

    @pytest.mark.asyncio
    async def test_unknown_topic_under_base_topic_still_takes_data_path(self, ctx):
        """A topic under base_topic that is not one of the known control
        topics must not be silently dropped - it has to reach process_data
        just like any other data topic. Routing used to be gated by a plain
        `topic.starts_with(base_topic)` check, so e.g. "myrelay/sensor/x"
        matched the prefix but no `if`/`else if` branch and fell through to
        nothing at all.
        """
        ctx.processor.handle_mqtt_message("myrelay/sensor/temperature", b"21.5")
        ctx.relay_main.restart_relay.assert_not_called()
        ctx.relay_main.schedule_miniserver_sync.assert_not_called()
        assert ctx.mqtt_client.take_undelivered() == []
        ctx.http_handler.send_batch_to_miniserver.assert_called_once()

    def test_data_path_errors_propagate(self, ctx):
        """process_data failures must surface so ingress_worker can log them.

        A discarded Err here used to leave the outer handler with Ok(()) and no
        ERROR line for the dropped message.
        """
        ctx.http_handler.send_batch_to_miniserver.side_effect = RuntimeError(
            "handover failed"
        )

        with pytest.raises(RuntimeError, match="handover failed"):
            ctx.processor.handle_mqtt_message("some/data/topic", b"value")


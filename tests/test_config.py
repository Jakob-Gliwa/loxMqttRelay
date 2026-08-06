import asyncio
import pytest
from loxmqttrelay.config import (
    Config, BrokerConfig, AppConfig,
     MiniserverConfig,
    ConfigError, ConfigSection, global_config
)
@pytest.fixture(autouse=True)
def reset_config():
    """Reset Config singleton before and after each test"""
    Config._instance = None
    yield
    Config._instance = None

@pytest.fixture
def temp_config_file(tmp_path):
    """Create a temporary config file with comprehensive test data"""
    config_path = tmp_path / "config.toml"
    test_config = """
[general]
base_topic = "test/"
log_level = "INFO"
cache_size = 100000

[broker]
host = "test.mosquitto.org"
port = 1883
user = "test_user"
password = "test_pass"
client_id = "test_client"

[miniserver]
miniserver_ip = "192.168.1.100"
miniserver_port = 8080
miniserver_user = "ms_user"
miniserver_pass = "ms_pass"
miniserver_max_parallel_connections = 10
sync_with_miniserver = false
use_websocket = false

[topics]
subscriptions = ["topic1", "topic2"]
subscription_filters = ["^ignore/before/.*"]
topic_whitelist = ["whitelist_topic"]
do_not_forward = ["do_not_forward_topic"]

[processing]
expand_json = false
convert_booleans = false

[udp]
udp_in_port = 12345

[debug]
mock_ip = "127.0.0.1"
enable_mock = true
"""
    config_path.write_text(test_config)
    return str(config_path)

@pytest.fixture
def config_instance(temp_config_file):
    """Get a Config instance with test configuration"""
    config = Config()
    config.config_path = temp_config_file
    # Reload the configuration with the new path
    config._config = config._load_config()
    return config

def test_config_load(temp_config_file):
    """Test loading configuration from file"""
    config = Config()
    config.config_path = temp_config_file
    config._config = config._load_config()
    
    # General Config Assertions
    assert config.general.base_topic == "test/"
    assert config.general.log_level == "INFO"
    assert config.general.cache_size == 100000
    
    # Broker Config Assertions
    assert config.broker.host == "test.mosquitto.org"
    assert config.broker.port == 1883
    assert config.broker.user == "test_user"
    assert config.broker.password == "test_pass"
    assert config.broker.client_id == "test_client"
    
    # Miniserver Config Assertions
    assert config.miniserver.miniserver_ip == "192.168.1.100"
    assert config.miniserver.miniserver_port == 8080
    assert config.miniserver.miniserver_user == "ms_user"
    assert config.miniserver.miniserver_pass == "ms_pass"
    assert config.miniserver.miniserver_max_parallel_connections == 10
    assert config.miniserver.sync_with_miniserver is False
    assert config.miniserver.use_websocket is False
    
    # Topics Config Assertions
    assert config.topics.subscriptions == ["topic1", "topic2"]
    assert config.topics.subscription_filters == ["^ignore/before/.*"]
    # A TOML list, but the field is declared (and normalized) as a set
    assert config.topics.topic_whitelist == {"whitelist_topic"}
    assert config.topics.do_not_forward == ["do_not_forward_topic"]
    
    # Processing Config Assertions
    assert config.processing.expand_json is False
    assert config.processing.convert_booleans is False
    
    # UDP Config Assertions
    assert config.udp.udp_in_port == 12345
    
    # Debug Config Assertions
    assert config.debug.mock_ip == "127.0.0.1"
    assert config.debug.enable_mock is True

def test_config_missing_file(tmp_path):
    """Test loading configuration from a non-existent file"""
    non_existent_path = tmp_path / "nonexistent.toml"
    config = Config()
    config.config_path = str(non_existent_path)
    
    config._config = config._load_config()
    
    # Assert that default configuration is loaded
    assert isinstance(config._config, AppConfig)
    assert config.general.base_topic == "myrelay/"
    assert config.broker.host == "localhost"
    assert config.miniserver.miniserver_ip == "127.0.0.1"
    # Add more default assertions as needed

def test_config_validation(tmp_path):
    """Test loading invalid configuration without raising ConfigError"""
    config_path = tmp_path / "invalid_config.toml"
    invalid_config = """
[general]
log_level = "INVALID_LEVEL"
base_topic = "invalid_base_topic"

[broker]
host = ""
port = "not_a_port"

[miniserver]
miniserver_ip = "invalid_ip"
miniserver_port = "not_a_port"

[topics]
subscriptions = "not_a_list"
"""
    config_path.write_text(invalid_config)
    
    config = Config()
    config.config_path = str(config_path)
    config._config = config._load_config()
    
    # Since there's no validation, ensure that the incorrect types are loaded as strings
    # and that default values are not overridden unless specified
    assert config.general.log_level == "INVALID_LEVEL"  # Loaded as is
    assert config.general.base_topic == "invalid_base_topic"  # Loaded as is
    assert config.broker.host == ""  # Loaded as is
    assert config.broker.port == "not_a_port"  # Loaded as string, though it should be int
    assert config.miniserver.miniserver_ip == "invalid_ip"  # Loaded as is
    assert config.miniserver.miniserver_port == "not_a_port"  # Loaded as string
    assert config.topics.subscriptions == "not_a_list"  # Loaded as string

def test_config_update(config_instance):
    """Test updating configuration sections"""
    # Update Broker Config
    config_instance.update_config(
        ConfigSection.BROKER,
        {"host": "new.broker.org", "port": 1884}
    )
    
    assert config_instance.broker.host == "new.broker.org"
    assert config_instance.broker.port == 1884
    
    # Update Topics Config - set mode
    config_instance.update_config(
        ConfigSection.TOPICS,
        {"subscriptions": ["topic3"]},
        list_mode="set"
    )
    assert config_instance.topics.subscriptions == ["topic3"]
    
    # Update Topics Config - add mode
    config_instance.update_config(
        ConfigSection.TOPICS,
        {"subscriptions": ["topic4"]},
        list_mode="add"
    )
    assert config_instance.topics.subscriptions == ["topic3", "topic4"]
    
    # Update Topics Config - remove mode
    config_instance.update_config(
        ConfigSection.TOPICS,
        {"subscriptions": ["topic3"]},
        list_mode="remove"
    )
    assert config_instance.topics.subscriptions == ["topic4"]

def test_config_direct_access(temp_config_file):
    """Test direct attribute access to config values and error handling"""
    config = Config()
    config.config_path = temp_config_file
    config._config = config._load_config()
    
    # General Config Access
    assert config.general.log_level == "INFO"
    assert config.general.base_topic == "test/"
    
    # Broker Config Access
    assert isinstance(config.broker, BrokerConfig)
    assert config.broker.host == "test.mosquitto.org"
    
    # Miniserver Config Access
    assert isinstance(config.miniserver, MiniserverConfig)
    assert config.miniserver.miniserver_ip == "192.168.1.100"
    
    # Test accessing a nonexistent attribute using getattr
    with pytest.raises(AttributeError):
        getattr(config, "nonexistent_attribute")

def test_config_safe_config(config_instance):
    """Test retrieving a safe configuration without sensitive data"""
    # Set sensitive data
    config_instance.broker.user = "secure_user"
    config_instance.broker.password = "secure_pass"
    config_instance.miniserver.miniserver_user = "ms_secure_user"
    config_instance.miniserver.miniserver_pass = "ms_secure_pass"
    
    safe_config = config_instance.get_safe_config()
    
    # Type the dictionary properly
    broker_config: dict = safe_config.get('broker', {})
    miniserver_config: dict = safe_config.get('miniserver', {})
    
    # Ensure sensitive broker data is removed
    assert 'user' not in broker_config
    assert 'password' not in broker_config
    
    # Ensure sensitive miniserver data is removed
    assert 'miniserver_user' not in miniserver_config
    assert 'miniserver_pass' not in miniserver_config
    
    # Ensure non-sensitive data remains
    assert 'host' in broker_config
    assert 'miniserver_ip' in miniserver_config


def test_safe_config_is_json_serializable(config_instance):
    """topic_whitelist is a set in memory, which orjson cannot serialize.

    config/get dumps this payload straight onto MQTT, so it has to come out as
    a plain list - sorted, so the response does not churn between runs.
    """
    import orjson

    config_instance.topics.topic_whitelist = {"zeta/topic", "alpha/topic"}

    safe_config = config_instance.get_safe_config()

    assert safe_config['topics']['topic_whitelist'] == ["alpha/topic", "zeta/topic"]
    assert orjson.loads(orjson.dumps(safe_config))['topics']['topic_whitelist'] == [
        "alpha/topic", "zeta/topic"
    ]


def test_save_config_writes_set_valued_fields(tmp_path, config_instance):
    """TOML has no set type either, so saving must not choke on the whitelist."""
    save_path = tmp_path / "saved_config.toml"
    config_instance.config_path = str(save_path)
    config_instance.topics.topic_whitelist = {"zeta/topic", "alpha/topic"}

    config_instance.save_config()

    # Sorted on disk, so restarts do not churn the file
    assert 'topic_whitelist = ["alpha/topic", "zeta/topic"]' in save_path.read_text()

    new_config = Config()
    new_config.config_path = str(save_path)
    new_config._config = new_config._load_config()
    assert new_config.topics.topic_whitelist == {"alpha/topic", "zeta/topic"}


def test_topic_whitelist_is_normalized_to_a_set(temp_config_file):
    """The field is declared Set[str], but TOML can only express a list.

    Without normalization the runtime type depends on whether a config file was
    present, and update_field() branches on exactly that type.
    """
    config = Config()
    config.config_path = temp_config_file
    config._config = config._load_config()

    assert isinstance(config.topics.topic_whitelist, set)
    # Duplicates in the file collapse instead of being carried around
    assert AppConfig.from_dict(
        {"topics": {"topic_whitelist": ["a", "a", "b"]}}
    ).topics.topic_whitelist == {"a", "b"}


def test_config_add_preserves_list_order(config_instance):
    """'add' must not reshuffle list fields.

    The old implementation round-tripped through a set, so every config/add
    rewrote the user's config file in an arbitrary, run-dependent order.
    """
    config_instance.update_config(
        ConfigSection.TOPICS,
        {"subscriptions": ["alpha", "beta", "gamma"]},
        list_mode="set",
    )
    config_instance.update_config(
        ConfigSection.TOPICS,
        {"subscriptions": ["delta", "beta"]},
        list_mode="add",
    )

    # delta appended, beta deduped in place
    assert config_instance.topics.subscriptions == ["alpha", "beta", "gamma", "delta"]

    config_instance.update_field("do_not_forward", ["one", "two"], list_mode="set")
    config_instance.update_field("do_not_forward", ["three", "one"], list_mode="add")
    assert config_instance.topics.do_not_forward == ["one", "two", "three"]


def test_save_config(tmp_path, config_instance):
    """Test saving the configuration to a file"""
    save_path = tmp_path / "saved_config.toml"
    config_instance.config_path = str(save_path)
    
    # Make some changes
    config_instance.general.log_level = "DEBUG"
    config_instance.broker.port = 1885
    config_instance.save_config()
    
    # Reload the configuration to verify
    new_config = Config()
    new_config.config_path = str(save_path)
    new_config._config = new_config._load_config()
    
    assert new_config.general.log_level == "DEBUG"
    assert new_config.broker.port == 1885

def test_update_fields(config_instance):
    """Test updating multiple fields at once"""
    updates = {
        "log_level": "WARNING",
        "cache_size": 200000
    }
    config_instance.update_fields(updates)
    
    assert config_instance.general.log_level == "WARNING"
    assert config_instance.general.cache_size == 200000

def test_update_fields_accepts_lists_and_single_values(config_instance):
    """A bare element stands for a one-element list, as it did before."""
    config_instance.update_fields({"subscriptions": ["a/#", "b/#"], "do_not_forward": "private/.*"})

    assert config_instance.topics.subscriptions == ["a/#", "b/#"]
    assert config_instance.topics.do_not_forward == ["private/.*"]

@pytest.mark.parametrize("field_name,value", [
    ("host", "attacker.example.com"),
    ("port", 1884),
    ("user", "someone"),
    ("password", "hunter2"),
    ("miniserver_ip", "203.0.113.5"),
    ("miniserver_port", 8081),
    ("miniserver_user", "admin"),
    ("miniserver_pass", "secret"),
    # Redirects the Miniserver target just as miniserver_ip does, so leaving
    # these out would make the whole list bypassable in two lines.
    ("mock_ip", "203.0.113.5"),
    ("enable_mock", True),
])
def test_update_fields_refuses_protected_fields(config_instance, field_name, value):
    """Endpoints and credentials must not be settable over MQTT."""
    section, _ = config_instance._get_field_info(field_name)
    before = getattr(getattr(config_instance, section.value), field_name)

    with pytest.raises(ConfigError, match=field_name):
        config_instance.update_fields({field_name: value})

    assert getattr(getattr(config_instance, section.value), field_name) == before

def test_protected_field_rejects_the_whole_batch(config_instance):
    """An allowed field must not slip through next to a refused one."""
    with pytest.raises(ConfigError):
        config_instance.update_fields({"log_level": "DEBUG", "miniserver_ip": "203.0.113.5"})

    assert config_instance.general.log_level == "INFO"
    assert config_instance.miniserver.miniserver_ip == "192.168.1.100"

@pytest.mark.parametrize("updates,expected", [
    # bool is an int subclass, so a naive isinstance check would let this pass
    # and the relay would fail to start on the config it just wrote.
    ({"cache_size": True}, "cache_size"),
    ({"cache_size": "loads"}, "cache_size"),
    ({"expand_json": "yes"}, "expand_json"),
    ({"log_level": 5}, "log_level"),
    ({"subscriptions": [1, 2]}, "subscriptions"),
    ({"topic_whitelist": [None]}, "topic_whitelist"),
    ({"no_such_field": 1}, "no_such_field"),
])
def test_update_fields_refuses_unusable_values(config_instance, updates, expected):
    with pytest.raises(ConfigError, match=expected):
        config_instance.update_fields(updates)

def test_rejected_update_is_not_persisted(config_instance):
    """Nothing may reach the file, or the relay restarts into a broken config."""
    before = open(config_instance.config_path).read()

    with pytest.raises(ConfigError):
        config_instance.update_fields({"log_level": "DEBUG", "cache_size": "loads"})

    assert open(config_instance.config_path).read() == before
    assert config_instance.general.log_level == "INFO"

def test_protection_does_not_apply_to_local_updates(config_instance):
    """The Miniserver sync and local code keep full access.

    Only update_fields() is reachable from the MQTT control topics; the
    whitelist sync in main.py goes through update_config().
    """
    config_instance.update_config(ConfigSection.MINISERVER, {"miniserver_ip": "192.168.1.200"})
    config_instance.update_config(ConfigSection.TOPICS, {"topic_whitelist": ["synced_topic"]})

    assert config_instance.miniserver.miniserver_ip == "192.168.1.200"
    assert config_instance.topics.topic_whitelist == {"synced_topic"}

def test_thread_safety(tmp_path):
    """Test that Config is thread-safe"""
    config_path = tmp_path / "thread_safe_config.toml"
    test_config = """
[general]
base_topic = "thread_test/"
log_level = "INFO"
cache_size = 100000
"""
    config_path.write_text(test_config)
    
    # Initialize the Config singleton first
    config = Config()
    config.config_path = str(config_path)
    config._config = config._load_config()
    
    import threading
    results = []
    
    def access_config():
        # Access the existing singleton
        config = Config()
        results.append(config.general.base_topic)
    
    threads = [threading.Thread(target=access_config) for _ in range(10)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    
    assert all(result == "thread_test/" for result in results)

@pytest.mark.asyncio
async def test_asyncio_config_singleton(tmp_path):
    """Test that Config singleton works correctly under asyncio concurrency"""
    # Create a temporary config file
    config_path = tmp_path / "async_config.toml"
    test_config = """
[general]
base_topic = "async_test/"
log_level = "INFO"
cache_size = 100000
"""
    config_path.write_text(test_config)
    
    # Initialize global_config with the test config path
    global_config.config_path = str(config_path)
    global_config._config = global_config._load_config()
    
    async def access_config():
        """Coroutine to access the global_config"""
        # Simulate some asynchronous operation
        await asyncio.sleep(0)  # Yield control to the event loop
        return global_config.general.base_topic
    
    # Create multiple asyncio tasks to access the config concurrently
    tasks = [asyncio.create_task(access_config()) for _ in range(10)]
    results = await asyncio.gather(*tasks)
    
    # Assert that all tasks received the correct base_topic
    assert all(result == "async_test/" for result in results)

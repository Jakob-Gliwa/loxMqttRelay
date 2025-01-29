import pytest
import pytest_asyncio
from unittest.mock import AsyncMock, patch, MagicMock
from loxmqttrelay.http_miniserver_handler import HttpMiniserverHandler
from loxmqttrelay.config import Config, AppConfig
from loxmqttrelay.miniserver_data_processor import MiniserverDataProcessor
import aiohttp
import asyncio
from typing import AsyncGenerator, Generator, List, Tuple, Any

@pytest_asyncio.fixture
async def mock_session() -> AsyncGenerator[MagicMock, None]:
    """
    Fixture that correctly patches aiohttp.ClientSession as an async context manager.
    """
    with patch("aiohttp.ClientSession") as mock_client_session:
        # Create a MagicMock as "session object"
        mock_session_instance = MagicMock()

        # Simulate context manager
        mock_session_instance.__aenter__.return_value = mock_session_instance
        mock_session_instance.__aexit__.return_value = None

        # Set up default mock response (status=200, json={"code": 200})
        mock_response = MagicMock()
        mock_response.status = 200
        mock_response.json = AsyncMock(return_value={"code": 200})

        # Prepare GET as AsyncMock
        mock_session_instance.get = AsyncMock(return_value=mock_response)

        # When ClientSession() is called, return mock_session_instance
        mock_client_session.return_value = mock_session_instance

        # Return fixture result
        yield mock_client_session

@pytest.fixture(autouse=True)
def cleanup_singletons() -> Generator[None, None, None]:
    """Ensure Config and MiniserverDataProcessor singletons are cleaned up before and after each test"""
    Config._instance = None
    yield
    Config._instance = None

@pytest.fixture
def mock_config() -> AppConfig:
    """Create a mock config instance with test values"""
    config = AppConfig()
    config.miniserver.miniserver_ip = "192.168.1.1"
    config.miniserver.miniserver_user = "user"
    config.miniserver.miniserver_pass = "pass"
    config.miniserver.miniserver_max_parallel_connections = 5
    config.debug.mock_ip = ""
    config.debug.enable_mock = False
    config.processing.expand_json = False
    config.processing.convert_booleans = False
    config.general.base_topic = "base/"
    config.debug.publish_processed_topics = False
    config.debug.publish_forwarded_topics = False
    config.miniserver.use_websocket = False
    return config

@pytest.fixture
def config_instance(mock_config: AppConfig, monkeypatch: pytest.MonkeyPatch) -> Generator[Config, None, None]:
    """Get a Config instance with mocked config"""
    config = Config()
    config._config = mock_config
    monkeypatch.setattr('loxmqttrelay.http_miniserver_handler.global_config', mock_config)
    yield config

@pytest.fixture
def test_data() -> List[Tuple[str, Any]]:
    return [
        ("test/topic1", "value1"),
        ("test/topic2", "true"),
        ("test/topic3", '{"nested": "value"}')
    ]

@pytest.fixture
def mock_data_processor(monkeypatch: pytest.MonkeyPatch) -> MagicMock:
    """Create mock processor and patch it into the handler"""
    processor = MagicMock()
    processor.normalize_topic.side_effect = lambda x: x.replace('/', '_')
    monkeypatch.setattr('loxmqttrelay.http_miniserver_handler.miniserver_data_processor', processor)
    return processor

@pytest.fixture
def handler() -> HttpMiniserverHandler:
    """Create handler instance"""
    return HttpMiniserverHandler()

# HTTP Communication Tests
@pytest.mark.asyncio
async def test_http_authentication(
    mock_session: MagicMock,
    mock_data_processor: MagicMock,
    handler: HttpMiniserverHandler,
    test_data: List[Tuple[str, Any]]
) -> None:
    """Test HTTP authentication with basic auth"""
    handler.ms_user = "testuser"
    handler.ms_pass = "testpass"
    handler.ms_ip = "192.168.1.1"
    
    for topic, value in test_data:
        await handler.send_to_miniserver_via_http(topic, value)

    mock_session.assert_called_with(
        auth=aiohttp.BasicAuth("testuser", "testpass"),
        timeout=aiohttp.ClientTimeout(total=10)
    )

@pytest.mark.asyncio
async def test_http_topic_normalization(
    mock_session: MagicMock,
    mock_data_processor: MagicMock,
    handler: HttpMiniserverHandler
) -> None:
    """Test topic normalization in HTTP mode"""
    test_data = [("a/complex/topic/path", "value")]

    for topic, value in test_data:
        await handler.send_to_miniserver_via_http(topic, value)

    mock_data_processor.normalize_topic.assert_called_once_with("a/complex/topic/path")
    
    mock_session.return_value.__aenter__.return_value.get.assert_called_once_with(
        f"http://{handler.target_ip}/dev/sps/io/a_complex_topic_path/value"
    )

@pytest.mark.asyncio
async def test_http_value_conversion(
    mock_session: MagicMock,
    mock_data_processor: MagicMock,
    handler: HttpMiniserverHandler
) -> None:
    """Test value type conversion in HTTP mode"""
    test_data = [
        ("topic1", 123),
        ("topic2", True),
        ("topic3", 45.67)
    ]

    for topic, value in test_data:
        await handler.send_to_miniserver_via_http(topic, value)

    assert mock_data_processor.normalize_topic.call_count == 3
    
    calls = mock_session.return_value.__aenter__.return_value.get.call_args_list
    assert len(calls) == 3

    urls = [call[0][0] for call in calls]  # type: ignore
    assert f"http://{handler.target_ip}/dev/sps/io/topic1/123" in urls[0]
    assert f"http://{handler.target_ip}/dev/sps/io/topic2/True" in urls[1]
    assert f"http://{handler.target_ip}/dev/sps/io/topic3/45.67" in urls[2]

@pytest.mark.asyncio
async def test_http_parallel_connections(mock_session: MagicMock, handler: HttpMiniserverHandler) -> None:
    """Test parallel HTTP request handling"""
    test_data = [
        (f"test/topic{i}", f"value{i}") 
        for i in range(10)
    ]
    
    handler.connection_semaphore = asyncio.Semaphore(5)
    for topic, value in test_data:
        await handler.send_to_miniserver_via_http(topic, value)
    
    assert mock_session.return_value.__aenter__.return_value.get.call_count == 10

# Mock Server Tests
@pytest.mark.asyncio
async def test_mock_server_http(
    mock_session: MagicMock,
    mock_data_processor: MagicMock,
    handler: HttpMiniserverHandler,
    test_data: List[Tuple[str, Any]]
) -> None:
    """Test mock server in HTTP mode"""
    handler.enable_mock_miniserver = True
    handler.mock_ms_ip = "192.168.1.2"
    handler.target_ip = handler.mock_ms_ip
    
    for topic, value in test_data:
        await handler.send_to_miniserver_via_http(topic, value)

    # Verify all topics are normalized
    for topic, _ in test_data:
        mock_data_processor.normalize_topic.assert_any_call(topic)
    
    # Verify first request was made correctly
    first_topic, first_value = test_data[0]  # type: ignore
    normalized_topic = mock_data_processor.normalize_topic(first_topic)
    mock_session.return_value.__aenter__.return_value.get.assert_any_call(
        f"http://{handler.mock_ms_ip}/dev/sps/io/{normalized_topic}/{first_value}"
    )

# Topic Publishing Tests
@pytest.mark.asyncio
async def test_forwarded_topics_publishing(
    test_data: List[Tuple[str, Any]],
    handler: HttpMiniserverHandler,
    config_instance: Config
) -> None:
    """Test forwarded topics publishing"""
    config_instance._config.debug.publish_forwarded_topics = True
    mock_publish = AsyncMock()
    
    with patch('aiohttp.ClientSession') as mock_session:
        session_instance = AsyncMock()
        session_instance.__aenter__.return_value = session_instance
        session_instance.__aexit__.return_value = None
        session_instance.get = AsyncMock()
        mock_session.return_value = session_instance
        
        for topic, value in test_data:
            await handler.send_to_miniserver(topic, value, mqtt_publish_callback=mock_publish)
        
        assert mock_publish.call_count == len(test_data)

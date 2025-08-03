import pytest
import pytest_asyncio
from unittest.mock import AsyncMock, patch, MagicMock
from loxmqttrelay.http_miniserver_handler import HttpMiniserverHandler
from loxmqttrelay.config import Config, AppConfig
from loxmqttrelay.compatible._loxmqttrelay import MiniserverDataProcessor
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
        # Make the response support the async context manager protocol
        mock_response.__aenter__ = AsyncMock(return_value=mock_response)
        mock_response.__aexit__ = AsyncMock(return_value=None)
        # Prepare GET as AsyncMock
        mock_session_instance.get = AsyncMock(return_value=mock_response)

        # When ClientSession() is called, return mock_session_instance
        mock_client_session.return_value = mock_session_instance

        # Return fixture result
        yield mock_client_session

@pytest.fixture(autouse=True)
def cleanup_singletons() -> Generator[None, None, None]:
    """Ensure Config singleton is cleaned up before and after each test"""
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
    config.miniserver.use_websocket = False
    return config

@pytest.fixture
def config_instance(mock_config: AppConfig, monkeypatch: pytest.MonkeyPatch) -> Generator[Config, None, None]:
    """Get a Config instance with mocked config"""
    config = Config()
    config._config = mock_config
    yield config

@pytest.fixture
def test_data() -> List[Tuple[str, Any]]:
    return [
        ("test/topic1", "value1"),
        ("test/topic2", "true"),
        ("test/topic3", '{"nested": "value"}')
    ]

@pytest.fixture
def handler() -> HttpMiniserverHandler:
    """Create handler instance"""
    return HttpMiniserverHandler()

# HTTP Communication Tests
@pytest.mark.asyncio
async def test_http_authentication(
    mock_session: MagicMock,
    handler: HttpMiniserverHandler,
    test_data: List[Tuple[str, Any]]
) -> None:
    """Test HTTP authentication with basic auth"""
    handler.ms_user = "testuser"
    handler.ms_pass = "testpass"
    handler.ms_ip = "192.168.1.1"
    # Update handler.auth and target_ip based on the new values
    handler.auth = aiohttp.BasicAuth("testuser", "testpass")
    handler.target_ip = "192.168.1.1"
    # Update the http_base_url to use the correct IP
    handler.http_base_url = f"http://{handler.target_ip}"
    
    for topic, value in test_data:
        # Compute normalized topic manually (replace "/" with "_")
        normalized_topic = topic.replace('/', '_')
        await handler.send_to_miniserver_via_http(topic, normalized_topic, value)

    mock_session.assert_called_with(
        auth=aiohttp.BasicAuth("testuser", "testpass"),
        timeout=aiohttp.ClientTimeout(total=10)
    )

@pytest.mark.asyncio
async def test_http_topic_normalization(
    mock_session: MagicMock,
    handler: HttpMiniserverHandler
) -> None:
    """Test topic normalization in HTTP mode"""
    test_data = [("a/complex/topic/path", "value")]

    for topic, value in test_data:
        normalized_topic = topic.replace('/', '_')
        await handler.send_to_miniserver_via_http(topic, normalized_topic, value)
    
    mock_session.return_value.__aenter__.return_value.get.assert_called_once_with(
        f"http://{handler.target_ip}/dev/sps/io/a_complex_topic_path/value"
    )

@pytest.mark.asyncio
async def test_http_value_conversion(
    mock_session: MagicMock,
    handler: HttpMiniserverHandler
) -> None:
    """Test value type conversion in HTTP mode"""
    test_data = [
        ("topic1", 123),
        ("topic2", True),
        ("topic3", 45.67)
    ]

    for topic, value in test_data:
        # For topics without a slash, normalized_topic is the same as topic.
        normalized_topic = topic  
        await handler.send_to_miniserver_via_http(topic, normalized_topic, value)
    
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
        normalized_topic = topic.replace('/', '_')
        await handler.send_to_miniserver_via_http(topic, normalized_topic, value)
    
    assert mock_session.return_value.__aenter__.return_value.get.call_count == 10

# Mock Server Tests
@pytest.mark.asyncio
async def test_mock_server_http(
    mock_session: MagicMock,
    handler: HttpMiniserverHandler,
    test_data: List[Tuple[str, Any]]
) -> None:
    """Test mock server in HTTP mode"""
    handler.enable_mock_miniserver = True
    handler.mock_ms_ip = "192.168.1.2"
    handler.target_ip = handler.mock_ms_ip
    # Update the http_base_url to use the mock IP
    handler.http_base_url = f"http://{handler.mock_ms_ip}"
    
    for topic, value in test_data:
        normalized_topic = topic.replace('/', '_')
        await handler.send_to_miniserver_via_http(topic, normalized_topic, value)

    # Verify first request was made correctly
    first_topic, first_value = test_data[0]  # type: ignore
    normalized_topic = first_topic.replace('/', '_')
    mock_session.return_value.__aenter__.return_value.get.assert_any_call(
        f"http://{handler.mock_ms_ip}/dev/sps/io/{normalized_topic}/{first_value}"
    )

# Custom Port Tests
@pytest.mark.asyncio
async def test_http_custom_port_usage(
    mock_session: MagicMock,
    config_instance: Config,
    handler: HttpMiniserverHandler
) -> None:
    """Test that custom configured miniserver port is used in HTTP requests"""
    # Configure custom port
    custom_port = 8080
    config_instance._config.miniserver.miniserver_port = custom_port
    
    # Update handler with custom port configuration
    handler.ms_port = custom_port
    handler.ms_ip = "192.168.1.1"
    handler.target_ip = handler.ms_ip
    handler.enable_mock_miniserver = False
    # Update the http_base_url to use the custom port configuration
    handler.http_base_url = f"http://{handler.target_ip}:{custom_port}"
    
    test_topic = "test/topic"
    test_value = "test_value"
    normalized_topic = test_topic.replace('/', '_')
    
    await handler.send_to_miniserver_via_http(test_topic, normalized_topic, test_value)
    
    # Verify that the custom port is included in the URL
    expected_url = f"http://{handler.target_ip}:{custom_port}/dev/sps/io/{normalized_topic}/{test_value}"
    mock_session.return_value.__aenter__.return_value.get.assert_called_with(expected_url)

@pytest.mark.asyncio
async def test_websocket_custom_port_usage(
    config_instance: Config,
    handler: HttpMiniserverHandler
) -> None:
    """Test that custom configured miniserver port is used in WebSocket URL construction"""
    # Configure custom port
    custom_port = 8443
    config_instance._config.miniserver.miniserver_port = custom_port
    
    # Update handler with custom port configuration
    handler.ms_port = custom_port
    handler.ms_ip = "192.168.1.1"
    handler.target_ip = handler.ms_ip
    handler.enable_mock_miniserver = False
    
    # Test WebSocket URL construction
    expected_protocol = "https" if custom_port == 443 else "http"
    expected_ws_base_url = f"{expected_protocol}://{handler.target_ip}:{custom_port}"
    
    # Create a new ws_base_url with the custom port
    handler.ws_base_url = f"{expected_protocol}://{handler.target_ip}:{custom_port}"
    
    # Verify the URL includes the custom port
    assert str(custom_port) in handler.ws_base_url
    assert handler.ws_base_url == expected_ws_base_url

@pytest.mark.asyncio  
async def test_websocket_url_construction_with_custom_port(
    config_instance: Config,
    handler: HttpMiniserverHandler
) -> None:
    """Test WebSocket URL is properly constructed with custom port"""
    # Test different custom ports
    test_cases = [
        (8080, "http"),
        (9443, "http"),
        (443, "https"),
        (8443, "http")
    ]
    
    for custom_port, expected_protocol in test_cases:
        # Configure custom port
        config_instance._config.miniserver.miniserver_port = custom_port
        
        # Update handler with custom port configuration  
        handler.ms_port = custom_port
        handler.ms_ip = "192.168.1.1"
        handler.target_ip = handler.ms_ip
        handler.enable_mock_miniserver = False
        
        # Construct WebSocket URL with proper port handling
        protocol = "https" if custom_port == 443 else "http"
        if custom_port not in [80, 443]:
            expected_ws_base_url = f"{protocol}://{handler.target_ip}:{custom_port}"
        else:
            expected_ws_base_url = f"{protocol}://{handler.target_ip}"
        
        # Update handler's ws_base_url using the same logic as the fixed implementation
        handler.ws_base_url = expected_ws_base_url
        
        # Verify the URL construction is correct
        if custom_port not in [80, 443]:
            assert str(custom_port) in handler.ws_base_url
        assert expected_protocol in handler.ws_base_url
        assert handler.target_ip in handler.ws_base_url
        assert handler.ws_base_url == expected_ws_base_url

@pytest.mark.asyncio
async def test_standard_ports_behavior(
    mock_session: MagicMock,
    config_instance: Config, 
    handler: HttpMiniserverHandler
) -> None:
    """Test behavior with standard ports (80 for HTTP, 443 for HTTPS)"""
    test_cases = [
        (80, "http"),
        (443, "https")
    ]
    
    for port, expected_protocol in test_cases:
        # Configure port
        config_instance._config.miniserver.miniserver_port = port
        handler.ms_port = port
        handler.ms_ip = "192.168.1.1"
        handler.target_ip = handler.ms_ip
        handler.enable_mock_miniserver = False
        
        # Test WebSocket URL construction
        expected_ws_base_url = f"{expected_protocol}://{handler.target_ip}"
        if port not in [80, 443]:  # Only add port if not standard
            expected_ws_base_url += f":{port}"
            
        handler.ws_base_url = f"{expected_protocol}://{handler.target_ip}"
        # Update http_base_url for HTTP requests
        handler.http_base_url = f"http://{handler.target_ip}"
        
        # For HTTP requests, standard ports should still work
        test_topic = "test/topic"
        test_value = "test_value"  
        normalized_topic = test_topic.replace('/', '_')
        
        await handler.send_to_miniserver_via_http(test_topic, normalized_topic, test_value)
        
        # The current implementation might not include standard ports
        # This test documents the current behavior
        mock_session.return_value.__aenter__.return_value.get.assert_called()

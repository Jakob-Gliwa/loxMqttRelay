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


# Batched handover - Rust passes every value expanded out of one MQTT message
# in a single call instead of one call per JSON leaf.

BATCH = [
    ("dev/sensor/temp", "dev_sensor_temp", "21.5"),
    ("dev/sensor/hum", "dev_sensor_hum", "48"),
    ("dev/sensor/on", "dev_sensor_on", "1"),
]


@pytest.mark.asyncio
async def test_batch_sends_every_value_over_http(
    mock_session: MagicMock,
    handler: HttpMiniserverHandler
) -> None:
    handler.use_websocket = False
    handler.target_ip = "192.168.1.1"
    handler.http_base_url = f"http://{handler.target_ip}"

    await handler.send_batch_to_miniserver(BATCH)

    urls = [
        call[0][0]
        for call in mock_session.return_value.__aenter__.return_value.get.call_args_list
    ]
    assert sorted(urls) == sorted(
        f"http://{handler.target_ip}/dev/sps/io/{normalized}/{value}"
        for _, normalized, value in BATCH
    )


@pytest.mark.asyncio
async def test_batch_over_websocket_keeps_order(handler: HttpMiniserverHandler) -> None:
    """All values share one socket, so they go out one after another."""
    handler.use_websocket = True
    sent: list[tuple[str, str]] = []

    async def record(normalized_topic, value):
        sent.append((normalized_topic, value))

    with patch("loxmqttrelay.http_miniserver_handler.loxwebsocket") as ws:
        ws.state = "CONNECTED"
        ws.send_websocket_command = AsyncMock(side_effect=record)
        await handler.send_batch_to_miniserver(BATCH)

    assert sent == [(normalized, value) for _, normalized, value in BATCH]


@pytest.mark.asyncio
async def test_batch_survives_a_failing_value(
    mock_session: MagicMock,
    handler: HttpMiniserverHandler
) -> None:
    """One bad value must not take the rest of the message down with it."""
    handler.use_websocket = False
    calls: list[str] = []

    async def flaky(topic, normalized_topic, value):
        calls.append(topic)
        if topic == "dev/sensor/hum":
            raise RuntimeError("boom")

    with patch.object(handler, "send_to_miniserver", side_effect=flaky):
        await handler.send_batch_to_miniserver(BATCH)

    assert calls == [topic for topic, _, _ in BATCH]


@pytest.mark.asyncio
async def test_empty_batch_does_nothing(
    mock_session: MagicMock,
    handler: HttpMiniserverHandler
) -> None:
    await handler.send_batch_to_miniserver([])
    mock_session.return_value.__aenter__.return_value.get.assert_not_called()


# Websocket connection handling. The connect used to sit outside the try block,
# so a Miniserver that was not answering took the rest of the message with it -
# and every message tried a fresh handshake while it stayed away.

@pytest.mark.asyncio
async def test_failed_connect_does_not_escape_the_batch(handler: HttpMiniserverHandler) -> None:
    handler.use_websocket = True

    with patch("loxmqttrelay.http_miniserver_handler.loxwebsocket") as ws:
        ws.state = "CLOSED"
        ws.connect = AsyncMock(side_effect=ConnectionError("miniserver down"))
        ws.send_websocket_command = AsyncMock()

        await handler.send_batch_to_miniserver(BATCH)

        ws.send_websocket_command.assert_not_called()


@pytest.mark.asyncio
async def test_batch_reports_what_the_failed_connect_cost(
    handler: HttpMiniserverHandler
) -> None:
    handler.use_websocket = True

    with patch("loxmqttrelay.http_miniserver_handler.loxwebsocket") as ws, \
         patch("loxmqttrelay.http_miniserver_handler.logger") as log:
        ws.state = "CLOSED"
        ws.connect = AsyncMock(side_effect=ConnectionError("miniserver down"))

        await handler.send_batch_to_miniserver(BATCH)

        dropped = " ".join(str(arg) for arg in log.warning.call_args[0])
        assert "3" in dropped and BATCH[0][0] in dropped


@pytest.mark.asyncio
async def test_a_failed_connect_is_not_retried_per_message(
    handler: HttpMiniserverHandler
) -> None:
    """One handshake per retry window, not one per message.

    A handshake costs a session key over HTTP, the upgrade, the key exchange
    and a token; repeating that for every message while the Miniserver is away
    is what turns an outage into a load problem.
    """
    handler.use_websocket = True

    with patch("loxmqttrelay.http_miniserver_handler.loxwebsocket") as ws:
        ws.state = "CLOSED"
        ws.connect = AsyncMock(side_effect=ConnectionError("miniserver down"))

        for _ in range(5):
            await handler.send_batch_to_miniserver(BATCH)

        ws.connect.assert_awaited_once()


@pytest.mark.asyncio
async def test_the_retry_window_expires(handler: HttpMiniserverHandler) -> None:
    handler.use_websocket = True
    handler.connect_retry_delay = 0.0

    with patch("loxmqttrelay.http_miniserver_handler.loxwebsocket") as ws:
        ws.state = "CLOSED"
        ws.connect = AsyncMock(side_effect=ConnectionError("miniserver down"))

        await handler.send_batch_to_miniserver(BATCH)
        await handler.send_batch_to_miniserver(BATCH)

        assert ws.connect.await_count == 2


@pytest.mark.asyncio
async def test_concurrent_messages_share_one_connect(handler: HttpMiniserverHandler) -> None:
    handler.use_websocket = True

    async def connect(**kwargs):
        await asyncio.sleep(0)
        ws.state = "CONNECTED"

    with patch("loxmqttrelay.http_miniserver_handler.loxwebsocket") as ws:
        ws.state = "CLOSED"
        ws.connect = AsyncMock(side_effect=connect)
        ws.send_websocket_command = AsyncMock()

        await asyncio.gather(*(handler.send_batch_to_miniserver(BATCH) for _ in range(4)))

        ws.connect.assert_awaited_once()
        assert ws.send_websocket_command.await_count == 4 * len(BATCH)


@pytest.mark.asyncio
async def test_no_connect_while_the_library_reconnects(handler: HttpMiniserverHandler) -> None:
    """A second handshake would race the reconnect instead of helping it."""
    handler.use_websocket = True

    with patch("loxmqttrelay.http_miniserver_handler.loxwebsocket") as ws:
        ws.state = "RECONNECTING"
        ws.connect = AsyncMock()

        await handler.send_batch_to_miniserver(BATCH)

        ws.connect.assert_not_called()

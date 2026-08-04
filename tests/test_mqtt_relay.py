import pytest
from unittest.mock import AsyncMock, patch, MagicMock
import logging
import json
from loxwebsocket.lox_ws_api import loxwebsocket
from loxmqttrelay.main import MQTTRelay, TOPIC
from loxmqttrelay.config import (
    Config, AppConfig, GeneralConfig,
    TopicsConfig, MiniserverConfig, global_config
)
import asyncio
import typing
from typing import AsyncGenerator, Generator, List

@pytest.fixture(autouse=True)
def cleanup_singletons() -> typing.Generator[None, None, None]:
    """Ensure Config singleton is cleaned up before and after each test"""
    Config._instance = None
    yield
    Config._instance = None

@pytest.fixture
def mock_config() -> typing.Generator[AppConfig, None, None]:
    config = AppConfig()
    config.topics.topic_whitelist = ["initial_topic1", "initial_topic2"]
    config.topics.subscription_filters = ["filter1"]
    config.miniserver.miniserver_ip = "192.168.1.1"
    config.miniserver.miniserver_user = "user"
    config.miniserver.miniserver_pass = "pass"
    config.miniserver.sync_with_miniserver = True
    yield config

@pytest.fixture
def config_instance(mock_config: AppConfig) -> typing.Generator[Config, None, None]:
    """Get a Config instance with mocked config"""
    Config._instance = None  # Reset singleton
    config = Config()
    config._config = mock_config
    global_config._config = mock_config  # Ensure global_config uses the same mock
    yield config
    Config._instance = None  # Reset singleton

@pytest.fixture
def mock_logger() -> typing.Generator[MagicMock, None, None]:
    """Creates a mocked logger."""
    with patch('loxmqttrelay.main.logger', new_callable=MagicMock) as mock_logger:
        yield mock_logger

@pytest.mark.asyncio
async def test_whitelist_loading_sequence(config_instance: Config, mock_logger: MagicMock) -> None:
    """Test: Whitelist wird zuerst aus Config geladen, dann vom Miniserver überschrieben."""
    with patch.object(config_instance, '_load_config', return_value=None):
        relay = MQTTRelay()
        with patch('loxmqttrelay.main.sync_miniserver_whitelist') as mock_sync:
            mock_sync.return_value = ["synced_topic1", "synced_topic2"]
            
            await relay.handle_miniserver_sync()

            # Logger-Infos einsammeln
            info_msgs: list[str] = [args[0] for args, kwargs in mock_logger.info.call_args_list]

            # Test 1: Erfolgreiches Sync-Log?
            assert any("Whitelist updated from miniserver configuration" in m for m in info_msgs)

            # Test 2: Finale Whitelist?
            assert global_config.topics.topic_whitelist == ["synced_topic1", "synced_topic2"]

@pytest.mark.asyncio
async def test_whitelist_loading_with_sync_failure(config_instance: Config, mock_logger: MagicMock) -> None:
    """Test: Bei Sync-Fehler bleibt die ursprüngliche Whitelist."""
    with patch.object(config_instance, '_load_config', return_value=None):
        relay = MQTTRelay()
        with patch('loxmqttrelay.main.sync_miniserver_whitelist') as mock_sync:
            mock_sync.side_effect = Exception("Sync failed")

            await relay.handle_miniserver_sync()

            info_msgs: list[str] = [args[0] for args, kwargs in mock_logger.info.call_args_list]
            error_msgs: list[str] = [args[0] for args, kwargs in mock_logger.error.call_args_list]

            # Ist der Fehler geloggt worden?
            assert any("Failed to sync with miniserver" in m for m in error_msgs)
            # Wurde geloggt, dass die alte Whitelist beibehalten wird?
            assert any("Keeping whitelist from config" in m for m in info_msgs)

            # Whitelist muss unverändert sein
            assert global_config.topics.topic_whitelist == ["initial_topic1", "initial_topic2"]

@pytest.mark.asyncio
async def test_whitelist_loading_with_sync_disabled(config_instance: Config, mock_logger: MagicMock) -> None:
    """Test: Wenn sync_with_miniserver=False, wird überhaupt nicht gesynct."""
    config_instance._config.miniserver.sync_with_miniserver = False

    with patch.object(config_instance, '_load_config', return_value=None):
        relay = MQTTRelay()
        with patch('loxmqttrelay.main.sync_miniserver_whitelist') as mock_sync:
            await relay.handle_miniserver_sync()

            # Sync sollte nicht aufgerufen werden
            mock_sync.assert_not_called()

            # Whitelist bleibt bei den Config-Werten
            assert global_config.topics.topic_whitelist == ["initial_topic1", "initial_topic2"]

@pytest.mark.asyncio
async def test_whitelist_sync_on_miniserver_startup(config_instance: Config, mock_logger: MagicMock) -> None:
    """Test: Bei miniserverevent/startup wird erneut gesynct."""
    with patch.object(config_instance, '_load_config', return_value=None):
        relay = MQTTRelay()
        with patch('loxmqttrelay.main.sync_miniserver_whitelist', return_value=["synced_topic1", "synced_topic2"]) as mock_sync:
            # Erstmalig syncen
            await relay.handle_miniserver_sync()

            # Reset der Mock-Objekte, damit wir den zweiten Sync klar erkennen
            mock_sync.reset_mock()
            mock_logger.reset_mock()

            # Startup-Event per MQTT simulieren - now using the Rust implementation
            # Since handle_mqtt_message is synchronous in Rust, we call it directly
            relay.miniserver_data_processor.handle_mqtt_message(
                TOPIC.MINISERVER_STARTUP_EVENT,
                b""
            )

            # Add a small delay to allow async operations to complete
            await asyncio.sleep(1)

            # Wurde sync erneut aufgerufen?
            mock_sync.assert_called_once()

            # Ist die erwartete Log-Message dabei?
            info_msgs: List[str] = [args[0] for args, kwargs in mock_logger.info.call_args_list]
            assert any("Miniserver startup detected, resyncing whitelist" in m for m in info_msgs)

            # Neue Whitelist sollte wieder "synced_topic1", "synced_topic2" enthalten
            assert global_config.topics.topic_whitelist == ["synced_topic1", "synced_topic2"]

@pytest.mark.asyncio
async def test_whitelist_sync_on_websocket_reconnect(config_instance: Config, mock_logger: MagicMock) -> None:
    """Test: Ein Websocket-Reconnect löst einen erneuten Sync aus."""
    with patch.object(config_instance, '_load_config', return_value=None):
        relay = MQTTRelay()
        with patch('loxmqttrelay.main.sync_miniserver_whitelist', return_value=["reconnect_topic"]) as mock_sync, \
             patch.object(relay, 'connect_and_subscribe_mqtt', new=AsyncMock()), \
             patch('loxmqttrelay.main.start_udp_server', new=AsyncMock()):
            main_task = asyncio.create_task(relay.main())
            try:
                # main() läuft bis zum abschließenden await asyncio.Future()
                await asyncio.sleep(0.1)

                # Reset, damit nur der Reconnect-Sync gezählt wird
                mock_sync.reset_mock()

                await loxwebsocket.send_event(loxwebsocket.EventType.RECONNECTED)
                await asyncio.sleep(0.1)
            finally:
                main_task.cancel()
                # _event_callbacks ist ein Klassenattribut und würde sonst in andere Tests lecken
                loxwebsocket._event_callbacks.pop(relay.handle_miniserver_sync, None)

            mock_sync.assert_called_once()
            assert global_config.topics.topic_whitelist == ["reconnect_topic"]

@pytest.mark.asyncio
async def test_no_sync_on_other_websocket_events(config_instance: Config, mock_logger: MagicMock) -> None:
    """Test: Nur RECONNECTED synct - der erste Connect hat das bereits erledigt."""
    with patch.object(config_instance, '_load_config', return_value=None):
        relay = MQTTRelay()
        with patch('loxmqttrelay.main.sync_miniserver_whitelist', return_value=["reconnect_topic"]) as mock_sync, \
             patch.object(relay, 'connect_and_subscribe_mqtt', new=AsyncMock()), \
             patch('loxmqttrelay.main.start_udp_server', new=AsyncMock()):
            main_task = asyncio.create_task(relay.main())
            try:
                await asyncio.sleep(0.1)
                mock_sync.reset_mock()

                await loxwebsocket.send_event(loxwebsocket.EventType.CONNECTED)
                await loxwebsocket.send_event(loxwebsocket.EventType.CONNECTION_CLOSED)
                await asyncio.sleep(0.1)
            finally:
                main_task.cancel()
                loxwebsocket._event_callbacks.pop(relay.handle_miniserver_sync, None)

            mock_sync.assert_not_called()

import pytest
from unittest.mock import patch, MagicMock
import logging
import json
from loxmqttrelay.main import MQTTRelay, TOPIC
from loxmqttrelay.config import (
    Config, AppConfig, GeneralConfig,
    TopicsConfig, MiniserverConfig, global_config
)
import asyncio
import typing
from typing import AsyncGenerator, Generator, List

@pytest.fixture(autouse=True)
async def cleanup_tasks() -> typing.AsyncGenerator[None, None]:
    """Cleanup any pending tasks after each test"""
    yield
    tasks = [t for t in asyncio.all_tasks() if t is not asyncio.current_task()]
    for task in tasks:
        task.cancel()
    if tasks:
        await asyncio.gather(*tasks, return_exceptions=True)

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
                ""
            )

            # Add a small delay to allow async operations to complete
            await asyncio.sleep(0.1)

            # Wurde sync erneut aufgerufen?
            mock_sync.assert_called_once()

            # Ist die erwartete Log-Message dabei?
            info_msgs: List[str] = [args[0] for args, kwargs in mock_logger.info.call_args_list]
            assert any("Miniserver startup detected, resyncing whitelist" in m for m in info_msgs)

            # Neue Whitelist sollte wieder "synced_topic1", "synced_topic2" enthalten
            assert global_config.topics.topic_whitelist == ["synced_topic1", "synced_topic2"]

"""Der Whitelist-Sync und was ihn auslöst.

Der Sync selbst ist Kaltpfad und bleibt in Python. Was ihn nach einem
Websocket-Reconnect anstößt, liegt inzwischen in Rust
(``src/miniserver.rs``) und wird dort geprüft - hier bleibt der
Startup-Event über MQTT, der weiterhin über die Sprachgrenze läuft.
"""

import asyncio
import typing
from typing import List
from unittest.mock import MagicMock, patch

import pytest

from loxmqttrelay.config import AppConfig, Config, global_config
from loxmqttrelay.main import TOPIC, MQTTRelay


@pytest.fixture(autouse=True)
def cleanup_singletons() -> typing.Generator[None, None, None]:
    """Ensure Config singleton is cleaned up before and after each test"""
    Config._instance = None
    yield
    Config._instance = None

@pytest.fixture
def mock_config() -> typing.Generator[AppConfig, None, None]:
    config = AppConfig()
    config.topics.topic_whitelist = {"initial_topic1", "initial_topic2"}
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
            assert global_config.topics.topic_whitelist == {"synced_topic1", "synced_topic2"}

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
            assert global_config.topics.topic_whitelist == {"initial_topic1", "initial_topic2"}

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
            assert global_config.topics.topic_whitelist == {"initial_topic1", "initial_topic2"}


@pytest.mark.asyncio
async def test_empty_sync_keeps_existing_whitelist(config_instance: Config, mock_logger: MagicMock) -> None:
    """An empty extract must not wipe a working whitelist into the fail-closed gate."""
    with patch.object(config_instance, '_load_config', return_value=None):
        relay = MQTTRelay()
        with patch('loxmqttrelay.main.sync_miniserver_whitelist', return_value=[]):
            await relay.handle_miniserver_sync()

            warn_msgs: list[str] = [args[0] for args, kwargs in mock_logger.warning.call_args_list]
            assert any("no virtual inputs" in m for m in warn_msgs)
            assert global_config.topics.topic_whitelist == {"initial_topic1", "initial_topic2"}
            assert relay.miniserver_data_processor.topic_whitelist == {
                "initial_topic1",
                "initial_topic2",
            }

@pytest.mark.asyncio
async def test_whitelist_sync_on_miniserver_startup(config_instance: Config, mock_logger: MagicMock) -> None:
    """Test: Bei miniserverevent/startup wird erneut gesynct."""
    with patch.object(config_instance, '_load_config', return_value=None):
        relay = MQTTRelay()
        # Das Startup-Event kommt sonst vom Rust-Ingress-Worker, der auf die in
        # main() gemerkte Loop plant. Dieser Test treibt es direkt und muss die
        # Loop deshalb selbst stellen.
        relay._loop = asyncio.get_running_loop()
        with patch('loxmqttrelay.main.sync_miniserver_whitelist', return_value=["synced_topic1", "synced_topic2"]) as mock_sync:
            # Erstmalig syncen
            await relay.handle_miniserver_sync()

            # Reset der Mock-Objekte, damit wir den zweiten Sync klar erkennen
            mock_sync.reset_mock()
            mock_logger.reset_mock()

            # Startup-Event per MQTT simulieren
            assert relay.miniserver_data_processor.handle_control(
                TOPIC.MINISERVER_STARTUP_EVENT, b""
            ) is True

            # Der Sync wird auf die Loop geplant, also einmal durchlaufen lassen.
            await asyncio.sleep(0.1)

            mock_sync.assert_called_once()
            assert global_config.topics.topic_whitelist == {"synced_topic1", "synced_topic2"}

@pytest.mark.asyncio
async def test_configured_do_not_forward_survives_startup_and_sync(config_instance: Config, mock_logger: MagicMock) -> None:
    """Test: do_not_forward aus der Config greift ohne Zutun eines Mutators.

    Der Miniserver-Sync tauscht danach die Whitelist aus - inklusive des
    gesperrten Topics - und darf die Filter trotzdem nicht verlieren.
    """
    config_instance._config.topics.topic_whitelist = set()
    config_instance._config.topics.do_not_forward = [r"^private\/.*"]

    relay = MQTTRelay()
    processor = relay.miniserver_data_processor
    assert processor.get_do_not_forward_patterns() == [r"^private\/.*"]

    with patch('loxmqttrelay.main.sync_miniserver_whitelist', return_value=["private_secret", "public_sensor"]):
        await relay.handle_miniserver_sync()

    assert processor.topic_whitelist == {"private_secret", "public_sensor"}
    assert processor.get_do_not_forward_patterns() == [r"^private\/.*"]

@pytest.mark.asyncio
async def test_invalid_do_not_forward_pattern_fails_startup(config_instance: Config, mock_logger: MagicMock) -> None:
    """Test: Ein kaputtes Regex bricht den Start mit klarer Meldung ab."""
    config_instance._config.topics.do_not_forward = [r"(unclosed"]

    with pytest.raises(ValueError, match="do_not_forward"):
        MQTTRelay()

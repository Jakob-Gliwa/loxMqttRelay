import asyncio
import importlib
from unittest.mock import patch

import pytest

import loxmqttrelay.http_miniserver_handler as handler_module
from loxmqttrelay.config import AppConfig, global_config
from loxmqttrelay.http_miniserver_handler import HttpMiniserverHandler
from tests.harness.mock_miniserver import mock_miniserver


@pytest.fixture
def handler() -> HttpMiniserverHandler:
    """Create handler instance"""
    return HttpMiniserverHandler()


@pytest.fixture
def reload_handler_module():
    """Import the handler again so it picks up the current config.

    The Miniserver address is read once, when the class is defined - the config
    is immutable between restarts - so reimporting is the only way to see the
    URL the relay would really connect to.
    """
    def reload_with(ip: str, port: int):
        global_config._config.miniserver.miniserver_ip = ip
        global_config._config.miniserver.miniserver_port = port
        return importlib.reload(handler_module)

    yield reload_with

    # Undone here rather than by the autouse config fixture: that one restores
    # the config after this teardown, which would leave the module built from
    # the test's address.
    global_config._config = AppConfig()
    importlib.reload(handler_module)


# The websocket URL. A non-standard port has to survive into the URL, and the
# standard ones must not show up in it.

@pytest.mark.parametrize("port,expected", [
    (80, "http://192.168.1.1"),
    (443, "https://192.168.1.1"),
    (8080, "http://192.168.1.1:8080"),
    (8443, "http://192.168.1.1:8443"),
])
def test_the_websocket_url_follows_the_configured_port(reload_handler_module, port, expected) -> None:
    module = reload_handler_module("192.168.1.1", port)

    assert module.HttpMiniserverHandler.ws_base_url == expected


# Batched handover - Rust passes every value expanded out of one MQTT message
# in a single call instead of one call per JSON leaf.

BATCH = [
    ("dev/sensor/temp", "dev_sensor_temp", "21.5"),
    ("dev/sensor/hum", "dev_sensor_hum", "48"),
    ("dev/sensor/on", "dev_sensor_on", "1"),
]


@pytest.mark.asyncio
async def test_batch_keeps_order(miniserver, handler: HttpMiniserverHandler) -> None:
    """All values share one socket, so they go out one after another."""
    await handler.send_batch_to_miniserver(BATCH)

    assert miniserver.commands == [(normalized, value) for _, normalized, value in BATCH]


@pytest.mark.asyncio
async def test_batch_survives_a_failing_value(handler: HttpMiniserverHandler) -> None:
    """One bad value must not take the rest of the message down with it.

    Driven by making the Miniserver reject one input rather than by patching the
    handler's own send: the batch no longer routes through the single-value entry
    point, and a failing input is what the relay actually meets.
    """
    with mock_miniserver(state="CONNECTED", fail_targets={"dev_sensor_hum"}) as fake:
        await handler.send_batch_to_miniserver(BATCH)

        assert fake.commands == [
            (normalized, value)
            for _, normalized, value in BATCH
            if normalized != "dev_sensor_hum"
        ]


@pytest.mark.asyncio
async def test_batch_decides_the_connection_once(
    miniserver, handler: HttpMiniserverHandler
) -> None:
    """All values of a message share the socket, so they share one decision.

    Re-deciding per value cost a coroutine and a state read for an answer that
    cannot have changed within the batch.
    """
    checks = 0
    real = handler._ensure_websocket

    async def counting():
        nonlocal checks
        checks += 1
        return await real()

    with patch.object(handler, "_ensure_websocket", side_effect=counting):
        await handler.send_batch_to_miniserver(BATCH)

    assert len(miniserver.commands) == len(BATCH)
    assert checks == 1


@pytest.mark.asyncio
async def test_empty_batch_does_nothing(miniserver, handler: HttpMiniserverHandler) -> None:
    await handler.send_batch_to_miniserver([])

    assert miniserver.commands == []


# Websocket connection handling. The connect used to sit outside the try block,
# so a Miniserver that was not answering took the rest of the message with it -
# and every message tried a fresh handshake while it stayed away.

@pytest.mark.asyncio
async def test_failed_connect_does_not_escape_the_batch(handler: HttpMiniserverHandler) -> None:
    with mock_miniserver(fail_connect=True) as fake:
        await handler.send_batch_to_miniserver(BATCH)

        assert fake.commands == []


@pytest.mark.asyncio
async def test_batch_reports_what_the_failed_connect_cost(
    handler: HttpMiniserverHandler
) -> None:
    with mock_miniserver(fail_connect=True), \
         patch("loxmqttrelay.http_miniserver_handler.logger") as log:
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
    with mock_miniserver(fail_connect=True) as fake:
        for _ in range(5):
            await handler.send_batch_to_miniserver(BATCH)

        assert len(fake.connects) == 1


@pytest.mark.asyncio
async def test_the_retry_window_expires(handler: HttpMiniserverHandler) -> None:
    handler.connect_retry_delay = 0.0

    with mock_miniserver(fail_connect=True) as fake:
        await handler.send_batch_to_miniserver(BATCH)
        await handler.send_batch_to_miniserver(BATCH)

        assert len(fake.connects) == 2


@pytest.mark.asyncio
async def test_concurrent_messages_share_one_connect(handler: HttpMiniserverHandler) -> None:
    with mock_miniserver() as fake:
        await asyncio.gather(*(handler.send_batch_to_miniserver(BATCH) for _ in range(4)))

        assert len(fake.connects) == 1
        assert len(fake.commands) == 4 * len(BATCH)


@pytest.mark.asyncio
async def test_no_connect_while_the_library_reconnects(handler: HttpMiniserverHandler) -> None:
    """A second handshake would race the reconnect instead of helping it."""
    with mock_miniserver(state="RECONNECTING") as fake:
        await handler.send_batch_to_miniserver(BATCH)

        assert fake.connects == []

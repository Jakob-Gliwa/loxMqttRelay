"""Shutdown path of the relay, plus the startup order that does not need a broker.

Most of startup is not exercised here - it needs a real broker - so these tests
drive the pieces that a signal or a config change reaches: the stop request, the
restart decision and the teardown itself. The one startup property that matters
without sockets is the order of sync vs MQTT subscribe.
"""

import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from loxmqttrelay.main import MQTTRelay


@pytest.fixture
def relay():
    relay = MQTTRelay()
    # The real clients would be fine (neither is connected), but the mocks are
    # what make the teardown observable.
    relay.mqtt_client = AsyncMock()
    relay.miniserver_client = AsyncMock()
    relay._stop = asyncio.Event()
    relay.udp_server = MagicMock(start=AsyncMock(), stop=AsyncMock())
    relay._udp_running = True
    return relay


@pytest.mark.asyncio
async def test_startup_syncs_whitelist_before_mqtt_subscribe(relay):
    """Retained MQTT traffic must not arrive before the whitelist is filled."""
    order: list[str] = []

    async def note_connect(_relay):
        order.append("miniserver_connect")

    async def note_sync():
        order.append("whitelist_sync")

    async def note_mqtt():
        order.append("mqtt_subscribe")

    async def note_udp_and_stop():
        order.append("udp_start")
        # Last step before `_stop.wait()`; end the run without hanging.
        assert relay._stop is not None
        relay._stop.set()

    relay.miniserver_client.connect = AsyncMock(side_effect=note_connect)
    relay.handle_miniserver_sync = AsyncMock(side_effect=note_sync)
    relay.connect_and_subscribe_mqtt = AsyncMock(side_effect=note_mqtt)
    relay.udp_server.start = AsyncMock(side_effect=note_udp_and_stop)
    relay._udp_running = False

    with patch.object(relay, "_install_signal_handlers"), \
         patch.object(relay, "shutdown", new_callable=AsyncMock):
        await relay.main()

    assert order == [
        "miniserver_connect",
        "whitelist_sync",
        "mqtt_subscribe",
        "udp_start",
    ]


def test_stop_request_carries_the_restart_decision(relay):
    relay.request_stop("configuration changed", restart=True)

    assert relay._stop.is_set()
    assert relay._restart_requested is True


def test_repeated_stop_requests_are_ignored(relay):
    """A second SIGTERM must not turn a plain shutdown into a restart."""
    relay.request_stop("SIGTERM")
    relay.request_stop("configuration changed", restart=True)

    assert relay._restart_requested is False


@pytest.mark.asyncio
async def test_shutdown_closes_inputs_and_connections(relay):
    await relay.shutdown()

    relay.udp_server.stop.assert_awaited_once()
    relay.mqtt_client.disconnect.assert_awaited_once()
    relay.miniserver_client.stop.assert_awaited_once()
    assert relay._udp_running is False


@pytest.mark.asyncio
async def test_shutdown_runs_once(relay):
    await relay.shutdown()
    await relay.shutdown()

    relay.udp_server.stop.assert_awaited_once()
    relay.mqtt_client.disconnect.assert_awaited_once()
    relay.miniserver_client.stop.assert_awaited_once()


@pytest.mark.asyncio
async def test_shutdown_survives_a_failing_disconnect(relay):
    """A broker that is already gone must not keep the websocket open."""
    relay.mqtt_client.disconnect.side_effect = RuntimeError("broker gone")

    await relay.shutdown()

    relay.miniserver_client.stop.assert_awaited_once()


@pytest.mark.asyncio
async def test_shutdown_survives_a_failing_websocket_close(relay):
    """And the reverse: a websocket that refuses to close is not fatal either."""
    relay.miniserver_client.stop.side_effect = RuntimeError("already gone")

    await relay.shutdown()

    relay.mqtt_client.disconnect.assert_awaited_once()


@pytest.mark.asyncio
async def test_restart_request_reaches_the_loop_from_another_thread():
    """restart_relay is called by the Rust ingress worker, off the loop thread.

    It must not exec there: that would replace the process image with the MQTT
    session and the UDP socket still open.
    """
    relay = MQTTRelay()
    relay._stop = asyncio.Event()
    relay._loop = asyncio.get_running_loop()

    with patch('loxmqttrelay.main.os.execv') as execv:
        await asyncio.to_thread(relay.restart_relay)
        await asyncio.wait_for(relay._stop.wait(), timeout=1)

    execv.assert_not_called()
    assert relay._restart_requested is True

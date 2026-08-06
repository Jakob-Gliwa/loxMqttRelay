"""Shutdown path of the relay.

Startup is not exercised here - it needs a broker - so these tests drive the
pieces that a signal or a config change reaches: the stop request, the restart
decision and the teardown itself.
"""

import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from loxmqttrelay.main import MQTTRelay


@pytest.fixture
def relay():
    relay = MQTTRelay()
    # The real client would be fine (it is not connected), but the mock is what
    # makes the teardown observable.
    relay.mqtt_client = AsyncMock()
    relay._stop = asyncio.Event()
    relay.udp_server = MagicMock(start=AsyncMock(), stop=AsyncMock())
    relay._udp_running = True
    return relay


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
async def test_shutdown_closes_inputs_and_connection(relay):
    await relay.shutdown()

    relay.mqtt_client.disconnect.assert_awaited_once()
    relay.udp_server.stop.assert_awaited_once()
    assert relay._udp_running is False


@pytest.mark.asyncio
async def test_shutdown_runs_once(relay):
    await relay.shutdown()
    await relay.shutdown()

    relay.udp_server.stop.assert_awaited_once()
    relay.mqtt_client.disconnect.assert_awaited_once()


@pytest.mark.asyncio
async def test_shutdown_survives_a_failing_disconnect(relay):
    """A broker that is already gone must not keep the websocket open."""
    relay.mqtt_client.disconnect.side_effect = RuntimeError("broker gone")

    with patch('loxmqttrelay.main.loxwebsocket') as websocket:
        websocket.state = "CONNECTED"
        websocket.stop = AsyncMock()
        await relay.shutdown()

        websocket.stop.assert_awaited_once()


@pytest.mark.asyncio
async def test_shutdown_leaves_an_unused_websocket_alone(relay):
    with patch('loxmqttrelay.main.loxwebsocket') as websocket:
        websocket.state = "CLOSED"
        websocket.stop = AsyncMock()
        await relay.shutdown()

        websocket.stop.assert_not_awaited()


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

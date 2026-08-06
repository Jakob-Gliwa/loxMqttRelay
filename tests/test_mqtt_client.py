"""What the MQTT client reports about messages it could not deliver.

The relay publishes at QoS 0 and keeps no outbox, so a message that cannot be
handed to the broker is gone. These tests pin down that the loss is at least
visible: the publish call says why, and the last few messages stay readable for
diagnostics.
"""

import pytest

from loxmqttrelay.config import global_config
from loxmqttrelay.compatible._loxmqttrelay import MqttClient


@pytest.fixture
def client():
    """A client that was never connected - every publish is therefore a drop."""
    global_config.broker.host = "127.0.0.1"
    global_config.broker.port = 1883
    global_config.general.base_topic = "myrelay/"
    return MqttClient(global_config)


def test_connected_is_false_without_a_session(client):
    """`connected` follows the MQTT session, not the existence of a handle.

    It reported the handle once, which stayed in place across every reconnect
    and therefore only ever said that connecting had worked at some point.
    """
    assert client.connected is False


@pytest.mark.asyncio
async def test_publish_without_connection_reports_the_reason(client):
    assert await client.publish("some/topic", "value") == "broker not connected"
    assert client.take_undelivered() == [
        ("some/topic", b"value", "broker not connected")
    ]


@pytest.mark.asyncio
async def test_undelivered_ring_is_drained_once(client):
    await client.publish("some/topic", "value")

    assert len(client.take_undelivered()) == 1
    assert client.take_undelivered() == []


@pytest.mark.asyncio
async def test_undelivered_ring_keeps_the_newest(client):
    """The ring is bounded, so a long outage rolls the oldest samples out."""
    for i in range(40):
        await client.publish(f"some/topic/{i}", "value")

    kept = client.take_undelivered()
    assert len(kept) == 32
    assert kept[0][0] == "some/topic/8"
    assert kept[-1][0] == "some/topic/39"

"""Worked examples for the mock Miniserver harness.

These are not part of the regression suite - they exist to be read and copied
when you want to watch the relay talk to a Miniserver that is not there. Run
them with:

    uv run pytest -m harness
"""

import pytest

from loxmqttrelay.http_miniserver_handler import HttpMiniserverHandler
from tests.harness.mock_miniserver import mock_miniserver

pytestmark = pytest.mark.harness


BATCH = [
    ("dev/sensor/temp", "dev_sensor_temp", "21.5"),
    ("dev/sensor/hum", "dev_sensor_hum", "48"),
]


@pytest.mark.asyncio
async def test_the_miniserver_receives_what_was_published(miniserver) -> None:
    """The everyday case: what goes in comes out on the other side."""
    handler = HttpMiniserverHandler()

    await handler.send_batch_to_miniserver(BATCH)

    assert miniserver.commands == [
        ("dev_sensor_temp", "21.5"),
        ("dev_sensor_hum", "48"),
    ]
    assert miniserver.values_for("dev_sensor_temp") == ["21.5"]


@pytest.mark.asyncio
async def test_an_outage_and_what_comes_after() -> None:
    """A Miniserver that is away loses those values, then picks up again."""
    handler = HttpMiniserverHandler()
    handler.connect_retry_delay = 0.0

    with mock_miniserver(fail_connect=True) as fake:
        await handler.send_batch_to_miniserver(BATCH)
        assert fake.commands == []

        fake.fail_connect = False
        await handler.send_batch_to_miniserver(BATCH)

        assert fake.values_for("dev_sensor_temp") == ["21.5"]

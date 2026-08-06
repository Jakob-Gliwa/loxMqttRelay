"""That the UDP listener survives being stopped and started again.

`stop` leaves a wakeup permit behind for the loop it is ending. While that
permit belonged to the server rather than to the run, the next `start` bound the
socket, consumed the stale permit on its first poll and closed it again - all
while reporting success. The socket is the witness here: the port stays taken
for exactly as long as the receive loop runs.
"""

import asyncio
import socket

import pytest

from loxmqttrelay.compatible._loxmqttrelay import MqttClient, UdpServer
from loxmqttrelay.config import global_config


def port_is_taken(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        try:
            probe.bind(("0.0.0.0", port))
        except OSError:
            return True
    return False


@pytest.fixture
def port() -> int:
    """A port the kernel just handed out, i.e. one nothing else is using."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
        probe.bind(("0.0.0.0", 0))
        return probe.getsockname()[1]


@pytest.fixture
def server(port: int) -> UdpServer:
    global_config.udp.udp_in_port = port
    # No filtering and a numeric address: nothing in these tests should depend
    # on a name resolving.
    global_config.udp.udp_source_filter_enabled = False
    global_config.miniserver.miniserver_ip = "127.0.0.1"
    return UdpServer(global_config, MqttClient(global_config))


@pytest.mark.asyncio
async def test_stop_releases_the_port(server, port):
    await server.start()
    assert port_is_taken(port)

    await server.stop()
    assert not port_is_taken(port)


@pytest.mark.asyncio
async def test_server_can_be_started_again_after_stop(server, port):
    await server.start()
    await server.stop()

    await server.start()
    # The failure this guards against is the loop ending on its own right after
    # the bind, so give it the chance to before looking.
    await asyncio.sleep(0.1)
    assert port_is_taken(port)

    await server.stop()


@pytest.mark.asyncio
async def test_starting_twice_is_refused(server, port):
    await server.start()

    with pytest.raises(RuntimeError, match="already listening"):
        await server.start()

    # The refusal left the running server alone.
    assert port_is_taken(port)
    await server.stop()
    assert not port_is_taken(port)

import socket

import aiohttp
import orjson
import pytest
from aiohttp.test_utils import TestClient, TestServer, make_mocked_request

from loxmqttrelay.config import global_config
from loxmqttrelay.http_api_server import HttpApiServer


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class FakeProcessor:
    """Mimics MiniserverDataProcessor.normalize_topic (Rust) in pure Python."""

    def normalize_topic(self, topic: str) -> str:
        return topic.replace("/", "_").replace("%", "_")


@pytest.fixture
def server():
    return HttpApiServer(FakeProcessor())


def test_build_vi_name_map_normalizes_topics(server):
    global_config.topics.topic_whitelist = {"device/status", "sensor_data", "home/living%room/temp"}

    mapping = server.build_vi_name_map()

    assert mapping == {
        "device/status": "device_status",
        "home/living%room/temp": "home_living_room_temp",
        "sensor_data": "sensor_data",
    }


def test_build_vi_name_map_is_sorted_by_key(server):
    global_config.topics.topic_whitelist = {"c", "a", "b"}

    mapping = server.build_vi_name_map()

    assert list(mapping.keys()) == ["a", "b", "c"]


def test_build_vi_name_map_empty_whitelist(server):
    global_config.topics.topic_whitelist = set()

    assert server.build_vi_name_map() == {}


@pytest.mark.asyncio
async def test_handle_vi_names_returns_json(server):
    global_config.topics.topic_whitelist = {"device/status"}

    request = make_mocked_request("GET", "/vi_names")
    response = await server.handle_vi_names(request)

    assert response.status == 200
    assert response.content_type == "application/json"
    assert orjson.loads(response.body) == {"device/status": "device_status"}


@pytest.mark.asyncio
async def test_vi_names_endpoint_over_http(server):
    global_config.topics.topic_whitelist = {"a/b", "c"}

    async with TestClient(TestServer(server.build_app())) as client:
        resp = await client.get("/vi_names")

        assert resp.status == 200
        assert resp.headers["Content-Type"] == "application/json"
        assert await resp.json() == {"a/b": "a_b", "c": "c"}


@pytest.mark.asyncio
async def test_start_binds_port_and_stop_releases(server):
    global_config.topics.topic_whitelist = {"x/y"}
    port = _free_port()
    global_config.http.http_api_port = port

    await server.start()
    try:
        async with aiohttp.ClientSession() as session:
            async with session.get(f"http://127.0.0.1:{port}/vi_names") as resp:
                assert resp.status == 200
                assert await resp.json() == {"x/y": "x_y"}
    finally:
        await server.stop()

    # After stop() the port must no longer be served.
    with pytest.raises(aiohttp.ClientError):
        async with aiohttp.ClientSession() as session:
            async with session.get(
                f"http://127.0.0.1:{port}/vi_names",
                timeout=aiohttp.ClientTimeout(total=1),
            ):
                pass

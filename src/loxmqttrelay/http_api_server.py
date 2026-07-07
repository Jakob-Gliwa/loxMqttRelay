import orjson
from aiohttp import web

from loxmqttrelay.config import global_config
from loxmqttrelay.logging_config import get_lazy_logger

logger = get_lazy_logger(__name__)


class HttpApiServer:
    """Small read-only HTTP API for inspecting the relay's runtime state.

    Currently exposes a single endpoint, ``GET /vi_names``, which returns the
    mapping of whitelisted MQTT topics to the Virtual Input names they are
    forwarded to on the Miniserver. The VI name is the *normalized* topic
    (``/`` and ``%`` replaced by ``_``) - exactly the name the relay uses when
    writing to ``/dev/sps/io/<name>/<value>`` - so the map tells you how each
    Virtual Input has to be named in Loxone Config.
    """

    def __init__(self, data_processor):
        # data_processor exposes normalize_topic(); reusing it keeps this map in
        # lock-step with the naming the forward path actually uses.
        self._data_processor = data_processor
        self._runner = None

    def build_vi_name_map(self) -> dict:
        """Return {mqtt_topic: virtual_input_name} for the current whitelist.

        Keys are the whitelisted topics as configured; values are their
        normalized form, i.e. the name the matching Virtual Input must have on
        the Miniserver. Sorted by key for a stable, human-friendly response.
        """
        mapping = {}
        for topic in sorted(global_config.topics.topic_whitelist):
            mapping[topic] = self._data_processor.normalize_topic(topic)
        return mapping

    async def handle_vi_names(self, request: web.Request) -> web.Response:
        mapping = self.build_vi_name_map()
        return web.Response(
            body=orjson.dumps(mapping),
            content_type="application/json",
        )

    def build_app(self) -> web.Application:
        app = web.Application()
        app.router.add_get("/vi_names", self.handle_vi_names)
        return app

    async def start(self) -> None:
        self._runner = web.AppRunner(self.build_app())
        await self._runner.setup()

        port = global_config.http.http_api_port
        site = web.TCPSite(self._runner, "0.0.0.0", port)
        await site.start()
        logger.info(f"HTTP-API listening on port {port}")

    async def stop(self) -> None:
        if self._runner is not None:
            await self._runner.cleanup()
            self._runner = None

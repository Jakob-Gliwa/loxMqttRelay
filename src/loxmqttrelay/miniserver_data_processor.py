from functools import lru_cache
import logging
import re
from typing import Dict, Any, List, Pattern, Optional, Callable, Awaitable, Set, AsyncGenerator
import orjson
from loxmqttrelay.config import global_config

logger = logging.getLogger(__name__)


# Boolean mapping dictionary for fast lookup
BOOLEAN_MAPPING = {
    "true": "1",
    "yes": "1",
    "on": "1",
    "enabled": "1",
    "enable": "1",
    "1": "1",
    "check": "1",
    "checked": "1",
    "select": "1",
    "selected": "1",
    "false": "0",
    "no": "0",
    "off": "0",
    "disabled": "0",
    "disable": "0",
    "0": "0",
}

class MiniserverDataProcessor:

    def __init__(self):
        logger.debug(f"Initializing MiniserverDataProcessor with cache_size={global_config.general.cache_size}")
        # Nur noch ein Filter-Pattern, das sowohl für den ersten
        # als auch den zweiten Durchlauf verwendet wird
        self.compiled_subscription_filter = self._compile_filters(global_config.topics.subscription_filters)
        self.topic_whitelist = global_config.topics.topic_whitelist
        self.do_not_forward_patterns: Optional[Pattern] = None
        logger.debug("MiniserverDataProcessor initialization complete")

    def _compile_filters(self, filters: List[str]) -> Optional[Pattern]:
        """Builds a single regex from all filter strings."""
        logger.debug(f"Compiling filters: {filters}")
        if not filters:
            logger.debug("No filters provided")
            return None
        valid_filters = []
        for flt in filters:
            try:
                re.compile(flt)
                valid_filters.append(flt)
                logger.debug(f"Filter '{flt}' is valid")
            except re.error as e:
                logger.error(f"Invalid filter '{flt}': {e}")

        if not valid_filters:
            logger.debug("No valid filters found")
            return None

        pattern = f"({'|'.join(valid_filters)})"
        logger.debug(f"Compiled pattern: {pattern}")
        return re.compile(pattern)

    def update_subscription_filters(self, filters: List[str]) -> None:
        logger.debug(f"Updating subscription filters: {filters}")
        self.compiled_subscription_filter = self._compile_filters(filters)

    def update_topic_whitelist(self, whitelist: Set[str]) -> None:
        logger.debug(f"Updating topic whitelist: {whitelist}")
        self.topic_whitelist = whitelist
        self.is_in_whitelist.cache_clear()

    def update_do_not_forward(self, filters: List[str]) -> None:
        logger.debug(f"Updating do_not_forward filters: {filters}")
        self.do_not_forward_patterns = self._compile_filters(filters)

    @lru_cache(maxsize=global_config.general.cache_size)
    def _convert_boolean(self, val: Any) -> Optional[str]:
        """Convert a value to "1" or "0" based on BOOLEAN_MAPPING"""
        logger.debug(f"Converting boolean value: {val}")
        if not val:
            return val
        normalized_val = str(val).strip().lower()
        converted_val = BOOLEAN_MAPPING.get(normalized_val)
        if converted_val is not None:
            logger.debug(f"Converted '{val}' to '{converted_val}'")
        else:
            logger.debug(f"No boolean mapping found for '{val}'")
            return val
        return converted_val
    
    @lru_cache(maxsize=global_config.general.cache_size)
    def normalize_topic(self, topic: str) -> str:
        """Normalize a topic by replacing forward slashes and percent signs with underscores"""
        if '/' not in topic and '%' not in topic:
            return topic  # Topic is already normalized
        normalized = topic.replace('/', '_').replace('%', '_')
        logger.debug(f"Normalized topic '{topic}' to '{normalized}'")
        return normalized

    @staticmethod
    def flatten_dict(d: dict, parent_key: str = '') -> set:
        items = set()
        MiniserverDataProcessor._flatten(d, parent_key, items)
        return items

    @staticmethod
    def _flatten(obj, prefix: str, items: set) -> None:
        if isinstance(obj, dict):
            for k, v in obj.items():
                new_key = "%s/%s" % (prefix, k) if prefix else k
                if isinstance(v, (dict, list)):
                    MiniserverDataProcessor._flatten(v, new_key, items)
                else:
                    items.add((new_key, v))
        elif isinstance(obj, list):
            for i, item in enumerate(obj):
                new_key = "%s/%s" % (prefix, i)
                if isinstance(item, (dict, list)):
                    MiniserverDataProcessor._flatten(item, new_key, items)
                else:
                    items.add((new_key, item))

    def expand_json(self, topic: str, val: str) -> frozenset:
        """Optimized JSON expansion for string input, returning a frozenset of tuples."""
        """
        Takes a topic and JSON string as bytes.
        Returns a frozenset of (topic/key, value) tuples.
        Uses caching and early short-circuit for non-JSON input.
        """
        # 1) Short-circuit for non-JSON cases
        if not val or val[0] not in '{[':
            return frozenset({(topic, val)})
        try:
            obj = orjson.loads(val)
            if not isinstance(obj, dict):
                return frozenset({(topic, val)})
            
            # Get flattened key-value pairs
            flat = self.flatten_dict(obj)
            # Add topic prefix to all keys
            return frozenset({(f"{topic}/{key}", value) for key, value in flat})
        except (ValueError, TypeError, orjson.JSONDecodeError):
            return frozenset({(topic, val)})

    @lru_cache(maxsize=global_config.general.cache_size)
    def is_in_whitelist(self, topic: str) -> bool:
        """Check if the topic is in the whitelist"""
        normalized = self.normalize_topic(topic)
        return normalized in self.topic_whitelist

    async def process_data(
        self,
        topic: str,
        message: str,
        mqtt_publish_callback: Optional[Callable[[str, str, bool], Awaitable[None]]] = None,
    ) -> AsyncGenerator[tuple[str, Any], None]:
        """Process data through filters and transformations."""
        logger.debug(f"Processing data - topic {topic}, message {message}")

        # 1) First pass subscription filter
        if self.compiled_subscription_filter and self.compiled_subscription_filter.search(topic):
            return
            # Let the generator end without yielding anything
        
        # 2) Flatten data
        logger.debug(f"Transforming data with expand_json={global_config.processing.expand_json}")
        if global_config.processing.expand_json:
            processed_tuples = self.expand_json(topic, message)
        else:
            logger.debug("Skipping JSON expansion")
            processed_tuples = [(topic, message)]
        logger.debug(f"Data after flattening: {processed_tuples}")

        # Optionales Debug-Publish der bereits verarbeiteten Topics
        if global_config.debug.publish_processed_topics and mqtt_publish_callback:
            logger.debug("Publishing processed topics for debugging")
            for topic, val in processed_tuples:
                dbg_topic = f"{global_config.general.base_topic}processedtopics/{self.normalize_topic(topic)}"
                logger.debug(f"Publishing debug topic: {dbg_topic} = {val}")
                await mqtt_publish_callback(dbg_topic, str(val), False)

        # 3) Final filtering
        for topic, value in processed_tuples:
            logger.debug(f"Final filtering for topic: {topic}")

            # Whitelist
            if self.topic_whitelist and not self.is_in_whitelist(topic): 
                logger.debug(f"Topic '{topic}' not in whitelist")
                continue

            # Subscription-Filter (2nd pass)
            if self.compiled_subscription_filter and \
               self.compiled_subscription_filter.search(topic):
                logger.debug(f"Topic '{topic}' filtered by second pass (subscription_filter)")
                continue

            # do_not_forward
            elif self.do_not_forward_patterns and \
                self.do_not_forward_patterns.search(topic):
                    logger.debug(f"Topic '{topic}' filtered by do_not_forward")
                    continue

            logger.debug(f"Topic '{topic}' passed all filters")
            yield (topic, self._convert_boolean(value))

    async def publish_forwarded_topic(
        self,
        topic: str,
        value: Any,
        http_code: int,
        mqtt_publish_callback: Callable[[str, str, bool], Awaitable[None]]
    ) -> None:
        """Publishes a forwarded topic with value and HTTP code."""
        logger.debug(f"Publishing forwarded topic: {topic} with value={value}, http_code={http_code}")
        if not mqtt_publish_callback:
            logger.debug("No mqtt_publish_callback provided, skipping publish")
            return
        mqtt_topic = f"{global_config.general.base_topic}forwardedtopics/{self.normalize_topic(topic)}"
        payload = {"value": value, "http_code": http_code}
        logger.debug(f"Publishing to MQTT topic '{mqtt_topic}': {payload}")
        await mqtt_publish_callback(mqtt_topic, orjson.dumps(payload).decode('utf-8'), False)

miniserver_data_processor = MiniserverDataProcessor()

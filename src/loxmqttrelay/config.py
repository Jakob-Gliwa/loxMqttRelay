import os
import logging
import re
from dataclasses import dataclass, field, asdict, replace, fields
import threading
from typing import Dict, Any, List, Optional, Literal, Union, get_args, get_origin, get_type_hints, Set
import tomlkit
from enum import Enum

# Use standard logging here to avoid circular imports
# (logging_config might depend on modules that depend on config)
logger = logging.getLogger(__name__)

class ConfigSection(Enum):
    GENERAL = "general"
    BROKER = "broker"
    MINISERVER = "miniserver"
    TOPICS = "topics"
    PROCESSING = "processing"
    UDP = "udp"
    DEBUG = "debug"

@dataclass
class GeneralConfig:
    log_level: str = "INFO"
    base_topic: str = "myrelay/"
    cache_size: int = 100000

@dataclass
class BrokerConfig:
    host: str = "localhost"
    port: int = 1883
    user: Optional[str] = None
    password: Optional[str] = None
    client_id: str = "loxmqttrelay"

@dataclass
class MiniserverConfig:
    miniserver_ip: str = "127.0.0.1"
    miniserver_port: int = 80
    miniserver_user: str = ""
    miniserver_pass: str = ""
    miniserver_max_parallel_connections: int = 5
    sync_with_miniserver: bool = True
    use_websocket: bool = True

@dataclass
class TopicsConfig:
    subscriptions: List[str] = field(default_factory=list)
    subscription_filters: List[str] = field(default_factory=list)
    topic_whitelist: Set[str] = field(default_factory=set)
    do_not_forward: List[str] = field(default_factory=list)

    def __post_init__(self):
        # TOML has no set type, so a loaded config hands in a list here. Runs on
        # dataclasses.replace() too, which is how update_config() mutates.
        self.topic_whitelist = set(self.topic_whitelist)

@dataclass
class ProcessingConfig:
    expand_json: bool = True
    convert_booleans: bool = True

@dataclass
class UdpConfig:
    udp_in_port: int = 11884
    udp_source_filter_enabled: bool = True
    # Additional senders besides the configured Miniserver; IPs or hostnames
    udp_allowed_sources: List[str] = field(default_factory=list)

@dataclass
class DebugConfig:
    mock_ip: str = ""
    enable_mock: bool = False

@dataclass
class AppConfig:
    general: GeneralConfig = field(default_factory=GeneralConfig)
    broker: BrokerConfig = field(default_factory=BrokerConfig)
    miniserver: MiniserverConfig = field(default_factory=MiniserverConfig)
    topics: TopicsConfig = field(default_factory=TopicsConfig)
    processing: ProcessingConfig = field(default_factory=ProcessingConfig)
    udp: UdpConfig = field(default_factory=UdpConfig)
    debug: DebugConfig = field(default_factory=DebugConfig)

    def to_dict(self) -> Dict[str, Any]:
        """Sections as plain dicts, ready to serialize.

        Set-valued fields (topic_whitelist) become sorted lists: neither TOML
        nor JSON has a set type, so both save_config() and get_safe_config()
        would otherwise fail on the default config. Sorting keeps the written
        file and the config/response payload stable across runs.
        """
        return {
            f.name: {
                key: sorted(value) if isinstance(value, (set, frozenset)) else value
                for key, value in asdict(getattr(self, f.name)).items()
            }
            for f in fields(self)
        }

    @classmethod
    def from_dict(cls, config_dict: Dict[str, Any]) -> "AppConfig":
        return cls(**{f.name: cls._create_section(f.name, config_dict) for f in fields(cls)})

    @staticmethod
    def _create_section(section: str, config_dict: Dict[str, Any]) -> Any:
        section_class = globals().get(section.capitalize() + "Config")
        if section_class is None:
            raise ConfigError(f"Invalid configuration section: {section}")
        data = config_dict.get(section, {})
        valid_fields = get_type_hints(section_class)
        valid_data = {}
        for key, value in data.items():
            if key in valid_fields:
                valid_data[key] = value
            else:
                logger.warning(
                    f"Unknown field '{key}' in config section '{section}' will be ignored."
                )
        return section_class(**valid_data)

class ConfigError(Exception):
    pass


# Fields the MQTT control topics must not touch.
#
# Validation cannot catch these: another host is a perfectly valid value, and
# after the restart that follows an update the relay would authenticate there
# with the configured credentials. mock_ip/enable_mock are on the list because
# they override the Miniserver target in http_miniserver_handler and add a UDP
# destination in udp_handler - the same redirect under a different name.
REMOTE_PROTECTED_FIELDS = frozenset({
    "host",
    "port",
    "user",
    "password",
    "miniserver_ip",
    "miniserver_port",
    "miniserver_user",
    "miniserver_pass",
    "mock_ip",
    "enable_mock",
})


def _matches_type(value: Any, expected: Any) -> bool:
    """``isinstance`` without the bool/int conflation.

    ``isinstance(True, int)`` holds in Python, so a plain check would accept
    ``{"cache_size": true}``. That value only fails once the Rust side extracts
    an i32 - by then it has been written to the config file and the relay has
    restarted into it.
    """
    if expected is bool:
        return isinstance(value, bool)
    if expected is int:
        return isinstance(value, int) and not isinstance(value, bool)
    return isinstance(value, expected)


def _type_mismatch(
    field_name: str, expected: Any, value: Any, allow_bare_item: bool = True
) -> Optional[str]:
    """Why *value* does not fit *expected*, or None if it does.

    ``allow_bare_item`` is what separates an MQTT update from the config file:
    a payload may name a single entry where a list is expected, a TOML file has
    real arrays and no reason to.
    """
    origin = get_origin(expected)

    if origin is Union:
        allowed = [arg for arg in get_args(expected) if arg is not type(None)]
        if value is None or any(_matches_type(value, arg) for arg in allowed):
            return None
        return f"'{field_name}' expects {expected}, got {type(value).__name__}"

    if origin in (list, set):
        args = get_args(expected)
        item_type = args[0] if args else str
        if isinstance(value, (list, set)):
            items = list(value)
        elif allow_bare_item:
            # A bare element stands for a one-element collection, the same way
            # the update itself unwraps the payload.
            items = [value]
        else:
            return f"'{field_name}' expects a list, got {type(value).__name__}"
        if all(_matches_type(item, item_type) for item in items):
            return None
        return f"'{field_name}' expects a list of {item_type.__name__}"

    if _matches_type(value, expected):
        return None
    return f"'{field_name}' expects {expected.__name__}, got {type(value).__name__}"


_PORT_FIELDS = frozenset({"port", "miniserver_port", "udp_in_port"})
_LOG_LEVELS = ("DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL")
_REGEX_FIELDS = frozenset({"subscription_filters", "do_not_forward"})


def _value_problem(field_name: str, value: Any) -> Optional[str]:
    """What is wrong with a value of the right type, or None.

    Only reached once the type fits, so the comparisons below are safe.
    """
    if field_name in _PORT_FIELDS and not 1 <= value <= 65535:
        return f"'{field_name}' must be between 1 and 65535, got {value}"
    if field_name == "miniserver_max_parallel_connections" and value < 1:
        return f"'{field_name}' must be at least 1, got {value}"
    if field_name == "cache_size" and value < 0:
        return f"'{field_name}' cannot be negative, got {value}"
    if field_name == "log_level" and value.upper() not in _LOG_LEVELS:
        return f"'log_level' must be one of {', '.join(_LOG_LEVELS)}, got '{value}'"
    if field_name == "base_topic" and not value.strip():
        return "'base_topic' cannot be empty - it prefixes every control topic"
    if field_name in ("host", "miniserver_ip") and not value.strip():
        return f"'{field_name}' cannot be empty"
    if field_name in _REGEX_FIELDS:
        for pattern in (value if isinstance(value, list) else [value]):
            try:
                # A first pass only: the patterns are matched by the Rust regex
                # engine, which rejects lookaround and backreferences that
                # Python accepts. Those still fail there, with their own error.
                re.compile(pattern)
            except re.error as e:
                return f"'{field_name}' has an invalid pattern '{pattern}': {e}"
    return None


def validate_config_dict(config_dict: Dict[str, Any]) -> List[str]:
    """Every reason the parsed file cannot be used, in file order.

    Runs before the dataclasses are built, so a value of the wrong type is
    named here instead of surfacing later as a TypeError from a section
    constructor, an unreadable value on the Rust side, or - worse - a string
    like "false" that is quietly truthy.

    Unknown sections and fields are not errors: an upgrade that drops an
    option must not stop the relay from starting. ``_create_section`` warns
    about those.
    """
    problems: List[str] = []
    for section, values in config_dict.items():
        section_class = globals().get(section.capitalize() + "Config")
        if section_class is None:
            logger.warning(f"Unknown configuration section '[{section}]' will be ignored.")
            continue
        if not isinstance(values, dict):
            problems.append(f"'[{section}]' must be a table")
            continue
        expected_types = get_type_hints(section_class)
        for key, value in values.items():
            if key not in expected_types:
                continue
            problem = _type_mismatch(
                key, expected_types[key], value, allow_bare_item=False
            ) or _value_problem(key, value)
            if problem:
                problems.append(f"[{section}] {problem}")
    return problems


class Config:
    _instance = None
    _lock = threading.Lock()

    def __new__(cls):
        if cls._instance is None:
            with cls._lock:
                if cls._instance is None:
                    cls._instance = super().__new__(cls)
        return cls._instance

    def __init__(self, config_path: str = "config/config.toml"):
        with self._lock:
            if not hasattr(self, '_initialized'):
                self.config_path = config_path
                self._config = self._load_config()
                self.field_mappings = self._map_fields_to_sections()
                self._initialized = True

    def _load_config(self) -> AppConfig:
        if not os.path.exists(self.config_path):
            logger.warning(f"Config file not found, creating default config: {self.config_path}")
            return AppConfig()

        with open(self.config_path, "r") as f:
            config_dict = tomlkit.parse(f.read()).unwrap()

        if problems := validate_config_dict(config_dict):
            for problem in problems:
                logger.error(f"Invalid configuration: {problem}")
            logger.error(
                f"Refusing to start: {self.config_path} has {len(problems)} unusable "
                f"value(s). Nothing was connected, nothing was changed."
            )
            # Raised while this module is imported, i.e. before the relay opens
            # a socket. SystemExit exits with a status instead of a traceback -
            # the lines above are the message.
            raise SystemExit(1)

        return AppConfig.from_dict(config_dict)

    def save_config(self) -> None:
        doc = tomlkit.document()
        config_dict = self._config.to_dict()
        
        # Convert None values to empty strings before saving
        for section, values in config_dict.items():
            table = tomlkit.table()
            cleaned_values = {}
            for key, value in values.items():
                if value is None:
                    cleaned_values[key] = ""
                elif isinstance(value, dict):
                    # Handle nested dictionaries
                    cleaned_values[key] = {k: "" if v is None else v for k, v in value.items()}
                elif isinstance(value, list):
                    # Handle lists - ensure no None values in lists
                    cleaned_values[key] = [item if item is not None else "" for item in value]
                else:
                    cleaned_values[key] = value
            
            # Add each key-value pair to the table individually
            for key, value in cleaned_values.items():
                table.add(key, value)
            
            doc.add(section, table)
        try:    
            with open(self.config_path, "w") as f:
                f.write(tomlkit.dumps(doc))
        except PermissionError:
            # Deliberately no chmod here. Widening the file to 0666 to get one
            # write through would leave the broker and Miniserver passwords
            # readable and writable for every account on the host, permanently,
            # to fix something temporary - and it only ever works when the file
            # is ours, in which case the obstacle was somewhere else anyway.
            logger.error(self._permission_hint())
            logger.error(
                "The configuration was NOT written. The relay keeps running with the "
                "values it already has, but the change is lost on the next restart."
            )
        except Exception as e:
            logger.error(f"Error saving config: {e}")

    def _permission_hint(self) -> str:
        """Who we are, who owns the file, and how to reconcile the two."""
        message = f"No write permission for {self.config_path}"
        try:
            info = os.stat(self.config_path)
            # getlogin() is not used on purpose: it raises without a controlling
            # terminal, which is exactly the container case this describes.
            message += (
                f" - running as uid {os.getuid()}, gid {os.getgid()}, "
                f"file owned by uid {info.st_uid}, gid {info.st_gid}. "
                f"Fix with: chown {os.getuid()}:{os.getgid()} {self.config_path}"
            )
        except Exception as e:
            message += f" ({e})"
        return message

    def update_field(self, field_name: str, value: Any, list_mode: Literal["set", "add", "remove"] = "set") -> None:
        self._apply_field(field_name, value, list_mode)
        self.save_config()

    def _apply_field(self, field_name: str, value: Any, list_mode: Literal["set", "add", "remove"]) -> None:
        section, _ = self._get_field_info(field_name)
        current_value = getattr(getattr(self._config, section.value), field_name)

        if isinstance(current_value, (list, set)):
            if isinstance(current_value, set):
                if list_mode == "set":
                    new_value = set(value) if isinstance(value, (list, set)) else {value}
                elif list_mode == "add":
                    new_value = current_value | (set(value) if isinstance(value, (list, set)) else {value})
                elif list_mode == "remove":
                    new_value = current_value - (set(value) if isinstance(value, (list, set)) else {value})
            else:  # list type
                if list_mode == "set":
                    new_value = list(value) if isinstance(value, list) else [value]
                elif list_mode == "add":
                    # dict.fromkeys dedupes but keeps insertion order; a set would
                    # scramble the user's config file on every add.
                    new_value = list(dict.fromkeys(current_value + (value if isinstance(value, list) else [value])))
                elif list_mode == "remove":
                    new_value = [item for item in current_value if item not in (value if isinstance(value, list) else [value])]
            value = new_value

        setattr(getattr(self._config, section.value), field_name, value)

    def update_fields(self, updates: Dict[str, Any], list_mode: Literal["set", "add", "remove"] = "set") -> None:
        """Apply a batch of updates, or none of them.

        This is what the MQTT control topics call, so everything is checked
        before the first field is touched: a rejected field must not leave the
        ones before it applied and written out, and a value the Rust side
        cannot read must never reach the file at all - the update triggers a
        restart, and the relay would not come back up.
        """
        self._reject_unusable(updates)
        for field_name, value in updates.items():
            self._apply_field(field_name, value, list_mode)
        self.save_config()

    def _reject_unusable(self, updates: Dict[str, Any]) -> None:
        problems: List[str] = []
        for field_name, value in updates.items():
            if field_name in REMOTE_PROTECTED_FIELDS:
                problems.append(f"'{field_name}' cannot be changed remotely")
                continue
            try:
                _, field_type = self._get_field_info(field_name)
            except ValueError as e:
                problems.append(str(e))
                continue
            if problem := _type_mismatch(field_name, field_type, value):
                problems.append(problem)
        if problems:
            raise ConfigError(f"Rejected configuration update: {'; '.join(problems)}")

    def update_config(self, section: ConfigSection, updates: Dict[str, Any], list_mode: Literal["set", "add", "remove"] = "set") -> None:
        section_config = getattr(self._config, section.value)
        for field_name, value in updates.items():
            current_value = getattr(section_config, field_name)
            if isinstance(current_value, (list, set)):
                if isinstance(current_value, set):
                    if list_mode == "set":
                        new_value = set(value) if isinstance(value, (list, set)) else {value}
                    elif list_mode == "add":
                        new_value = current_value | (set(value) if isinstance(value, (list, set)) else {value})
                    elif list_mode == "remove":
                        new_value = current_value - (set(value) if isinstance(value, (list, set)) else {value})
                else:  # list type
                    if list_mode == "set":
                        new_value = list(value) if isinstance(value, list) else [value]
                    elif list_mode == "add":
                        # dict.fromkeys dedupes but keeps insertion order; a set would
                        # scramble the user's config file on every add.
                        new_value = list(dict.fromkeys(current_value + (value if isinstance(value, list) else [value])))
                    elif list_mode == "remove":
                        new_value = [item for item in current_value if item not in (value if isinstance(value, list) else [value])]
                updates[field_name] = new_value
        setattr(self._config, section.value, replace(section_config, **updates))
        self.save_config()

    def _get_field_info(self, field_name: str) -> tuple[ConfigSection, type]:
        if field_name not in self.field_mappings:
            raise ValueError(f"Unknown configuration field: {field_name}")
        return self.field_mappings[field_name]

    @staticmethod
    def _map_fields_to_sections() -> Dict[str, tuple[ConfigSection, type]]:
        mappings = {}
        for section in ConfigSection:
            config_class = globals()[section.value.capitalize() + "Config"]
            for field_name, field_type in get_type_hints(config_class).items():
                mappings[field_name] = (section, field_type)
        return mappings

    def shutdown(self):
        self.save_config()

    @property
    def general(self) -> GeneralConfig:
        return self._config.general

    @property
    def broker(self) -> BrokerConfig:
        return self._config.broker

    @property
    def miniserver(self) -> MiniserverConfig:
        return self._config.miniserver

    @property
    def topics(self) -> TopicsConfig:
        return self._config.topics

    @property
    def processing(self) -> ProcessingConfig:
        return self._config.processing

    @property
    def udp(self) -> UdpConfig:
        return self._config.udp

    @property
    def debug(self) -> DebugConfig:
        return self._config.debug

    def get_safe_config(self) -> Dict[str, Any]:
        """Return a copy of the config with sensitive data removed."""
        config_dict = self._config.to_dict()
        
        # Remove sensitive broker data
        if 'broker' in config_dict:
            broker = config_dict['broker'].copy()
            broker.pop('user', None)
            broker.pop('password', None)
            config_dict['broker'] = broker
        
        # Remove miniserver credentials
        if 'miniserver' in config_dict:
            miniserver = config_dict['miniserver'].copy()
            miniserver.pop('miniserver_user', None)
            miniserver.pop('miniserver_pass', None)
            config_dict['miniserver'] = miniserver
            
        return config_dict

global_config = Config()

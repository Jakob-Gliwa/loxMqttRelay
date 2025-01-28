import os
import logging
from dataclasses import dataclass, field, asdict, replace
import threading
from typing import Dict, Any, List, Optional, Union, Literal, get_type_hints, Set
import tomlkit
from enum import Enum

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

@dataclass
class ProcessingConfig:
    expand_json: bool = True
    convert_booleans: bool = True

@dataclass
class UdpConfig:
    udp_in_port: int = 11884

@dataclass
class DebugConfig:
    publish_processed_topics: bool = False
    publish_forwarded_topics: bool = False
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
        return {section: asdict(getattr(self, section)) for section in self.__annotations__}

    @classmethod
    def from_dict(cls, config_dict: Dict[str, Any]) -> "AppConfig":
        return cls(**{section: cls._create_section(section, config_dict) for section in cls.__annotations__})

    @staticmethod
    def _create_section(section: str, config_dict: Dict[str, Any]) -> Any:
        section_class = globals().get(section.capitalize() + "Config")
        if section_class is None:
            raise ConfigError(f"Invalid configuration section: {section}")
        return section_class(**config_dict.get(section, {}))

class ConfigError(Exception):
    pass

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
            
        with open(self.config_path, "w") as f:
            f.write(tomlkit.dumps(doc))

    def update_field(self, field_name: str, value: Any, list_mode: Literal["set", "add", "remove"] = "set") -> None:
        section, field_type = self._get_field_info(field_name)
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
                    new_value = list(set(current_value + (value if isinstance(value, list) else [value])))
                elif list_mode == "remove":
                    new_value = [item for item in current_value if item not in (value if isinstance(value, list) else [value])]
            value = new_value

        setattr(getattr(self._config, section.value), field_name, value)
        self.save_config()

    def update_fields(self, updates: Dict[str, Any], list_mode: Literal["set", "add", "remove"] = "set") -> None:
        for field_name, value in updates.items():
            self.update_field(field_name, value, list_mode)

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
                        new_value = list(set(current_value + (value if isinstance(value, list) else [value])))
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

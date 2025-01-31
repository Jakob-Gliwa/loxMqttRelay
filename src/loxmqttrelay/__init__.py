"""
MQTT Relay for Loxone

This package provides a bridge between MQTT and Loxone Miniserver, allowing bidirectional
communication between MQTT topics and Loxone controls.
"""
__version__ = "0.1.0"
from loxmqttrelay.config import global_config
from loxmqttrelay._loxmqttrelay import (
    MiniserverDataProcessor,
    init_rust_logger
)
from .utils import setup_logging
from .miniserver_data_processor import MiniserverDataProcessor
from .config import AppConfig, GeneralConfig, TopicsConfig, ProcessingConfig, DebugConfig
# Only expose the version number at package level
# Let modules import directly from specific files to avoid circular dependencies

setup_logging()

__all__ = [
    'global_config',
    'MiniserverDataProcessor',
    'init_rust_logger'
]
"""
MQTT Relay for Loxone

This package provides a bridge between MQTT and Loxone Miniserver, allowing bidirectional
communication between MQTT topics and Loxone controls.
"""
__version__ = "0.1.0"
from .config import global_config
from .utils import setup_logging
# Only expose the version number at package level
# Let modules import directly from specific files to avoid circular dependencies

setup_logging()
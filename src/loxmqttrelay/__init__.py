"""
MQTT Relay for Loxone

This package provides a bridge between MQTT and Loxone Miniserver, allowing bidirectional
communication between MQTT topics and Loxone controls.
"""
__version__ = "0.1.0"

from loxmqttrelay.utils import prefer_optimized_build

# Pick the native build from a single, centralized CPU decision. On an AVX2 host
# the optimized extension MUST be present (the build verifies this) — no silent
# fallback, so a packaging bug surfaces loudly instead of being masked.
# The selection is recorded here (not logged: this runs before logging is set
# up) and reported once at startup by utils.log_runtime_environment().
if prefer_optimized_build():
    from loxmqttrelay.optimized._loxmqttrelay import (
        MiniserverDataProcessor,
        init_rust_logger
    )
    ACTIVE_RUST_BUILD = "optimized"
    ACTIVE_RUST_MODULE = "loxmqttrelay.optimized._loxmqttrelay"
else:
    from loxmqttrelay.compatible._loxmqttrelay import (
        MiniserverDataProcessor,
        init_rust_logger
    )
    ACTIVE_RUST_BUILD = "compatible"
    ACTIVE_RUST_MODULE = "loxmqttrelay.compatible._loxmqttrelay"

from loxmqttrelay.config import global_config
from .utils import setup_logging
from .config import AppConfig, GeneralConfig, TopicsConfig, ProcessingConfig, DebugConfig
# Only expose the version number at package level
# Let modules import directly from specific files to avoid circular dependencies

setup_logging()

__all__ = [
    'global_config',
    'MiniserverDataProcessor',
    'init_rust_logger',
    'ACTIVE_RUST_BUILD',
    'ACTIVE_RUST_MODULE',
]
"""
MQTT Relay for Loxone

This package provides a bridge between MQTT and Loxone Miniserver, allowing bidirectional
communication between MQTT topics and Loxone controls.
"""
__version__ = "0.1.0"

from loxmqttrelay.utils import prefer_optimized_build, setup_logging
from loxmqttrelay.logging_config import get_lazy_logger

# Configure logging BEFORE loading any native extension. Loading a native module
# can hard-crash the process (e.g. SIGILL on a CPU missing an instruction the
# build assumed), which Python cannot catch. By logging right before each native
# load, the LAST log line pinpoints exactly which load triggered such a crash.
setup_logging()
logger = get_lazy_logger(__name__)

# Pick the native build from a single, centralized CPU decision. On an AVX2 host
# the optimized extension MUST be present (the build verifies this) — no silent
# fallback, so a packaging bug surfaces loudly instead of being masked.
if prefer_optimized_build():
    logger.info("Loading Rust extension: optimized build (x86_64 + AVX2) ...")
    from loxmqttrelay.optimized._loxmqttrelay import (
        MiniserverDataProcessor,
        MqttClient,
        UdpServer,
        init_rust_logger
    )
    ACTIVE_RUST_BUILD = "optimized"
    ACTIVE_RUST_MODULE = "loxmqttrelay.optimized._loxmqttrelay"
else:
    logger.info("Loading Rust extension: compatible build (arm64 or no AVX2) ...")
    from loxmqttrelay.compatible._loxmqttrelay import (
        MiniserverDataProcessor,
        MqttClient,
        UdpServer,
        init_rust_logger
    )
    ACTIVE_RUST_BUILD = "compatible"
    ACTIVE_RUST_MODULE = "loxmqttrelay.compatible._loxmqttrelay"
logger.info("Rust extension loaded: %s build", ACTIVE_RUST_BUILD)

from loxmqttrelay.config import global_config
from .config import AppConfig, GeneralConfig, TopicsConfig, ProcessingConfig
# Only expose the version number at package level
# Let modules import directly from specific files to avoid circular dependencies

__all__ = [
    'global_config',
    'MiniserverDataProcessor',
    'MqttClient',
    'UdpServer',
    'init_rust_logger',
    'ACTIVE_RUST_BUILD',
    'ACTIVE_RUST_MODULE',
]
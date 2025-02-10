"""
MQTT Relay for Loxone

This package provides a bridge between MQTT and Loxone Miniserver, allowing bidirectional
communication between MQTT topics and Loxone controls.
"""
__version__ = "0.1.0"
import platform
import subprocess
import logging

logger = logging.getLogger(__name__)

# Determine which implementation to use based on CPU architecture and features
if "arm" in platform.machine().lower():
    from loxmqttrelay.compatible._loxmqttrelay import (
        MiniserverDataProcessor,
        init_rust_logger
    )
    logger.info("Using ARM compatible implementation")
else:
    system = platform.system()
    try:
        if system == "Linux":
            output = subprocess.check_output("lscpu", shell=True, text=True)
        elif system == "Darwin":  # macOS
            output = subprocess.check_output("sysctl -a | grep machdep.cpu", shell=True, text=True)
        elif system == "Windows":
            output = subprocess.check_output("wmic cpu get Caption", shell=True, text=True)
            
        if output and ("avx" in output.lower() and "avx2" in output.lower()):
            from loxmqttrelay.optimized._loxmqttrelay import (
                MiniserverDataProcessor,
                init_rust_logger
            )
            logger.info("Using optimized implementation with AVX/AVX2 support")
        else:
            from loxmqttrelay.compatible._loxmqttrelay import (
                MiniserverDataProcessor,
                init_rust_logger
            )
            logger.info("Using compatible implementation (AVX/AVX2 not detected)")

    except subprocess.CalledProcessError:
        logger.error("Error checking CPU features. Using compatible implementation.")
        from loxmqttrelay.compatible._loxmqttrelay import (
            MiniserverDataProcessor,
            init_rust_logger
        )

from loxmqttrelay.config import global_config
from .utils import setup_logging
from .config import AppConfig, GeneralConfig, TopicsConfig, ProcessingConfig, DebugConfig
# Only expose the version number at package level
# Let modules import directly from specific files to avoid circular dependencies

setup_logging()

__all__ = [
    'global_config',
    'MiniserverDataProcessor',
    'init_rust_logger'
]
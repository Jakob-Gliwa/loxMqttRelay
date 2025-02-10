from setuptools import find_packages, setup
from setuptools_rust import Binding, RustExtension
import platform
import logging

# Logging konfigurieren
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

# Basis-Setup-Parameter
base_setup = {
    "name": "loxmqttrelay",
    "version": "1.0",
    "packages": find_packages(where="src"),
    "package_dir": {"": "src"},
}

# Plattform bestimmen
arch = platform.uname().machine.lower()
logger.info(f"Detected platform: {arch}")

rust_extensions = []

# AMD64 optimierte und kompatible Builds
if arch in ("x86_64", "amd64"):
    logger.info("Building for AMD64 architecture - optimized & compatible versions")
    
    rust_extensions.append(
        RustExtension(
            "loxmqttrelay.optimized._loxmqttrelay",
            path="Cargo.toml",
            binding=Binding.PyO3,
            rustc_flags=["-C", "opt-level=3", "-C", "target-cpu=native"]
        )
    )
    
    rust_extensions.append(
        RustExtension(
            "loxmqttrelay.compatible._loxmqttrelay",
            path="Cargo.toml",
            binding=Binding.PyO3,
            rustc_flags=["-C", "opt-level=2", "-C", "target-cpu=generic"]
        )
    )
else:
    logger.info("Building for non-AMD64 architecture - compatible version only")
    rust_extensions.append(
        RustExtension(
            "loxmqttrelay.compatible._loxmqttrelay",
            path="Cargo.toml",
            binding=Binding.PyO3,
            rustc_flags=["-C", "opt-level=2", "-C", "target-cpu=generic"]
        )
    )
# Setup-Aufruf mit allen Extensions in einer Liste
setup(
    **base_setup,
    rust_extensions=rust_extensions
)
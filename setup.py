from setuptools import find_packages, setup
from setuptools_rust import Binding, RustExtension
import platform
import logging

# Logging konfigurieren
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)


################################################################################
# Rust Setup
################################################################################

# No version here: pyproject.toml declares it, and check_version_parity.py holds
# that one to Cargo.toml and __init__.py. A second one would win over all three.
base_setup = {
    "name": "loxmqttrelay",
    "packages": find_packages(where="src"),
    "package_dir": {"": "src"},
}

# Plattform bestimmen
arch = platform.uname().machine.lower()
logger.info(f"Detected platform: {arch}")

rust_extensions = []

# Off in Cargo.toml so `cargo test` can link a harness against libpython; every
# build that ships has to ask for it back.
PYO3_FEATURES = ["extension-module"]

# AMD64 optimierte und kompatible Builds
if arch in ("x86_64", "amd64"):
    logger.info("Building for AMD64 architecture - optimized & compatible versions")
    
    rust_extensions.append(
        RustExtension(
            "loxmqttrelay.optimized._loxmqttrelay",
            path="Cargo.toml",
            binding=Binding.PyO3,
            features=PYO3_FEATURES,
            # x86-64-v3 (AVX2/FMA/BMI) instead of native: portable across ALL
            # AVX2-capable hosts. "native" tunes to the build machine's CPU and
            # can crash with SIGILL on other amd64 CPUs in a distributed image.
            rustc_flags=["-C", "opt-level=3", "-C", "target-cpu=x86-64-v3"]
        )
    )
    
    rust_extensions.append(
        RustExtension(
            "loxmqttrelay.compatible._loxmqttrelay",
            path="Cargo.toml",
            binding=Binding.PyO3,
            features=PYO3_FEATURES,
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
            features=PYO3_FEATURES,
            rustc_flags=["-C", "opt-level=2", "-C", "target-cpu=generic"]
        )
    )

################################################################################
# Combined Setup call (Rust extensions only)
################################################################################

setup(
    **base_setup,
    rust_extensions=rust_extensions,
    zip_safe=False,
)
from setuptools import find_packages, setup
from setuptools_rust import Binding, RustExtension
import platform
import logging
from setuptools.extension import Extension
from Cython.Build import cythonize
import shutil
import os
from setuptools.command.build_ext import build_ext

# Logging konfigurieren
logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)


################################################################################
# Rust Setup
################################################################################

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

################################################################################
# Cython Setup
################################################################################

# Definiere zwei Build-Varianten: optimized & compatible
build_variants = {
    "optimized": ["-O3", "-march=native", "-ffast-math"],
    "compatible": ["-O2", "-mtune=generic"]
}

cython_extensions = []
for variant, compile_args in build_variants.items():
    ext_name = f"loxwebsocket.cython_modules.extractor_{variant}"  # Updated package path
    pyx_original = "src/loxwebsocket/cython_modules/extractor.pyx"
    pyx_variant = f"src/loxwebsocket/cython_modules/extractor_{variant}.pyx" # Dumb hack to force a separate build because by god i cant get it to not cache the previous compilation...
    # Copy the original .pyx to a variant-specific file to force a separate build.
    shutil.copyfile(pyx_original, pyx_variant)

    ext = Extension(
        ext_name,
        sources=[pyx_variant],  # Use the variant-specific source file
        extra_compile_args=compile_args,
        extra_link_args=compile_args,
        define_macros=[("CYTHON_BUILD_VARIANT", f'"{variant}"')]
    )
    cy_ext = cythonize(
        ext,
        force=True,
        cache=False,
        language_level="3",
        compiler_directives={
            'boundscheck': False,
            'wraparound': False,
            'cdivision': True,
            'nonecheck': False,
            'initializedcheck': False,
            'embedsignature': False,
        }
    )
    cython_extensions.extend(cy_ext)

################################################################################
# Combined Setup call for both Rust and Cython parts
################################################################################

class CleanUpBuildExt(build_ext):
    def run(self):
        # Run the standard build_ext command
        super().run()
        # Remove the variant-specific .pyx files after compilation
        for variant in ("optimized", "compatible"):
            variant_file = os.path.join("src", "loxwebsocket", "cython_modules", f"extractor_{variant}.pyx")
            if os.path.exists(variant_file):
                os.remove(variant_file)
                print(f"Removed variant-specific file: {variant_file}")

cmdclass = {"build_ext": CleanUpBuildExt}

setup(
    **base_setup,
    rust_extensions=rust_extensions,
    ext_modules=cython_extensions,
    cmdclass=cmdclass,
    zip_safe=False,
)
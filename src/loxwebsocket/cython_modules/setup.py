import os
import logging
from setuptools import setup
from Cython.Build import cythonize
from setuptools.extension import Extension

logger = logging.getLogger(__name__)
logging.basicConfig(level=logging.INFO)

# Definiere zwei Build-Varianten: optimized & compatible
build_variants = {
    "optimized": ["-O3", "-march=native", "-ffast-math"],
    "compatible": ["-O2", "-mtune=generic"]
}

extensions = []
for variant, compile_args in build_variants.items():
    ext_name = f"extractor_{variant}"  # Unterschiedlicher Name für jede Variante
    ext = Extension(
        ext_name,
        ["extractor.pyx"],
        extra_compile_args=compile_args,
        extra_link_args=compile_args
    )
    # Compile each extension individually to avoid sorting issues
    cy_ext = cythonize(
        ext,
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
    extensions.extend(cy_ext)

setup(
    name="extractor",
    ext_modules=extensions,
    zip_safe=False,
)
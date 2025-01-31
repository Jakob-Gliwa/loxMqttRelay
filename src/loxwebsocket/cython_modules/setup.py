# setup.py

import os
from setuptools import setup
from Cython.Build import cythonize
from setuptools.extension import Extension

# Get optimization flags from environment or use default
optimization_flags = os.environ.get('CFLAGS', '-O3 -march=native -ffast-math').split()

extensions = [
    Extension(
        "extractor",
        ["extractor.pyx"],
        extra_compile_args=optimization_flags,
        extra_link_args=["-O3"]
    )
]

setup(
    name="extractor",
    ext_modules=cythonize(
        extensions,
        language_level="3",
        compiler_directives={
            'boundscheck': False,
            'wraparound': False,
            'cdivision': True,
            'nonecheck': False,
            'initializedcheck': False,
            'embedsignature': False,
        }
    ),
    zip_safe=False,
)
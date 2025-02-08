# setup.py

import os
import logging
from setuptools import setup
from Cython.Build import cythonize
from setuptools.extension import Extension

logger = logging.getLogger(__name__)
# Set up basic logging configuration
logging.basicConfig(level=logging.INFO)

# Read optimization flags from environment, if available.
cython_opt_flags = os.environ.get("CYTHON_OPT_FLAGS")
logger.info(f"All environment variables: {dict(os.environ)}")
logger.info(f"CYTHON_OPT_FLAGS value: {cython_opt_flags!r}")

if cython_opt_flags:
    logger.info(f"Using CYTHON_OPT_FLAGS: {cython_opt_flags}")
    compile_args = cython_opt_flags.split()  # convert string flags into a list
else:
    logger.warning("No CYTHON_OPT_FLAGS environment variable found. Using default optimization flags.")
    compile_args = ["-O3", "-march=native", "-ffast-math"]  # fallback default

extensions = [
    Extension(
        "extractor",
        ["extractor.pyx"],
        extra_compile_args=compile_args,  # use flags from env or default
        extra_link_args=compile_args        # apply same optimization flags for linking
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
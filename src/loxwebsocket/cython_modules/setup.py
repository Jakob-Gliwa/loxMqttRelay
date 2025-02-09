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
cython_opt_flags = os.environ.get("CYTHON_OPT_FLAGS", "")
logger.info(f"All environment variables: {dict(os.environ)}")
logger.info(f"CYTHON_OPT_FLAGS value: {cython_opt_flags!r}")

# Clean up the flags - remove any surrounding quotes
if cython_opt_flags:
    # Remove surrounding quotes if present
    cython_opt_flags = cython_opt_flags.strip('"\'')
    logger.info(f"Using CYTHON_OPT_FLAGS: {cython_opt_flags}")
    compile_args = cython_opt_flags.split()
else:
    logger.warning("No CYTHON_OPT_FLAGS environment variable found. Using default optimization flags.")
    compile_args = ["-O3", "-march=native", "-ffast-math"]

extensions = [
    Extension(
        "extractor",
        ["extractor.pyx"],
        extra_compile_args=compile_args,
        extra_link_args=compile_args
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
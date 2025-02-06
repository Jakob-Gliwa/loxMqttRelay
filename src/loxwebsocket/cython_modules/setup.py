# setup.py

from setuptools import setup
from Cython.Build import cythonize
from setuptools.extension import Extension

extensions = [
    Extension(
        "extractor",
        ["extractor.pyx"],
        extra_compile_args=["-O3", "-march=native", "-ffast-math"],  # Maximum optimization
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
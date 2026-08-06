import argparse
import functools
import logging
import os
import platform
import subprocess
import time
import sys
from importlib import metadata
from typing import Callable, TypeVar, ParamSpec, Optional
from loxmqttrelay.config import global_config
from loxmqttrelay.logging_config import get_lazy_logger, set_log_level

T = TypeVar('T')
P = ParamSpec('P')


################################################################################
# CPU / architecture detection
#
# Decides which Rust extension to load (optimized vs compatible). pygixml is no
# longer gated here: it ships as a portable build and is instead verified at
# runtime by native_import_runs() below.
#
# Dependency-free and avoids external tools like ``lscpu`` (absent from slim
# images): reads ``/proc/cpuinfo`` on Linux and ``sysctl`` on macOS, defaulting
# to False when in doubt so we never pick a path the CPU cannot run.
################################################################################

_X86_TOKENS = frozenset({"x86_64", "amd64", "i386", "i686", "x86"})


@functools.lru_cache(maxsize=1)
def machine() -> str:
    return platform.machine().lower()


@functools.lru_cache(maxsize=1)
def is_x86() -> bool:
    return machine() in _X86_TOKENS


@functools.lru_cache(maxsize=1)
def has_avx2() -> bool:
    """Whether this (x86) CPU supports AVX2. Always False on non-x86."""
    if not is_x86():
        return False

    system = platform.system()
    try:
        if system == "Linux":
            with open("/proc/cpuinfo", "r", encoding="utf-8", errors="ignore") as fh:
                text = fh.read().lower()
        elif system == "Darwin":
            text = subprocess.check_output(["sysctl", "-a"], text=True).lower()
        elif system == "Windows":
            text = subprocess.check_output(
                "wmic cpu get Caption", shell=True, text=True
            ).lower()
        else:
            return False
    except (OSError, subprocess.SubprocessError):
        return False

    return "avx2" in text


def prefer_optimized_build() -> bool:
    """Load the AVX2-optimized native build instead of the generic one.

    True only on x86_64 hosts with AVX2; arm64 and AVX2-less x86 use compatible.
    """
    return is_x86() and has_avx2()


@functools.lru_cache(maxsize=None)
def native_import_runs(probe_code: str) -> bool:
    """True iff *probe_code* runs to completion in a fresh subprocess on THIS CPU.

    Gate for native deps that ship CPU-specific code with NO runtime dispatch:
    pygixml is compiled per build host (``-march=native``/AVX2), so on a CPU
    missing an instruction it was built for even importing it raises SIGILL
    (exit 132) and takes the whole process down — uncatchable in-process. Running
    the risky import in a throwaway child turns that hard crash into a boolean,
    so we can fall back (e.g. to lxml) instead of dying. Result is cached.
    """
    try:
        proc = subprocess.run(
            [sys.executable, "-c", probe_code],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return proc.returncode == 0


def log_performance(name: Optional[str] = None, severity: Optional[int] = logging.DEBUG):
    """
    A decorator that logs the execution time of a function if logging level is DEBUG or lower.
    
    Args:
        name: Optional name to use in the log message. If not provided, uses the function name.
    """
    def decorator(func: Callable[P, T]) -> Callable[P, T]:
        @functools.wraps(func)
        def wrapper(*args: P.args, **kwargs: P.kwargs) -> T:
            logger = get_lazy_logger(func.__module__)
            
            operation_name = name or func.__name__
            start_time = time.perf_counter_ns()
            
            try:
                result = func(*args, **kwargs)
                end_time = time.perf_counter_ns()
                duration_ns = (end_time - start_time)
                logger.log(severity or logging.DEBUG, f"Performance: {operation_name} took {duration_ns:.2f}ns")
                return result
            except Exception as e:
                end_time = time.perf_counter_ns()
                duration_ns = (end_time - start_time)
                logger.debug(f"Performance: {operation_name} failed after {duration_ns:.2f}ns with error: {str(e)}")
                raise
                
        return wrapper
    return decorator


_parser = argparse.ArgumentParser(description="MQTT Relay")
_args = None

def get_args() -> argparse.Namespace:
    global _args, _parser
    if _args is None:
        _parser.add_argument(
            "--log-level",
            type=str,
            choices=["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"],
            help="Set the logging level (overrides config.json setting)"
        )

        # When running tests, ignore unknown arguments
        if 'pytest' in sys.modules:
            _args, _ = _parser.parse_known_args()
        else:
            _args = _parser.parse_args()
    return _args


def setup_logging():
    """Initialize logging with command line arguments and config."""
    args = get_args()
    # Priority: CLI args > Environment variable > Config file
    if args.log_level:
        log_level = args.log_level.upper()
    elif env_level := os.getenv("LOG_LEVEL"):
        log_level = env_level.upper()
    else:
        log_level = global_config.general.log_level.upper()
    
    # The config file is validated, but LOG_LEVEL and --log-level are not, and a
    # typo used to land on DEBUG - the loudest level, and the one that logs
    # every payload.
    level = getattr(logging, log_level, None)
    if not isinstance(level, int):
        logging.warning(f"Unknown log level '{log_level}', using INFO")
        level = logging.INFO
    logging.basicConfig(
        level=level,
        format='%(asctime)s %(levelname)s [%(name)s] %(message)s'
    )
    set_log_level(level)


################################################################################
# Startup diagnostics
#
# Reports, once and early in main() (after logging is configured), which native
# code paths are actually active. The decisions themselves are made at import
# time in __init__ / miniserver_sync (before logging exists, so they only record
# facts); this is the single place that logs them. If the process later
# misbehaves, the banner pins down the exact build/parser combination in use.
################################################################################

# Dependencies worth pinning down when triaging crashes / SIGILLs.
_KEY_PACKAGES = (
    "aiohttp",
    "orjson",
    "pycryptodome",
    "uvloop",
    "lxml",
    "pygixml",
    "lz4",
    "loxwebsocket",
)


def _module_path(dotted: Optional[str]) -> str:
    if not dotted:
        return "n/a"
    module = sys.modules.get(dotted)
    return getattr(module, "__file__", None) or "n/a"


def _dependency_versions() -> str:
    parts = []
    for name in _KEY_PACKAGES:
        try:
            parts.append(f"{name}={metadata.version(name)}")
        except metadata.PackageNotFoundError:
            parts.append(f"{name}=<not installed>")
    return ", ".join(parts)


def log_runtime_environment() -> None:
    """Emit the startup environment banner at INFO level."""
    # Imported lazily to read the facts recorded at import time and avoid an
    # import cycle (this module is imported very early during package init).
    import loxmqttrelay
    from loxmqttrelay import miniserver_sync

    logger = get_lazy_logger(__name__)
    logger.info("----- loxMqttRelay runtime environment -----")
    logger.info(
        "version=%s  python=%s  executable=%s",
        loxmqttrelay.__version__,
        platform.python_version(),
        sys.executable,
    )
    logger.info(
        "platform=%s %s  arch=%s  (x86=%s, avx2=%s)",
        platform.system(),
        platform.release(),
        platform.machine(),
        is_x86(),
        has_avx2(),
    )
    logger.info(
        "rust extension: %s build  ->  %s",
        loxmqttrelay.ACTIVE_RUST_BUILD,
        _module_path(loxmqttrelay.ACTIVE_RUST_MODULE),
    )
    logger.info(
        "xml parser: %s  (%s)",
        miniserver_sync.ACTIVE_XML_PARSER,
        miniserver_sync.XML_PARSER_REASON,
    )
    # The values the processor bakes in at construction. Reported here because a
    # relay that silently ignored one of them is indistinguishable from a
    # misconfigured one when reading a bug report.
    logger.info(
        "processing: expand_json=%s  convert_booleans=%s",
        global_config.processing.expand_json,
        global_config.processing.convert_booleans,
    )
    logger.info(
        "topic filters: subscriptions=%d  subscription_filters=%d  do_not_forward=%d  whitelist=%d",
        len(global_config.topics.subscriptions),
        len(global_config.topics.subscription_filters),
        len(global_config.topics.do_not_forward),
        len(global_config.topics.topic_whitelist),
    )
    logger.info("dependencies: %s", _dependency_versions())
    logger.info("--------------------------------------------")
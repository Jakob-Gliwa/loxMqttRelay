"""
LazyLogger for loxMqttRelay.

Provides a logger wrapper with cached level checks.
Level is set once at startup and never changes.
"""

import logging
from typing import Optional

# Effective log level - set once at startup by setup_logging()
_log_level: int = logging.INFO


def set_log_level(level: int):
    """Set the global log level. Called by setup_logging()."""
    global _log_level
    _log_level = level


class LazyLogger:
    """Logger wrapper with fast level checks using global cached level."""
    
    __slots__ = ('_logger',)
    
    def __init__(self, logger: logging.Logger):
        self._logger = logger
    
    def debug(self, msg: str, *args, **kwargs):
        if _log_level <= logging.DEBUG:
            self._logger.debug(msg, *args, **kwargs)
    
    def info(self, msg: str, *args, **kwargs):
        if _log_level <= logging.INFO:
            self._logger.info(msg, *args, **kwargs)
    
    def warning(self, msg: str, *args, **kwargs):
        if _log_level <= logging.WARNING:
            self._logger.warning(msg, *args, **kwargs)
    
    def error(self, msg: str, *args, **kwargs):
        if _log_level <= logging.ERROR:
            self._logger.error(msg, *args, **kwargs)
    
    def exception(self, msg: str, *args, **kwargs):
        if _log_level <= logging.ERROR:
            self._logger.exception(msg, *args, **kwargs)
    
    def log(self, level: int, msg: str, *args, **kwargs):
        if _log_level <= level:
            self._logger.log(level, msg, *args, **kwargs)


_lazy_loggers: dict[str, LazyLogger] = {}


def get_lazy_logger(name: Optional[str] = None) -> LazyLogger:
    """Get a LazyLogger instance. Drop-in replacement for logging.getLogger()."""
    cache_key = name or "__root__"
    if cache_key not in _lazy_loggers:
        logger = logging.getLogger(name) if name else logging.getLogger()
        _lazy_loggers[cache_key] = LazyLogger(logger)
    return _lazy_loggers[cache_key]


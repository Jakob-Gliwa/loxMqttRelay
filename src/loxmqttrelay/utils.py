import argparse
import functools
import logging
import time
import sys
from typing import Callable, TypeVar, ParamSpec
from loxmqttrelay.config import global_config

T = TypeVar('T')
P = ParamSpec('P')

from typing import Optional

def log_performance(name: Optional[str] = None, severity: Optional[int] = logging.DEBUG):
    """
    A decorator that logs the execution time of a function if logging level is DEBUG or lower.
    
    Args:
        name: Optional name to use in the log message. If not provided, uses the function name.
    """
    def decorator(func: Callable[P, T]) -> Callable[P, T]:
        @functools.wraps(func)
        def wrapper(*args: P.args, **kwargs: P.kwargs) -> T:
            logger = logging.getLogger(func.__module__)
            
            operation_name = name or func.__name__
            start_time = time.perf_counter_ns()
            
            try:
                result = func(*args, **kwargs)
                end_time = time.perf_counter_ns()
                duration_ms = (end_time - start_time)
                logger.log(severity or logging.DEBUG, f"Performance: {operation_name} took {duration_ms:.2f}ns")
                return result
            except Exception as e:
                end_time = time.perf_counter_ns()
                duration_ms = (end_time - start_time)
                logger.debug(f"Performance: {operation_name} failed after {duration_ms:.2f}ns with error: {str(e)}")
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
        _parser.add_argument(
            "--headless",
            action="store_true",
            help="Start the MQTT Relay without the UI"
        )
        
        # When running tests, ignore unknown arguments
        if 'pytest' in sys.modules:
            _args, _ = _parser.parse_known_args()
        else:
            _args = _parser.parse_args()
    return _args

def setup_logging():
    args = get_args()
    # Set logging level - prioritize args over config
    log_level = args.log_level.upper() if args.log_level else global_config.general.log_level.upper()
    logging.basicConfig(
        level=getattr(logging, log_level, logging.DEBUG),
        format='%(asctime)s %(levelname)s [%(name)s] %(message)s'
    )
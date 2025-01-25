import os
import sys
import pytest
from loxmqttrelay.config import AppConfig, global_config

# Add the src directory to Python path
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../src')))

@pytest.fixture(autouse=True)
def reset_global_config():
    """Reset global_config to default state before and after each test"""
    original_config = global_config._config
    global_config._config = AppConfig()
    yield
    global_config._config = original_config

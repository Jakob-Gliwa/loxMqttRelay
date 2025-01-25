import streamlit as st
import tomlkit
from tomlkit.toml_document import TOMLDocument
from tomlkit.exceptions import ParseError
from pathlib import Path
import os
import sys
import asyncio
import logging
import time
from gmqtt import Client as MQTTClient
from gmqtt import constants as MQTTConstants

# Configure logging
logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

# Log UI startup
logger.info("Starting Streamlit UI on localhost:8501")

st.set_page_config(
    page_title="MQTT Relay Config",
    page_icon="🔧",
    layout="wide"
)

# Initialize session state
if 'config_data' not in st.session_state:
    st.session_state.config_data = None
if 'config_path' not in st.session_state:
    st.session_state.config_path = None
# Initialize default values
initial_values = {
    'broker': {
        'host': 'localhost',
        'port': 1883,
        'user': '',
        'password': '',
        'client_id': 'loxmqttrelay'
    },
    'general': {
        'base_topic': 'myrelay/',
        'log_level': 'INFO',
        'cache_size': 100000
    },
    'udp': {
        'udp_in_port': 11884
    },
    'processing': {
        'expand_json': False,
        'convert_booleans': False
    },
    'miniserver': {
        'miniserver_ip': '127.0.0.1',
        'miniserver_port': 80,
        'miniserver_user': '',
        'miniserver_pass': '',
        'miniserver_max_parallel_connections': 5,
        'use_websocket': True,
        'sync_with_miniserver': False
    },
    'debug': {
        'mock_ip': '',
        'enable_mock': False,
        'publish_processed_topics': False,
        'publish_forwarded_topics': False
    },
    'topics': {
        'subscriptions': [],
        'subscription_filters': [],
        'do_not_forward': [],
        'topic_whitelist': []
    }
}

# Sidebar
st.sidebar.title("Settings")

# Default config path
default_config_path = Path('config/config.toml')

# Config path input in sidebar
config_path_input = st.sidebar.text_input(
    "Config File Path",
    value=str(default_config_path) if default_config_path.exists() else "",
    help="Path where the config.toml will be saved"
)

# Update session state with config path if valid
if config_path_input:
    try:
        path = Path(config_path_input)
        # Create parent directories if they don't exist
        if not path.parent.exists():
            path.parent.mkdir(parents=True, exist_ok=True)
            st.sidebar.success(f"Created directory: {path.parent}")
        st.session_state.config_path = path
    except Exception as e:
        st.sidebar.error(f"Invalid config path: {str(e)}")
        st.session_state.config_path = None
else:
    st.session_state.config_path = None

# Add the project root to Python path for imports
current_dir = Path(os.path.dirname(os.path.abspath(__file__)))
project_root = current_dir.parent.parent
if str(project_root) not in sys.path:
    sys.path.insert(0, str(project_root))

# Config file uploader in sidebar (for pre-filling UI only)
config_file = st.sidebar.file_uploader(
    "Upload config file to pre-fill UI",
    type=['toml'],
    help="Upload a config file to pre-fill the UI fields (will not overwrite the actual config file)"
)

# Load configuration logic
def load_config_data(config_content):
    """Load config and store passwords in session state"""
    try:
        config = tomlkit.loads(config_content)
        st.session_state.config_data = config
        return True
    except ParseError as e:
        st.sidebar.error(f"Error parsing TOML: {str(e)}")
        return False
    except Exception as e:
        st.sidebar.error(f"Error loading config: {str(e)}")
        return False

if config_file is not None:
    if load_config_data(config_file.getvalue().decode('utf-8')):
        st.sidebar.success("Config file loaded to pre-fill UI!")
elif st.session_state.config_path and st.session_state.config_path.exists():
    try:
        with open(st.session_state.config_path, 'r') as f:
            content = f.read()
            if not content:
                st.sidebar.warning("Config file is empty, using default values")
                load_config_data(tomlkit.dumps(initial_values))
            else:
                try:
                    # Try to parse TOML first to provide better error messages
                    tomlkit.loads(content)
                    if load_config_data(content):
                        st.sidebar.info(f"Loaded config from: {st.session_state.config_path}")
                except ParseError as e:
                    st.sidebar.error(f"Invalid TOML in config file: {str(e)}")
                    st.sidebar.warning("Using default values")
                    load_config_data(tomlkit.dumps(initial_values))
    except Exception as e:
        st.sidebar.error(f"Error reading config file: {str(e)}")
        st.sidebar.warning("Using default values")
        load_config_data(tomlkit.dumps(initial_values))

# Helper functions
import re
import ipaddress

def validate_ip_port(ip_str: str, allow_empty: bool = False, is_broker: bool = False) -> tuple[str, int | None]:
    """Validate IP address with optional port, returns (host, port)"""
    if not ip_str:
        if allow_empty:
            return '', None
        raise ValueError("IP address is required")
        
    # Handle IPv6 addresses with brackets
    if ip_str.startswith('['):
        if ']' not in ip_str:
            raise ValueError("Invalid IPv6 address format (missing closing bracket)")
        bracket_end = ip_str.index(']')
        ip = ip_str[1:bracket_end]
        port_part = ip_str[bracket_end + 1:]
        if port_part and not port_part.startswith(':'):
            raise ValueError("Invalid IPv6 address format (expected ':' after closing bracket)")
        port_str = port_part[1:] if port_part else None
    else:
        # Split IP and port for IPv4/hostname
        parts = ip_str.split(':')
        if len(parts) > 2:
            # Could be IPv6 without brackets
            try:
                ipaddress.ip_address(ip_str)
                ip = ip_str
                port_str = None
            except ValueError:
                raise ValueError("Invalid format. Use 'IP' or 'IP:port' or '[IPv6]:port'")
        else:
            ip = parts[0]
            port_str = parts[1] if len(parts) == 2 else None
    
    # Validate IP/hostname
    try:
        ipaddress.ip_address(ip)
    except ValueError:
        if ip.lower() != 'localhost':
            raise ValueError("Invalid IP address")
            
    # Parse port if present
    port = None
    if port_str:
        try:
            port = int(port_str)
            if not 1 <= port <= 65535:
                raise ValueError()
        except ValueError:
            raise ValueError("Port must be between 1 and 65535")
    
    # For broker, ensure we have a valid port
    if is_broker:
        if not port:
            # Use port from port field if not specified in host
            port = st.session_state.broker_port
        elif port != st.session_state.broker_port:
            # Warn if port in host differs from port field
            logger.warning(f"Using port {port} from host field instead of {st.session_state.broker_port} from port field")
        
        # Ensure we have a port set
        if not port:
            raise ValueError("Broker port is required")
            
    return ip, port

def validate_base_topic(topic: str) -> str:
    """Validate and normalize base topic"""
    # Remove whitespace
    topic = topic.strip()
    
    # Check for invalid characters
    invalid_chars = ['#', '+', '$']
    for char in invalid_chars:
        if char in topic:
            raise ValueError(f"Base topic cannot contain MQTT wildcards or system topics ({', '.join(invalid_chars)})")
    
    # Ensure it starts with a letter or number
    if not topic[0].isalnum():
        raise ValueError("Base topic must start with a letter or number")
    
    # Ensure it ends with a forward slash
    if not topic.endswith('/'):
        topic += '/'
    
    return topic

def save_config(save_path: Path):
    # Ensure directory exists
    save_path.parent.mkdir(parents=True, exist_ok=True)
    
    try:
        # Validate base topic
        base_topic = validate_base_topic(st.session_state.base_topic)
        
        # Validate IP addresses and ports
        broker_host, broker_port = validate_ip_port(st.session_state.broker_host, is_broker=True)
        miniserver_ip, _ = validate_ip_port(st.session_state.miniserver_ip, allow_empty=True)
        mock_miniserver_ip, _ = validate_ip_port(st.session_state.mock_miniserver_ip, allow_empty=True)
    except ValueError as e:
        st.session_state.save_status = e
        return None
    
    config = {
        'broker': {
            'host': broker_host,
            'port': broker_port,
            'user': st.session_state.broker_user,
            'password': st.session_state.broker_pass,
            'client_id': st.session_state.broker_client_id
        },
        'general': {
            'base_topic': base_topic,
            'log_level': st.session_state.log_level,
            'cache_size': st.session_state.cache_size
        },
        'udp': {
            'udp_in_port': st.session_state.udp_in_port
        },
        'processing': {
            'expand_json': st.session_state.expand_json,
            'convert_booleans': st.session_state.convert_booleans
        },
        'miniserver': {
            'miniserver_ip': miniserver_ip,
            'miniserver_port': st.session_state.miniserver_port,
            'miniserver_user': st.session_state.miniserver_user,
            'miniserver_pass': st.session_state.miniserver_pass,
            'miniserver_max_parallel_connections': st.session_state.miniserver_max_parallel_connections,
            'use_websocket': st.session_state.use_websocket,
            'sync_with_miniserver': st.session_state.sync_with_miniserver
        },
        'debug': {
            'mock_ip': mock_miniserver_ip,
            'enable_mock': st.session_state.enable_mock_miniserver,
            'publish_processed_topics': st.session_state.publish_processed_topics,
            'publish_forwarded_topics': st.session_state.publish_forwarded_topics
        },
        'topics': {
            'subscriptions': [line.strip() for line in st.session_state.subscriptions.splitlines() if line.strip()],
            'subscription_filters': [line.strip() for line in st.session_state.subscription_filters.splitlines() if line.strip()],
            'do_not_forward': [line.strip() for line in st.session_state.do_not_forward.splitlines() if line.strip()],
            'topic_whitelist': [line.strip() for line in st.session_state.topic_whitelist.splitlines() if line.strip()]
        }
    }
    
    try:
        doc = tomlkit.document()
        for section, values in config.items():
            table = tomlkit.table()
            for key, value in values.items():
                table.add(key, value)
            doc.add(section, table)
            
        with open(save_path, 'w') as f:
            f.write(tomlkit.dumps(doc))
        st.session_state.config_data = config
        st.session_state.save_status = True
        return config
    except Exception as e:
        st.session_state.save_status = e
        return None

async def restart_relay(config, max_retries=3):
    """Send restart command to relay via MQTT with retries"""
    try:
        if not config or 'broker' not in config:
            raise KeyError("'broker'")

        broker = config.get('broker', {})
        broker_user = broker.get('user', '')
        broker_pass = broker.get('password', '')
        broker_host = broker.get('host')
        broker_port = broker.get('port')
        base_topic = config.get('general', {}).get('base_topic', 'myrelay/')

        if not broker_host or not broker_port:
            raise ValueError("Invalid broker configuration: Missing host or port")

        # Attempt connection with retries
        last_error = None
        for attempt in range(max_retries):
            try:
                logger.info(f"Connecting to MQTT broker at {broker_host}:{broker_port} " +
                          f"(attempt {attempt + 1}/{max_retries})" +
                          f"{' with auth' if broker_user else ''}")
                
                client = MQTTClient(f'loxberry_ui_{int(time.time())}')
                if broker_user:
                    client.set_auth_credentials(broker_user, broker_pass)
                
                await client.connect(
                    broker_host,
                    port=broker_port,
                    version=MQTTConstants.MQTTv311
                )
                
                restart_topic = f"{base_topic}config/restart"
                logger.info(f"Sending restart command to {restart_topic}")
                # Send restart command with QoS 1 to ensure delivery
                client.publish(
                    restart_topic,
                    b"",
                    qos=1,  # Ensure at-least-once delivery
                    retain=False  # Don't retain the restart command
                )
                
                await asyncio.sleep(1)  # Give time for message to be sent
                await client.disconnect()
                    
                st.session_state.restart_status = True
                logger.info("Restart command sent successfully")
                return  # Success, exit function
                
            except Exception as e:
                last_error = e
                if attempt < max_retries - 1:  # Don't sleep on last attempt
                    await asyncio.sleep(1)  # Wait before retry
                    logger.warning(f"Retry {attempt + 1}/{max_retries} after error: {str(e)}")
                
        # If we get here, all retries failed
        raise last_error or Exception("Failed to send restart command after all retries")
        
    except KeyError as e:
        st.session_state.restart_status = e
        logger.error(f"Configuration error: {str(e)}")
    except Exception as e:
        st.session_state.restart_status = e
        logger.error(f"Failed to restart relay: {str(e)}")

# Relay control button
if st.sidebar.button("Restart Relay"):
    if st.session_state.config_data:
        config = st.session_state.config_data.copy()
        asyncio.run(restart_relay(config))
    else:
        st.sidebar.error("No configuration loaded")

# Main content
st.title("MQTT Relay Configuration")

# Initialize status messages in session state if not present
if 'save_status' not in st.session_state:
    st.session_state.save_status = None
if 'restart_status' not in st.session_state:
    st.session_state.restart_status = None

# Display status messages if they exist
if st.session_state.save_status:
    if isinstance(st.session_state.save_status, Exception):
        st.error(f"Error saving configuration: {str(st.session_state.save_status)}")
    else:
        st.success("Configuration saved successfully!")
    st.session_state.save_status = None

if st.session_state.restart_status:
    if isinstance(st.session_state.restart_status, Exception):
        st.error(f"Error restarting relay: {str(st.session_state.restart_status)}")
    else:
        st.success("Relay restart initiated!")
    st.session_state.restart_status = None

# Initialize config data
config_data = st.session_state.get('config_data', initial_values)
if not config_data:
    config_data = initial_values
    st.session_state.config_data = initial_values

with st.form("config_form"):
    st.subheader("MQTT Broker Settings")
    broker = config_data.get('broker', {}) if config_data else {}
    broker_host = st.text_input("Broker Host", value=broker.get('host', 'localhost'), key='broker_host')
    broker_port = st.number_input("Broker Port", value=broker.get('port', 1883), min_value=1, max_value=65535, key='broker_port')
    broker_user = st.text_input("Broker User", value=broker.get('user', ''), key='broker_user')
    broker_pass = st.text_input("Broker Password", value=broker.get('password', ''), key='broker_pass')
    broker_client_id = st.text_input("Client ID", value=broker.get('client_id', 'loxmqttrelay'), key='broker_client_id')

    st.subheader("General Settings")
    general = config_data.get('general', {})
    log_level = st.selectbox(
        "Log Level", 
        options=['DEBUG', 'INFO', 'WARNING', 'ERROR', 'CRITICAL'],
        key='log_level',
        index=1  # Default to INFO
    )
    base_topic = st.text_input("Relay Topic Base", value=general.get('base_topic', 'myrelay/'), key='base_topic')
    cache_size = st.number_input("Cache Size", value=general.get('cache_size', 100000), min_value=1000, max_value=1000000, key='cache_size')

    st.subheader("UDP Settings")
    udp = config_data.get('udp', {})
    udp_in_port = st.number_input("UDP In Port", value=udp.get('udp_in_port', 11884), min_value=1, max_value=65535, key='udp_in_port')

    st.subheader("Processing Options")
    processing = config_data.get('processing', {})
    expand_json = st.checkbox("Expand JSON Messages", value=processing.get('expand_json', False), key='expand_json')
    convert_booleans = st.checkbox("Convert booleans", value=processing.get('convert_booleans', False), key='convert_booleans')

    st.subheader("Miniserver Settings")
    miniserver = config_data.get('miniserver', {})
    miniserver_ip = st.text_input("Miniserver IP", value=miniserver.get('miniserver_ip', '127.0.0.1'), key='miniserver_ip')
    miniserver_port = st.number_input("Miniserver Port", value=miniserver.get('miniserver_port', 80), min_value=1, max_value=65535, key='miniserver_port')
    miniserver_user = st.text_input("Miniserver User", value=miniserver.get('miniserver_user', ''), key='miniserver_user')
    miniserver_pass = st.text_input("Miniserver Password", value=miniserver.get('miniserver_pass', ''), key='miniserver_pass')
    miniserver_max_parallel_connections = st.number_input("Miniserver Max Parallel Connections", 
                                                      value=miniserver.get('miniserver_max_parallel_connections', 5),
                                                      min_value=1, max_value=100,
                                                      key='miniserver_max_parallel_connections')
    use_websocket = st.checkbox("Use WebSocket", value=miniserver.get('use_websocket', True), key='use_websocket')
    sync_with_miniserver = st.checkbox("Sync with Miniserver", value=miniserver.get('sync_with_miniserver', False), key='sync_with_miniserver')

    st.subheader("Debug Settings")
    debug = config_data.get('debug', {})
    mock_miniserver_ip = st.text_input("Mock Miniserver IP/Port", value=debug.get('mock_ip', ''), key='mock_miniserver_ip')
    enable_mock_miniserver = st.checkbox("Enable Mock Miniserver", value=debug.get('enable_mock', False), key='enable_mock_miniserver')
    publish_processed_topics = st.checkbox("Publish Processed Topics", value=debug.get('publish_processed_topics', False), key='publish_processed_topics')
    publish_forwarded_topics = st.checkbox("Publish Forwarded Topics", value=debug.get('publish_forwarded_topics', False), key='publish_forwarded_topics')

    st.subheader("Topics")
    topics = config_data.get('topics', {})
    subscriptions_str = st.text_area("Subscriptions (one per line)", 
                                   value='\n'.join(topics.get('subscriptions', [])),
                                   key='subscriptions')

    st.subheader("Subscription Filters (Regex)")
    filters_str = st.text_area("Filters (one per line)", 
                              value='\n'.join(topics.get('subscription_filters', [])),
                              key='subscription_filters')

    st.subheader("Do Not Forward")
    dnf_str = st.text_area("Do Not Forward (one per line)", 
                          value='\n'.join(topics.get('do_not_forward', [])),
                          key='do_not_forward')

    st.subheader("Topic Whitelist")
    topic_whitelist_str = st.text_area("Topic Whitelist (one per line)", 
                                     value='\n'.join(topics.get('topic_whitelist', [])),
                                     key='topic_whitelist')

    # Check if config path is set
    config_path_set = st.session_state.config_path is not None
    
    col1, col2 = st.columns(2)
    with col1:
        save_to_file = st.form_submit_button(
            "Save config to file...",
            disabled=not config_path_set,
            help="Disabled: No config file path specified" if not config_path_set else "Save configuration to file"
        )
    with col2:
        save_and_restart = st.form_submit_button(
            "Save config to file and restart relay...",
            disabled=not config_path_set,
            help="Disabled: No config file path specified" if not config_path_set else "Save configuration and restart the relay"
        )

async def save_and_restart_relay():
    """Save config and restart relay"""
    config = save_config(st.session_state.config_path)
    if not config:
        st.error("Failed to save configuration. Skipping restart.")
        return

    try:
        # Give filesystem time to fully write the config
        await asyncio.sleep(0.5)
        await restart_relay(config)
    except Exception as e:
        st.error(f"Config saved but restart failed: {str(e)}")
        st.info("You may need to restart the relay manually.")

if save_to_file and st.session_state.config_path:
    save_config(st.session_state.config_path)
elif save_and_restart and st.session_state.config_path:
    asyncio.run(save_and_restart_relay())

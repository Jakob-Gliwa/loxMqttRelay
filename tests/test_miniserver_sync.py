import pytest
from unittest.mock import Mock, patch, mock_open, AsyncMock, MagicMock
from io import BytesIO
import struct
import zlib
import zipfile
from loxmqttrelay.miniserver_sync import (
    load_miniserver_config,
    extract_inputs,
    sync_miniserver_whitelist
)
from loxmqttrelay.config import (
    Config, BrokerConfig, AppConfig,
    MiniserverConfig, global_config
)

@pytest.fixture
def mock_ftp():
    with patch('ftplib.FTP') as mock:
        ftp_instance = Mock()
        mock.return_value = ftp_instance
        yield ftp_instance

@pytest.fixture
def sample_config_xml():
    return b'''<?xml version="1.0" encoding="utf-8"?>
    <C>
        <C Type="VirtualInCaption">
            <C Title="Input1"/>
            <C Title="Input2"/>
            <C Type="Other">
                <C Title="Input3"/>
            </C>
        </C>
        <C Type="Other">
            <C Title="NotAnInput"/>
        </C>
    </C>'''

@pytest.fixture
def compressed_config():
    # Create a mock compressed configuration file
    data = b"Test configuration data"
    compressed = zlib.compress(data)
    
    # Create file structure
    file_content = struct.pack('<L', 0xaabbccee)  # Header
    file_content += struct.pack('<LLL',
        len(compressed),  # compressed size
        len(data),       # uncompressed size
        zlib.crc32(data) # checksum
    )
    file_content += compressed
    
    return file_content

def test_extract_inputs(sample_config_xml):
    inputs = extract_inputs(sample_config_xml)
    assert set(inputs) == {"Input1", "Input2", "Input3"}

def test_extract_inputs_empty():
    empty_xml = b'''<?xml version="1.0" encoding="utf-8"?>
    <C>
        <C Type="Other">
            <C Title="NotAnInput"/>
        </C>
    </C>'''
    inputs = extract_inputs(empty_xml)
    assert inputs == []

def test_extract_inputs_invalid_xml():
    with pytest.raises(Exception):
        extract_inputs(b"Invalid XML")

def test_load_miniserver_config_no_files(mock_ftp):
    mock_ftp.nlst.return_value = []
    with pytest.raises(Exception, match="No configuration files found"):
        load_miniserver_config("192.168.1.1", "user", "pass")

def test_load_miniserver_config_ftp_error(mock_ftp):
    mock_ftp.login.side_effect = Exception("FTP Error")
    with pytest.raises(Exception):
        load_miniserver_config("192.168.1.1", "user", "pass")

@pytest.fixture(autouse=True)
def setup_global_config():
    """Set up global config for tests"""
    # Save original config
    original_config = global_config._config
    
    # Create test config
    config = AppConfig()
    config.miniserver.miniserver_ip = "192.168.1.1"
    config.miniserver.miniserver_user = "user"
    config.miniserver.miniserver_pass = "pass"
    config.miniserver.sync_with_miniserver = True
    
    # Set test config
    global_config._config = config
    
    yield global_config
    
    # Restore original config
    global_config._config = original_config

@patch('loxmqttrelay.miniserver_sync.load_miniserver_config')
@patch('loxmqttrelay.miniserver_sync.extract_inputs')
def test_sync_miniserver_whitelist(mock_extract, mock_load):
    # Setup mocks
    mock_load.return_value = "test config xml"
    mock_extract.return_value = ["Input1", "Input2"]

    result = sync_miniserver_whitelist()

    # Verify correct IP extraction and function calls
    mock_load.assert_called_with("192.168.1.1", "user", "pass")
    mock_extract.assert_called_with('test config xml')
    assert result == ["Input1", "Input2"]

def test_sync_miniserver_whitelist_disabled():
    global_config._config.miniserver.sync_with_miniserver = False
    result = sync_miniserver_whitelist()
    assert result == []

def test_sync_miniserver_whitelist_missing_config():
    global_config._config.miniserver.miniserver_ip = ''
    with pytest.raises(Exception):
        sync_miniserver_whitelist()

@patch('loxmqttrelay.miniserver_sync.load_miniserver_config')
def test_sync_miniserver_whitelist_load_error(mock_load):
    mock_load.side_effect = Exception("Load error")
    with pytest.raises(Exception):
        sync_miniserver_whitelist()

def test_extract_inputs_complex_xml():
    complex_xml = b'''<?xml version="1.0" encoding="utf-8"?>
    <C>
        <C Type="VirtualInCaption">
            <C Title="Input1"/>
            <C Title="Input2"/>
            <C Type="VirtualInCaption">
                <C Title="Input3"/>
                <C Title="Input4"/>
            </C>
        </C>
        <C Type="VirtualInCaption">
            <C Title="Input5"/>
        </C>
    </C>'''
    inputs = extract_inputs(complex_xml)
    assert set(inputs) == {"Input1", "Input2", "Input3", "Input4", "Input5"}

def test_extract_inputs_with_special_characters():
    xml_with_special_chars = b'''<?xml version="1.0" encoding="utf-8"?>
    <C>
        <C Type="VirtualInCaption">
            <C Title="Input/With/Slashes"/>
            <C Title="Input With Spaces"/>
            <C Title="Input_With_Underscores"/>
        </C>
    </C>'''
    inputs = extract_inputs(xml_with_special_chars)
    assert set(inputs) == {
        "Input/With/Slashes",
        "Input With Spaces",
        "Input_With_Underscores"
    }

def test_extract_inputs_malformed_xml_recovery():
    # Test XML with duplicate attributes (common Loxone v16 issue)
    malformed_xml = b'''<?xml version="1.0" encoding="utf-8"?>
    <C>
        <C Type="VirtualInCaption" Type="Duplicate">
            <C Title="Input1" Title="DuplicateTitle"/>
            <C Title="Input2"/>
        </C>
    </C>'''
    # Should still work with lxml recovery mode
    inputs = extract_inputs(malformed_xml)
    assert "Input1" in inputs
    assert "Input2" in inputs

def test_extract_inputs_with_bom():
    # Test XML with UTF-8 BOM
    xml_with_bom = b'\xef\xbb\xbf<?xml version="1.0" encoding="utf-8"?>\n<C><C Type="VirtualInCaption"><C Title="InputWithBOM"/></C></C>'
    inputs = extract_inputs(xml_with_bom)
    assert inputs == ["InputWithBOM"]

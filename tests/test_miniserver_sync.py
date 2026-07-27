import pytest
from unittest.mock import patch, AsyncMock
from io import BytesIO
import struct
import zlib
import zipfile
import lz4.block as lz4b
from loxmqttrelay import miniserver_sync
from loxmqttrelay.miniserver_sync import (
    load_miniserver_config,
    extract_inputs,
    sync_miniserver_whitelist,
    _select_newest_config
)
from loxmqttrelay.config import (
    AppConfig, global_config
)


class _FakeResponse:
    """Minimal async-context-manager stand-in for an aiohttp response."""
    def __init__(self, *, text="", data=b"", raise_exc=None):
        self._text = text
        self._data = data
        self._raise_exc = raise_exc

    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        return False

    def raise_for_status(self):
        if self._raise_exc is not None:
            raise self._raise_exc

    async def text(self):
        return self._text

    async def read(self):
        return self._data


class _FakeSession:
    """Stand-in for aiohttp.ClientSession matching responses by URL substring."""
    def __init__(self, responses):
        self._responses = responses

    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        return False

    def get(self, url):
        for key, resp in self._responses.items():
            if key in url:
                return resp
        raise AssertionError(f"No fake response registered for URL: {url}")


def _patch_session(responses):
    """Patch aiohttp.ClientSession used inside miniserver_sync."""
    return patch(
        'loxmqttrelay.miniserver_sync.aiohttp.ClientSession',
        return_value=_FakeSession(responses),
    )


def _build_loxcc(xml_bytes: bytes) -> bytes:
    """Build a bare LoxCC container (LZ4-block compressed)."""
    compressed = lz4b.compress(xml_bytes, store_size=False)
    loxcc = struct.pack('<L', 0xaabbccee)
    loxcc += struct.pack('<LLL', len(compressed), len(xml_bytes), zlib.crc32(xml_bytes))
    return loxcc + compressed


def _build_config_zip(xml_bytes: bytes) -> bytes:
    """Build a valid Loxone config ZIP (sps0.LoxCC, LZ4-block compressed)."""
    buf = BytesIO()
    with zipfile.ZipFile(buf, 'w') as zf:
        zf.writestr('sps0.LoxCC', _build_loxcc(xml_bytes))
    return buf.getvalue()


_SAMPLE_LISTING = (
    "d      0 Apr 30 00:31 .\n"
    "d      0 Jan 01 01:00 ..\n"
    "- 405797 Oct 26 21:38 sps_0248_20241026213840.zip\n"
    "- 425720 Jan 04 12:14 sps_0252_20260104121455.zip\n"
    "- 420877 Jul 08 22:06 sps_0252_20250708220631.zip\n"
)

# Firmware 17 keeps the deployment archive and, a few seconds later, the
# running program as a bare .LoxCC. Emergency.LoxCC must never be picked.
_FW17_LISTING = (
    "d      0 Jul 27 22:37 .\n"
    "d      0 Jan 01 01:00 ..\n"
    "- 429913 Jun 30 17:12 sps_0252_20260630171238.zip\n"
    "- 437279 Jul 27 22:35 sps_0272_20260727223718.zip\n"
    "- 409799 Jul 27 22:37 sps_0272_20260727223721.LoxCC\n"
    "-  77943 Jul 27 22:37 Emergency.LoxCC\n"
    "-      2 Jul 27 22:37 Music.json\n"
)

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

@pytest.mark.asyncio
async def test_load_miniserver_config_no_files():
    responses = {
        "/dev/fslist/prog/": _FakeResponse(text="d      0 Apr 30 00:31 .\nMusic.json\n"),
    }
    with _patch_session(responses):
        with pytest.raises(Exception, match="No configuration files found"):
            await load_miniserver_config("192.168.1.1", 80, "user", "pass")

@pytest.mark.asyncio
async def test_load_miniserver_config_http_error():
    responses = {
        "/dev/fslist/prog/": _FakeResponse(raise_exc=Exception("401 Unauthorized")),
    }
    with _patch_session(responses):
        with pytest.raises(Exception):
            await load_miniserver_config("192.168.1.1", 80, "user", "pass")

@pytest.mark.asyncio
async def test_load_miniserver_config_selects_newest_and_decompresses(sample_config_xml):
    zip_bytes = _build_config_zip(sample_config_xml)
    responses = {
        "/dev/fslist/prog/": _FakeResponse(text=_SAMPLE_LISTING),
        # newest by sort is sps_0252_20260104121455.zip
        "/dev/fsget/prog/sps_0252_20260104121455.zip": _FakeResponse(data=zip_bytes),
    }
    with _patch_session(responses):
        config_xml = await load_miniserver_config("192.168.1.1", 80, "user", "pass")
    assert config_xml == sample_config_xml
    assert set(extract_inputs(config_xml)) == {"Input1", "Input2", "Input3"}

@pytest.mark.asyncio
async def test_load_miniserver_config_handles_bare_loxcc(sample_config_xml):
    """Firmware 17 writes the running program as a bare .LoxCC next to the ZIP."""
    responses = {
        "/dev/fslist/prog/": _FakeResponse(text=_FW17_LISTING),
        "/dev/fsget/prog/sps_0272_20260727223721.LoxCC": _FakeResponse(
            data=_build_loxcc(sample_config_xml)
        ),
    }
    with _patch_session(responses):
        config_xml = await load_miniserver_config("192.168.1.1", 80, "user", "pass")
    assert config_xml == sample_config_xml

@pytest.mark.asyncio
async def test_load_miniserver_config_rejects_unexpected_payload():
    """A Loxone error body (HTTP 200) must not surface as a cryptic zip error."""
    responses = {
        "/dev/fslist/prog/": _FakeResponse(text=_SAMPLE_LISTING),
        "/dev/fsget/prog/": _FakeResponse(data=b'{"LL":{"control":"dev/fsget","Code":"403"}}'),
    }
    with _patch_session(responses):
        with pytest.raises(Exception, match="Unexpected configuration payload"):
            await load_miniserver_config("192.168.1.1", 80, "user", "pass")

def test_select_newest_config_prefers_newest_timestamp():
    assert _select_newest_config(_FW17_LISTING) == "sps_0272_20260727223721.LoxCC"

def test_select_newest_config_compares_versions_numerically():
    listing = (
        "- 100 Jan 01 00:00 sps_9_20260101000000.zip\n"
        "- 100 Jan 01 00:00 sps_10_20250101000000.zip\n"
    )
    assert _select_newest_config(listing) == "sps_10_20250101000000.zip"

def test_select_newest_config_without_candidates():
    with pytest.raises(Exception, match="No configuration files found"):
        _select_newest_config("d 0 Apr 30 00:31 .\nMusic.json\n")

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

@pytest.mark.asyncio
@patch('loxmqttrelay.miniserver_sync.load_miniserver_config', new_callable=AsyncMock)
@patch('loxmqttrelay.miniserver_sync.extract_inputs')
async def test_sync_miniserver_whitelist(mock_extract, mock_load):
    # Setup mocks
    mock_load.return_value = "test config xml"
    mock_extract.return_value = ["Input1", "Input2"]

    result = await sync_miniserver_whitelist()

    # Verify correct IP/port extraction and function calls
    mock_load.assert_called_with("192.168.1.1", 80, "user", "pass")
    mock_extract.assert_called_with('test config xml')
    assert result == ["Input1", "Input2"]

@pytest.mark.asyncio
async def test_sync_miniserver_whitelist_disabled():
    global_config._config.miniserver.sync_with_miniserver = False
    result = await sync_miniserver_whitelist()
    assert result == []

@pytest.mark.asyncio
async def test_sync_miniserver_whitelist_missing_config():
    global_config._config.miniserver.miniserver_ip = ''
    with pytest.raises(Exception):
        await sync_miniserver_whitelist()

@pytest.mark.asyncio
@patch('loxmqttrelay.miniserver_sync.load_miniserver_config', new_callable=AsyncMock)
async def test_sync_miniserver_whitelist_load_error(mock_load):
    mock_load.side_effect = Exception("Load error")
    with pytest.raises(Exception):
        await sync_miniserver_whitelist()

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


def test_extract_inputs_duplicate_title_uses_fast_path():
    """Duplicate-Title (Loxone v16 bug) is handled by the pygixml fast path.

    pygixml keeps the first Title — matching the old lxml ``.get('Title')``
    semantics — so the lxml recover fallback must NOT be triggered here.
    """
    if not miniserver_sync._PYGIXML_AVAILABLE:
        pytest.skip("pygixml not available on this platform")

    dup_title_xml = b'''<?xml version="1.0" encoding="utf-8"?>
    <C>
        <C Type="VirtualInCaption">
            <C Title="Input1" Title="DuplicateTitle"/>
            <C Title="Input2"/>
        </C>
    </C>'''
    with patch.object(
        miniserver_sync, "_extract_inputs_lxml", wraps=miniserver_sync._extract_inputs_lxml
    ) as spy_fallback:
        inputs = extract_inputs(dup_title_xml)

    spy_fallback.assert_not_called()
    assert inputs == ["Input1", "Input2"]


def test_extract_inputs_structural_malformed_triggers_fallback():
    """Structurally malformed XML (unclosed <C>) must trip the lxml fallback.

    pygixml's strict parser rejects unclosed tags; the fallback then produces
    the same result as the previous production (lxml recover) implementation.
    """
    if not miniserver_sync._PYGIXML_AVAILABLE:
        pytest.skip("pygixml not available on this platform")

    # Unclosed inner <C> — strict parsers reject this, lxml recover repairs it.
    malformed_xml = b'''<?xml version="1.0" encoding="utf-8"?>
    <C>
        <C Type="VirtualInCaption">
            <C Title="Input1">
            <C Title="Input2"/>
        </C>
    </C>'''
    with patch.object(
        miniserver_sync, "_extract_inputs_lxml", wraps=miniserver_sync._extract_inputs_lxml
    ) as spy_fallback:
        inputs = extract_inputs(malformed_xml)

    spy_fallback.assert_called_once()
    # Same output as the production lxml recover path.
    assert inputs == miniserver_sync._extract_inputs_lxml(malformed_xml)
    assert "Input1" in inputs and "Input2" in inputs


def test_extract_inputs_lxml_path_when_pygixml_unavailable(sample_config_xml):
    """When pygixml is unavailable the lxml path is used transparently."""
    with patch.object(miniserver_sync, "_PYGIXML_AVAILABLE", False):
        inputs = extract_inputs(sample_config_xml)
    assert set(inputs) == {"Input1", "Input2", "Input3"}

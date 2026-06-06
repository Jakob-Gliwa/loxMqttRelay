from lxml import etree
from typing import List
import struct
import zipfile
import zlib
from io import BytesIO
import aiohttp
from .config import global_config
from .logging_config import get_lazy_logger
import re

# LZ4 Import - wird als verfügbar angenommen
import lz4.block as lz4b
import lz4.frame as lz4f

logger = get_lazy_logger(__name__)

# Fast XML path: pygixml (pugixml-backed) + XPath on the @Title axis.
# Guarded so the relay keeps working on platforms without a pygixml wheel
# (e.g. linux/arm64) — extract_inputs then transparently uses the lxml path.
#
# IMPORTANT: the published x86_64 pygixml wheel is compiled with AVX2
# unconditionally (no runtime CPU dispatch), so even importing it SIGILLs on
# x86_64 CPUs without AVX2. can_load_x86_avx2_wheel() centralizes that decision.
from loxmqttrelay.utils import can_load_x86_avx2_wheel

# Active XML parser is recorded here and reported once at startup by
# utils.log_runtime_environment() (this runs before logging is set up).
if can_load_x86_avx2_wheel():
    # Breadcrumb before the native import: if the pygixml wheel still SIGILLs
    # (e.g. CPU misdetection), this is the last log line before the hard crash.
    logger.info("Importing pygixml (AVX2-safe path) ...")
    try:
        import pygixml
        from pygixml import PygiXMLError

        _VIC_TITLES_XPATH = pygixml.XPathQuery("//C[@Type='VirtualInCaption']//C/@Title")
        _PYGIXML_AVAILABLE = True
        ACTIVE_XML_PARSER = "pygixml"
        XML_PARSER_REASON = "fast path"
    except Exception as _pygixml_import_error:  # pragma: no cover - platform dependent
        pygixml = None
        PygiXMLError = Exception  # type: ignore[assignment, misc]
        _VIC_TITLES_XPATH = None
        _PYGIXML_AVAILABLE = False
        ACTIVE_XML_PARSER = "lxml"
        XML_PARSER_REASON = f"pygixml import failed: {_pygixml_import_error}"
else:
    pygixml = None
    PygiXMLError = Exception  # type: ignore[assignment, misc]
    _VIC_TITLES_XPATH = None
    _PYGIXML_AVAILABLE = False
    ACTIVE_XML_PARSER = "lxml"
    XML_PARSER_REASON = "x86_64 host without AVX2; AVX2-only pygixml wheel skipped"

# Matches the timestamped Loxone config archives in the /prog directory,
# e.g. "sps_0252_20260430003125.zip". There is no fixed-name pointer to the
# active config (confirmed by Loxone), so we list and pick the newest.
_CONFIG_FILE_PATTERN = re.compile(r'(sps_\d+_\d+\.(?:zip|LoxCC))')

def _is_lz4_frame(data: bytes) -> bool:
    """
    Extended LZ4 frame detection including skippable frames.
    """
    if len(data) < 4:
        return False
    m = int.from_bytes(data[:4], "little")
    return m in (0x184D2204, 0x184C2102) or 0x184D2A50 <= m <= 0x184D2A5F

def _decompress_loxcc_block_lz4(data: bytes, uncompressed_size: int) -> bytes:
    """
    LZ4 decompression function for LoxCC blocks.
    Extended automatic detection of LZ4-Frame vs. LZ4-Block.
    Extremely fast compared to the current implementation.
    """
    if _is_lz4_frame(data):
        return lz4f.decompress(data)
    try:
        return lz4b.decompress(data, uncompressed_size=uncompressed_size)
    except Exception as e:
        # last attempt: possibly misidentified
        try:
            return lz4f.decompress(data)
        except Exception:
            raise ValueError(f"LZ4 decompression failed: {e}")


def _build_base_url(ip: str, port: int) -> str:
    """
    Build the plain-HTTP base URL for the Miniserver filesystem API.

    The fsget/fslist endpoints are only served as plaintext (they cannot be
    command-encrypted), so we always use http here, mirroring the URL handling
    in http_miniserver_handler.
    """
    if port not in (80, 443):
        return f"http://{ip}:{port}"
    return f"http://{ip}"


def _decompress_loxcc(loxcc_zip: bytes) -> bytes:
    """
    Extract sps0.LoxCC from a Loxone config ZIP archive and decompress it to
    the raw configuration XML bytes.

    Validates the LoxCC header magic, payload length, CRC32 checksum and the
    uncompressed size. Raw bytes are returned so the XML parser can perform its
    own encoding detection.
    """
    zf = zipfile.ZipFile(BytesIO(loxcc_zip))
    with zf.open('sps0.LoxCC') as f:
        header, = struct.unpack('<L', f.read(4))
        if header != 0xaabbccee:
            raise Exception("Invalid file format")

        compressedSize, uncompressedSize, checksum, = struct.unpack('<LLL', f.read(12))
        data = f.read(compressedSize)

        # Strict payload length validation
        if len(data) != compressedSize:
            raise Exception(f"Payload length mismatch: got {len(data)}, expected {compressedSize}")

        # Decompression method - always LZ4
        logger.debug("Using LZ4 decompression")
        resultStr = _decompress_loxcc_block_lz4(data, uncompressedSize)

        if checksum != zlib.crc32(resultStr):
            raise Exception('Checksum verification failed')

        if len(resultStr) != uncompressedSize:
            raise Exception(f'Uncompressed filesize mismatch: {len(resultStr)} != {uncompressedSize}')

        return bytes(resultStr)


async def load_miniserver_config(ip: str, port: int, username: str, password: str) -> bytes:
    """
    Load the most recent version of the currently active configuration file
    from the Miniserver via the HTTP filesystem API.

    Lists ``/dev/fslist/prog/`` to find the newest ``sps_<ver>_<ts>.zip``
    archive (there is no fixed-name pointer to the active config), downloads it
    via ``/dev/fsget/prog/<file>`` and decompresses the contained sps0.LoxCC.

    Args:
        ip: Miniserver IP address
        port: Miniserver HTTP port
        username: Miniserver username (HTTP BasicAuth)
        password: Miniserver password (HTTP BasicAuth)
    """
    base_url = _build_base_url(ip, port)
    auth = aiohttp.BasicAuth(username, password)
    timeout = aiohttp.ClientTimeout(total=30)
    try:
        logger.debug(f"Loading miniserver configuration from {base_url} with username {username}")
        async with aiohttp.ClientSession(auth=auth, timeout=timeout) as session:
            # 1) List the /prog directory (plain-text listing, one entry per line)
            async with session.get(f"{base_url}/dev/fslist/prog/") as resp:
                resp.raise_for_status()
                listing = await resp.text()
            logger.debug(f"Received prog directory listing ({len(listing)} bytes)")

            # 2) Pick the newest configuration archive
            filelist = sorted(set(_CONFIG_FILE_PATTERN.findall(listing)))
            if not filelist:
                raise Exception("No configuration files found")

            filename = filelist[-1]
            logger.info(f"Selected configuration file: {filename}")

            # 3) Download the selected archive
            async with session.get(f"{base_url}/dev/fsget/prog/{filename}") as resp:
                resp.raise_for_status()
                raw = await resp.read()

        # 4) Extract and decompress the configuration
        return _decompress_loxcc(raw)

    except Exception as e:
        logger.error(f"Error loading miniserver configuration: {str(e)}")
        raise

def _extract_inputs_lxml(config_xml: bytes) -> List[str]:
    """
    Robust extraction path using lxml's recovery parser.

    Handles malformed Loxone configs (duplicate attributes, unclosed/mismatched
    tags, encoding issues, ...) that the strict pygixml fast path rejects.
    """
    parser = etree.XMLParser(recover=True)
    root = etree.fromstring(config_xml, parser)

    titles: List[str] = []

    def find_titles_under_virtual_in_caption(element):
        if element.tag == "C" and element.get("Type") == "VirtualInCaption":
            for child in element.findall(".//C"):
                title = child.get("Title")
                if title:
                    titles.append(title)
        for child in element:
            find_titles_under_virtual_in_caption(child)

    find_titles_under_virtual_in_caption(root)
    return titles


def extract_inputs(config_xml: bytes) -> List[str]:
    """
    Extract all possible inputs from the Loxone configuration XML.

    Fast path: a strict pygixml parse + XPath directly on the ``@Title`` axis
    (``//C[@Type='VirtualInCaption']//C/@Title``). On a strict-parse error
    (structurally malformed XML) — or when pygixml is unavailable — it falls
    back to the previous lxml ``recover=True`` walk, which produces identical
    output for the normal case and tolerates the malformed cases pygixml does
    not handle.
    """
    if _PYGIXML_AVAILABLE:
        try:
            doc = pygixml.parse_string(config_xml.decode("utf-8", errors="replace"))
            titles: List[str] = []
            for node in _VIC_TITLES_XPATH.evaluate_node_set(doc.root):
                attr = node.attribute
                if attr is not None and attr.value:
                    titles.append(attr.value)
            logger.info(f"Extracted {len(titles)} inputs (pygixml fast path)")
            return titles
        except PygiXMLError as e:
            logger.warning(
                f"pygixml strict parse failed ({e}); falling back to lxml recover mode"
            )

    try:
        titles = _extract_inputs_lxml(config_xml)
        logger.info(f"Extracted {len(titles)} inputs (lxml recover fallback)")
        return titles
    except Exception as e:
        logger.error(f"Error extracting inputs from configuration: {str(e)}")
        raise

async def sync_miniserver_whitelist() -> List[str]:
    """
    Sync the whitelist with the miniserver configuration.
    Uses Config singleton to access configuration values.
    Returns the list of extracted inputs.
    """
    try:
        if not global_config.miniserver.sync_with_miniserver:
            return []

        # Extract IP from miniserver_ip (which might include port)
        ms_ip = global_config.miniserver.miniserver_ip.split(':')[0]
        
        # Load the configuration from miniserver
        config_xml = await load_miniserver_config(
            ms_ip,
            global_config.miniserver.miniserver_port,
            global_config.miniserver.miniserver_user,
            global_config.miniserver.miniserver_pass
        )
        
        # Extract inputs from the configuration
        inputs = extract_inputs(config_xml)
        logger.info(f"Extracted {len(inputs)} inputs from miniserver configuration")
        
        return inputs
        
    except Exception as e:
        logger.error(f"Error syncing miniserver whitelist: {str(e)}")
        raise

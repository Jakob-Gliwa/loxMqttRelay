from lxml import etree
from typing import List
import ftplib
import struct
import zipfile
import zlib
from io import BytesIO
from .config import global_config
from .logging_config import get_lazy_logger
import re

# LZ4 Import - wird als verfügbar angenommen
import lz4.block as lz4b
import lz4.frame as lz4f

logger = get_lazy_logger(__name__)

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


def load_miniserver_config(ip: str, username: str, password: str) -> bytes:
    """
    Load the most recent version of the currently active configuration file
    from the Miniserver via FTP.
    
    Args:
        ip: Miniserver IP address
        username: FTP username
        password: FTP password
    """
    try:
        logger.debug(f"Loading miniserver configuration from {ip} with username {username}")
        ftp = ftplib.FTP(ip)
        try:
            ftp.login(username, password)
            logger.debug(f"Logged in successfully - files/folders in root: {ftp.nlst()}")
            ftp.cwd('prog')
            filesInFolder = ftp.nlst()
            logger.debug(f"Found files in prog folder: {filesInFolder}")
        except ftplib.all_errors as e:
            logger.error(f"Error with ftp login to miniserver during miniserver sync: {e}")

        # Change to prog directory
  
        
        # Find the most recent configuration file
        filelist = []
        pattern = r'(sps_\d+_\d+\.(?:zip|LoxCC))'
        for line in ftp.nlst():
            match = re.search(pattern, line)
            if match:
                filelist.append(match.group(1))
        
        if not filelist:
            raise Exception("No configuration files found")
        
                    
        filename = sorted(filelist)[-1]
        logger.info(f"Selected configuration file: {filename}")
        
        # Download the file
        download_file = BytesIO()
        ftp.retrbinary(f"RETR /prog/{filename}", download_file.write)
        download_file.seek(0)
        ftp.quit()

        # Extract and decompress the configuration
        zf = zipfile.ZipFile(download_file)
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
                
            # Return raw bytes - let XML parser handle encoding detection
            return bytes(resultStr)
            
    except Exception as e:
        logger.error(f"Error loading miniserver configuration: {str(e)}")
        raise

def extract_inputs(config_xml: bytes) -> List[str]:
    """
    Extract all possible inputs from the Loxone configuration XML.
    """
    # Try normal XML parsing first
    try:
        root = etree.fromstring(config_xml)
        logger.info("XML parsed successfully with standard parser")
    except etree.XMLSyntaxError as e:
        logger.warning(f"Standard XML parsing failed: {str(e)}")
        logger.warning("Attempting XML parsing with recovery mode for malformed XML")
        
        # Use lxml recovery mode for malformed XML (handles duplicate attributes, encoding issues, etc.)
        parser = etree.XMLParser(recover=True)
        root = etree.fromstring(config_xml, parser)
        logger.warning("Successfully parsed malformed XML using lxml recovery mode")
    
    # Extract titles from parsed XML
    try:
        titles = []

        def find_titles_under_virtual_in_caption(element):
            if element.tag == "C" and element.get("Type") == "VirtualInCaption":
                for child in element.findall(".//C"):
                    title = child.get("Title")
                    if title:
                        titles.append(title)
            for child in element:
                find_titles_under_virtual_in_caption(child)

        find_titles_under_virtual_in_caption(root)
        logger.info(f"Extracted {len(titles)} inputs from configuration")
        return titles

    except Exception as e:
        logger.error(f"Error extracting inputs from configuration: {str(e)}")
        raise

def sync_miniserver_whitelist() -> List[str]:
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
        config_xml = load_miniserver_config(
            ms_ip,
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

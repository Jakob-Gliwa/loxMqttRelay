import logging
from lxml import etree
from typing import List
import ftplib
import struct
import zipfile
import zlib
from io import BytesIO
from .config import global_config
import re

logger = logging.getLogger(__name__)

def load_miniserver_config(ip: str, username: str, password: str) -> bytes:
    """
    Load the most recent version of the currently active configuration file
    from the Miniserver via FTP.
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
            
            # Decompress the data
            index = 0
            resultStr = bytearray()
            while index < len(data):
                byte, = struct.unpack('<B', data[index:index+1])
                index += 1
                copyBytes = byte >> 4
                byte &= 0xf
                
                if copyBytes == 15:
                    while True:
                        addByte = data[index]
                        copyBytes += addByte
                        index += 1
                        if addByte != 0xff:
                            break
                            
                if copyBytes > 0:
                    resultStr += data[index:index+copyBytes]
                    index += copyBytes
                    
                if index >= len(data):
                    break
                    
                bytesBack, = struct.unpack('<H', data[index:index+2])
                index += 2
                bytesBackCopied = 4 + byte
                
                if byte == 15:
                    while True:
                        val, = struct.unpack('<B', data[index:index+1])
                        bytesBackCopied += val
                        index += 1
                        if val != 0xff:
                            break
                            
                while bytesBackCopied > 0:
                    if -bytesBack+1 == 0:
                        resultStr += resultStr[-bytesBack:]
                    else:
                        resultStr += resultStr[-bytesBack:-bytesBack+1]
                    bytesBackCopied -= 1
                    
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

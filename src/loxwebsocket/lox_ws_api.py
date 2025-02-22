"""
Adapted from: https://github.com/JoDehli/PyLoxone
Thank you for your work!
"""

import asyncio
import binascii
import enum
import logging
import queue
import time
from typing import Optional
import uuid
from struct import iter_unpack
import aiohttp
from aiohttp import WSMsgType
import orjson as json
from Crypto.Hash import HMAC, SHA1, SHA256
import loxwebsocket.const as c
from loxwebsocket.exceptions import LoxoneException
from loxwebsocket.lxtoken import LxToken
from loxwebsocket.encryption import LxJsonKeySalt, LxEncryptionHandler
from construct import Struct, Int32ul, Bytes, this
import subprocess
import platform
_LOGGER = logging.getLogger(__name__)

if "arm" in platform.machine().lower():
    from .cython_modules.extractor_compatible import parse_message, parse_type_3_message
else:
    system = platform.system()
    try:
        if system == "Linux":
            output = subprocess.check_output("lscpu", shell=True, text=True)
        elif system == "Darwin":  # macOS
            output = subprocess.check_output("sysctl -a | grep machdep.cpu", shell=True, text=True)
        elif system == "Windows":
            output = subprocess.check_output("wmic cpu get Caption", shell=True, text=True)            
        
        if output and ("avx" in output.lower() and "avx2" in output.lower()):
            from .cython_modules.extractor_optimized import parse_message, parse_type_3_message
        else:
            from .cython_modules.extractor_compatible import parse_message, parse_type_3_message

    except subprocess.CalledProcessError:
        _LOGGER.error("Error checking CPU features. Using compatible extractor.")
        from .cython_modules.extractor_compatible import parse_message, parse_type_3_message


# Definition von EvDataText
EvDataText = Struct(
    "uuid" / Bytes(16),           # 128-Bit UUID
    "uuidIcon" / Bytes(16),       # 128-Bit UUID des Icons
    "textLength" / Int32ul,       # 32-Bit unsigned integer (little endian)
    "text" / Bytes(this.textLength)  # Text mit Länge textLength
)

async def own_dumps(obj) -> str:
    return json.dumps(obj).decode("utf-8")



class LoxWs:

    class EventType(enum.IntEnum):
        ANY = 0
        INITIALIZED = 1
        CONNECTED = 2
        CONNECTION_CLOSED = 3
        RECONNECTED = 4

    """Loxone Websocket singleton class."""
    _instance = None
    _lock = asyncio.Lock()  # Lock object to ensure thread safety
    _initialized = False
    _receive_updates = True
    reconnect_event = asyncio.Event()
    _event_callbacks = {}

    def __init__(
        self,
        version=15.0
    ):
        _LOGGER.info("Websocket Client Initializing...")
        if self._initialized:
            return

        self._version = version
        self._initialized = True
        self._token = LxToken()
        self._session_key = None
        self._session = None
        self._ws = None
        self._current_message_type = None
        self._visual_hash = None
        self._message_callbacks = {i: [] for i in range(8)}
        self.background_tasks = set()
        self.state = "CLOSED"
        self._secured_queue = queue.Queue(maxsize=1)
        self._encryption_handler = LxEncryptionHandler()
        self._connect_lock = asyncio.Lock()  # Add a lock for the connect method

        self._message_handler = {
            0: self.extract_type_0_message,
            1: self.extract_type_1_message,
            2: self.extract_type_2_message,
            3: self.extract_type_3_message,
            4: self.extract_other_messages,
            5: self.extract_other_messages,
            6: self.extract_type_6_message,
            7: self.extract_other_messages,
        }
        
    async def connect(self, user, password, loxone_url, receive_updates=True, max_reconnect_attempts=c.CONNECT_RETRIES):
        async with self._connect_lock:  # Use the lock to ensure single execution
            if not self._initialized:
                return  
            self._max_reconnect_attempts = max_reconnect_attempts
            self._receive_updates = receive_updates
            if not self._ws or self._ws.closed or self.state != "CONNECTED":
                self._username = user
                self._password = password
                self._loxone_url = loxone_url
                self._loxone_ws_url = loxone_url.replace("https", "wss") if loxone_url.startswith("https:") else loxone_url.replace("http", "ws")
                await self.async_init()
                await self.start()

    async def async_init(self):
        """Initialize encryption, connect to Loxone, exchange keys, authenticate."""
        self._session_key = await self._encryption_handler.generate_session_key(
            self._username, self._password, self._loxone_url)

        _LOGGER.debug("Connecting to Websocket with aiohttp...")
        self._session = aiohttp.ClientSession(json_serialize=json.dumps,)
        self._ws = await self._session.ws_connect(
            f"{self._loxone_ws_url}/ws/rfc6455",
            timeout=c.TIMEOUT,
            heartbeat=None,
            autoping=False
        )
        
        _LOGGER.debug("Connection established, CDM-KEY-EXCHANGE starting...")
        await self._ws.send_str(f"{c.CMD_KEY_EXCHANGE}{self._session_key}")

        # 1) wait for session key header
        header_msg = await self._ws.receive()
        if header_msg.type in (WSMsgType.TEXT, WSMsgType.BINARY):
            await self.parse_loxone_message_header_message(header_msg.data)

        # 2) wait for session key response
        data_msg = await self._ws.receive()
        if data_msg.type in (WSMsgType.TEXT, WSMsgType.BINARY):
            resp_json = json.loads(data_msg.data)
            if resp_json.get("LL").get("Code") != "200":
                raise ConnectionError("Session key exchange failed.")
        elif data_msg.type == WSMsgType.CLOSED:
            _LOGGER.debug("Websocket closed during session key exchange.")
            raise ConnectionError("Websocket closed during session key exchange.")
        else:
            raise ValueError("Unexpected Message Type during session key exchange.")

        _LOGGER.debug("ENCRYPTION READY")

        if (
            self._token is None
            or self._token.token == ""
            or self._token.get_seconds_to_expire() < 300
        ):
            await self.acquire_token()
        else:
            _LOGGER.debug("use loaded token...")
            try:
                await self.use_token()
            except Exception as e:
                _LOGGER.error("Error using existing token. %s. Trying to acquire new token...", e)
                await self.acquire_token()

        if self._receive_updates:
            await self.send_command(c.CMD_ENABLE_UPDATES)

        self.state = "CONNECTED"
        
        return True

    async def start(self) -> None:

        for task in self.background_tasks:
            task.cancel()
        self.background_tasks.clear()

        """Start listening tasks."""
        tasks = [
            asyncio.create_task(self.ws_listen(), name="consumer_task"),
            asyncio.create_task(self.keep_alive(c.KEEP_ALIVE_PERIOD), name="keepalive"),
            asyncio.create_task(self.refresh_token(), name="refresh_token"),
        ]
        for task in tasks:
            self.background_tasks.add(task)
            task.add_done_callback(self.background_tasks.discard)

    async def reconnect(self) -> None:
        if self.state == "RECONNECTING":
            return
        await self.stop()
        self.token = LxToken()
        self.state = "RECONNECTING"
        """Reconnect the websocket using a series of attempts."""
        attempt = 0
        while self._max_reconnect_attempts == 0 or self._max_reconnect_attempts > attempt:
            attempt += 1
            _LOGGER.info(f"Reconnect attempt {attempt + 1} of {self._max_reconnect_attempts}")
            _LOGGER.info(f"Waiting for {c.CONNECT_DELAY} seconds before retrying...")
            await asyncio.sleep(c.CONNECT_DELAY)
            # check if the loxone server is reachable
            if not await self.http_ping():
                continue
            try:
                if await self.async_init():
                    _LOGGER.debug("Reconnection successful.")
                    await self.start()
                    break
                else:
                    _LOGGER.debug("Reconnection failed.")
            except Exception as e:
                _LOGGER.error("Reconnection failed: %s", e)
        else:
            _LOGGER.error("All reconnection attempts failed.")
            raise LoxoneException("All reconnection attempts failed.")
        
    async def http_ping(self):
        try:
            async with aiohttp.ClientSession() as session:
                async with session.get(self._loxone_url, timeout=3) as response:
                    return response.status == 200
        except Exception as e:
            print(f"HTTP-Check fehlgeschlagen: {e}")
            return False

    async def stop(self) -> int:
        """Close the websocket and the underlying session."""
        try:
            self.state = "STOPPING"
            if self._ws:
                await self._ws.close()
            if self._session:
                await self._session.close()
            self.state = "CLOSED"
        except Exception as e:
            _LOGGER.error(e)
            return -1

    async def keep_alive(self, second: int) -> None:
        """Send keepalive messages to keep the websocket open."""
        try:
            while self.state == "CONNECTED":
                await asyncio.sleep(second)
                async with asyncio.Lock():
                    await self._ws.send_str("keepalive")
        except Exception as e:
            await self.handle_connection_interrupt(exception=e)

    async def refresh_token(self):
        """Refresh the token periodically."""
        while True:
            seconds_to_refresh = self._token.get_seconds_to_expire()
            await asyncio.sleep(seconds_to_refresh)
            self._token = LxToken()
            self._refresh_token()

    async def _refresh_token(self):
        """Refresh the token after it has expired."""
        _LOGGER.debug("Try to refresh token.")
        # Send command to get the key
        token_hash = await self.hash_token()

        cmd = (
            f"{c.CMD_REFRESH_TOKEN if self._version < 10.2 else c.CMD_REFRESH_TOKEN_JSON_WEB}"
            f"{token_hash}/{self._username}"
        )
        message = await self.send_command(cmd)
        await self.handleValidUntilMessage(message)

    async def handleValidUntilMessage(self, message):
        try:
            resp_json = json.loads(message)
            if resp_json["LL"]["code"] == "200" and resp_json["LL"]["value"]["validUntil"]:
                self._token.set_valid_until(resp_json["LL"]["value"]["validUntil"])
            else:
                raise LoxoneException("Error authenticating with token. Unexpected content in Loxone response.")
        except (KeyError, TypeError, json.JSONDecodeError) as e:
            _LOGGER.error("Error authenticating with token. Unexpected content in Loxone response.")
            raise LoxoneException("Error authenticating with token. Unexpected content in Loxone response.") from e

    async def send_command(self, command):
        """Send a command over the websocket, wait for response (header + payload)."""
        enc_command = await self._encryption_handler.encrypt(command)
        await self._ws.send_str(enc_command)

        # 1) wait for header
        header_msg = await self._ws.receive()
        if header_msg.type in (WSMsgType.TEXT, WSMsgType.BINARY):
            header = header_msg.data
        elif header_msg.type == WSMsgType.CLOSED:
            raise ConnectionError("Websocket closed while waiting for header.")
        else:
            header = None

        if header:
            await self.parse_loxone_message_header_message(header)

        # 2) wait for actual data
        data_msg = await self._ws.receive()
        if data_msg.type in (WSMsgType.TEXT, WSMsgType.BINARY):
            message = data_msg.data
        elif data_msg.type == WSMsgType.CLOSED:
            raise ConnectionError("Websocket closed while waiting for data.")
        else:
            message = None

        return message

    async def send_command_to_visu_password_secured_control(self, device_uuid: str, value: str, visu_pw: str):
        """Send commands to a Loxone Control that is secured by a visualization password."""
        visu_hash = self._encryption_handler.hash_visu_password_secured_command(self._visual_hash, visu_pw)
        command = f"jdev/sps/ios/{visu_hash}/{device_uuid}/{value}"
        enc_command = await self._encryption_handler.encrypt(command)
        await self._ws.send_str(enc_command)

    async def send_websocket_command_to_visu_password_secured_control(
        self, device_uuid: str, value: str, visu_pw: str
    ) -> None:
        """Queue a secured command to be sent later."""
        self._secured_queue.put((device_uuid, value, visu_pw))
        enc_command = await self._encryption_handler.encrypt_visual_command(self._username)
        await self._ws.send_str(enc_command)

    async def send_websocket_command(self, device_uuid: str, value: str) -> None:
        """Send a websocket command to the Miniserver."""
        command = f"jdev/sps/io/{device_uuid.decode() if isinstance(device_uuid, bytes) else device_uuid}/{value}"
        _LOGGER.debug("send command: %s", command)
        enc_command = await self._encryption_handler.encrypt(command)
        await self._ws.send_str(enc_command)

    async def get_key_salt_for_secure_commands_and_send(self, loxone_json_text_message):
        """
        Check if the returned data is a salt/key for secure commands 
        and flush the secured_queue if so.
        """
        key_and_salt = LxJsonKeySalt()
        key_and_salt.read_user_salt_responce(loxone_json_text_message)
        key_and_salt.time_elapsed_in_seconds = round(time.time())
        self._visual_hash = key_and_salt

        while not self._secured_queue.empty():
            secured_message = self._secured_queue.get()
            await self.send_command_to_visu_password_secured_control(
                secured_message[0],
                secured_message[1],
                secured_message[2],
            )

    async def handle_connection_interrupt(self, msg_type: Optional[int] = None, exception: Optional[Exception] = None):
        close_code = self._ws.close_code if self._ws else None
        if msg_type == WSMsgType.CLOSED or msg_type == WSMsgType.CLOSING:
            _LOGGER.error("Connection closed unexpectedly%s", f" with code: {close_code}" if close_code is not None else "!")
        elif exception:
            _LOGGER.error("Connection error: %s of type %s%s", exception, type(exception), f" with code: {close_code}" if close_code is not None else "")
        elif msg_type == WSMsgType.ERROR:
            _LOGGER.error("Connection error - most likely from listener %s", f" with code: {close_code}" if close_code is not None else "!")

        match close_code:
            case 4004:
                _LOGGER.error("Connection closed: Some user has been changed (code: %s)", close_code)
            case 4005:
                _LOGGER.error("Connection closed: The user currently connected has been changed either by themself or another user (code: %s)", close_code)
            case 4006:
                _LOGGER.error("Connection closed: The user trying to establish a connection has been disabled (code: %s)", close_code)
            case 4007:
                _LOGGER.error("Connection closed: The Miniserver is currently performing an update (code: %s)", close_code)
            case 4008:
                _LOGGER.error("Connection closed: The Miniserver doesn't have any event slots for the initiated WebSocket session (code: %s)", close_code)
            case None:
                _LOGGER.error("Connection closed unexpectedly without a close code.")
            case _:
                _LOGGER.error("Connection closed unexpectedly with unknown code: %s", close_code)
        
        await self.reconnect()

    async def ws_listen(self) -> None:
        """
        Listen for websocket messages in a background task.
        """
        try:
            async for msg in self._ws:
                try:
                    await self._async_process_message(msg.data)
                except Exception as inner_exception:
                    _LOGGER.error(f"Error processing message: {inner_exception}")
                    continue
            await self.handle_connection_interrupt(msg_type=msg.type)

        except Exception as e:
            await self.handle_connection_interrupt(exception=e)

    async def _async_process_message(self, message: bytes) -> None:
        """
        Process the incoming Loxone message. 
        First check if it's a header, if not parse the content.
        """
        if not await self.parse_loxone_message_header_message(message):
            parsed_data = await self._message_handler[self._current_message_type](message, {})
            if parsed_data:
                _LOGGER.debug("message [type:%s]: %s",self._current_message_type,parsed_data)

                for callback in self._message_callbacks[self._current_message_type]:
                    task = asyncio.create_task(callback(parsed_data, self._current_message_type))
                    task.add_done_callback(lambda t: _LOGGER.error(f"Error in message callback: {t.exception()}") if t.exception() else None)

            self._current_message_type = None

    async def parse_loxone_message_header_message(self, message):
        """
        Parse the 8-byte Loxone message header.
        [0]: fixed 0x03
        [1]: message type
        [2]: info flags
        [3]: reserved
        [4:8]: payload length (uint32, little endian)
        If it's exactly 8 bytes, treat it as a header. Otherwise it's payload.
        """
        if len(message) == 8:
            try:
                self._current_message_type = message[1]
                _LOGGER.debug("Current message type:%s", self._current_message_type)
                return True
            except ValueError:
                _LOGGER.warning("error parse_loxone_message...")
                raise ValueError("error parse_loxone_message:{}".format(message))
        return False


    async def extract_type_0_message(self, message, event_dict):
        """Type 0: Text message"""
        try:
            json_message =  json.loads(message)

            if not json_message.get("LL"):
                return None
            
            if not json_message["LL"].get("Code") or json_message["LL"]["Code"] != "200":
                if json_message["LL"].get("Code") == "404" and json_message["LL"].get("control"):
                        #TODO this could potentially flood the logs - maybe add a config to surpess this warning 
                        _LOGGER.warning("Unrecognized command or control not found: %s. If you receive all values that you expect and/or don't have a control with this name on your Miniserver, you can ignore this message. If you want to get rid of this message, you can add the control to the whitelist/filter/donotforward in the configuration.", json_message["LL"]["control"])
                return None

            if json_message["LL"].get("value") and not isinstance(json_message["LL"]["value"], str) and json_message["LL"]["value"].get("key") and json_message["LL"]["value"].get("salt"):
                await self.get_key_salt_for_secure_commands_and_send(json_message)
                return None
            
            if json_message["LL"].get("control"):
                json_message["LL"]["control"] = await self._encryption_handler.decrypt_control_response(json_message["LL"]["control"])
                event_dict[json_message["LL"]["control"].split("/")[-2].encode()] = json_message["LL"]
                return event_dict

            return json_message
        except json.JSONDecodeError as e:
            _LOGGER.debug(f"Error parsing JSON: {e}")
            return message.decode("utf-8") if isinstance(message, bytes) else message

    async def extract_type_1_message(self, message, event_dict):
        """Type 1: Binary file (not further processed here)"""
        return event_dict
    
    def process_packet(self,packet):
        # Rückgabe des Ergebnisses als Tupel
        event_uuid = json.dumps(uuid.UUID(bytes_le=packet[0]))
        return event_uuid[1:24] + event_uuid[25:37], packet[1]
    
    async def extract_type_2_message(self, message, event_dict):
        #event_dict.update(parse_message(message))
        #event_dict.update(map(self.process_packet, iter_unpack("16s d", message)))
        return parse_message(message)
    
    async def extract_type_3_message(self, message: bytes, event_dict: dict) -> dict:
       return parse_type_3_message(message)

    async def extract_type_6_message(self, message, event_dict):
        """Type 6: Keepalive response."""
        event_dict["keep_alive"] = "received"
        _LOGGER.debug("Keep alive response received...")
        return event_dict

    async def extract_other_messages(self, message, event_dict):
        """
        We set _current_message_type to 7 because 
        4,5,7 are all 'other' in this code path.
        """
        self._current_message_type = 7
        return event_dict

    async def use_token(self):
        """Use an existing token for authentication."""
        token_hash = await self.hash_token()

        cmd = f"{c.CMD_AUTH_WITH_TOKEN}{token_hash}/{self._username}"
        message = await self.send_command(cmd)
        await self.handleValidUntilMessage(message)
        
    async def hash_token(self):
        """Hash the token using the current key from the miniserver."""
        message = await self.send_command(c.CMD_GET_KEY)
        try:
            resp_json = json.loads(message)
            key = resp_json["LL"]["value"]
            match key:
                case "SHA1":
                    hash_alg = SHA1
                case "SHA256":
                    hash_alg = SHA256
                case "":
                    hash_alg = SHA1 if self._version < 12.0 else SHA256
                case _:
                    raise LoxoneException("Error hasing token. Unexpected content in Loxone response.")
            
            return HMAC.new(
                    binascii.unhexlify(key),
                    self._token.token.encode("utf-8"),
                    hash_alg,
                ).hexdigest()
        except (KeyError, TypeError, json.JSONDecodeError) as e:
            _LOGGER.error("Error hasing token. Unexpected content in Loxone response.")
            raise LoxoneException("Error hasing token. Unexpected content in Loxone response.") from e

    async def acquire_token(self):
        """Request a new token from the Loxone miniserver."""
        _LOGGER.debug("acquire_token")
        message = await self.send_command(f"{c.CMD_GET_KEY_AND_SALT}{self._username}")

        key_and_salt = LxJsonKeySalt()
        key_and_salt.read_user_salt_responce(message)

        new_hash = self._encryption_handler.hash_credentials(
            key_and_salt, self._password, self._username
        )
        command = (
            f"{c.CMD_REQUEST_TOKEN_JSON_WEB if self._version >= 10.2 else c.CMD_REQUEST_TOKEN}"
            f"{new_hash}/{self._username}/{c.TOKEN_PERMISSION}/edfc5f9a-df3f-4cad-9dddcdc42c732be2/loxinflux"
        )
        message = await self.send_command(command)

        try:
            resp_json = json.loads(message)
            token = resp_json["LL"]["value"]["token"]
            valid_until = resp_json["LL"]["value"]["validUntil"]
            if token and valid_until:
                self._token = LxToken(token,valid_until,key_and_salt.hash_alg)
        except (KeyError, TypeError, json.JSONDecodeError) as e:
            _LOGGER.error("Error acquiring token. Unexpected content in Loxone response.")
            raise LoxoneException("Error acquiring token. Unexpected content in Loxone response.") from e

    def add_message_callback(self, callback, message_types: list[int] = None):
        """Add a message callback function with optional message types filter."""
        self._message_callbacks[callback] = message_types
        for message_type in message_types:
            self._message_callbacks[message_type].append(callback)

    def add_event_callback(self, callback, event_types:list[EventType] = [EventType.ANY] ):
        self._event_callbacks[callback] = event_types

    async def send_event(self, event_type:EventType):
        for callback, event_types in self._event_callbacks.items():
            if self.EventType.ANY in event_types or event_type in event_types:
                asyncio.create_task(callback()).add_done_callback(lambda t: _LOGGER.error(f"Error in event callback: {t.exception()}") if t.exception() else None)
         
    def remove_message_callback(self, callback, message_types:list[int]):
        """Remove a previously registered callback."""
        for message_type in message_types:
            self._message_callbacks[message_type].remove(callback)


loxwebsocket = LoxWs()
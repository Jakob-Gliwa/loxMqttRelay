import aiohttp
import orjson as json
import logging
import binascii
import hashlib
import time
from base64 import b64encode, b64decode
from Crypto.Random import get_random_bytes
from Crypto.Cipher import AES, PKCS1_v1_5
from Crypto.Hash import HMAC, SHA1, SHA256
from Crypto.PublicKey import RSA
from Crypto.Util import Padding
import urllib.request as req

import loxwebsocket.const as c
_LOGGER = logging.getLogger(__name__)

class LxJsonKeySalt:
    def __init__(self):
        self.key = None
        self.salt = None
        self.response = None
        self.time_elapsed_in_seconds = None
        self.hash_alg = None

    def read_user_salt_responce(self, reponse):
        js = json.loads(reponse)
        value = js["LL"]["value"]
        self.key = value["key"]
        self.salt = value["salt"]
        hashAlg = value.get("hashAlg", "SHA1")
        if hashAlg == "SHA1":
            self.hash_alg = hashlib.sha1()
        elif hashAlg == "SHA256":
            self.hash_alg = hashlib.sha256()
        else:
            _LOGGER.error("Unrecognised hash algorithm: {}".format(hashAlg))
            raise ValueError("Unrecognised hash algorithm: {}".format(hashAlg))

class LxEncryptionHandler:

    _salt = ""
    _salt_used_count = 0
    _salt_time_stamp = 0

    def __init__(self):
        self._iv = get_random_bytes(c.IV_BYTES)
        self._key = get_random_bytes(c.AES_KEY_SIZE)

    @staticmethod
    async def get_public_key(username, password, loxone_url):
        command = f"{loxone_url}/{c.CMD_GET_PUBLIC_KEY}"
        _LOGGER.debug("Attempting to get public key from: %s", command)
        
        async with aiohttp.ClientSession(
            auth=aiohttp.BasicAuth(login=username, password=password),
            timeout=aiohttp.ClientTimeout(total=c.TIMEOUT)
        ) as client:
            async with client.get(
                command,
                allow_redirects=True,
                ssl=False  # Disable SSL verification
            ) as response:
                _LOGGER.debug("Received response with status: %s", response.status)

                # Raise exception for non-200 status codes
                if response.status != 200:
                    _LOGGER.error("Non-200 response received: %s", response.status)
                    raise ValueError(f"Non-200 response: {response.status}")

                # Parse JSON response
                try:
                    resp_json = await response.json(loads=json.loads)
                except aiohttp.ContentTypeError as e:
                    _LOGGER.error("Response is not in JSON format")
                    raise ValueError("Invalid JSON response from server") from e

                if not ("LL" in resp_json and "value" in resp_json["LL"]):
                    _LOGGER.error("Response missing required fields")
                    raise ValueError("Response missing LL.value field")

                public_key = resp_json["LL"]["value"]
                _LOGGER.debug("Successfully retrieved public key")
                if not public_key:
                    _LOGGER.error("Public key is empty")
                    raise ValueError("Public key is empty")
                public_key = public_key.replace(
                        "-----BEGIN CERTIFICATE-----", "-----BEGIN PUBLIC KEY-----\n"
                    ).replace(
                        "-----END CERTIFICATE-----", "\n-----END PUBLIC KEY-----\n"
                    )
                return public_key
    
    async def generate_session_key(self,username, password, loxone_url):
        try:
            public_key = await self.get_public_key(username, password, loxone_url)
            rsa_cipher = PKCS1_v1_5.new(RSA.importKey(public_key))
            aes_key = binascii.hexlify(self._key).decode("utf-8")
            iv = binascii.hexlify(self._iv).decode("utf-8")
            sess = aes_key + ":" + iv
            sess = rsa_cipher.encrypt(bytes(sess, "utf-8"))
            _LOGGER.debug("generate_session_key successfully...")
            return b64encode(sess).decode("utf-8")
        except Exception as e:
            _LOGGER.debug("error generate_session_key...")
            raise ValueError("error generate_session_key...") from e
        
        

    def genarate_salt(self):
        salt = get_random_bytes(c.SALT_BYTES)
        salt = binascii.hexlify(salt).decode("utf-8")
        salt = req.pathname2url(salt)
        self._salt_time_stamp = round(time.time())
        self._salt_used_count = 0
        self._salt = salt

    def hash_credentials(self, key_salt, password, username):
        pwd_hash = username + ":" + self.generate_password_hash(key_salt, password)
        return HMAC.new(
                binascii.unhexlify(key_salt.key),
                pwd_hash.encode("utf-8"),
                SHA1 if key_salt.hash_alg == "SHA1" else SHA256,
            ).hexdigest()        

    def hash_visu_password_secured_command(self, key_salt: LxJsonKeySalt, visu_pw: str):
        pwd_hash = self.generate_password_hash(key_salt, visu_pw)
        return HMAC.new(
                binascii.unhexlify(key_salt.key),
                pwd_hash.encode("utf-8"),
                SHA1 if key_salt.hash_alg == "SHA1" else SHA256,
            ).hexdigest()


    def generate_password_hash(self, key_salt: LxJsonKeySalt, password: str):
        try:
            pwd_hash_str = str(password) + ":" + key_salt.salt
            m = key_salt.hash_alg
            m.update(pwd_hash_str.encode("utf-8"))
            pwd_hash = m.hexdigest().upper()
            _LOGGER.debug("generate_password_hash successfully...")
            return pwd_hash
        except ValueError as e:
            _LOGGER.debug("error hash_credentials...")
            raise ValueError("error hash_credentials...") from e
        
    def new_salt_needed(self):
        self._salt_used_count += 1
        return self._salt_used_count > c.SALT_MAX_USE_COUNT or round(time.time()) - self._salt_time_stamp > c.SALT_MAX_AGE_SECONDS
       
    
    async def encrypt_visual_command(self, username):
        command = "{}{}".format(c.CMD_GET_VISUAL_PASSWD, username)
        enc_command = await self.encrypt(command)
        return enc_command

    async def encrypt(self, command):
        if self._salt != "" and self.new_salt_needed():
            prev_salt = self._salt
            self._salt = self.genarate_salt()
            s = "nextSalt/{}/{}/{}\0".format(prev_salt, self._salt, command)
        else:
            if self._salt == "":
                self._salt = self.genarate_salt()
            s = "salt/{}/{}\0".format(self._salt, command)
        
        cipher = AES.new(self._key, AES.MODE_CBC, self._iv)
        s = Padding.pad(bytes(s, "utf-8"), 16)
        encrypted = cipher.encrypt(s)
        encoded = b64encode(encrypted)
        encoded_url = req.pathname2url(encoded.decode("utf-8"))
        return c.CMD_ENCRYPT_CMD + encoded_url
    
    async def decrypt_control_response(self, response):
        try:
            # Entfernen Sie das Präfix "jdev/sys/enc/" und Base64-dekodieren Sie den Rest
            encoded_data = response.split("jdev/sys/enc/")[-1]
            encrypted_data = b64decode(encoded_data)

            # Initialisieren des AES-Ciphers mit dem Schlüssel und IV
            cipher = AES.new(self._key, AES.MODE_CBC, self._iv)

            # Entschlüsseln der Daten
            decrypted_data = cipher.decrypt(encrypted_data)

            # Entfernen des Paddings
            plain_text = Padding.unpad(decrypted_data, 16).decode("utf-8")

            return plain_text
        except Exception as e:
            _LOGGER.error(f"Error decrypting response: {str(e)}")
            raise ValueError("Failed to decrypt the response") from e
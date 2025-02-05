import time
from datetime import datetime
import logging

_LOGGER = logging.getLogger(__name__)

class LxToken:
    def __init__(self, token="", valid_until=0, hash_alg="SHA1"):
        self._token = token
        self._valid_until = valid_until
        self._hash_alg = hash_alg

    def get_seconds_to_expire(self):
        dt = datetime.strptime("1.1.2009", "%d.%m.%Y")
        try:
            start_date = int(dt.strftime("%s"))
        except Exception as e:
            _LOGGER.debug("get_seconds_to_expire error: {}".format(e))
            start_date = int(dt.timestamp())
        start_date = int(start_date) + self._valid_until
        return start_date - int(round(time.time()))

    @property
    def token(self):
        return self._token

    @property
    def valid_until(self):
        return self._valid_until

    def set_valid_until(self, value):
        self._valid_until = value

    def set_token(self, token):
        self._token = token

    @property
    def hash_alg(self):
        return self._hash_alg

    def set_hash_alg(self, hash_alg):
        self._hash_alg = hash_alg
class LoxoneException(Exception):
    """Base class for all Loxone Exceptions"""


class LoxoneHTTPStatusError(LoxoneException):
    """An exception indicating an unusual http response from the miniserver"""


class LoxoneRequestError(Exception):
    """An exception raised during an http request"""
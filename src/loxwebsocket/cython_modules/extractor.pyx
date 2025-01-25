# extractor.pyx

# Cython directives für Optimierung
# cython: boundscheck=False, wraparound=False, cdivision=True, nonecheck=False, language_level=3

from libc.stdint cimport uint8_t
from cpython.bytes cimport PyBytes_AsStringAndSize, PyBytes_FromStringAndSize
from cpython.dict cimport PyDict_New, PyDict_SetItem
from cpython.float cimport PyFloat_FromDouble
from cython cimport inline

# Für Dict-Vorallokation (Python >= 3.7)
cdef extern from "Python.h":
    object PyDict_New()
    int _PyDict_SetItem_KnownHash(object, object, object, Py_ssize_t) nogil
    Py_ssize_t PyObject_Hash(object) nogil

cdef char[16] hex_digits = [
    b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7',
    b'8', b'9', b'a', b'b', b'c', b'd', b'e', b'f'
]

cdef inline char nibble_to_hex(uint8_t nibble):
    return hex_digits[nibble & 0xF]

# Neue interne Funktion die direkt mit Pointern arbeitet
cdef bytes _convert_bytes_to_uuid_ptr(const uint8_t* byte_ptr):
    cdef char uuid_str[35]  # 36 Zeichen ohne Null-Terminierung, hier 35 + 0
    cdef int j = 0

    # --- 4 bytes (little-endian) ---
    for i in range(3, -1, -1):
        uuid_str[j]   = nibble_to_hex(byte_ptr[i] >> 4)
        uuid_str[j+1] = nibble_to_hex(byte_ptr[i] & 0x0F)
        j += 2
    uuid_str[j] = b'-'
    j += 1

    # --- 2 bytes (little-endian) ---
    for i in range(5, 3, -1):
        uuid_str[j]   = nibble_to_hex(byte_ptr[i] >> 4)
        uuid_str[j+1] = nibble_to_hex(byte_ptr[i] & 0x0F)
        j += 2
    uuid_str[j] = b'-'
    j += 1

    # --- 2 bytes (little-endian) ---
    for i in range(7, 5, -1):
        uuid_str[j]   = nibble_to_hex(byte_ptr[i] >> 4)
        uuid_str[j+1] = nibble_to_hex(byte_ptr[i] & 0x0F)
        j += 2
    uuid_str[j] = b'-'
    j += 1

    # --- 2 bytes (big-endian) ---
    for i in range(8, 10):
        uuid_str[j]   = nibble_to_hex(byte_ptr[i] >> 4)
        uuid_str[j+1] = nibble_to_hex(byte_ptr[i] & 0x0F)
        j += 2

    # --- 6 bytes (big-endian) ---
    for i in range(10, 16):
        uuid_str[j]   = nibble_to_hex(byte_ptr[i] >> 4)
        uuid_str[j+1] = nibble_to_hex(byte_ptr[i] & 0x0F)
        j += 2

    return PyBytes_FromStringAndSize(uuid_str, 35)

# Public Wrapper-Funktion für Python-API-Kompatibilität
cpdef bytes convert_bytes_to_uuid(bytes input_bytes):
    cdef Py_ssize_t len_input
    cdef const uint8_t* byte_ptr

    if len(input_bytes) != 16:
        raise ValueError("Input bytes must be 16 bytes long")

    if PyBytes_AsStringAndSize(input_bytes, <char **>&byte_ptr, &len_input) == -1:
        raise ValueError("Failed to extract bytes from input")

    return _convert_bytes_to_uuid_ptr(byte_ptr)

cpdef dict parse_message(bytes message):
    cdef Py_ssize_t len_msg
    cdef const uint8_t* byte_ptr
    cdef Py_ssize_t packet_size = 24
    cdef Py_ssize_t offset
    cdef bytes uuid_str
    cdef double value
    cdef dict result
    cdef Py_ssize_t i, num_packets

    # Bytes extrahieren
    if PyBytes_AsStringAndSize(message, <char **>&byte_ptr, &len_msg) == -1:
        raise ValueError("Failed to extract bytes from message")

    if len_msg < packet_size:
        raise ValueError("Message must be at least 24 bytes long "
                         "(16 for UUID, 8 for double)")

    # Anzahl Pakete (Ganzzahldivision)
    num_packets = len_msg // packet_size

    # Normales Dict erstellen
    result = PyDict_New()

    for i in range(num_packets):
        offset = i * packet_size

        # UUID direkt aus Pointer erstellen
        uuid_str = _convert_bytes_to_uuid_ptr(byte_ptr + offset)

        # Double lesen (Little Endian)
        value = (<double *>(byte_ptr + offset + 16))[0]

        # Ins Dict eintragen
        PyDict_SetItem(result, uuid_str, PyFloat_FromDouble(value))

    return result

cpdef dict parse_type_3_message(bytes message):

    cdef Py_ssize_t len_msg
    cdef const uint8_t* byte_ptr
    cdef Py_ssize_t offset = 0
    cdef Py_ssize_t chunk_size, aligned_size
    cdef Py_ssize_t text_length
    cdef bytes key_uuid, text_data
    cdef dict result = PyDict_New()

    # Bytes-Inhalt und -Länge ermitteln
    if PyBytes_AsStringAndSize(message, <char **>&byte_ptr, &len_msg) == -1:
        raise ValueError("Konnte aus den Bytes keinen Inhalt extrahieren")

    # Schleife durchs gesamte Byte-Array
    while offset + 16 + 16 + 4 <= len_msg:
        # --- UUID extrahieren ---
        key_uuid = _convert_bytes_to_uuid_ptr(byte_ptr + offset)
        offset += 16

        # --- Icon-UUID überspringen ---
        offset += 16

        # --- Textlänge lesen (4 Bytes, little-endian) ---
        text_length = (<uint8_t *> (byte_ptr + offset))[0]        \
                    | (<uint8_t *> (byte_ptr + offset))[1] << 8   \
                    | (<uint8_t *> (byte_ptr + offset))[2] << 16  \
                    | (<uint8_t *> (byte_ptr + offset))[3] << 24
        offset += 4

        # Sicherstellen, dass genügend Bytes für den Text vorhanden sind
        if offset + text_length > len_msg:
            raise ValueError("Textlänge überschreitet verfügbare Daten")

        # --- Text extrahieren ---
        text_data = PyBytes_FromStringAndSize(
            <char *> (byte_ptr + offset),
            text_length
        )
        offset += text_length

        # Dict füllen
        PyDict_SetItem(result, key_uuid, text_data)

        # --- 4-Byte Align (aufrunden) ---
        chunk_size = 16 + 16 + 4 + text_length
        aligned_size = (chunk_size + 3) & ~3
        # Wir haben schon chunk_size konsumiert, also skippen wir jetzt
        offset += (aligned_size - chunk_size)

    return result

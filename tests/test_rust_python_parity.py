"""Hold the Rust whitelist sync against the Python one, on real Miniserver output.

TEMPORARY. This exists for the duration of the port and goes when
``src/loxmqttrelay/miniserver_sync.py`` does.

The Rust unit tests in ``src/whitelist/`` cover the shapes - a container, an
archive, a nested ``VirtualInCaption``, a duplicate attribute. What they cannot
cover is what a Miniserver actually writes: 2.5 MB of XML with a BOM, CRLF line
endings, and whatever else eight years of firmware has put in there. That is what
``config/sps0.LoxCC`` is, and it is the only input that has ever exercised the
real thing.

The file is gitignored on purpose - it is the operator's own house, with room
and device names in it - so these tests skip when it is absent. Drop any other
``sps_*.zip`` or ``*.LoxCC`` into ``fixtures/`` (also gitignored) and they will
be picked up too; an older firmware's zip is the only way to get a real archive
in front of the hand-rolled zip reader.

Note which Python side is compared against. ``extract_inputs`` is the pygixml
path, which is what runs in production and in the image. ``_extract_inputs_lxml``
is a fallback that disagrees with it on nested ``VirtualInCaption`` elements, and
the Rust port reproduces pygixml. The comparison here is on *ordered* lists,
unlike the older tests which compared sets and therefore could not have seen the
difference.
"""

from pathlib import Path

import pytest

from loxmqttrelay import miniserver_sync
from loxmqttrelay.compatible._loxmqttrelay import (
    _parity_decompress_loxcc,
    _parity_extract_inputs,
)

ROOT = Path(__file__).resolve().parent.parent
REAL_CONFIG = ROOT / "config" / "sps0.LoxCC"
EXTRA_FIXTURES = ROOT / "fixtures"


def _configurations() -> list[Path]:
    """Every real Miniserver payload available on this machine."""
    found = [REAL_CONFIG] if REAL_CONFIG.exists() else []
    if EXTRA_FIXTURES.is_dir():
        found += sorted(
            path
            for path in EXTRA_FIXTURES.iterdir()
            if path.suffix in {".zip", ".LoxCC"}
        )
    return found


CONFIGURATIONS = _configurations()
requires_real_config = pytest.mark.skipif(
    not CONFIGURATIONS,
    reason="no real Miniserver configuration on this machine (config/sps0.LoxCC is gitignored)",
)


@requires_real_config
@pytest.mark.parametrize("path", CONFIGURATIONS, ids=lambda p: p.name)
def test_the_container_unwraps_to_the_same_bytes(path: Path):
    """Byte for byte: the header checks, the LZ4 and, for a zip, the archive."""
    raw = path.read_bytes()
    assert _parity_decompress_loxcc(raw) == miniserver_sync._decompress_loxcc(raw)


@requires_real_config
@pytest.mark.parametrize("path", CONFIGURATIONS, ids=lambda p: p.name)
def test_the_same_inputs_come_out_in_the_same_order(path: Path):
    """The whole point of the port: the whitelist is identical, order included.

    Compared against ``extract_inputs`` - the pygixml path that actually runs -
    and as a list, so a difference in ordering or in de-duplication cannot hide
    the way it did behind the old ``set()`` comparisons.
    """
    config_xml = miniserver_sync._decompress_loxcc(path.read_bytes())
    assert _parity_extract_inputs(config_xml) == miniserver_sync.extract_inputs(config_xml)


@pytest.mark.skipif(not REAL_CONFIG.exists(), reason="config/sps0.LoxCC is gitignored")
def test_the_known_configuration_still_measures_the_same():
    """The numbers this port was accepted against, pinned.

    If either of these moves, the configuration on this machine was replaced -
    which is fine, but it means the parity runs above are covering a different
    document than the one the port was signed off on.
    """
    config_xml = _parity_decompress_loxcc(REAL_CONFIG.read_bytes())
    assert len(config_xml) == 2_514_869
    assert config_xml.startswith(b"\xef\xbb\xbf<?xml version=")
    assert len(_parity_extract_inputs(config_xml)) == 122

#!/usr/bin/env python3
"""Record what the Python config module does, so the Rust port can be held to it.

Throwaway. This runs against the *current* ``loxmqttrelay.config`` and writes
``golden/config/``, which the Rust tests then read. It exists because the
alternative - transcribing several hundred error strings by hand into Rust
assertions and hoping - is exactly the kind of parity work that quietly goes
wrong. Delete this script together with ``src/loxmqttrelay/config.py``; keep the
corpus it produced, which stays useful as a regression suite long after.

Four corpora, one per behaviour the port has to reproduce:

``inputs/<name>.toml``     the document under test (checked in, hand-written below)
``<name>.problems``        ``validate_config_dict`` output, one per line, in order.
                           An empty file means the document is usable.
``<name>.warnings``        the WARNING lines the load emits - this is where
                           unknown sections and fields show up, since they are
                           deliberately not errors.
``<name>.saved.toml``      what ``save_config()`` writes. Only for usable documents.
``<name>.safe.json``       ``orjson.dumps(get_safe_config())``, byte for byte.
                           Only for usable documents.
``updates.jsonl``          one MQTT config update per line: the starting document,
                           the payload, the mode, and either the refusal message
                           or the document that resulted.

Run from the repository root:  uv run python scripts/gen_golden.py
"""

from __future__ import annotations

import json
import logging
import shutil
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "src"))

import orjson  # noqa: E402
import tomlkit  # noqa: E402

from loxmqttrelay.config import (  # noqa: E402
    AppConfig,
    ConfigError,
    global_config,
    validate_config_dict,
)

OUT = ROOT / "golden" / "config"
INPUTS = OUT / "inputs"


# ---------------------------------------------------------------------------
# The documents under test.
#
# Grouped by what they are meant to provoke rather than by section, and written
# out as text rather than built from dicts: the file is the input, and a reader
# comparing a Rust failure against it should see exactly what the parser saw.
# ---------------------------------------------------------------------------

# Every field, so a wrong-type document can set all 22 at once. Setting them
# together rather than one per file is deliberate: it also pins the order the
# problems are reported in, which is the file order and nothing else.
ALL_FIELDS = {
    "general": ["log_level", "base_topic", "cache_size"],
    "broker": ["host", "port", "user", "password", "client_id"],
    "miniserver": [
        "miniserver_ip",
        "miniserver_port",
        "miniserver_user",
        "miniserver_pass",
        "sync_with_miniserver",
    ],
    "topics": [
        "subscriptions",
        "subscription_filters",
        "topic_whitelist",
        "do_not_forward",
    ],
    "processing": ["expand_json", "convert_booleans"],
    "udp": ["udp_in_port", "udp_source_filter_enabled", "udp_allowed_sources"],
}


def every_field_set_to(literal: str) -> str:
    """A document assigning the same TOML literal to all 22 fields."""
    out = []
    for section, fields in ALL_FIELDS.items():
        out.append(f"[{section}]")
        out.extend(f"{name} = {literal}" for name in fields)
        out.append("")
    return "\n".join(out)


DOCUMENTS: dict[str, str] = {
    # -- usable documents ---------------------------------------------------
    "empty": "",
    "defaults_explicit": every_field_set_to("0").replace("= 0", "= 0"),  # replaced below
    "full_valid": """
[general]
log_level = "DEBUG"
base_topic = "myrelay/"
cache_size = 100000

[broker]
host = "broker.example"
port = 1883
user = "u"
password = "p"
client_id = "loxmqttrelay"

[miniserver]
miniserver_ip = "192.168.1.10"
miniserver_port = 80
miniserver_user = "admin"
miniserver_pass = "secret"
sync_with_miniserver = true

[topics]
subscriptions = ["a/#", "b/#"]
subscription_filters = ["^skip/"]
topic_whitelist = ["zeta/topic", "alpha/topic", "alpha/topic"]
do_not_forward = ["^noisy/"]

[processing]
expand_json = true
convert_booleans = true

[udp]
udp_in_port = 11884
udp_source_filter_enabled = true
udp_allowed_sources = ["10.0.0.1", "sensor.local"]
""".lstrip(),
    "optional_credentials_empty": """
[broker]
host = "localhost"
port = 1883
user = ""
password = ""
""".lstrip(),
    "log_level_lowercase": '[general]\nlog_level = "debug"\n',
    "port_low_boundary": "[broker]\nport = 1\n",
    "port_high_boundary": "[broker]\nport = 65535\n",
    "cache_size_zero": "[general]\ncache_size = 0\n",
    # -- unknown sections and fields: warnings, never errors -----------------
    "unknown_section": '[nonsense]\nwhatever = 1\n\n[general]\nlog_level = "INFO"\n',
    "unknown_field": '[general]\nlog_level = "INFO"\nnot_a_field = 1\n',
    # -- wrong types, one document per TOML type ----------------------------
    "wrong_type_bool": every_field_set_to("true"),
    "wrong_type_int": every_field_set_to("42"),
    "wrong_type_float": every_field_set_to("1.5"),
    "wrong_type_str": every_field_set_to('"x"'),
    "wrong_type_str_list": every_field_set_to('["a", "b"]'),
    "wrong_type_int_list": every_field_set_to("[1, 2]"),
    "wrong_type_mixed_list": every_field_set_to('["a", 1]'),
    "wrong_type_inline_table": every_field_set_to('{ k = "v" }'),
    "wrong_type_datetime": every_field_set_to("1979-05-27T07:32:00Z"),
    # The string "false" is truthy in Python. Pinning it is the whole reason
    # the type check exists separately from the value check.
    "bool_as_string": """
[processing]
expand_json = "false"
convert_booleans = "true"

[miniserver]
sync_with_miniserver = "false"
""".lstrip(),
    # -- value problems -----------------------------------------------------
    "port_zero": "[broker]\nport = 0\n",
    "port_negative": "[broker]\nport = -1\n",
    "port_too_high": "[broker]\nport = 65536\n",
    "all_ports_bad": """
[broker]
port = 0

[miniserver]
miniserver_port = 70000

[udp]
udp_in_port = -5
""".lstrip(),
    "cache_size_negative": "[general]\ncache_size = -1\n",
    "log_level_unknown": '[general]\nlog_level = "TRACE"\n',
    "base_topic_empty": '[general]\nbase_topic = ""\n',
    "base_topic_blank": '[general]\nbase_topic = "   "\n',
    "host_blank": '[broker]\nhost = "  "\n',
    "miniserver_ip_empty": '[miniserver]\nminiserver_ip = ""\n',
    # -- regex fields -------------------------------------------------------
    "regex_empty_pattern": '[topics]\nsubscription_filters = ["^ok/", ""]\n',
    "regex_blank_pattern": '[topics]\ndo_not_forward = ["   "]\n',
    "regex_unbalanced": '[topics]\nsubscription_filters = ["^(unclosed"]\n',
    "regex_bad_quantifier": '[topics]\ndo_not_forward = ["*nope"]\n',
    # The parity case that matters: Python's `re` accepts lookaround, the Rust
    # engine does not. Today this passes validation and the relay then fails to
    # start after the restart the update triggers.
    "regex_lookaround": '[topics]\nsubscription_filters = ["^(?!keep/).*"]\n',
    "regex_backreference": r'[topics]' + '\n' + r'do_not_forward = ["(a)\\1"]' + "\n",
    # -- structural ---------------------------------------------------------
    "section_is_scalar": 'general = "oops"\n\n[broker]\nport = 1883\n',
    "section_is_array": "general = [1, 2]\n",
    # -- several problems at once, to pin the reporting order ---------------
    "many_problems": """
[general]
log_level = "NOPE"
cache_size = -1

[broker]
port = 0
host = ""

[topics]
subscription_filters = [""]

[udp]
udp_in_port = 99999
""".lstrip(),
}

# Written out separately: an all-defaults document with every field spelled at
# its documented default, which is the one round-trip that has to be a no-op.
DOCUMENTS["defaults_explicit"] = """
[general]
log_level = "INFO"
base_topic = "myrelay/"
cache_size = 100000

[broker]
host = "localhost"
port = 1883
user = ""
password = ""
client_id = "loxmqttrelay"

[miniserver]
miniserver_ip = "127.0.0.1"
miniserver_port = 80
miniserver_user = ""
miniserver_pass = ""
sync_with_miniserver = true

[topics]
subscriptions = []
subscription_filters = []
topic_whitelist = []
do_not_forward = []

[processing]
expand_json = true
convert_booleans = true

[udp]
udp_in_port = 11884
udp_source_filter_enabled = true
udp_allowed_sources = []
""".lstrip()


# ---------------------------------------------------------------------------
# MQTT config updates.
#
# (starting document, payload, mode) -> refusal message, or the saved document.
# ---------------------------------------------------------------------------

UPDATES: list[tuple[str, str, dict, str]] = [
    # name, starting document, payload, mode
    ("protected_host", "full_valid", {"host": "evil.example"}, "set"),
    ("protected_port", "full_valid", {"port": 8883}, "set"),
    ("protected_user", "full_valid", {"user": "x"}, "set"),
    ("protected_password", "full_valid", {"password": "x"}, "set"),
    ("protected_ms_ip", "full_valid", {"miniserver_ip": "203.0.113.5"}, "set"),
    ("protected_ms_port", "full_valid", {"miniserver_port": 443}, "set"),
    ("protected_ms_user", "full_valid", {"miniserver_user": "x"}, "set"),
    ("protected_ms_pass", "full_valid", {"miniserver_pass": "x"}, "set"),
    ("protected_several", "full_valid", {"host": "a", "port": 1, "cache_size": 5}, "set"),
    ("unknown_field", "full_valid", {"no_such_field": 1}, "set"),
    ("unknown_and_protected", "full_valid", {"no_such_field": 1, "host": "a"}, "set"),
    # A bare item where a list is expected is allowed over MQTT, unlike in the
    # file. This is the whole allow_bare_item difference.
    ("bare_item_for_list", "full_valid", {"subscriptions": "single/#"}, "set"),
    ("bare_item_for_set", "full_valid", {"topic_whitelist": "OnlyOne"}, "set"),
    ("bare_item_add", "full_valid", {"subscriptions": "c/#"}, "add"),
    ("bare_item_remove", "full_valid", {"subscriptions": "a/#"}, "remove"),
    # Order-preserving dedupe: `add` dedupes the existing entries too.
    ("add_dedupes_existing", "dup_subscriptions", {"subscriptions": ["b/#"]}, "add"),
    ("add_keeps_order", "full_valid", {"subscriptions": ["z/#", "a/#", "m/#"]}, "add"),
    ("set_replaces", "full_valid", {"subscriptions": ["only/#"]}, "set"),
    ("remove_several", "full_valid", {"subscriptions": ["a/#", "b/#"]}, "remove"),
    ("remove_absent_is_noop", "full_valid", {"subscriptions": ["nope/#"]}, "remove"),
    # Set semantics on the whitelist, including the sorted serialization.
    ("whitelist_add", "full_valid", {"topic_whitelist": ["beta", "alpha"]}, "add"),
    ("whitelist_remove", "full_valid", {"topic_whitelist": ["alpha/topic"]}, "remove"),
    ("whitelist_set", "full_valid", {"topic_whitelist": ["m", "b", "z"]}, "set"),
    # Type problems.
    ("type_bool_for_int", "full_valid", {"cache_size": True}, "set"),
    ("type_str_for_int", "full_valid", {"cache_size": "50"}, "set"),
    ("type_str_for_bool", "full_valid", {"expand_json": "false"}, "set"),
    ("type_int_for_str", "full_valid", {"base_topic": 5}, "set"),
    ("type_int_list_for_str_list", "full_valid", {"subscriptions": [1, 2]}, "set"),
    ("type_null_for_str", "full_valid", {"base_topic": None}, "set"),
    # Value problems, which must be caught before anything is written.
    ("value_log_level", "full_valid", {"log_level": "TRACE"}, "set"),
    ("value_udp_port", "full_valid", {"udp_in_port": 0}, "set"),
    ("value_cache_size", "full_valid", {"cache_size": -1}, "set"),
    ("value_base_topic_blank", "full_valid", {"base_topic": "  "}, "set"),
    ("value_regex_empty", "full_valid", {"subscription_filters": [""]}, "set"),
    ("value_regex_invalid", "full_valid", {"do_not_forward": ["^("]}, "set"),
    ("value_regex_lookaround", "full_valid", {"subscription_filters": ["(?!x)"]}, "set"),
    # Several problems at once: order and the "; " join.
    (
        "many_problems",
        "full_valid",
        {"host": "a", "cache_size": -1, "log_level": "NOPE", "nope": 1},
        "set",
    ),
    # A batch is all-or-nothing: the good field must not survive the bad one.
    ("all_or_nothing", "full_valid", {"cache_size": 7, "udp_in_port": 0}, "set"),
    # Usable updates.
    ("ok_single", "full_valid", {"cache_size": 500}, "set"),
    ("ok_several", "full_valid", {"cache_size": 500, "expand_json": False}, "set"),
    ("ok_log_level", "full_valid", {"log_level": "WARNING"}, "set"),
]

# An extra starting document only the dedupe case needs.
DOCUMENTS["dup_subscriptions"] = """
[topics]
subscriptions = ["a/#", "b/#", "a/#", "c/#"]
""".lstrip()


class Captured(logging.Handler):
    """Collects the WARNING lines a load emits, in order."""

    def __init__(self) -> None:
        super().__init__(level=logging.WARNING)
        self.lines: list[str] = []

    def emit(self, record: logging.LogRecord) -> None:
        self.lines.append(record.getMessage())


def load_with_warnings(text: str) -> tuple[list[str], list[str], AppConfig | None]:
    """Validate and build one document, reporting problems and warnings."""
    handler = Captured()
    logger = logging.getLogger("loxmqttrelay.config")
    logger.addHandler(handler)
    previous = logger.propagate
    logger.propagate = False
    try:
        parsed = tomlkit.parse(text).unwrap()
        problems = validate_config_dict(parsed)
        config = None if problems else AppConfig.from_dict(parsed)
        return problems, handler.lines, config
    finally:
        logger.propagate = previous
        logger.removeHandler(handler)


def install(config: AppConfig, path: Path) -> None:
    """Point the singleton at a fresh state without going through __init__."""
    global_config._config = config
    global_config.config_path = str(path)


def main() -> int:
    if OUT.exists():
        shutil.rmtree(OUT)
    INPUTS.mkdir(parents=True)

    scratch = OUT / ".scratch.toml"
    usable: dict[str, str] = {}

    for name in sorted(DOCUMENTS):
        text = DOCUMENTS[name]
        (INPUTS / f"{name}.toml").write_text(text)

        problems, warnings, config = load_with_warnings(text)
        (OUT / f"{name}.problems").write_text(
            "".join(f"{p}\n" for p in problems)
        )
        (OUT / f"{name}.warnings").write_text(
            "".join(f"{w}\n" for w in warnings)
        )

        if config is None:
            continue
        usable[name] = text

        install(config, scratch)
        global_config.save_config()
        (OUT / f"{name}.saved.toml").write_text(scratch.read_text())
        (OUT / f"{name}.safe.json").write_bytes(
            orjson.dumps(global_config.get_safe_config())
        )

    # -- the update corpus --------------------------------------------------
    lines = []
    for case, start, payload, mode in UPDATES:
        if start not in usable:
            raise SystemExit(f"update case '{case}' starts from unusable '{start}'")
        _, _, config = load_with_warnings(usable[start])
        assert config is not None
        install(config, scratch)
        # save_config normalizes None to "", so the starting point recorded here
        # is what the relay would actually have had on disk.
        global_config.save_config()
        before = scratch.read_text()

        record = {
            "case": case,
            "start": start,
            "before": before,
            # As TEXT, not as a nested object. update_fields reports its problems
            # in payload order, and the record itself is written with
            # sort_keys=True - which applies recursively and would quietly sort
            # the very order under test.
            "payload": json.dumps(payload),
            "mode": mode,
        }
        try:
            global_config.update_fields(dict(payload), mode)
        except ConfigError as e:
            record["error"] = str(e)
        else:
            record["after"] = scratch.read_text()
            record["safe_json"] = orjson.dumps(
                global_config.get_safe_config()
            ).decode()
        lines.append(json.dumps(record, sort_keys=True, ensure_ascii=False))

    (OUT / "updates.jsonl").write_text("".join(f"{line}\n" for line in lines))
    scratch.unlink(missing_ok=True)

    print(f"{len(DOCUMENTS)} documents ({len(usable)} usable), {len(UPDATES)} updates")
    print(f"written to {OUT.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

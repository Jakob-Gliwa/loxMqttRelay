"""
The payload corpus for the flatten regression suite.

Everything here is built in memory at import time - there is no checked-in
JSON, because the corpus is fully determined by this module.

Three sources:
- anonymized real device payloads (heating controller, solar station, air
  sensor); structure preserved, identifiers replaced
- handcrafted oddities, aimed at the places a scanner-based flattener could
  plausibly disagree with the DOM implementation
- deterministic synthetics from a small LCG, plus a generator the fuzz test
  drives with its own seed
"""

from __future__ import annotations

import json

# ---------------------------------------------------------------------------
# Anonymized real payloads
# ---------------------------------------------------------------------------

REAL = {
    # Heating controller status block. Timestamp fixed, program label
    # genericized; structure and value types are untouched.
    "real_heating": {
        "datetime": "01.01.2020 00:00",
        "intoffset": 0.0,
        "floordry": "off",
        "dampedoutdoortemp": 22.9,
        "floordrytemp": 0,
        "building": "medium",
        "minexttemp": -14,
        "damping": "on",
        "hc1": {
            "seltemp": 20.5,
            "haclimate": "selTemp",
            "mode": "auto",
            "modetype": "comfort",
            "ecotemp": 12.0,
            "manualtemp": 17.0,
            "comforttemp": 20.5,
            "summertemp": 19,
            "designtemp": 45,
            "offsettemp": 0,
            "minflowtemp": 25,
            "maxflowtemp": 48,
            "roominfluence": 0,
            "roominflfactor": 4.0,
            "curroominfl": 0.0,
            "nofrostmode": "outdoor",
            "nofrosttemp": 5,
            "targetflowtemp": 23,
            "heatingtype": "floor",
            "summersetmode": "winter",
            "summermode": "winter",
            "controlmode": "weather compensated",
            "program": "prog 1",
            "tempautotemp": -1.0,
            "remoteseltemp": 0.0,
            "fastheatup": 0,
            "switchonoptimization": "off",
            "reducemode": "outdoor",
            "noreducetemp": -31,
            "reducetemp": 0,
            "coolingon": "on",
            "hpmode": "heating",
            "control": "CTRL000",
            "remotetemp": None,
            "remotehum": None,
            "switchprogmode": "level",
        },
        "dhw": {
            "mode": "own prog",
            "settemp": 60,
            "settemplow": 45,
            "circmode": "off",
            "chargeduration": 60,
            "charge": "off",
            "extra": "off",
            "disinfecting": "off",
            "disinfectday": "sa",
            "disinfecttime": 390,
            "dailyheating": "off",
            "dailyheattime": 120,
        },
    },
    # Solar station. No identifiers in this one; counters rounded.
    "real_solar": {
        "collectortemp": 19.8,
        "cylbottomtemp": 50.6,
        "solarpump": "off",
        "pumpworktime": 382271,
        "cylmaxtemp": 70,
        "collectorshutdown": "off",
        "cylheated": "off",
        "solarpumpmod": 0,
        "pumpminmod": 45,
        "turnondiff": 10.0,
        "turnoffdiff": 4.0,
        "heatassistvalve": "off",
        "solarpump2mod": 0,
        "cylpumpmod": 0,
        "valvestatus": "off",
        "collectormaxtemp": 120,
        "collectormintemp": 20,
        "energylasthour": 0.0,
        "energytoday": 0,
        "energytotal": 9744.5,
        "pump2worktime": 0,
        "m1worktime": 0,
        "heattransfersystem": "off",
        "externalcyl": "off",
        "thermaldisinfect": "off",
        "heatmetering": "off",
        "activated": "on",
        "solarpumpmode": "pwm",
        "solarpumpkick": "off",
        "plainwatermode": "off",
        "doublematchflow": "off",
        "climatezone": 90,
        "collector1area": 5.8,
        "heatcnt": 0,
    },
    # Air quality sensor. MAC and timestamps replaced with fixed dummies.
    "real_sensor": {
        "type": "12",
        "mac": "AABBCCDDEEFF",
        "version": "4.8.5",
        "timezone": 80,
        "timestamp": 51959580,
        "sensorData": [
            {
                "timestamp": {"value": 51959580},
                "battery": {"value": 100, "status": 1},
                "temperature": {
                    "value": 22.16,
                    "status": 0,
                    "unit": "C",
                    "co2_calib_state": 0,
                },
                "humidity": {"value": 53.99, "status": 0, "unit": "%"},
                "co2": {"value": 768, "status": 0},
                "pm25": {"value": 1, "status": 0},
                "pm10": {"value": 1, "status": 0},
            }
        ],
    },
}


# ---------------------------------------------------------------------------
# Handcrafted oddities - written as raw text so the exact bytes reach the parser
# ---------------------------------------------------------------------------

ODD: dict[str, str] = {
    "odd_empty_object": "{}",
    "odd_single_int": '{"n":1}',
    "odd_empty_string_value": '{"s":""}',
    "odd_all_scalar_kinds": '{"i":1,"f":1.5,"s":"x","t":true,"f2":false,"n":null}',
    "odd_nested_empty": '{"o":{},"a":[],"n":1}',
    "odd_empty_key": '{"":1,"a":2}',
    "odd_key_with_slash": '{"room/temp":21.5}',
    "odd_key_with_percent": '{"hum%":48}',
    "odd_key_slash_and_percent": '{"a/b%c":1}',
    # Both leaves normalize to the same target - order decides the final value
    "odd_colliding_targets": '{"a_b":1,"a":{"b":2}}',
    "odd_reverse_alpha_keys": '{"z":1,"y":2,"x":3,"m":4,"a":5}',
    "odd_nested_reverse_keys": '{"z":{"y":1,"a":2},"a":{"z":3}}',
    "odd_prefix_keys": '{"b":1,"bx":2,"bxy":3}',
    "odd_numeric_keys": '{"0":1,"1":2,"10":3,"2":4}',
    "odd_bool_keywords": (
        '{"t":"true","T":"TRUE","y":"yes","o":"ON","f":"false","F":"False",'
        '"n":"no","off":"OFF","en":"enabled","dis":"disabled","one":"1",'
        '"zero":"0","sel":"selected","chk":"checked"}'
    ),
    "odd_near_bool_words": '{"a":"tru","b":"truthy","c":"offline","d":"yesplease","e":"  on  "}',
    "odd_bool_padded": '{"a":" true ","b":"\\ton\\t","c":"        true"}',
    "odd_scientific_numbers": '{"a":1e3,"b":1.50,"c":-0,"d":0e0,"e":2.5e10,"f":1.00,"g":12.10,"h":0.10}',
    "odd_large_numbers": '{"a":999999999999999,"b":1234567890123456789,"c":51959580}',
    "odd_huge_numbers": '{"a":99999999999999999999999,"b":-99999999999999999999999}',
    "odd_tiny_numbers": '{"a":0.001,"b":0.0001,"c":1e-3,"d":-0.5,"e":-0.0}',
    "odd_long_decimal": '{"a":0.1234567890123456789,"b":3.141592653589793}',
    "odd_unicode_values": '{"unit":"\\u00b0C","name":"W\u00e4rme","emoji":"\U0001f525","cjk":"\u6e29\u5ea6"}',
    "odd_unicode_keys": '{"\u6e29\u5ea6":22.1,"W\u00e4rme":"on"}',
    "odd_escaped_quote": '{"s":"say \\"hi\\""}',
    "odd_escaped_backslash": '{"s":"a\\\\b"}',
    "odd_escaped_newline": '{"s":"line1\\nline2"}',
    "odd_escaped_unicode": '{"s":"\\u00b0C"}',
    "odd_escaped_slash": '{"s":"a\\/b"}',
    "odd_duplicate_key": '{"a":1,"a":2}',
    "odd_duplicate_nested": '{"o":{"x":1,"x":2}}',
    "odd_pretty_printed": '{\n  "a" : 1 ,\n  "b" : {\n    "c" : 2\n  }\n}\n',
    "odd_tabs_crlf": '{\t"a":1,\r\n\t"b":2\r\n}',
    "odd_leading_whitespace": '   \n\t{"a":1}',
    "odd_array_ints": '{"xs":[1,2,3]}',
    "odd_array_mixed": '{"xs":[1,"two",true,null,1.5]}',
    "odd_array_of_objects": '{"items":[{"id":1,"v":"on"},{"id":2,"v":"off"}]}',
    "odd_array_nested": '{"m":[[1,2],[3,4]]}',
    "odd_array_empty": '{"xs":[]}',
    "odd_array_of_empty": '{"xs":[{},{},[]]}',
    # Non-object roots and malformed input: forwarded unexpanded as one value
    "odd_root_array": "[1,2,3]",
    "odd_root_string": '"hello"',
    "odd_root_number": "42",
    "odd_root_true": "true",
    "odd_root_null": "null",
    "odd_empty_payload": "",
    "odd_plain_text": "on",
    "odd_plain_number_text": "21.5",
    "odd_truncated": '{"a":1,"b":',
    "odd_trailing_garbage": '{"a":1}xx',
    "odd_trailing_comma": '{"a":1,}',
    "odd_missing_value": '{"a":}',
    "odd_single_brace": "{",
    "odd_looks_like_json": "{not json at all}",
    "odd_leading_zero": '{"a":01}',
    "odd_plus_exponent": '{"a":1e+3}',
    "odd_base64_marker": "[base64:eJyLVspIzcnJVyjPL8pJUQIAJAoEXQ==]",
}


# ---------------------------------------------------------------------------
# Deterministic generation
# ---------------------------------------------------------------------------


class Lcg:
    """Small linear congruential generator, so nothing here drifts between runs."""

    def __init__(self, seed: int) -> None:
        self.state = seed & 0xFFFFFFFFFFFFFFFF

    def next(self) -> int:
        self.state = (self.state * 6364136223846793005 + 1) & 0xFFFFFFFFFFFFFFFF
        return self.state

    def below(self, n: int) -> int:
        return self.next() % n if n else 0


class RawLiteral:
    """A number written verbatim, so forms like ``1e3`` survive into the JSON."""

    def __init__(self, text: str) -> None:
        self.text = text


_WORDS = [
    "on", "off", "true", "false", "yes", "no", "enabled", "disabled",
    "ON", "OFF", "True", "maybe", "open", "closed", "auto", "",
    # values the scanner has to hand back to the DOM path
    'say "hi"', "back\\slash", "line\nbreak", "\u00b0C", "\u6e29\u5ea6",
]
_NUM_LITERALS = ["1e3", "1.50", "0.10", "-0", "0e0", "1E-2", "12.0", "0.0"]


def _leaf(rng: Lcg):
    kind = rng.below(9)
    if kind == 0:
        return rng.below(10_000) - 100
    if kind == 1:
        return round(rng.below(10_000) / 100, 2)
    if kind == 2:
        return True
    if kind == 3:
        return False
    if kind == 4:
        return None
    if kind in (5, 6):
        return _WORDS[rng.below(len(_WORDS))]
    if kind == 7:
        return RawLiteral(_NUM_LITERALS[rng.below(len(_NUM_LITERALS))])
    n = 1 + rng.below(12)
    return "".join(chr(ord("a") + ((rng.below(26) + i) % 26)) for i in range(n))


def _key(rng: Lcg, i: int) -> str:
    kind = rng.below(12)
    if kind == 0:
        return f"k{i}/sub"
    if kind == 1:
        return f"k{i}%"
    if kind == 2:
        return f"k_{i}"
    if kind == 3:
        return f"K{i}"
    return f"k{i}"


def _value(rng: Lcg, depth: int, budget: list[int]):
    if budget[0] <= 0 or depth <= 0:
        return _leaf(rng)
    budget[0] -= 1
    kind = rng.below(5)
    if kind == 0:
        return _object(rng, depth - 1, budget, 1 + rng.below(6))
    if kind == 1:
        return [_value(rng, depth - 1, budget) for _ in range(rng.below(5))]
    return _leaf(rng)


def _object(rng: Lcg, depth: int, budget: list[int], nkeys: int) -> dict:
    return {_key(rng, i): _value(rng, depth, budget) for i in range(nkeys)}


class _Encoder(json.JSONEncoder):
    def default(self, o):
        if isinstance(o, RawLiteral):
            return {"__raw__": o.text}
        return super().default(o)


_RAW_OPEN = '{"__raw__":"'


def _dump(value) -> str:
    """json.dumps that writes RawLiteral numbers as bare literals."""
    text = json.dumps(value, cls=_Encoder, ensure_ascii=False, separators=(",", ":"))
    while _RAW_OPEN in text:
        start = text.index(_RAW_OPEN)
        end = text.index('"}', start)
        text = text[:start] + text[start + len(_RAW_OPEN) : end] + text[end + 2 :]
    return text


def random_payload(rng: Lcg, depth: int = 5, budget: int = 40) -> str:
    """One arbitrary JSON object - what the fuzz test throws at both paths."""
    return _dump(_object(rng, depth, [budget], 1 + rng.below(8)))


def _synthetics() -> dict[str, str]:
    out: dict[str, str] = {}

    for seed in range(1, 41):
        rng = Lcg(seed * 17 + 3)
        out[f"synth_small_{seed:02d}"] = _dump(_object(rng, 3, [12], 1 + seed % 5))

    for seed in range(1, 26):
        rng = Lcg(seed * 31 + 11)
        out[f"synth_mid_{seed:02d}"] = _dump(_object(rng, 6, [80], 4 + seed % 6))

    for seed in range(1, 9):
        rng = Lcg(seed * 99 + 7)
        out[f"synth_large_{seed}"] = _dump(_object(rng, 8, [400], 12))

    # Device-shaped: several heating zones in one message
    for zones in (1, 4, 12):
        out[f"synth_hvac_zones_{zones:02d}"] = _dump({
            "datetime": "01.01.2020 00:00",
            "zones": [
                {
                    "id": z,
                    "name": f"zone{z}",
                    "seltemp": 18 + (z % 5) + (z % 10) / 10,
                    "mode": "auto",
                    "modetype": "comfort",
                    "ecotemp": 12.0,
                    "manualtemp": 17.0,
                    "comforttemp": 20.5,
                    "summertemp": 19,
                    "program": f"prog {(z % 3) + 1}",
                    "coolingon": "on",
                    "hpmode": "heating",
                    "remotetemp": None,
                    "flags": {"a": True, "b": False, "c": "off"},
                    "history": [z, z + 1, z + 2],
                }
                for z in range(zones)
            ],
            "dhw": {
                "mode": "own prog",
                "settemp": 60,
                "settemplow": 45,
                "circmode": "off",
                "extra": "off",
            },
            "meta": {"fw": "4.8.5", "ok": True},
        })

    out["synth_sensor_array_30"] = _dump({
        "type": "12",
        "mac": "AABBCCDDEEFF",
        "version": "4.8.5",
        "sensorData": [
            {
                "timestamp": {"value": 50_000_000 + i},
                "battery": {"value": 100 - (i % 20), "status": 1},
                "temperature": {
                    "value": 20 + (i % 8) + (i % 10) / 10,
                    "status": 0,
                    "unit": "C",
                },
                "humidity": {
                    "value": 40 + (i % 30) + (i % 10) / 10,
                    "status": 0,
                    "unit": "%",
                },
                "co2": {"value": 400 + i * 3, "status": 0},
            }
            for i in range(30)
        ],
    })

    deep: object = 1
    for i in reversed(range(24)):
        deep = {f"l{i}": deep}
    out["synth_deep_chain_24"] = _dump(deep)

    out["synth_wide_flat_200"] = _dump({f"k{i}": i for i in range(200)})
    out["synth_long_string_2k"] = _dump({"blob": "x" * 2048})

    return out


def build() -> dict[str, str]:
    payloads = {
        name: json.dumps(doc, ensure_ascii=False, separators=(",", ":"))
        for name, doc in REAL.items()
    }
    payloads.update(ODD)
    payloads.update(_synthetics())
    return payloads


PAYLOADS = build()

# Where this corpus came from

These files are a recording of the **Python** configuration module that this
relay used to be, made while it was still in the tree. `src/config/tests.rs`
asserts the Rust module reproduces it.

For each document in `inputs/`:

| file | what it records |
|---|---|
| `<name>.problems` | every reason `validate_config_dict` refused it, one per line, in file order. Empty means the document is usable. |
| `<name>.warnings` | the warnings the load emitted - unknown sections and fields, which are deliberately not errors |
| `<name>.saved.toml` | what `Config.save_config()` wrote, byte for byte (usable documents only) |
| `<name>.safe.json` | `orjson.dumps(get_safe_config())`, byte for byte - the exact payload published to `{base_topic}config/response` (usable documents only) |

`updates.jsonl` is one MQTT config update per line: the starting document, the
payload as text, the mode, and either the refusal message or the document that
resulted.

## It cannot be regenerated

The generator ran against `loxmqttrelay.config`, which no longer exists. That is
deliberate rather than an oversight: the corpus is a recording of a program that
is gone, and its value is precisely that it was taken before the port rather
than derived from it. Regenerating it from the Rust implementation would make it
a test of the Rust implementation against itself.

So treat it as frozen. If a behaviour here turns out to be wrong, change the
Rust *and* the golden in the same commit, and say in the message why the old
behaviour was not worth keeping - the way
`src/config/tests.rs::DIVERGENT` already documents the two places where the port
deliberately differs.

## Two cases already diverge

Both are the same decision, and both are named in `tests::DIVERGENT`:
`regex_lookaround` and `regex_backreference` have empty `.problems` files
because Python's `re` accepted those patterns, while the relay compiles filters
with the `regex` crate that actually has to run them. Under Python such a
pattern passed validation, was written to the file, restarted the relay, and
then failed at startup.

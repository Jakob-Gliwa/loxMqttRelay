# The configuration file, specified by example

This directory is the specification for `src/config`: 40 documents and 41
control-topic updates, each paired with exactly what the relay does with it.
`src/config/tests.rs` asserts all of it.

It is written as data rather than as assertions because of what is being pinned.
The interesting behaviour here is a few hundred error strings, their order, and
the exact bytes of two output formats - and those are far easier to read, review
and extend as files than as a wall of `assert_eq!`.

## What is recorded

For each document in `inputs/`:

| file | what it pins |
|---|---|
| `<name>.problems` | every reason the document is unusable, one per line, **in file order**. An empty file - or no file - means the document is fine. |
| `<name>.warnings` | what was ignored but is worth saying out loud. Only unknown sections and fields produce these, so most documents have no such file. |
| `<name>.saved.toml` | what `ConfigStore::save` writes, byte for byte. Only for usable documents. |
| `<name>.safe.json` | the exact payload published to `{base_topic}config/response`, byte for byte. Only for usable documents. |

`updates.jsonl` is one MQTT config update per line: the document it starts from,
the payload as text, the mode (`set`, `add` or `remove`), and either the refusal
message or the document and response that resulted.

Between them they cover every field against every wrong type, both port
boundaries, the log-level names, blank and unusable regex patterns, unknown
sections and fields, the fields a remote caller may not touch, list merging with
order-preserving de-duplication, set semantics on the whitelist, and that a
refused batch changes nothing.

## Changing it

The files are the expected behaviour, so a change here is a behaviour change.
Update the Rust and the file in the same commit, and say in the message why the
old behaviour was not worth keeping.

Two documents are refused although their `.problems` files are empty:
`regex_lookaround` and `regex_backreference`. Both are named in
`src/config/tests.rs::EXPECTED_TO_BE_REFUSED` with the reason - filter patterns
are compiled with the same regex engine that has to run them, so a pattern that
cannot run is refused when it is read rather than after the restart it triggers.

## Adding a case

Write the document into `inputs/`, run the tests, and read the failure: it
prints what the relay actually produced. Once that is right, write it into the
matching file. There is no generator - the expectations are meant to be looked
at and agreed with, not produced by the code they check.

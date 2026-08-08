//! The config module, checked against what the Python one did.
//!
//! Most of this reads `golden/config/`, which `scripts/gen_golden.py` produced
//! by running the *actual* Python implementation over 40 documents and 41 MQTT
//! updates. Comparing against a recording rather than against hand-written
//! assertions is the whole point: several hundred error strings were involved,
//! and transcribing them by eye is exactly where a port loses behaviour it was
//! supposed to keep.
//!
//! Where Rust deliberately differs, the case is named in [`DIVERGENT`] with the
//! reason. Nothing diverges silently.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::schema::{ConfigSection, FieldKind, FIELDS, field, fields_of};
use super::update::ListMode;
use super::value::{CfgValue, parse_json, parse_toml};
use super::validate::validate_document;
use super::{AppConfig, ConfigStore};

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("golden/config")
}

fn read_golden(name: &str) -> String {
    let path = golden_dir().join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every document in the corpus, by name.
fn documents() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(golden_dir().join("inputs"))
        .expect("golden inputs")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".toml")
                .to_owned()
        })
        .collect();
    names.sort();
    names
}

/// Cases where Rust is deliberately not what Python was.
///
/// Both entries are the same decision: config validation now compiles filter
/// patterns with `regex`, the engine that actually has to run them, instead of
/// with Python's `re`. The two disagree about lookaround and backreferences, and
/// under `re` such a pattern passed validation here, was written to the file,
/// restarted the relay, and *then* failed in `Core::new` - a relay that does not
/// come back, reported to the operator as an update that was accepted.
///
/// So these two documents have an empty `.problems` golden and are rejected
/// here. That is the fix, not a regression.
const DIVERGENT: [&str; 2] = ["regex_lookaround", "regex_backreference"];

/// The same decision, on the update corpus.
const DIVERGENT_UPDATES: [&str; 1] = ["value_regex_lookaround"];

/// Drop the engine-specific tail of an invalid-pattern message.
///
/// The wrapper - `'name' has an invalid pattern 'p':` - is ours and has to
/// match. What follows is the regex engine's own complaint, and `regex` explains
/// itself over several lines with a caret where `re` managed one clause. Holding
/// the port to Python's phrasing there would mean shipping a worse message to
/// keep a test quiet.
fn strip_engine_detail(problem: &str) -> String {
    match problem.find("has an invalid pattern '") {
        // Cut after the quote-colon that closes the pattern we echoed back.
        Some(_) => match problem.rfind("': ") {
            Some(at) => problem[..at + 2].to_owned(),
            None => problem.to_owned(),
        },
        None => problem.to_owned(),
    }
}

/// The golden file, one problem per line.
///
/// Safe to split on newlines because Python's `re.error` fits on one; the Rust
/// side must *not* be split that way, since `regex` explains itself over four -
/// which is exactly what [`strip_engine_detail`] removes.
fn golden_problems(text: &str) -> Vec<String> {
    text.lines().map(strip_engine_detail).collect()
}

// ---------------------------------------------------------------------------
// The table and the model agree
// ---------------------------------------------------------------------------

/// The field table lists exactly the fields the model has, in the same order.
///
/// Nothing enforces this at compile time - the table is written by hand because
/// a macro that emitted the structs would make the module unreadable - so this
/// is what catches a field added to one and not the other.
#[test]
fn the_field_table_matches_the_model() {
    assert_eq!(FIELDS.len(), 22, "22 fields across six sections");

    // Every section is represented, and the table is grouped by section in the
    // order the file is written.
    let order: Vec<ConfigSection> = FIELDS.iter().map(|spec| spec.section).collect();
    let mut expected = Vec::new();
    for section in ConfigSection::ALL {
        let count = FIELDS.iter().filter(|s| s.section == section).count();
        assert!(count > 0, "section [{}] has no fields", section.as_str());
        expected.extend(std::iter::repeat_n(section, count));
    }
    assert_eq!(order, expected, "the table is not grouped in file order");
}

/// No field name is shared between two sections.
///
/// The whole reason the table exists is that a `config/set` payload names a
/// field without its section. If two sections ever held the same name, that
/// lookup would silently pick one of them - Python's `_map_fields_to_sections`
/// had the same hazard and let the later section win.
#[test]
fn no_field_name_is_shared_between_sections() {
    let unique: BTreeSet<&str> = FIELDS.iter().map(|spec| spec.name).collect();
    assert_eq!(unique.len(), FIELDS.len());
}

/// Every field can be read and written by name.
///
/// `get_field` and `set_field` are two hand-written matches over the same 22
/// names, and a field missing from either would otherwise fail silently: a
/// `config/set` that appears to work and changes nothing.
#[test]
fn every_field_round_trips() {
    for spec in FIELDS {
        let mut config = AppConfig::default();
        let written = match spec.kind {
            FieldKind::Bool => CfgValue::Bool(!matches!(
                AppConfig::default().get_field(spec),
                CfgValue::Bool(true)
            )),
            FieldKind::Int => CfgValue::Int(4242),
            FieldKind::Str | FieldKind::OptStr => CfgValue::Str("probe".to_owned()),
            FieldKind::StrList | FieldKind::StrSet => {
                CfgValue::from_strings(["probe-a", "probe-b"])
            }
        };
        config.set_field(spec, written.clone());
        assert_eq!(
            config.get_field(spec),
            written,
            "'{}' did not survive a set/get",
            spec.name
        );
        assert_ne!(
            config, AppConfig::default(),
            "'{}' was written but nothing changed",
            spec.name
        );
    }
}

/// The defaults are the documented ones.
#[test]
fn the_defaults_are_what_the_file_says() {
    let saved = read_golden("empty.saved.toml");
    let expected = read_golden("defaults_explicit.saved.toml");
    assert_eq!(saved, expected, "an empty file is the explicit defaults");

    let store = ConfigStore::new("unused.toml", AppConfig::default());
    assert_eq!(String::from_utf8(store.safe_json()).unwrap(), read_golden("empty.safe.json").trim_end());
}

// ---------------------------------------------------------------------------
// Goldens
// ---------------------------------------------------------------------------

/// Every document is refused, or accepted, for exactly the reasons Python gave.
#[test]
fn the_problems_are_the_ones_python_reported() {
    for name in documents() {
        let text = read_golden(&format!("inputs/{name}.toml"));
        let document = parse_toml(&text).expect("the corpus is valid TOML");
        let found = validate_document(&document);

        if DIVERGENT.contains(&name.as_str()) {
            assert!(
                read_golden(&format!("{name}.problems")).is_empty(),
                "{name}: the golden was expected to be empty"
            );
            assert_eq!(
                found.problems.len(),
                1,
                "{name}: the pattern should now be refused"
            );
            assert!(
                found.problems[0].contains("has an invalid pattern"),
                "{name}: {:?}",
                found.problems
            );
            continue;
        }

        assert_eq!(
            found
                .problems
                .iter()
                .map(|p| strip_engine_detail(p))
                .collect::<Vec<_>>(),
            golden_problems(&read_golden(&format!("{name}.problems"))),
            "{name}: problems differ"
        );
    }
}

/// Unknown sections and fields are ignored, and said out loud.
#[test]
fn the_warnings_are_the_ones_python_reported() {
    for name in documents() {
        let text = read_golden(&format!("inputs/{name}.toml"));
        let document = parse_toml(&text).expect("the corpus is valid TOML");
        let found = validate_document(&document);
        let mut warnings = found.warnings.clone();
        if found.problems.is_empty() {
            warnings.extend(AppConfig::from_document(&document).1);
        }
        assert_eq!(
            warnings,
            read_golden(&format!("{name}.warnings"))
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            "{name}: warnings differ"
        );
    }
}

/// A usable document is written back out the way Python wrote it.
#[test]
fn a_saved_file_is_byte_for_byte_what_python_wrote() {
    for name in documents() {
        let path = golden_dir().join(format!("{name}.saved.toml"));
        if !path.exists() {
            continue; // The document was refused; there is nothing to save.
        }
        let text = read_golden(&format!("inputs/{name}.toml"));
        let document = parse_toml(&text).expect("the corpus is valid TOML");
        let (config, _) = AppConfig::from_document(&document);

        let dir = tempdir(&format!("save-{name}"));
        let file = dir.join("config.toml");
        ConfigStore::new(&file, config).save();

        assert_eq!(
            fs::read_to_string(&file).expect("written"),
            fs::read_to_string(&path).expect("golden"),
            "{name}: saved file differs"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

/// The `config/get` response is byte for byte what orjson produced.
///
/// One assertion covering three things at once: which fields are redacted, the
/// order the keys come in, and that the whitelist is a sorted array.
#[test]
fn the_config_response_is_byte_for_byte_what_python_published() {
    for name in documents() {
        let path = golden_dir().join(format!("{name}.safe.json"));
        if !path.exists() {
            continue;
        }
        let text = read_golden(&format!("inputs/{name}.toml"));
        let document = parse_toml(&text).expect("the corpus is valid TOML");
        let (config, _) = AppConfig::from_document(&document);
        assert_eq!(
            String::from_utf8(config.safe_json()).unwrap(),
            fs::read_to_string(&path).expect("golden"),
            "{name}: config/response differs"
        );
    }
}

/// Every recorded MQTT update is refused, or applied, exactly as Python did.
#[test]
fn the_updates_do_what_python_did() {
    let corpus = read_golden("updates.jsonl");
    let mut seen = 0;
    for line in corpus.lines() {
        let record: serde_json::Value = serde_json::from_str(line).expect("a corpus record");
        let case = record["case"].as_str().expect("case");
        let mode = match record["mode"].as_str().expect("mode") {
            "set" => ListMode::Set,
            "add" => ListMode::Add,
            "remove" => ListMode::Remove,
            other => panic!("{case}: unknown mode {other}"),
        };

        let before = record["before"].as_str().expect("before");
        let document = parse_toml(before).expect("the corpus is valid TOML");
        let (config, _) = AppConfig::from_document(&document);

        let dir = tempdir(&format!("update-{case}"));
        let file = dir.join("config.toml");
        fs::write(&file, before).expect("seed");
        let store = ConfigStore::new(&file, config);

        // The payload is stored as text so its key order survives the corpus,
        // and parsed with our own reader so the order survives serde too.
        let payload = record["payload"].as_str().expect("payload text");
        let updates = match parse_json(payload).expect("the corpus is valid JSON") {
            CfgValue::Table(entries) => entries,
            other => panic!("{case}: payload is {}", other.py_type()),
        };
        let outcome = store.update_fields(&updates, mode);

        if DIVERGENT_UPDATES.contains(&case) {
            assert!(
                record.get("error").is_none(),
                "{case}: the golden was expected to succeed"
            );
            let message = outcome.expect_err("the pattern should now be refused").0;
            assert!(message.contains("has an invalid pattern"), "{case}: {message}");
            let _ = fs::remove_dir_all(&dir);
            seen += 1;
            continue;
        }

        match (record.get("error").and_then(|e| e.as_str()), outcome) {
            (Some(expected), Err(actual)) => assert_eq!(
                strip_engine_detail(&actual.to_string()),
                strip_engine_detail(expected),
                "{case}: refusal differs"
            ),
            (Some(expected), Ok(())) => panic!("{case}: should have been refused: {expected}"),
            (None, Err(actual)) => panic!("{case}: should have been applied, got {actual}"),
            (None, Ok(())) => {
                assert_eq!(
                    fs::read_to_string(&file).expect("written"),
                    record["after"].as_str().expect("after"),
                    "{case}: the saved file differs"
                );
                assert_eq!(
                    String::from_utf8(store.safe_json()).unwrap(),
                    record["safe_json"].as_str().expect("safe_json"),
                    "{case}: the config/response differs"
                );
            }
        }
        let _ = fs::remove_dir_all(&dir);
        seen += 1;
    }
    assert!(seen >= 40, "the update corpus shrank to {seen} cases");
}

// ---------------------------------------------------------------------------
// Behaviours the corpus cannot record
// ---------------------------------------------------------------------------

/// A missing file is the defaults, not a failure.
#[test]
fn a_missing_file_starts_on_the_defaults() {
    let dir = tempdir("missing");
    let store = ConfigStore::load(dir.join("nowhere.toml")).expect("defaults");
    assert_eq!(store.snapshot(), AppConfig::default());
    let _ = fs::remove_dir_all(&dir);
}

/// A file that is not TOML at all is reported, not panicked over.
///
/// Python let tomlkit's exception escape at import time, which produced a
/// traceback instead of the message the operator needed.
#[test]
fn a_file_that_is_not_toml_is_refused() {
    let dir = tempdir("not-toml");
    let file = dir.join("config.toml");
    fs::write(&file, "[general\nlog_level = ").expect("seed");
    assert!(ConfigStore::load(&file).is_err());
    let _ = fs::remove_dir_all(&dir);
}

/// The whitelist sync writes the one field it is allowed to, without the checks
/// a remote caller gets.
#[test]
fn a_local_section_update_is_not_subject_to_the_remote_rules() {
    let dir = tempdir("section-update");
    let file = dir.join("config.toml");
    let store = ConfigStore::new(&file, AppConfig::default());

    store.update_section(
        ConfigSection::Topics,
        &[(
            "topic_whitelist".to_owned(),
            CfgValue::from_strings(["zeta", "alpha"]),
        )],
        ListMode::Set,
    );
    assert_eq!(
        store.snapshot().topics.topic_whitelist,
        BTreeSet::from(["alpha".to_owned(), "zeta".to_owned()])
    );

    // A protected field is refused over MQTT but reachable here - and the
    // section scoping means naming it under the wrong section does nothing.
    store.update_section(
        ConfigSection::Topics,
        &[("host".to_owned(), CfgValue::Str("elsewhere".to_owned()))],
        ListMode::Set,
    );
    assert_eq!(store.snapshot().broker.host, "localhost");
    let _ = fs::remove_dir_all(&dir);
}

/// A refused batch changes nothing at all, not even the fields before the bad
/// one.
#[test]
fn a_refused_batch_leaves_the_file_alone() {
    let dir = tempdir("all-or-nothing");
    let file = dir.join("config.toml");
    let store = ConfigStore::new(&file, AppConfig::default());
    store.save();
    let before = fs::read_to_string(&file).expect("written");

    let error = store
        .update_fields(
            &[
                ("cache_size".to_owned(), CfgValue::Int(7)),
                ("udp_in_port".to_owned(), CfgValue::Int(0)),
            ],
            ListMode::Set,
        )
        .expect_err("refused");
    assert!(error.to_string().contains("must be between 1 and 65535"));
    assert_eq!(store.snapshot().general.cache_size, 100_000);
    assert_eq!(fs::read_to_string(&file).expect("written"), before);
    let _ = fs::remove_dir_all(&dir);
}

/// Python's `str.strip()` counts four separators Rust's `trim()` does not, and a
/// `base_topic` made of them has to read as empty either way.
#[test]
fn the_blank_check_uses_pythons_idea_of_whitespace() {
    let spec = field("base_topic").expect("a known field");
    let problem = super::validate::value_problem(
        spec.name,
        spec.checks,
        &CfgValue::Str("\u{1c}\u{1f}".to_owned()),
    );
    assert!(problem.is_some(), "U+001C..U+001F should count as blank");
}

/// Sets are written sorted, whatever order they arrived in.
#[test]
fn the_whitelist_is_written_sorted() {
    let mut config = AppConfig::default();
    let spec = field("topic_whitelist").expect("a known field");
    config.set_field(spec, CfgValue::from_strings(["zulu", "alpha", "mike"]));
    let json = String::from_utf8(config.safe_json()).unwrap();
    assert!(
        json.contains(r#""topic_whitelist":["alpha","mike","zulu"]"#),
        "{json}"
    );
}

/// Every section named in the model is one the parser knows, and vice versa.
#[test]
fn the_section_names_round_trip() {
    for section in ConfigSection::ALL {
        assert_eq!(ConfigSection::parse(section.as_str()), Some(section));
        assert!(fields_of(section).count() > 0);
    }
    assert_eq!(ConfigSection::parse("nonsense"), None);
}

/// Every document in the corpus loads, or is refused, as its goldens say.
///
/// `validate_document` is what the goldens above exercise; this is the wiring
/// around it - that a document with problems actually stops the start, and that
/// one without them produces the configuration the goldens recorded.
#[test]
fn load_refuses_exactly_the_documents_with_problems() {
    for name in documents() {
        let dir = tempdir(&format!("load-{name}"));
        let file = dir.join("config.toml");
        fs::write(&file, read_golden(&format!("inputs/{name}.toml"))).expect("seed");

        let refused = !read_golden(&format!("{name}.problems")).is_empty()
            || DIVERGENT.contains(&name.as_str());
        match ConfigStore::load(&file) {
            Ok(store) => {
                assert!(!refused, "{name}: should have been refused");
                assert_eq!(
                    String::from_utf8(store.safe_json()).unwrap(),
                    read_golden(&format!("{name}.safe.json")),
                    "{name}: loaded to a different configuration"
                );
            }
            Err(_) => assert!(refused, "{name}: should have loaded"),
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

/// A file that cannot be written is reported, and left exactly as it was.
///
/// The point is what does *not* happen: no chmod. Widening the file to get one
/// write through would leave the broker and Miniserver passwords readable for
/// every account on the host, permanently, to fix something temporary.
#[cfg(unix)]
#[test]
fn a_config_that_cannot_be_written_keeps_its_permissions() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::getuid().is_root() {
        // Root writes through a read-only file, so there is nothing to observe.
        return;
    }

    let dir = tempdir("read-only");
    let file = dir.join("config.toml");
    let store = ConfigStore::new(&file, AppConfig::default());
    store.save();
    let original = fs::read_to_string(&file).expect("written");

    fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).expect("chmod");
    let mode_before = fs::metadata(&file).expect("stat").permissions().mode();

    store
        .update_fields(
            &[("cache_size".to_owned(), CfgValue::Int(4242))],
            ListMode::Set,
        )
        .expect("the update itself is usable");

    assert_eq!(
        fs::metadata(&file).expect("stat").permissions().mode(),
        mode_before,
        "the file was made writable to get a save through"
    );
    assert_eq!(
        fs::read_to_string(&file).expect("read"),
        original,
        "the file changed despite the failed write"
    );
    // The relay keeps running with the value it was given; only the file is
    // stale, which is what the second error message says.
    assert_eq!(store.snapshot().general.cache_size, 4242);

    let _ = fs::set_permissions(&file, fs::Permissions::from_mode(0o644));
    let _ = fs::remove_dir_all(&dir);
}

/// Saving is idempotent: what was written reads back and writes out the same.
///
/// Not an identity, and deliberately not asserted as one. A `user` that is unset
/// is `None` in the model, `""` in the file, and therefore `Some("")` once
/// reloaded - TOML has no null, and spelling an absent broker user as the empty
/// string is how the file has always looked. Python normalized it in exactly the
/// same place, so the first save is where the value settles and every save after
/// it is a no-op. That second property is the one worth holding, because it is
/// what stops a relay from rewriting its own config file on every restart.
#[test]
fn saving_a_reloaded_file_changes_nothing() {
    for name in documents() {
        // A `.saved.toml` golden only means Python accepted the document. The
        // two divergent ones carry a pattern this build refuses, so writing them
        // out and reading them back is not a round trip - it is the refusal
        // working. (Python did not run on them either; it got as far as
        // `Core::new` and failed there, which is the same outcome reported
        // worse.)
        if !golden_dir().join(format!("{name}.saved.toml")).exists()
            || DIVERGENT.contains(&name.as_str())
        {
            continue;
        }
        let text = read_golden(&format!("inputs/{name}.toml"));
        let (config, _) = AppConfig::from_document(&parse_toml(&text).expect("valid TOML"));

        let dir = tempdir(&format!("roundtrip-{name}"));
        let once = dir.join("once.toml");
        ConfigStore::new(&once, config).save();

        let reloaded = ConfigStore::load(&once).expect("what we wrote must load");
        let twice = dir.join("twice.toml");
        ConfigStore::new(&twice, reloaded.snapshot()).save();

        assert_eq!(
            fs::read_to_string(&twice).expect("second save"),
            fs::read_to_string(&once).expect("first save"),
            "{name}: a reload-and-save changed the file"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

/// A section update merges the same way a remote one does.
///
/// `update_fields` is covered exhaustively by the corpus; this is the other
/// entry point, and the modes have to mean the same thing on both.
#[test]
fn a_section_update_merges_in_every_mode() {
    let dir = tempdir("section-modes");
    let file = dir.join("config.toml");
    let store = ConfigStore::new(&file, AppConfig::default());

    store.update_section(
        ConfigSection::Topics,
        &[("subscriptions".to_owned(), CfgValue::from_strings(["a/#", "b/#"]))],
        ListMode::Set,
    );
    assert_eq!(store.snapshot().topics.subscriptions, ["a/#", "b/#"]);

    // `add` appends and dedupes, keeping first-seen order.
    store.update_section(
        ConfigSection::Topics,
        &[("subscriptions".to_owned(), CfgValue::from_strings(["b/#", "c/#"]))],
        ListMode::Add,
    );
    assert_eq!(store.snapshot().topics.subscriptions, ["a/#", "b/#", "c/#"]);

    store.update_section(
        ConfigSection::Topics,
        &[("subscriptions".to_owned(), CfgValue::from_strings(["b/#"]))],
        ListMode::Remove,
    );
    assert_eq!(store.snapshot().topics.subscriptions, ["a/#", "c/#"]);

    // A set stays sorted whatever order it is merged in.
    store.update_section(
        ConfigSection::Topics,
        &[("topic_whitelist".to_owned(), CfgValue::from_strings(["zulu", "alpha"]))],
        ListMode::Add,
    );
    assert_eq!(
        store.snapshot().topics.topic_whitelist,
        BTreeSet::from(["alpha".to_owned(), "zulu".to_owned()])
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The store is shared across tasks and threads, so it has to be usable from
/// several at once without losing an update.
///
/// Python had a singleton behind a lock and a test that hammered it. Here the
/// store is passed around as an `Arc` instead, and this is the equivalent
/// question: does concurrent use stay consistent.
#[test]
fn the_store_survives_concurrent_use() {
    use std::sync::Arc;
    use std::thread;

    let dir = tempdir("concurrent");
    let store = Arc::new(ConfigStore::new(dir.join("config.toml"), AppConfig::default()));

    let writers: Vec<_> = (0..8)
        .map(|i| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                for round in 0..8 {
                    store
                        .update_fields(
                            &[(
                                "subscriptions".to_owned(),
                                CfgValue::from_strings([format!("t/{i}/{round}")]),
                            )],
                            ListMode::Add,
                        )
                        .expect("a usable update");
                    // A reader alongside, because `snapshot` takes the same lock.
                    let _ = store.snapshot();
                    let _ = store.safe_json();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("no writer panicked");
    }

    // Every one of the 64 additions is present exactly once.
    let subscriptions = store.snapshot().topics.subscriptions;
    assert_eq!(subscriptions.len(), 64, "an update was lost");
    let unique: BTreeSet<&String> = subscriptions.iter().collect();
    assert_eq!(unique.len(), 64, "an entry was duplicated");
    let _ = fs::remove_dir_all(&dir);
}

/// A scratch directory of our own, so the tests do not have to run one at a time.
fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("loxmqttrelay-config-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

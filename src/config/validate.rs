//! Why a configuration value cannot be used.
//!
//! Every message in here is transcribed from `config.py`, deliberately down to
//! the punctuation. They end up in a startup log next to `Refusing to start`,
//! and an operator who has hit one before should find the same words - so this
//! is one of the few places where matching the previous wording is the
//! requirement rather than a nicety. `golden/config/*.problems` is what holds
//! them to it.
//!
//! The one difference between checking a file and checking a `config/set`
//! payload is `allow_bare_item`: a payload may name a single entry where a list
//! is expected, because that is a natural thing to publish, whereas a TOML file
//! has real arrays and no reason to. Everything else is shared.

use crate::config::schema::{Check, ConfigSection, FieldKind, FieldSpec, field};
use crate::config::value::CfgValue;
use crate::util::py_strip;

const LOG_LEVELS: [&str; 5] = ["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"];

/// Why `value` does not fit `kind`, or `None` if it does.
pub(crate) fn type_mismatch(
    name: &str,
    kind: FieldKind,
    value: &CfgValue,
    allow_bare_item: bool,
) -> Option<String> {
    match kind {
        FieldKind::OptStr => match value {
            CfgValue::Null | CfgValue::Str(_) => None,
            other => Some(format!(
                "'{name}' expects {}, got {}",
                kind.expected(),
                other.py_type()
            )),
        },
        FieldKind::StrList | FieldKind::StrSet => {
            let items: &[CfgValue] = match value {
                CfgValue::List(items) => items,
                // A bare element stands for a one-element collection, the same
                // way the update itself unwraps the payload.
                _ if allow_bare_item => std::slice::from_ref(value),
                other => {
                    return Some(format!(
                        "'{name}' expects a list, got {}",
                        other.py_type()
                    ));
                }
            };
            if items.iter().all(|item| matches!(item, CfgValue::Str(_))) {
                None
            } else {
                Some(format!("'{name}' expects a list of str"))
            }
        }
        FieldKind::Bool => match value {
            CfgValue::Bool(_) => None,
            other => Some(format!(
                "'{name}' expects bool, got {}",
                other.py_type()
            )),
        },
        FieldKind::Int => match value {
            CfgValue::Int(_) => None,
            other => Some(format!("'{name}' expects int, got {}", other.py_type())),
        },
        FieldKind::Str => match value {
            CfgValue::Str(_) => None,
            other => Some(format!("'{name}' expects str, got {}", other.py_type())),
        },
    }
}

/// What is wrong with a value of the right type, or `None`.
///
/// Only reached once the type fits, so reading the value out below cannot fail
/// for a reason that has not already been reported.
pub(crate) fn value_problem(name: &str, checks: &[Check], value: &CfgValue) -> Option<String> {
    for check in checks {
        let problem = match check {
            Check::Port => value
                .as_int()
                .filter(|n| !(1..=65535).contains(n))
                .map(|n| format!("'{name}' must be between 1 and 65535, got {n}")),
            Check::NonNegative => value
                .as_int()
                .filter(|n| *n < 0)
                .map(|n| format!("'{name}' cannot be negative, got {n}")),
            Check::LogLevel => value
                .as_str()
                .filter(|s| !LOG_LEVELS.contains(&s.to_uppercase().as_str()))
                .map(|s| {
                    format!(
                        "'log_level' must be one of {}, got '{s}'",
                        LOG_LEVELS.join(", ")
                    )
                }),
            Check::NonBlankTopic => value
                .as_str()
                .filter(|s| py_strip(s).is_empty())
                .map(|_| {
                    "'base_topic' cannot be empty - it prefixes every control topic".to_owned()
                }),
            Check::NonBlank => value
                .as_str()
                .filter(|s| py_strip(s).is_empty())
                .map(|_| format!("'{name}' cannot be empty")),
            Check::RegexList => regex_problem(name, value),
        };
        if problem.is_some() {
            return problem;
        }
    }
    None
}

/// The first unusable pattern in a filter list.
///
/// Compiled with `regex`, the engine that actually has to run them, rather than
/// with something bent towards accepting what Python's `re` accepted. The two
/// disagree about lookaround and backreferences, and a pattern only `re` accepts
/// used to pass validation here, get written to the file, restart the relay, and
/// then fail in `Core::new` - which is a relay that does not come back, reported
/// as a configuration update that was accepted.
fn regex_problem(name: &str, value: &CfgValue) -> Option<String> {
    let patterns = value.as_strings()?;
    for pattern in patterns {
        if py_strip(&pattern).is_empty() {
            // An empty expression matches every topic, so one stray "" in the
            // list filters away everything instead of the one thing it named.
            return Some(format!(
                "'{name}' has an empty pattern - an empty expression matches every topic"
            ));
        }
        if let Err(e) = regex::Regex::new(&pattern) {
            return Some(format!("'{name}' has an invalid pattern '{pattern}': {e}"));
        }
    }
    None
}

/// What checking a document turned up.
///
/// Warnings are handed back rather than logged from inside, so that a test can
/// assert on them without installing a global logger - which under `cargo test`
/// only one test per process could do. [`super::ConfigStore::load`] logs them.
#[derive(Debug, Default)]
pub(crate) struct Findings {
    /// Every reason the file cannot be used, in file order.
    pub(crate) problems: Vec<String>,
    /// Things that were ignored but are worth saying out loud.
    pub(crate) warnings: Vec<String>,
}

/// Check a parsed document, in file order.
///
/// Runs before the typed configuration is built, so a value of the wrong type is
/// named here instead of surfacing later as an unreadable field or - worse - as
/// a string like "false" that is quietly truthy.
///
/// Unknown sections and fields are deliberately not errors: an upgrade that
/// drops an option must not stop the relay from starting. Unknown sections are
/// warned about here; unknown fields in [`super::AppConfig::from_document`],
/// where the section they landed in is unambiguous.
pub(crate) fn validate_document(document: &[(String, CfgValue)]) -> Findings {
    let mut found = Findings::default();
    for (section_name, section_value) in document {
        let Some(section) = ConfigSection::parse(section_name) else {
            found.warnings.push(format!(
                "Unknown configuration section '[{section_name}]' will be ignored."
            ));
            continue;
        };
        let Some(entries) = section_value.as_table() else {
            found
                .problems
                .push(format!("'[{section_name}]' must be a table"));
            continue;
        };
        for (key, value) in entries {
            // A field this build does not know is not a problem, only a
            // warning - and that warning belongs to construction, where the
            // section it lands in is unambiguous.
            let Some(spec) = field_in(section, key) else {
                continue;
            };
            let problem = type_mismatch(key, spec.kind, value, false)
                .or_else(|| value_problem(key, spec.checks, value));
            if let Some(problem) = problem {
                found.problems.push(format!("[{section_name}] {problem}"));
            }
        }
    }
    found
}

/// The spec for `name`, but only if it belongs to `section`.
///
/// No field name is shared between two sections today (`super::tests` asserts
/// that), so this is the same lookup as [`field`] with a guard that would catch
/// it if one ever were.
fn field_in(section: ConfigSection, name: &str) -> Option<&'static FieldSpec> {
    field(name).filter(|spec| spec.section == section)
}

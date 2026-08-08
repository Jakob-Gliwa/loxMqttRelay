//! Changing the configuration while the relay runs.
//!
//! Two callers, and the difference between them is the point of this file.
//!
//! [`ConfigStore::update_fields`] is what a `config/set`, `config/add` or
//! `config/remove` payload reaches. It is all-or-nothing and checks everything
//! before touching the first field, because a rejected field must not leave the
//! ones before it applied and written out - and because the update triggers a
//! restart, so a value the relay cannot read must never reach the file at all.
//! That would not be a refused update, it would be a relay that does not come
//! back.
//!
//! [`ConfigStore::update_section`] is what the whitelist sync uses. It checks
//! nothing and protects nothing: the values come from the Miniserver's own
//! configuration rather than from the network, and the whitelist is exactly the
//! field a remote caller may not be trusted with but this one must write.

use std::collections::{BTreeSet, HashSet};
use std::fmt;

use crate::config::schema::{ConfigSection, FieldKind, field, fields_of};
use crate::config::validate::{type_mismatch, value_problem};
use crate::config::value::CfgValue;
use crate::config::{AppConfig, ConfigStore};

/// What to do with a collection-valued field.
///
/// Scalar fields ignore this - `set`, `add` and `remove` all just assign, which
/// is what `_apply_field` did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ListMode {
    Set,
    Add,
    Remove,
}

impl ListMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ListMode::Set => "set",
            ListMode::Add => "add",
            ListMode::Remove => "remove",
        }
    }

    /// The mode a control topic names.
    ///
    /// The same three words `ControlTopic::update_mode` hands to Python, read
    /// back on the native side so the two cannot spell them differently.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "set" => ListMode::Set,
            "add" => ListMode::Add,
            "remove" => ListMode::Remove,
            _ => return None,
        })
    }
}

/// An update that was refused, with every reason it was refused.
///
/// The reasons are joined rather than reported one at a time so that publishing
/// a payload with three mistakes in it tells you about all three, instead of
/// costing three round trips.
#[derive(Debug)]
pub(crate) struct ConfigError(pub(crate) String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rejected configuration update: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// `dict.fromkeys(current + incoming)`: dedupe, keep first-seen order.
///
/// Note it dedupes the *existing* entries too, not only the incoming ones. A
/// file that already listed a topic twice comes back with it once, which is what
/// the Python version did and what the goldens record.
fn merge_list(current: &[String], incoming: Vec<String>, mode: ListMode) -> Vec<String> {
    match mode {
        ListMode::Set => incoming,
        ListMode::Add => {
            let mut seen = HashSet::new();
            current
                .iter()
                .cloned()
                .chain(incoming)
                .filter(|item| seen.insert(item.clone()))
                .collect()
        }
        ListMode::Remove => {
            let drop: HashSet<String> = incoming.into_iter().collect();
            current
                .iter()
                .filter(|item| !drop.contains(*item))
                .cloned()
                .collect()
        }
    }
}

fn merge_set(current: &BTreeSet<String>, incoming: Vec<String>, mode: ListMode) -> BTreeSet<String> {
    match mode {
        ListMode::Set => incoming.into_iter().collect(),
        ListMode::Add => current.iter().cloned().chain(incoming).collect(),
        ListMode::Remove => {
            let drop: HashSet<String> = incoming.into_iter().collect();
            current
                .iter()
                .filter(|item| !drop.contains(*item))
                .cloned()
                .collect()
        }
    }
}

/// Apply one field to a configuration, merging if it is a collection.
fn apply(config: &mut AppConfig, name: &str, value: &CfgValue, mode: ListMode) {
    let Some(spec) = field(name) else { return };
    let merged = match spec.kind {
        // A bare scalar stands for a one-element collection in every mode.
        FieldKind::StrList => {
            let current = config.get_field(spec).as_strings().unwrap_or_default();
            CfgValue::from_strings(merge_list(
                &current,
                value.as_strings().unwrap_or_default(),
                mode,
            ))
        }
        FieldKind::StrSet => {
            let current: BTreeSet<String> = config
                .get_field(spec)
                .as_strings()
                .unwrap_or_default()
                .into_iter()
                .collect();
            CfgValue::from_strings(merge_set(
                &current,
                value.as_strings().unwrap_or_default(),
                mode,
            ))
        }
        _ => value.clone(),
    };
    config.set_field(spec, merged);
}

impl ConfigStore {
    /// Apply a batch of updates, or none of them.
    ///
    /// This is what the MQTT control topics call, so everything is checked
    /// before the first field is touched.
    pub(crate) fn update_fields(
        &self,
        updates: &[(String, CfgValue)],
        mode: ListMode,
    ) -> Result<(), ConfigError> {
        reject_unusable(updates)?;
        {
            let mut config = self.write();
            for (name, value) in updates {
                apply(&mut config, name, value, mode);
            }
        }
        self.save();
        Ok(())
    }

    /// Apply updates to one section, without the remote checks.
    pub(crate) fn update_section(
        &self,
        section: ConfigSection,
        updates: &[(String, CfgValue)],
        mode: ListMode,
    ) {
        {
            let mut config = self.write();
            for (name, value) in updates {
                // Scoped to the section the caller named, so a typo cannot reach
                // across into a field it did not mean.
                if fields_of(section).any(|spec| spec.name == name) {
                    apply(&mut config, name, value, mode);
                }
            }
        }
        self.save();
    }
}

/// Every reason this batch cannot be applied, in payload order.
fn reject_unusable(updates: &[(String, CfgValue)]) -> Result<(), ConfigError> {
    let mut problems: Vec<String> = Vec::new();
    for (name, value) in updates {
        let Some(spec) = field(name) else {
            problems.push(format!("Unknown configuration field: {name}"));
            continue;
        };
        if spec.protected {
            problems.push(format!("'{name}' cannot be changed remotely"));
            continue;
        }
        // `allow_bare_item` is true here and false when a file is read: a
        // payload may name a single entry where a list is expected.
        if let Some(problem) = type_mismatch(name, spec.kind, value, true) {
            problems.push(problem);
            continue;
        }
        // The same checks the file gets. Without them an unusable value would be
        // written out and the restart that follows would run into it.
        if let Some(problem) = value_problem(name, spec.checks, value) {
            problems.push(problem);
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ConfigError(problems.join("; ")))
    }
}

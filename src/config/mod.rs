//! The configuration file: what is in it, and what may change it.
//!
//! Three things live here: the typed model ([`AppConfig`] and its six
//! sections), the file itself ([`ConfigStore::load`] and [`ConfigStore::save`]),
//! and the two ways it is changed at runtime - a `config/set` payload over MQTT
//! ([`update_fields`]) and the whitelist sync ([`update_section`]).
//!
//! The shape of a field lives next door in [`schema`], not here, because field
//! names are addressed flat: a payload names `cache_size`, not
//! `general.cache_size`. What is here is the storage; what is there is the
//! mapping and the rules.
//!
//! [`update_fields`]: ConfigStore::update_fields
//! [`update_section`]: ConfigStore::update_section

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use log::{error, warn};

pub(crate) mod schema;
pub(crate) mod update;
pub(crate) mod validate;
pub(crate) mod value;

#[cfg(test)]
mod tests;

use schema::{FieldSpec, fields_of};
use value::CfgValue;

pub(crate) use update::ListMode;
pub use schema::ConfigSection;

// ---------------------------------------------------------------------------
// The model
//
// Field order is load-bearing: it is the order the file is written in and the
// order the `config/get` response spells its keys, both of which are pinned by
// goldens. It matches `schema::FIELDS`, and `tests` asserts that it still does.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GeneralConfig {
    pub(crate) log_level: String,
    pub(crate) base_topic: String,
    /// Signed, and wide, because the value has to survive validation in order to
    /// be named in the message that rejects it. Consumers clamp it themselves.
    pub(crate) cache_size: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrokerConfig {
    pub(crate) host: String,
    pub(crate) port: i64,
    pub(crate) user: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) client_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MiniserverConfig {
    pub(crate) miniserver_ip: String,
    pub(crate) miniserver_port: i64,
    pub(crate) miniserver_user: String,
    pub(crate) miniserver_pass: String,
    pub(crate) sync_with_miniserver: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TopicsConfig {
    pub(crate) subscriptions: Vec<String>,
    pub(crate) subscription_filters: Vec<String>,
    /// A set, and a sorted one: neither TOML nor JSON has a set type, so it is
    /// written out as an array, and sorting is what keeps that array stable
    /// across runs instead of reshuffling the file on every save.
    pub(crate) topic_whitelist: BTreeSet<String>,
    pub(crate) do_not_forward: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProcessingConfig {
    pub(crate) expand_json: bool,
    pub(crate) convert_booleans: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UdpConfig {
    pub(crate) udp_in_port: i64,
    pub(crate) udp_source_filter_enabled: bool,
    /// Additional senders besides the configured Miniserver; IPs or hostnames.
    pub(crate) udp_allowed_sources: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppConfig {
    pub(crate) general: GeneralConfig,
    pub(crate) broker: BrokerConfig,
    pub(crate) miniserver: MiniserverConfig,
    pub(crate) topics: TopicsConfig,
    pub(crate) processing: ProcessingConfig,
    pub(crate) udp: UdpConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            general: GeneralConfig {
                log_level: "INFO".to_owned(),
                base_topic: "myrelay/".to_owned(),
                cache_size: 100_000,
            },
            broker: BrokerConfig {
                host: "localhost".to_owned(),
                port: 1883,
                user: None,
                password: None,
                client_id: "loxmqttrelay".to_owned(),
            },
            miniserver: MiniserverConfig {
                miniserver_ip: "127.0.0.1".to_owned(),
                miniserver_port: 80,
                miniserver_user: String::new(),
                miniserver_pass: String::new(),
                sync_with_miniserver: true,
            },
            topics: TopicsConfig {
                subscriptions: Vec::new(),
                subscription_filters: Vec::new(),
                topic_whitelist: BTreeSet::new(),
                do_not_forward: Vec::new(),
            },
            processing: ProcessingConfig {
                expand_json: true,
                convert_booleans: true,
            },
            udp: UdpConfig {
                udp_in_port: 11884,
                udp_source_filter_enabled: true,
                udp_allowed_sources: Vec::new(),
            },
        }
    }
}

impl AppConfig {
    /// One field, by the name a control topic would use.
    ///
    /// The `expect` cannot fire for a spec out of [`schema::FIELDS`]; it only
    /// catches a field added to the table and not to this match, which is the
    /// one way the two can drift apart. `tests::every_field_round_trips` walks
    /// the whole table through here so that never reaches a running relay.
    pub(crate) fn get_field(&self, spec: &FieldSpec) -> CfgValue {
        let opt_str = |value: &Option<String>| match value {
            Some(s) => CfgValue::Str(s.clone()),
            None => CfgValue::Null,
        };
        match spec.name {
            "log_level" => CfgValue::Str(self.general.log_level.clone()),
            "base_topic" => CfgValue::Str(self.general.base_topic.clone()),
            "cache_size" => CfgValue::Int(i128::from(self.general.cache_size)),
            "host" => CfgValue::Str(self.broker.host.clone()),
            "port" => CfgValue::Int(i128::from(self.broker.port)),
            "user" => opt_str(&self.broker.user),
            "password" => opt_str(&self.broker.password),
            "client_id" => CfgValue::Str(self.broker.client_id.clone()),
            "miniserver_ip" => CfgValue::Str(self.miniserver.miniserver_ip.clone()),
            "miniserver_port" => CfgValue::Int(i128::from(self.miniserver.miniserver_port)),
            "miniserver_user" => CfgValue::Str(self.miniserver.miniserver_user.clone()),
            "miniserver_pass" => CfgValue::Str(self.miniserver.miniserver_pass.clone()),
            "sync_with_miniserver" => CfgValue::Bool(self.miniserver.sync_with_miniserver),
            "subscriptions" => CfgValue::from_strings(self.topics.subscriptions.clone()),
            "subscription_filters" => {
                CfgValue::from_strings(self.topics.subscription_filters.clone())
            }
            "topic_whitelist" => CfgValue::from_set(&self.topics.topic_whitelist),
            "do_not_forward" => CfgValue::from_strings(self.topics.do_not_forward.clone()),
            "expand_json" => CfgValue::Bool(self.processing.expand_json),
            "convert_booleans" => CfgValue::Bool(self.processing.convert_booleans),
            "udp_in_port" => CfgValue::Int(i128::from(self.udp.udp_in_port)),
            "udp_source_filter_enabled" => CfgValue::Bool(self.udp.udp_source_filter_enabled),
            "udp_allowed_sources" => CfgValue::from_strings(self.udp.udp_allowed_sources.clone()),
            other => unreachable!("field '{other}' is in the table but not in get_field"),
        }
    }

    /// Write one field, by the name a control topic would use.
    ///
    /// Only ever reached for a value the checks in [`validate`] already passed,
    /// so a value of the wrong shape here would be a bug upstream rather than
    /// bad input - hence the silent fallbacks rather than a `Result`.
    pub(crate) fn set_field(&mut self, spec: &FieldSpec, value: CfgValue) {
        let text = |value: &CfgValue| value.as_str().unwrap_or_default().to_owned();
        let int = |value: &CfgValue| i64::try_from(value.as_int().unwrap_or(0)).unwrap_or(i64::MAX);
        let flag = |value: &CfgValue| matches!(value, CfgValue::Bool(true));
        let list = |value: &CfgValue| value.as_strings().unwrap_or_default();
        match spec.name {
            "log_level" => self.general.log_level = text(&value),
            "base_topic" => self.general.base_topic = text(&value),
            "cache_size" => self.general.cache_size = int(&value),
            "host" => self.broker.host = text(&value),
            "port" => self.broker.port = int(&value),
            "user" => self.broker.user = value.as_str().map(str::to_owned),
            "password" => self.broker.password = value.as_str().map(str::to_owned),
            "client_id" => self.broker.client_id = text(&value),
            "miniserver_ip" => self.miniserver.miniserver_ip = text(&value),
            "miniserver_port" => self.miniserver.miniserver_port = int(&value),
            "miniserver_user" => self.miniserver.miniserver_user = text(&value),
            "miniserver_pass" => self.miniserver.miniserver_pass = text(&value),
            "sync_with_miniserver" => self.miniserver.sync_with_miniserver = flag(&value),
            "subscriptions" => self.topics.subscriptions = list(&value),
            "subscription_filters" => self.topics.subscription_filters = list(&value),
            "topic_whitelist" => self.topics.topic_whitelist = list(&value).into_iter().collect(),
            "do_not_forward" => self.topics.do_not_forward = list(&value),
            "expand_json" => self.processing.expand_json = flag(&value),
            "convert_booleans" => self.processing.convert_booleans = flag(&value),
            "udp_in_port" => self.udp.udp_in_port = int(&value),
            "udp_source_filter_enabled" => self.udp.udp_source_filter_enabled = flag(&value),
            "udp_allowed_sources" => self.udp.udp_allowed_sources = list(&value),
            other => unreachable!("field '{other}' is in the table but not in set_field"),
        }
    }

    /// Build a configuration from a parsed document, keeping the defaults for
    /// anything it does not mention.
    ///
    /// Warns about fields this build does not know rather than refusing them: an
    /// upgrade that drops an option must not stop the relay from starting.
    /// Unknown *sections* were already warned about during validation.
    fn from_document(document: &[(String, CfgValue)]) -> (Self, Vec<String>) {
        let mut config = AppConfig::default();
        let mut warnings = Vec::new();
        for (section_name, section_value) in document {
            let Some(section) = ConfigSection::parse(section_name) else {
                continue;
            };
            let Some(entries) = section_value.as_table() else {
                continue;
            };
            for (key, value) in entries {
                match fields_of(section).find(|spec| spec.name == key) {
                    Some(spec) => config.set_field(spec, value.clone()),
                    None => warnings.push(format!(
                        "Unknown field '{key}' in config section '{section_name}' will be ignored."
                    )),
                }
            }
        }
        (config, warnings)
    }

    /// The configuration as it is written to disk.
    ///
    /// `None` becomes `""`, because TOML has no null and an absent broker user
    /// has always been spelled as the empty string in the file.
    fn to_document(&self) -> Vec<(ConfigSection, Vec<(&'static str, CfgValue)>)> {
        ConfigSection::ALL
            .into_iter()
            .map(|section| {
                let entries = fields_of(section)
                    .map(|spec| {
                        let value = match self.get_field(spec) {
                            CfgValue::Null => CfgValue::Str(String::new()),
                            other => other,
                        };
                        (spec.name, value)
                    })
                    .collect();
                (section, entries)
            })
            .collect()
    }

    /// The configuration with the credentials taken out.
    ///
    /// Exactly what is published to `{base_topic}config/response`, so the key
    /// order is part of the contract and comes from [`schema::FIELDS`] - which
    /// is the same order the file is written in.
    pub(crate) fn safe_json(&self) -> Vec<u8> {
        const REDACTED: [&str; 4] = ["user", "password", "miniserver_user", "miniserver_pass"];
        let mut out = String::from("{");
        for (i, section) in ConfigSection::ALL.into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_json_string(&mut out, section.as_str());
            out.push_str(":{");
            let mut first = true;
            for spec in fields_of(section).filter(|s| !REDACTED.contains(&s.name)) {
                if !first {
                    out.push(',');
                }
                first = false;
                write_json_string(&mut out, spec.name);
                out.push(':');
                write_json_value(&mut out, &self.get_field(spec));
            }
            out.push('}');
        }
        out.push('}');
        out.into_bytes()
    }
}

/// Compact JSON, with the keys in the order they are declared.
///
/// Hand-rolled rather than derived because the whole point is the key order, and
/// the shapes involved are four: string, integer, boolean and array-of-string.
/// `serde_json`'s map would sort the keys - its `preserve_order` feature is off
/// on purpose, see [`value::parse_json`].
fn write_json_value(out: &mut String, value: &CfgValue) {
    match value {
        CfgValue::Str(s) => write_json_string(out, s),
        CfgValue::Int(i) => out.push_str(&i.to_string()),
        CfgValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        CfgValue::Null => out.push_str("null"),
        CfgValue::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_value(out, item);
            }
            out.push(']');
        }
        // Not reachable from a configuration value; spelled out so a future one
        // cannot silently produce invalid JSON.
        other => write_json_string(out, &other.to_string()),
    }
}

fn write_json_string(out: &mut String, text: &str) {
    // Escaping one string at a time through a real serializer, rather than
    // hand-rolling it: the payload carries topic names, and getting a quote or a
    // backslash wrong there produces JSON nobody can read.
    out.push_str(&serde_json::Value::String(text.to_owned()).to_string());
}

// ---------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------

/// The configuration could not be used, and the reason has already been logged.
///
/// Carried rather than returned as a message because by the time this exists the
/// operator has been told everything that is wrong, one line per problem; the
/// caller's only remaining job is to exit non-zero.
#[derive(Debug)]
pub struct StartupAbort;

impl fmt::Display for StartupAbort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the configuration file cannot be used")
    }
}

impl std::error::Error for StartupAbort {}

/// The configuration, and the file it came from.
///
/// Shared as an `Arc` and passed in rather than reached for, which is what lets
/// the config tests run in parallel against files of their own.
pub struct ConfigStore {
    path: PathBuf,
    config: RwLock<AppConfig>,
}

impl ConfigStore {
    pub(crate) fn new(path: impl Into<PathBuf>, config: AppConfig) -> Self {
        ConfigStore {
            path: path.into(),
            config: RwLock::new(config),
        }
    }

    /// Read and check the file, or report why the relay will not start.
    ///
    /// A missing file is not an error - the relay comes up on the defaults and
    /// writes them out on the first change. Anything else that is wrong is
    /// reported in full, every problem at once, before a single socket is
    /// opened.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, StartupAbort> {
        let path = path.into();
        if !path.exists() {
            warn!(
                "Config file not found, creating default config: {}",
                path.display()
            );
            return Ok(ConfigStore::new(path, AppConfig::default()));
        }

        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                error!("Invalid configuration: {} cannot be read: {e}", path.display());
                return Err(refuse(&path, 1));
            }
        };

        let document = match value::parse_toml(&text) {
            Ok(document) => document,
            Err(e) => {
                // A stray bracket is the commonest way to break this file, so
                // it gets the same treatment as an unusable value: named, with
                // the path, and refused before anything connects.
                error!("Invalid configuration: {} is not valid TOML: {e}", path.display());
                return Err(refuse(&path, 1));
            }
        };

        let found = validate::validate_document(&document);
        for warning in &found.warnings {
            warn!("{warning}");
        }
        if !found.problems.is_empty() {
            for problem in &found.problems {
                error!("Invalid configuration: {problem}");
            }
            return Err(refuse(&path, found.problems.len()));
        }

        let (config, warnings) = AppConfig::from_document(&document);
        for warning in warnings {
            warn!("{warning}");
        }
        Ok(ConfigStore::new(path, config))
    }

    pub fn snapshot(&self) -> AppConfig {
        self.read().clone()
    }

    pub(crate) fn safe_json(&self) -> Vec<u8> {
        self.read().safe_json()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, AppConfig> {
        self.config.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, AppConfig> {
        self.config.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Write the configuration out, reporting rather than raising on failure.
    ///
    /// A relay that cannot save is still a relay that works; it just loses the
    /// change on the next restart, which is what the second message says.
    pub(crate) fn save(&self) {
        let text = render_toml(&self.read().to_document());
        match fs::write(&self.path, text) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                // Deliberately no chmod here. Widening the file to 0666 to get
                // one write through would leave the broker and Miniserver
                // passwords readable and writable for every account on the host,
                // permanently, to fix something temporary - and it only ever
                // works when the file is ours, in which case the obstacle was
                // somewhere else anyway.
                error!("{}", self.permission_hint());
                error!(
                    "The configuration was NOT written. The relay keeps running with the \
                     values it already has, but the change is lost on the next restart."
                );
            }
            Err(e) => error!("Error saving config: {e}"),
        }
    }

    /// Who we are, who owns the file, and how to reconcile the two.
    #[cfg(unix)]
    fn permission_hint(&self) -> String {
        use std::os::unix::fs::MetadataExt;

        let path = self.path.display();
        let mut message = format!("No write permission for {path}");
        let (uid, gid) = (
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        );
        match fs::metadata(&self.path) {
            Ok(info) => message.push_str(&format!(
                " - running as uid {uid}, gid {gid}, file owned by uid {}, gid {}. \
                 Fix with: chown {uid}:{gid} {path}",
                info.uid(),
                info.gid()
            )),
            Err(e) => message.push_str(&format!(" ({e})")),
        }
        message
    }

    #[cfg(not(unix))]
    fn permission_hint(&self) -> String {
        format!("No write permission for {}", self.path.display())
    }
}

/// Report the count and hand back the abort.
fn refuse(path: &Path, problems: usize) -> StartupAbort {
    error!(
        "Refusing to start: {} has {problems} unusable value(s). Nothing was connected, \
         nothing was changed.",
        path.display()
    );
    StartupAbort
}

/// The configuration as TOML text.
///
/// Hand-rolled for one reason: the file is the operator's, and its layout should
/// not shift because a serializer changed its mind about blank lines. The shapes
/// are four - string, integer, boolean, array of strings - and the escaping is
/// borrowed from a real serializer rather than guessed at.
fn render_toml(document: &[(ConfigSection, Vec<(&'static str, CfgValue)>)]) -> String {
    let mut out = String::new();
    for (i, (section, entries)) in document.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push('[');
        out.push_str(section.as_str());
        out.push_str("]\n");
        for (name, value) in entries {
            out.push_str(name);
            out.push_str(" = ");
            write_toml_value(&mut out, value);
            out.push('\n');
        }
    }
    out
}

fn write_toml_value(out: &mut String, value: &CfgValue) {
    match value {
        CfgValue::Str(s) => write_toml_string(out, s),
        CfgValue::Int(i) => out.push_str(&i.to_string()),
        CfgValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        CfgValue::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_toml_value(out, item);
            }
            out.push(']');
        }
        // Never produced by `to_document`, which turns null into "" and has no
        // other shapes; spelled out so it cannot silently write invalid TOML.
        other => write_toml_string(out, &other.to_string()),
    }
}

/// A TOML *basic* string - always, even where a literal one would be shorter.
///
/// `toml::Value`'s own Display switches to a literal string (`'…'`) whenever
/// that avoids an escape, so a filter pattern would have its quoting rewritten
/// on the first save. The file is the operator's; a save should change the
/// values they changed and nothing else.
fn write_toml_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            // The rest of C0, plus DEL, which TOML does not allow raw.
            c if c < ' ' || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

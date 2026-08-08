//! One value model for the two ways a value arrives.
//!
//! A configuration value comes either from the TOML file or from a `config/set`
//! payload, and both are checked by the same rules against the same messages.
//! Rather than write the validation twice, both are converted to [`CfgValue`]
//! first.
//!
//! [`CfgValue::type_name`] is why this type is shaped the way it is: a mismatch
//! message has to name the type that actually arrived (`got int`, `got dict`,
//! `got NoneType`), and those names are part of the message an operator reads
//! and searches for. Getting them right is most of the job here.
//!
//! Note that a boolean and an integer are simply different variants, so
//! `cache_size = true` is a mismatch by construction rather than by a check
//! somebody has to remember to write.

use std::collections::BTreeSet;
use std::fmt;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// A configuration value, whichever format it arrived in.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CfgValue {
    Null,
    Bool(bool),
    /// Wide enough for every TOML integer (i64) and every JSON one serde_json
    /// will hand over (u64), so the value can always be named in a message
    /// rather than being clamped or rejected for the wrong reason.
    Int(i128),
    Float(f64),
    Str(String),
    List(Vec<CfgValue>),
    /// Ordered, always: `update_fields` reports its problems in payload order.
    Table(Vec<(String, CfgValue)>),
    /// A TOML date or time. Carries the name a mismatch message calls it.
    Other(&'static str),
}

impl CfgValue {
    /// What this value's type is called in a mismatch message.
    ///
    /// `NoneType` and `dict` rather than `null` and `table`: these names are
    /// part of the message an operator reads, and the corpus pins them.
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            CfgValue::Null => "NoneType",
            CfgValue::Bool(_) => "bool",
            CfgValue::Int(_) => "int",
            CfgValue::Float(_) => "float",
            CfgValue::Str(_) => "str",
            CfgValue::List(_) => "list",
            CfgValue::Table(_) => "dict",
            CfgValue::Other(name) => name,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            CfgValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_int(&self) -> Option<i128> {
        match self {
            CfgValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub(crate) fn as_table(&self) -> Option<&[(String, CfgValue)]> {
        match self {
            CfgValue::Table(entries) => Some(entries),
            _ => None,
        }
    }

    /// The value read as a collection of strings.
    ///
    /// A bare string counts as a one-element collection, which is what lets a
    /// `config/add` name a single topic instead of a list of one. Returns `None`
    /// if anything in there is not a string - the type check has already
    /// reported that, and this is only reached once it passed.
    pub(crate) fn as_strings(&self) -> Option<Vec<String>> {
        match self {
            CfgValue::Str(s) => Some(vec![s.clone()]),
            CfgValue::List(items) => items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect(),
            _ => None,
        }
    }

    pub(crate) fn from_strings<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        CfgValue::List(
            items
                .into_iter()
                .map(|s| CfgValue::Str(s.into()))
                .collect(),
        )
    }

    pub(crate) fn from_set(items: &BTreeSet<String>) -> Self {
        CfgValue::from_strings(items.iter().cloned())
    }
}

impl fmt::Display for CfgValue {
    /// How a value reads inside a problem message.
    ///
    /// Only ever reached for the scalars the value checks look at, so the
    /// collection arms are a formality.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CfgValue::Null => f.write_str("None"),
            CfgValue::Bool(b) => write!(f, "{}", if *b { "True" } else { "False" }),
            CfgValue::Int(i) => write!(f, "{i}"),
            CfgValue::Float(x) => write!(f, "{x}"),
            CfgValue::Str(s) => f.write_str(s),
            CfgValue::List(_) => f.write_str("[...]"),
            CfgValue::Table(_) => f.write_str("{...}"),
            CfgValue::Other(name) => f.write_str(name),
        }
    }
}

impl From<toml::Value> for CfgValue {
    fn from(value: toml::Value) -> Self {
        match value {
            toml::Value::String(s) => CfgValue::Str(s),
            toml::Value::Integer(i) => CfgValue::Int(i128::from(i)),
            toml::Value::Float(x) => CfgValue::Float(x),
            toml::Value::Boolean(b) => CfgValue::Bool(b),
            // Reported as whichever of the three it is, so the message names
            // the thing that was actually written.
            toml::Value::Datetime(dt) => CfgValue::Other(match (dt.date, dt.time) {
                (Some(_), Some(_)) => "datetime",
                (Some(_), None) => "date",
                _ => "time",
            }),
            toml::Value::Array(items) => {
                CfgValue::List(items.into_iter().map(CfgValue::from).collect())
            }
            toml::Value::Table(entries) => CfgValue::Table(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, CfgValue::from(v)))
                    .collect(),
            ),
        }
    }
}

/// Parse a TOML document into ordered values.
///
/// Order matters twice over: unusable values are reported in file order, and a
/// reader given `[general] 'cache_size' cannot be negative` should be able to
/// walk down their file and find it.
pub(crate) fn parse_toml(text: &str) -> Result<Vec<(String, CfgValue)>, toml::de::Error> {
    let value: toml::Value = toml::from_str(text)?;
    match CfgValue::from(value) {
        CfgValue::Table(entries) => Ok(entries),
        // Unreachable: a TOML document is a table by definition.
        _ => Ok(Vec::new()),
    }
}

/// Parse a `config/set` payload into ordered values.
///
/// Deserializes straight into [`CfgValue`] rather than through
/// `serde_json::Value`, whose map is a `BTreeMap` unless the `preserve_order`
/// feature is on - and that feature must stay off, because `process::flatten`
/// relies on the sorted iteration it gives (see the test there). Going direct
/// keeps payload order here without changing anything on the message path.
pub(crate) fn parse_json(text: &str) -> Result<CfgValue, serde_json::Error> {
    serde_json::from_str(text)
}

impl<'de> Deserialize<'de> for CfgValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(CfgValueVisitor)
    }
}

struct CfgValueVisitor;

impl<'de> Visitor<'de> for CfgValueVisitor {
    type Value = CfgValue;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<CfgValue, E> {
        Ok(CfgValue::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<CfgValue, E> {
        Ok(CfgValue::Null)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<CfgValue, E> {
        Ok(CfgValue::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<CfgValue, E> {
        Ok(CfgValue::Int(i128::from(v)))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<CfgValue, E> {
        Ok(CfgValue::Int(i128::from(v)))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<CfgValue, E> {
        Ok(CfgValue::Float(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<CfgValue, E> {
        Ok(CfgValue::Str(v.to_owned()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<CfgValue, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(CfgValue::List(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<CfgValue, A::Error> {
        let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
        // A repeated key is kept rather than merged: the caller walks these in
        // order, so the later assignment wins, which is what every JSON reader
        // does with a duplicate.
        while let Some((key, value)) = map.next_entry::<String, CfgValue>()? {
            entries.push((key, value));
        }
        Ok(CfgValue::Table(entries))
    }
}

//! What the configuration consists of, as data.
//!
//! The section structs in [`super`] carry the fields and their defaults. This
//! table carries everything else about them: which section a field belongs to,
//! what shape its value has, what has to be true of that value, and whether a
//! remote update may touch it at all.
//!
//! It exists because field names are addressed *flat*. A `config/set` payload
//! names `cache_size`, not `general.cache_size`, so something has to map a bare
//! name onto a section - that is what `Config._map_fields_to_sections` did in
//! Python, by walking the dataclasses at import time. Rust cannot walk its own
//! structs, so the mapping is written down; [`super::tests`] then asserts the
//! table and the structs still agree, which is the part that would otherwise
//! drift.

/// The six tables a configuration file has.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub(crate) enum ConfigSection {
    General,
    Broker,
    Miniserver,
    Topics,
    Processing,
    Udp,
}

impl ConfigSection {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ConfigSection::General => "general",
            ConfigSection::Broker => "broker",
            ConfigSection::Miniserver => "miniserver",
            ConfigSection::Topics => "topics",
            ConfigSection::Processing => "processing",
            ConfigSection::Udp => "udp",
        }
    }

    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "general" => ConfigSection::General,
            "broker" => ConfigSection::Broker,
            "miniserver" => ConfigSection::Miniserver,
            "topics" => ConfigSection::Topics,
            "processing" => ConfigSection::Processing,
            "udp" => ConfigSection::Udp,
            _ => return None,
        })
    }

    /// All six, in the order they are written to the file.
    pub(crate) const ALL: [ConfigSection; 6] = [
        ConfigSection::General,
        ConfigSection::Broker,
        ConfigSection::Miniserver,
        ConfigSection::Topics,
        ConfigSection::Processing,
        ConfigSection::Udp,
    ];
}

/// The shape of a field's value.
///
/// Coarser than the Rust type on the struct: what it has to distinguish is the
/// cases the mismatch messages distinguish, and those name `str`, `int`, `bool`,
/// `str | None` and "a list".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FieldKind {
    Bool,
    Int,
    Str,
    /// `Optional[str]`, which Python 3.14 renders as `str | None`.
    OptStr,
    /// An ordered list, deduplicated on `add` but never sorted.
    StrList,
    /// A set, which is written out sorted because neither TOML nor JSON has one.
    StrSet,
}

impl FieldKind {
    /// The name this kind goes by in a mismatch message.
    ///
    /// These are Python type names because that is what the messages have always
    /// said, and an operator searching for one of them should still find it.
    pub(crate) fn expected(self) -> &'static str {
        match self {
            FieldKind::Bool => "bool",
            FieldKind::Int => "int",
            FieldKind::Str => "str",
            FieldKind::OptStr => "str | None",
            FieldKind::StrList | FieldKind::StrSet => "a list",
        }
    }

    pub(crate) fn is_collection(self) -> bool {
        matches!(self, FieldKind::StrList | FieldKind::StrSet)
    }
}

/// Something that has to be true of a value whose type already fits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Check {
    /// 1..=65535.
    Port,
    /// `cache_size`, which may be zero but not negative.
    NonNegative,
    /// One of the five level names, in any case.
    LogLevel,
    /// Not empty once stripped.
    NonBlank,
    /// Same, but `base_topic` explains itself differently.
    NonBlankTopic,
    /// Every entry compiles, and none of them is empty.
    RegexList,
}

/// One field, and everything about it that is not its value.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FieldSpec {
    pub(crate) name: &'static str,
    pub(crate) section: ConfigSection,
    pub(crate) kind: FieldKind,
    pub(crate) checks: &'static [Check],
    /// Refused over MQTT.
    ///
    /// Validation cannot catch these: another host is a perfectly valid value,
    /// and after the restart an update triggers the relay would authenticate
    /// there with the configured credentials.
    pub(crate) protected: bool,
}

/// Every field, in the order it is written to the file.
pub(crate) static FIELDS: &[FieldSpec] = &[
    // -- general ------------------------------------------------------------
    FieldSpec {
        name: "log_level",
        section: ConfigSection::General,
        kind: FieldKind::Str,
        checks: &[Check::LogLevel],
        protected: false,
    },
    FieldSpec {
        name: "base_topic",
        section: ConfigSection::General,
        kind: FieldKind::Str,
        checks: &[Check::NonBlankTopic],
        protected: false,
    },
    FieldSpec {
        name: "cache_size",
        section: ConfigSection::General,
        kind: FieldKind::Int,
        checks: &[Check::NonNegative],
        protected: false,
    },
    // -- broker -------------------------------------------------------------
    FieldSpec {
        name: "host",
        section: ConfigSection::Broker,
        kind: FieldKind::Str,
        checks: &[Check::NonBlank],
        protected: true,
    },
    FieldSpec {
        name: "port",
        section: ConfigSection::Broker,
        kind: FieldKind::Int,
        checks: &[Check::Port],
        protected: true,
    },
    FieldSpec {
        name: "user",
        section: ConfigSection::Broker,
        kind: FieldKind::OptStr,
        checks: &[],
        protected: true,
    },
    FieldSpec {
        name: "password",
        section: ConfigSection::Broker,
        kind: FieldKind::OptStr,
        checks: &[],
        protected: true,
    },
    FieldSpec {
        name: "client_id",
        section: ConfigSection::Broker,
        kind: FieldKind::Str,
        checks: &[],
        protected: false,
    },
    // -- miniserver ---------------------------------------------------------
    FieldSpec {
        name: "miniserver_ip",
        section: ConfigSection::Miniserver,
        kind: FieldKind::Str,
        checks: &[Check::NonBlank],
        protected: true,
    },
    FieldSpec {
        name: "miniserver_port",
        section: ConfigSection::Miniserver,
        kind: FieldKind::Int,
        checks: &[Check::Port],
        protected: true,
    },
    FieldSpec {
        name: "miniserver_user",
        section: ConfigSection::Miniserver,
        kind: FieldKind::Str,
        checks: &[],
        protected: true,
    },
    FieldSpec {
        name: "miniserver_pass",
        section: ConfigSection::Miniserver,
        kind: FieldKind::Str,
        checks: &[],
        protected: true,
    },
    FieldSpec {
        name: "sync_with_miniserver",
        section: ConfigSection::Miniserver,
        kind: FieldKind::Bool,
        checks: &[],
        protected: false,
    },
    // -- topics -------------------------------------------------------------
    FieldSpec {
        name: "subscriptions",
        section: ConfigSection::Topics,
        kind: FieldKind::StrList,
        checks: &[],
        protected: false,
    },
    FieldSpec {
        name: "subscription_filters",
        section: ConfigSection::Topics,
        kind: FieldKind::StrList,
        checks: &[Check::RegexList],
        protected: false,
    },
    FieldSpec {
        name: "topic_whitelist",
        section: ConfigSection::Topics,
        kind: FieldKind::StrSet,
        checks: &[],
        protected: false,
    },
    FieldSpec {
        name: "do_not_forward",
        section: ConfigSection::Topics,
        kind: FieldKind::StrList,
        checks: &[Check::RegexList],
        protected: false,
    },
    // -- processing ---------------------------------------------------------
    FieldSpec {
        name: "expand_json",
        section: ConfigSection::Processing,
        kind: FieldKind::Bool,
        checks: &[],
        protected: false,
    },
    FieldSpec {
        name: "convert_booleans",
        section: ConfigSection::Processing,
        kind: FieldKind::Bool,
        checks: &[],
        protected: false,
    },
    // -- udp ----------------------------------------------------------------
    FieldSpec {
        name: "udp_in_port",
        section: ConfigSection::Udp,
        kind: FieldKind::Int,
        checks: &[Check::Port],
        protected: false,
    },
    FieldSpec {
        name: "udp_source_filter_enabled",
        section: ConfigSection::Udp,
        kind: FieldKind::Bool,
        checks: &[],
        protected: false,
    },
    FieldSpec {
        name: "udp_allowed_sources",
        section: ConfigSection::Udp,
        kind: FieldKind::StrList,
        checks: &[],
        protected: false,
    },
];

/// The field a bare name refers to, wherever it lives.
pub(crate) fn field(name: &str) -> Option<&'static FieldSpec> {
    FIELDS.iter().find(|spec| spec.name == name)
}

/// The fields of one section, in file order.
pub(crate) fn fields_of(section: ConfigSection) -> impl Iterator<Item = &'static FieldSpec> {
    FIELDS.iter().filter(move |spec| spec.section == section)
}

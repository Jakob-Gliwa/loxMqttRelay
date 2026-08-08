//! The command line.
//!
//! One flag came over from argparse - `--log-level` - and one is new:
//! `--config`, which used to be hard-coded at `config/config.toml`. That is a
//! strict superset with the same default, and it is what lets the config tests
//! run against files of their own rather than reaching into a singleton.
//!
//! The `'pytest' in sys.modules -> parse_known_args` hack that `utils.get_args`
//! needed disappears with it.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use log::LevelFilter;

#[derive(Parser, Debug)]
#[command(name = "loxmqttrelay", about = "MQTT Relay", version)]
pub struct Args {
    /// Set the logging level (overrides the config file setting)
    #[arg(long, value_enum)]
    pub log_level: Option<LogLevel>,

    /// Path to the configuration file
    #[arg(long, default_value = "config/config.toml", env = "CONFIG_PATH")]
    pub config: PathBuf,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[value(rename_all = "UPPER")]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
    Critical,
}

impl From<LogLevel> for LevelFilter {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Debug => LevelFilter::Debug,
            LogLevel::Info => LevelFilter::Info,
            LogLevel::Warning => LevelFilter::Warn,
            // `log` has no level above error, so CRITICAL lands there as well.
            LogLevel::Error | LogLevel::Critical => LevelFilter::Error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_ones_the_relay_always_had() {
        let args = Args::parse_from(["loxmqttrelay"]);
        assert_eq!(args.log_level, None);
        assert_eq!(args.config, PathBuf::from("config/config.toml"));
    }

    #[test]
    fn the_level_names_are_pythons() {
        for (text, expected) in [
            ("DEBUG", LevelFilter::Debug),
            ("INFO", LevelFilter::Info),
            ("WARNING", LevelFilter::Warn),
            ("ERROR", LevelFilter::Error),
            // No `log` level is above error, so this is where CRITICAL lands.
            ("CRITICAL", LevelFilter::Error),
        ] {
            let args = Args::parse_from(["loxmqttrelay", "--log-level", text]);
            assert_eq!(LevelFilter::from(args.log_level.expect("a level")), expected);
        }
    }

    /// argparse exited 2 on an unknown choice, and so does this.
    #[test]
    fn an_unknown_level_is_refused_rather_than_guessed_at() {
        assert!(Args::try_parse_from(["loxmqttrelay", "--log-level", "TRACE"]).is_err());
        assert!(Args::try_parse_from(["loxmqttrelay", "--nonsense"]).is_err());
    }
}

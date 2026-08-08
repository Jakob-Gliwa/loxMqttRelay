//! Where log output goes, and at what level.
//!
//! The precedence is the one `utils.setup_logging` established: `--log-level`
//! beats `LOG_LEVEL`, which beats `general.log_level` in the file. The env var
//! is what `docker-entrypoint.sh` used to translate into a flag; it is read
//! directly now, so the script has nothing left to do.
//!
//! There is one ordering problem worth naming. env_logger fixes its filter at
//! `init()`, but the third source lives in a file whose *loading* is itself
//! logged - and reporting a broken config at a level the broken config was
//! supposed to choose is circular. So the file is peeked at first, tolerantly
//! and ignoring every error, and the real load reports its problems afterwards
//! at a level that is already settled.

use std::io::Write as _;
use std::path::Path;

use log::LevelFilter;

use crate::cli::Args;

/// Install the logger, and report what decided its level.
///
/// Returns the level so the banner can name it. `RUST_LOG` still overrides
/// everything, per module, which is why the level here is a *default* filter
/// rather than a hard setting.
pub fn init(args: &Args) -> LevelFilter {
    let (level, source) = resolve(args);

    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(level.as_str().to_lowercase()),
    )
    // timestamp, level, module, message - the shape every line has had, so an
    // old log and a new one still read as one file.
    .format(|buf, record| {
        writeln!(
            buf,
            "{} {} [{}] {}",
            buf.timestamp(),
            record.level(),
            record.target(),
            record.args()
        )
    })
    .init();

    log::debug!("Log level {level} taken from {source}");
    level
}

fn resolve(args: &Args) -> (LevelFilter, &'static str) {
    if let Some(level) = args.log_level {
        return (level.into(), "--log-level");
    }
    if let Ok(text) = std::env::var("LOG_LEVEL") {
        return match parse_level(&text) {
            Some(level) => (level, "the LOG_LEVEL environment variable"),
            None => {
                // Neither LOG_LEVEL nor a typo in it is validated anywhere, and
                // a typo used to land on DEBUG - the loudest level, and the one
                // that logs every payload.
                eprintln!("Unknown log level '{text}', using INFO");
                (LevelFilter::Info, "INFO after an unusable LOG_LEVEL")
            }
        };
    }
    match peek_config_level(&args.config) {
        Some(level) => (level, "the configuration file"),
        None => (LevelFilter::Info, "the default"),
    }
}

/// `general.log_level` out of the file, or nothing.
///
/// Deliberately incurious about failure: the file is read again, properly, a
/// moment later, and that read is what reports a missing file, a syntax error
/// or an unusable level. All this needs to decide is how loudly to say so.
fn peek_config_level(path: &Path) -> Option<LevelFilter> {
    let text = std::fs::read_to_string(path).ok()?;
    let document: toml::Value = toml::from_str(&text).ok()?;
    parse_level(document.get("general")?.get("log_level")?.as_str()?)
}

fn parse_level(text: &str) -> Option<LevelFilter> {
    Some(match text.to_ascii_uppercase().as_str() {
        "DEBUG" => LevelFilter::Debug,
        "INFO" => LevelFilter::Info,
        "WARNING" | "WARN" => LevelFilter::Warn,
        "ERROR" | "CRITICAL" => LevelFilter::Error,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::LogLevel;
    use std::path::PathBuf;

    fn args(log_level: Option<LogLevel>, config: &str) -> Args {
        Args {
            log_level,
            config: PathBuf::from(config),
        }
    }

    #[test]
    fn the_flag_beats_everything() {
        // Safety: single-threaded test, and the variable is read below in the
        // same test rather than by anything concurrent.
        unsafe { std::env::set_var("LOG_LEVEL", "ERROR") };
        let (level, source) = resolve(&args(Some(LogLevel::Debug), "/nonexistent"));
        assert_eq!(level, LevelFilter::Debug);
        assert_eq!(source, "--log-level");
        unsafe { std::env::remove_var("LOG_LEVEL") };
    }

    #[test]
    fn a_missing_file_and_no_environment_is_info() {
        unsafe { std::env::remove_var("LOG_LEVEL") };
        let (level, _) = resolve(&args(None, "/nonexistent/config.toml"));
        assert_eq!(level, LevelFilter::Info);
    }

    #[test]
    fn every_level_name_is_understood_including_critical() {
        assert_eq!(parse_level("warning"), Some(LevelFilter::Warn));
        assert_eq!(parse_level("CRITICAL"), Some(LevelFilter::Error));
        assert_eq!(parse_level("DeBuG"), Some(LevelFilter::Debug));
        assert_eq!(parse_level("TRACE"), None);
    }

    /// The peek must survive anything the file can be, because reporting what
    /// is wrong with it is somebody else's job and happens later.
    #[test]
    fn peeking_at_an_unusable_file_yields_nothing_rather_than_failing() {
        let dir = std::env::temp_dir().join("loxmqttrelay-logging");
        let _ = std::fs::create_dir_all(&dir);

        let broken = dir.join("broken.toml");
        std::fs::write(&broken, "[general\nlog_level =").expect("seed");
        assert_eq!(peek_config_level(&broken), None);

        let wrong_type = dir.join("wrong.toml");
        std::fs::write(&wrong_type, "[general]\nlog_level = 5\n").expect("seed");
        assert_eq!(peek_config_level(&wrong_type), None);

        let good = dir.join("good.toml");
        std::fs::write(&good, "[general]\nlog_level = \"DEBUG\"\n").expect("seed");
        assert_eq!(peek_config_level(&good), Some(LevelFilter::Debug));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

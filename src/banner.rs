//! The startup banner.
//!
//! One block at INFO, before anything connects, recording which build is
//! running and what it was configured with. It exists because a relay that
//! silently ignored a setting is indistinguishable from a misconfigured one
//! when all you have is a bug report - so the values the components baked in at
//! construction are stated out loud, once, where they can be quoted back.
//!
//! The build line is worth reading twice on x86: the relay ships in two
//! variants and a launcher picks between them, so `target-cpu` here is how you
//! tell which one you got.

use log::info;

use crate::config::AppConfig;

/// Compiled-in facts, from `build.rs`.
const TARGET: &str = env!("RELAY_TARGET");
const PROFILE: &str = env!("RELAY_PROFILE");
const TARGET_CPU: &str = env!("RELAY_TARGET_CPU");
const DEPENDENCIES: &str = env!("RELAY_DEPS");

pub fn log_runtime_environment(config: &AppConfig, config_path: &std::path::Path) {
    info!("----- loxMqttRelay runtime environment -----");
    info!(
        "version={}  build={PROFILE}/{TARGET_CPU}  target={TARGET}",
        env!("CARGO_PKG_VERSION")
    );
    info!(
        "executable={}  config={}",
        std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "n/a".to_owned()),
        config_path.display()
    );
    info!("platform={}  arch={}  {}", system(), arch(), cpu_features());
    // The values the processing core bakes in at construction.
    info!(
        "processing: expand_json={}  convert_booleans={}",
        config.processing.expand_json, config.processing.convert_booleans
    );
    info!(
        "topic filters: subscriptions={}  subscription_filters={}  do_not_forward={}  whitelist={}",
        config.topics.subscriptions.len(),
        config.topics.subscription_filters.len(),
        config.topics.do_not_forward.len(),
        config.topics.topic_whitelist.len()
    );
    info!("dependencies: {DEPENDENCIES}");
    info!("--------------------------------------------");
}

fn system() -> String {
    let info = rustix::system::uname();
    format!(
        "{} {}",
        info.sysname().to_string_lossy(),
        info.release().to_string_lossy()
    )
}

fn arch() -> String {
    rustix::system::uname()
        .machine()
        .to_string_lossy()
        .into_owned()
}

/// Which instruction sets this CPU actually has.
///
/// Replaces the whole `/proc/cpuinfo` + `sysctl` + `wmic` block that
/// `utils.has_avx2` needed, because std can simply ask. Reported rather than
/// acted on: the AVX2 decision is made by the launcher, before this process
/// starts, and this line is how you tell which of the two you got.
fn cpu_features() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        format!(
            "(x86_64, avx2={}, fma={})",
            std::arch::is_x86_feature_detected!("avx2"),
            std::arch::is_x86_feature_detected!("fma")
        )
    }
    #[cfg(target_arch = "aarch64")]
    {
        format!(
            "(aarch64, neon={})",
            std::arch::is_aarch64_feature_detected!("neon")
        )
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        String::from("(no feature detection on this architecture)")
    }
}

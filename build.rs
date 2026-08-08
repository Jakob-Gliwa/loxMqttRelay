//! What the startup banner reports about this build.
//!
//! The dependency versions come out of `Cargo.lock` rather than from a crate
//! like `built`: it is twenty lines against another build dependency, and the
//! handful worth naming in a bug report is a short and stable list - the two
//! protocol implementations, the TLS stack, and the three the message path runs
//! on.

fn main() {
    const INTERESTING: [&str; 8] = [
        "mqtt-glide",
        "loxwebsocket",
        "rustls",
        "ring",
        "tokio",
        "regex",
        "serde_json",
        "quick-xml",
    ];

    println!("cargo:rerun-if-changed=Cargo.lock");
    println!(
        "cargo:rustc-env=RELAY_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    println!(
        "cargo:rustc-env=RELAY_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_default()
    );
    // Whatever `-C target-cpu=` the build was given, so the banner can say which
    // of the two x86 builds is running.
    let target_cpu = std::env::var("CARGO_ENCODED_RUSTFLAGS")
        .unwrap_or_default()
        .split('\u{1f}')
        .find_map(|flag| flag.strip_prefix("target-cpu=").map(str::to_owned))
        .unwrap_or_else(|| "default".to_owned());
    println!("cargo:rustc-env=RELAY_TARGET_CPU={target_cpu}");

    let lock = std::fs::read_to_string("Cargo.lock").unwrap_or_default();
    let mut versions: Vec<String> = Vec::new();
    let mut name: Option<&str> = None;
    for line in lock.lines() {
        if let Some(value) = line.strip_prefix("name = \"") {
            name = value.strip_suffix('"');
        } else if let Some(value) = line.strip_prefix("version = \"")
            && let Some(found) = name.take()
            && INTERESTING.contains(&found)
        {
            versions.push(format!("{found}={}", value.trim_end_matches('"')));
        }
    }
    println!("cargo:rustc-env=RELAY_DEPS={}", versions.join(", "));
}

//! Make the unit test harness runnable.
//!
//! The wheel is a `cdylib` built with pyo3's `extension-module`, which leaves
//! libpython to the interpreter that loads it. `cargo test` instead produces an
//! executable, so it links libpython itself - and then cannot find it at
//! runtime, because nothing recorded where it lives. The rpath below fixes
//! that.
//!
//! The wheel builds ask for `extension-module` (see setup.py) and return here
//! before emitting anything, so the shipped library never carries an rpath into
//! the build machine's Python.

fn main() {
    emit_banner_facts();

    if std::env::var_os("CARGO_FEATURE_EXTENSION_MODULE").is_some() {
        return;
    }
    let config = pyo3_build_config::get();
    if let Some(lib_dir) = config.lib_dir.as_deref() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{lib_dir}");
    }
}

/// What the startup banner reports about this build.
///
/// The dependency versions come out of `Cargo.lock` rather than a crate like
/// `built`: it is twenty lines against another build dependency, and the handful
/// of crates worth naming in a bug report is a short and stable list - the two
/// protocol implementations, the TLS stack, and the three the message path runs
/// on.
fn emit_banner_facts() {
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

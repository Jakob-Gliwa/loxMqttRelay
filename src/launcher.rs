//! Picks the relay build this CPU can actually run, and becomes it.
//!
//! The wheel shipped two extensions and chose between them at import time,
//! because one amd64 image runs on every x86 host and only some of them have
//! AVX2. A multi-arch manifest picks by architecture, not by instruction set, so
//! that is still true and the choice still has to be made at runtime.
//!
//! It cannot be made *inside* the optimized build: a binary compiled for
//! `x86-64-v3` can fault on an instruction before `main` is reached, which is
//! uncatchable. So the decision lives in this third binary, compiled for the
//! baseline, whose only job is to look and then `exec`.
//!
//! Also why this is a binary rather than a line of shell: the image is
//! `FROM scratch` and has no shell to run it in.

use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;

/// The two relay builds, in the order they are preferred.
const OPTIMIZED: &str = "loxmqttrelay-relay-v3";
const GENERIC: &str = "loxmqttrelay-relay-generic";

fn main() -> ! {
    let relay = choose();
    let path = beside_me(relay);

    // exec only returns on failure, and there is no fallback worth attempting:
    // the alternative build would be the one this CPU was found not to run.
    let error = std::process::Command::new(&path)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("loxmqttrelay: could not start {}: {error}", path.display());
    std::process::exit(1);
}

/// Which build to run.
///
/// `LOXMQTTRELAY_BUILD` forces one, so a suspected mis-detection can be checked
/// without rebuilding anything - and so the generic build can be measured
/// against the optimized one on the same machine.
fn choose() -> &'static str {
    match std::env::var("LOXMQTTRELAY_BUILD").as_deref() {
        Ok("optimized") => return OPTIMIZED,
        Ok("generic") => return GENERIC,
        Ok(other) => eprintln!(
            "loxmqttrelay: ignoring LOXMQTTRELAY_BUILD='{other}', expected 'optimized' or 'generic'"
        ),
        Err(_) => {}
    }

    // x86-64-v3 is AVX2 + FMA + BMI1/2 + LZCNT + MOVBE. AVX2 is the one that
    // arrived with the others on every CPU that has it, so it stands for the
    // set - which is what `utils.has_avx2` did too, by reading /proc/cpuinfo.
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2")
            && std::arch::is_x86_feature_detected!("fma")
            && std::arch::is_x86_feature_detected!("bmi2")
        {
            return OPTIMIZED;
        }
    }
    GENERIC
}

/// The relay next to this launcher, not whatever is on PATH.
///
/// `scratch` has no PATH worth speaking of, and resolving a sibling by name
/// would be a way to run something else entirely.
fn beside_me(name: &str) -> PathBuf {
    match std::env::current_exe() {
        Ok(exe) => exe.with_file_name(name),
        Err(_) => PathBuf::from(name),
    }
}

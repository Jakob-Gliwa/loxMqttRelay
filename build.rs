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
    if std::env::var_os("CARGO_FEATURE_EXTENSION_MODULE").is_some() {
        return;
    }
    let config = pyo3_build_config::get();
    if let Some(lib_dir) = config.lib_dir.as_deref() {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{lib_dir}");
    }
}

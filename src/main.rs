//! The relay, as a program.
//!
//! Everything here is sequencing. The one thing worth reading twice is where
//! the re-exec happens: at the very end, after the runtime has been shut down,
//! and never from inside the control-topic handler that asked for it. A
//! `config/set` arrives on the ingress worker, and replacing the process image
//! there would do it with the MQTT session and the UDP socket still open.
//! `main.py` deferred it for exactly that reason, and so does this.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser as _;
use log::{error, info};

use loxmqttrelay::{Exit, Relay, Signals, cli, config, logging};

/// How long the runtime is given to let its tasks finish before the process
/// ends. Everything that matters has already been closed by `shutdown`; this is
/// only so the sockets are released before a re-exec re-binds them.
const RUNTIME_GRACE: Duration = Duration::from_secs(5);

fn main() {
    let args = cli::Args::parse();
    logging::init(&args);

    let config = match config::ConfigStore::load(&args.config) {
        Ok(config) => Arc::new(config),
        // Every problem has already been reported, one line each.
        Err(_) => std::process::exit(1),
    };

    loxmqttrelay::banner::log_runtime_environment(&config.snapshot(), &args.config);

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            error!("Could not start the async runtime: {e}");
            std::process::exit(1);
        }
    };

    let (signals, stop_rx) = Signals::new();
    let outcome = runtime.block_on(async move {
        let relay = Arc::new(Relay::build(config, signals)?);
        relay.run(stop_rx).await
    });

    runtime.shutdown_timeout(RUNTIME_GRACE);
    info!("MQTT Relay exited");

    match outcome {
        Ok(Exit::Restart) => reexec(),
        Ok(Exit::Normal) => {}
        // Already reported, at the point of failure and before the shutdown it
        // triggered - which is the order that reads correctly.
        Err(_) => std::process::exit(1),
    }
}

/// Replace this process with a fresh copy of itself, same arguments.
///
/// What `os.execv(sys.executable, [sys.executable] + sys.argv)` did. Note this
/// re-execs the binary this process was started from: if it has since been
/// replaced on disk, the old inode is what comes back. Python had the same
/// property through `sys.executable`, and a relay that restarts into a
/// half-written binary would be worse.
#[cfg(unix)]
fn reexec() -> ! {
    use std::os::unix::process::CommandExt as _;

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            error!("Could not restart the relay: {e}");
            std::process::exit(1);
        }
    };
    info!("Restarting {}", exe.display());
    // exec only returns on failure.
    let error = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec();
    error!("Could not restart the relay: {error}");
    std::process::exit(1);
}

#[cfg(not(unix))]
fn reexec() -> ! {
    // No exec on Windows: spawn a replacement and step aside.
    let exe = std::env::current_exe().unwrap_or_else(|e| {
        error!("Could not restart the relay: {e}");
        std::process::exit(1);
    });
    match std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .spawn()
    {
        Ok(_) => std::process::exit(0),
        Err(e) => {
            error!("Could not restart the relay: {e}");
            std::process::exit(1);
        }
    }
}

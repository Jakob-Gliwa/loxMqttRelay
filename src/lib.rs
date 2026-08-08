//! An MQTT relay for the Loxone Miniserver.
//!
//! Messages arrive over MQTT ([`mqtt`]) or as UDP datagrams ([`udp`]), are
//! flattened, filtered and normalized by the processing core ([`process`]), and
//! are written to the Miniserver over a websocket ([`miniserver`]). The relay's
//! own control topics - `config/get`, `config/set` and the restart triggers -
//! are recognized on the way in and handled by [`control`] instead.
//!
//! [`relay`] owns all of that and decides the order it starts and stops in;
//! [`config`] is the file it is built from. The binary in `src/main.rs` is a
//! thin sequencing layer over the two, and `src/launcher.rs` is the small
//! program that picks which build of it to run.
//!
//! # Where the comments talk about Python
//!
//! This was a Python program, and the parts facing the Miniserver were ported
//! rather than rewritten: the UDP message format is whatever the old parser
//! accepted, and the configuration file is whatever the old loader wrote. Where
//! a comment says "Python did X", it records why something looks the way it
//! does - it is not describing code that still exists.

pub mod banner;
pub mod cli;
pub mod config;
pub mod error;
pub mod logging;
pub mod relay;

mod control;
mod egress;
mod miniserver;
mod mqtt;
mod process;
mod signals;
mod udp;
mod util;
mod whitelist;

pub use error::RelayError;
pub use relay::{Exit, Relay};
pub use signals::Signals;

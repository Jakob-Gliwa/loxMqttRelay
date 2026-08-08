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
//! # Two things the comments keep coming back to
//!
//! The UDP parser and the configuration loader both implement formats that were
//! defined by an earlier implementation and are now fixed by what is already
//! deployed. Where one of them looks odd, the comment says which rule it is
//! following rather than leaving it as a puzzle - `str.isspace()` semantics for
//! whitespace, latin-1 for HTTP Basic, and the attribute normalization every
//! conforming XML parser applies.

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

//! What can stop the relay from starting, or from stopping cleanly.
//!
//! One type across the four components, because the only thing done with any of
//! them is to report it and abort the start. Distinguishing them further would
//! be distinguishing them for nobody.

use std::fmt;

#[derive(Debug)]
pub(crate) enum RelayError {
    /// The broker could not be reached, or the url was not usable.
    Mqtt(String),
    /// The Miniserver websocket could not be opened.
    Miniserver(String),
    /// The UDP socket could not be bound, or the source filter refused.
    Udp(String),
    /// A filter list in the configuration could not be compiled.
    Filter(String),
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RelayError::Mqtt(detail) => write!(f, "MQTT connection failed: {detail}"),
            RelayError::Miniserver(detail) => {
                write!(f, "Miniserver websocket failed: {detail}")
            }
            RelayError::Udp(detail) => write!(f, "UDP listener failed: {detail}"),
            RelayError::Filter(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for RelayError {}

impl From<crate::process::FilterError> for RelayError {
    fn from(error: crate::process::FilterError) -> Self {
        RelayError::Filter(error.0)
    }
}

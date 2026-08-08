//! Fetching the Miniserver's own list of virtual inputs.
//!
//! This is the cold path: it runs at startup, on a Miniserver startup event, and
//! after every websocket reconnect. What it produces is the topic whitelist - so
//! when `miniserver.sync_with_miniserver` is on, this decides what the relay is
//! willing to forward at all.
//!
//! The route is four steps and a wrapper each: list `/dev/fslist/prog/`, pick
//! the newest `sps_<version>_<timestamp>` file, download it via
//! `/dev/fsget/prog/`, unwrap the LoxCC container, and scan the XML. Each step
//! lives in its own module.
//!
//! Note this is the one place the relay still speaks plain HTTP to the
//! Miniserver. The filesystem endpoints cannot be command-encrypted and are
//! served as plaintext only; everything else goes over the websocket in
//! [`crate::miniserver`].

use std::fmt;

use log::info;

mod http;
mod loxcc;
mod xml;

use crate::util::loggable_bytes;

/// Why a whitelist sync did not finish.
///
/// The `Display` strings are the contract, not an implementation detail: they
/// are what an operator reads in the log and what they will have searched for
/// before, so the ones inherited from the Python implementation are reproduced
/// word for word.
#[derive(Debug)]
pub(crate) enum SyncError {
    /// The configured address has no host part.
    NoHost,
    /// The credentials cannot be spelled the way the Miniserver expects.
    Credentials(String),
    /// Dialling, handshaking or reading failed.
    Transport { url: String, detail: String },
    /// The request did not finish inside its budget.
    Timeout { url: String },
    /// The Miniserver answered with an error status.
    Status { url: String, status: u16 },
    /// The `/prog` listing held no configuration file.
    NoConfigFiles,
    /// What came back is not a configuration at all.
    UnexpectedPayload { len: usize, head: Vec<u8> },
    /// The LoxCC container did not check out.
    Container(String),
    /// The deployment archive did not yield `sps0.LoxCC`.
    Archive(String),
    /// The configuration did not parse as XML.
    Xml(String),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::NoHost => f.write_str("no Miniserver address is configured"),
            SyncError::Credentials(detail) => write!(f, "the credentials cannot be sent: {detail}"),
            SyncError::Transport { url, detail } => write!(f, "{url} could not be reached: {detail}"),
            SyncError::Timeout { url } => write!(f, "{url} did not answer in time"),
            SyncError::Status { url, status } => write!(f, "{url} answered with status {status}"),
            SyncError::NoConfigFiles => f.write_str("No configuration files found"),
            SyncError::UnexpectedPayload { len, head } => write!(
                f,
                "Unexpected configuration payload: {len} bytes starting with {}",
                loggable_bytes(head)
            ),
            SyncError::Container(detail) | SyncError::Archive(detail) | SyncError::Xml(detail) => {
                f.write_str(detail)
            }
        }
    }
}

impl std::error::Error for SyncError {}

/// A Miniserver's plaintext filesystem API.
///
/// Holds the configured port rather than the dialled one, because for port 443
/// the two differ and both are needed - see [`Endpoint::dialled_port`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Endpoint {
    host: String,
    port: u16,
}

impl Endpoint {
    /// `miniserver_ip` may carry a port of its own; only the host part is used,
    /// exactly as `sync_miniserver_whitelist` did with `.split(':')[0]`.
    pub(crate) fn new(miniserver_ip: &str, port: u16) -> Result<Self, SyncError> {
        let host = miniserver_ip.split(':').next().unwrap_or_default().trim();
        if host.is_empty() {
            return Err(SyncError::NoHost);
        }
        Ok(Endpoint {
            host: host.to_owned(),
            port,
        })
    }

    /// The port actually dialled.
    ///
    /// BUG, ported deliberately: 443 is treated as a default and therefore
    /// dialled on **80**. `_build_base_url` wrote `http://{ip}` without a port
    /// for both 80 and 443, and `crate::miniserver::endpoint` maps 443 to https
    /// for the websocket - so the two halves have disagreed since the websocket
    /// move, and only the websocket side is right. Left as it was here so the
    /// port is a port; the fix is its own commit.
    fn dialled_port(&self) -> u16 {
        if matches!(self.port, 80 | 443) { 80 } else { self.port }
    }

    /// What goes in the `Host` header, and what a log line calls this server.
    fn authority(&self) -> String {
        match self.port {
            80 | 443 => self.host.clone(),
            port => format!("{}:{port}", self.host),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.authority())
    }
}

/// List, pick, download, unwrap, scan. The whole cold path.
pub(crate) async fn sync_whitelist(
    endpoint: &Endpoint,
    user: &str,
    password: &str,
) -> Result<Vec<String>, SyncError> {
    let config_xml = load_miniserver_config(endpoint, user, password).await?;
    let inputs = xml::extract_inputs(&config_xml)?;
    info!("Extracted {} inputs from miniserver configuration", inputs.len());
    Ok(inputs)
}

/// The configuration XML, downloaded and unwrapped.
///
/// Split out from [`sync_whitelist`] so the download and the scan can be checked
/// against the Python implementation separately - which is what
/// `tests/test_rust_python_parity.py` does with a real configuration.
pub(crate) async fn load_miniserver_config(
    endpoint: &Endpoint,
    user: &str,
    password: &str,
) -> Result<Vec<u8>, SyncError> {
    let authorization = http::basic_auth(user, password)?;
    log::debug!(
        "Loading miniserver configuration from {} with username {user}",
        endpoint.base_url()
    );

    // There is no fixed-name pointer to the active configuration, so the
    // directory is listed and the newest file picked out of it.
    let listing = http::get(endpoint, "/dev/fslist/prog/", &authorization).await?;
    log::debug!("Received prog directory listing ({} bytes)", listing.len());

    let filename = http::select_newest_config(&listing)?;
    info!("Selected configuration file: {filename}");

    let raw = http::get(endpoint, &format!("/dev/fsget/prog/{filename}"), &authorization).await?;
    loxcc::decompress(&raw)
}

/// The two halves on their own.
///
/// Reachable individually so `tests/test_rust_python_parity.py` can compare each
/// against the Python implementation on a real configuration - the container
/// bytes first, then the titles. Both re-exports go when Python does.
pub(crate) use self::loxcc::decompress as decompress_loxcc;
pub(crate) use self::xml::extract_inputs;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_carried_in_the_address_is_not_part_of_the_host() {
        let endpoint = Endpoint::new("192.168.1.10:8080", 80).expect("an address");
        assert_eq!(endpoint.host, "192.168.1.10");
        assert_eq!(endpoint.port, 80);
    }

    #[test]
    fn an_empty_address_is_refused() {
        assert!(matches!(Endpoint::new("", 80), Err(SyncError::NoHost)));
        assert!(matches!(Endpoint::new("   ", 80), Err(SyncError::NoHost)));
        assert!(matches!(Endpoint::new(":8080", 80), Err(SyncError::NoHost)));
    }

    /// The default ports are left out of the URL, and 443 is dialled on 80.
    ///
    /// The second half is a bug being reproduced rather than a decision - see
    /// [`Endpoint::dialled_port`].
    #[test]
    fn the_default_ports_are_left_out_of_the_url() {
        for (port, url, dialled) in [
            (80u16, "http://ms.local", 80u16),
            (443, "http://ms.local", 80),
            (8080, "http://ms.local:8080", 8080),
        ] {
            let endpoint = Endpoint::new("ms.local", port).expect("an address");
            assert_eq!(endpoint.base_url(), url, "port {port}");
            assert_eq!(endpoint.dialled_port(), dialled, "port {port}");
        }
    }

    /// The container messages reach the log unchanged.
    #[test]
    fn the_inherited_messages_are_passed_through_verbatim() {
        assert_eq!(
            SyncError::Container("Invalid file format".to_owned()).to_string(),
            "Invalid file format"
        );
        assert_eq!(
            SyncError::NoConfigFiles.to_string(),
            "No configuration files found"
        );
        assert!(
            SyncError::UnexpectedPayload {
                len: 42,
                head: b"{\"LL\":".to_vec(),
            }
            .to_string()
            .starts_with("Unexpected configuration payload: 42 bytes starting with")
        );
    }
}

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

    /// The port actually dialled: the one that was configured.
    ///
    /// `_build_base_url` treated 443 as a default alongside 80 and wrote
    /// `http://{ip}` with no port for both, so a Miniserver configured on 443
    /// had its filesystem API dialled on **80** - while
    /// `crate::miniserver::endpoint` correctly maps 443 to https for the
    /// websocket. The two halves had disagreed since the websocket move, and
    /// only the websocket side was right.
    fn dialled_port(&self) -> u16 {
        self.port
    }

    /// What goes in the `Host` header, and what a log line calls this server.
    ///
    /// Only 80 is elided, because only 80 is the default for the `http` scheme
    /// these requests use. Spelling 443 out is what makes the port above reach
    /// the port below.
    fn authority(&self) -> String {
        match self.port {
            80 => self.host.clone(),
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
/// Split out from [`sync_whitelist`] so the download and the scan can be
/// exercised apart from each other - the tests below drive this half against a
/// stub server, and `tests::the_real_configuration_yields_its_inputs` drives the
/// other against a real Miniserver's output.
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

/// The two halves on their own, for the test that reads a real configuration
/// off this machine.
#[cfg(test)]
use self::loxcc::decompress as decompress_loxcc;
#[cfg(test)]
use self::xml::extract_inputs;

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

    /// Only 80 is elided, and every configured port is the one dialled.
    ///
    /// The 443 row is the regression test: `_build_base_url` elided it too, so a
    /// Miniserver configured on 443 had its filesystem API dialled on 80.
    #[test]
    fn only_the_http_default_port_is_left_out_of_the_url() {
        for (port, url) in [
            (80u16, "http://ms.local"),
            (443, "http://ms.local:443"),
            (8080, "http://ms.local:8080"),
        ] {
            let endpoint = Endpoint::new("ms.local", port).expect("an address");
            assert_eq!(endpoint.base_url(), url, "port {port}");
            assert_eq!(endpoint.dialled_port(), port, "port {port}");
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
    // -- the whole cold path, end to end ------------------------------------
    //
    // List, pick, download, unwrap, scan - against a stub that answers the two
    // requests in order. The pieces are covered on their own in the sibling
    // modules; what these add is that they are wired together correctly.

    use super::http::tests::{Reply, stub_sequence};

    const LISTING: &str = "\
Emergency.LoxCC
sps_0252_20260430003125.zip
sps_0272_20260727223721.LoxCC
Music.json
";

    const XML: &[u8] =
        br#"<ControlList><C Type="VirtualInCaption"><C Title="Input1"/><C Title="Input2"/></C></ControlList>"#;

    /// A LoxCC container around `xml`, as `/dev/fsget` serves it.
    fn container(xml: &[u8]) -> Vec<u8> {
        let compressed = lz4_flex::block::compress(xml);
        let mut out = Vec::new();
        out.extend_from_slice(&0xaabb_cceeu32.to_le_bytes());
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc32fast::hash(xml).to_le_bytes());
        out.extend_from_slice(&compressed);
        out
    }

    /// The newest file is picked, downloaded and unwrapped - and the request
    /// that fetched it names that file.
    #[tokio::test]
    async fn the_newest_configuration_is_downloaded_and_unwrapped() {
        let (endpoint, server) = stub_sequence(vec![
            Reply::Body(LISTING.as_bytes().to_vec()),
            Reply::Body(container(XML)),
        ])
        .await;

        let config_xml = load_miniserver_config(&endpoint, "admin", "secret")
            .await
            .expect("a configuration");
        assert_eq!(config_xml, XML);

        let requests = server.await.expect("the stub");
        assert!(requests[0].starts_with("GET /dev/fslist/prog/ "), "{}", requests[0]);
        assert!(
            requests[1].starts_with("GET /dev/fsget/prog/sps_0272_20260727223721.LoxCC "),
            "{}",
            requests[1]
        );
    }

    /// And the whole way through to the input names.
    #[tokio::test]
    async fn a_sync_yields_the_virtual_input_names() {
        let (endpoint, server) = stub_sequence(vec![
            Reply::Body(LISTING.as_bytes().to_vec()),
            Reply::Body(container(XML)),
        ])
        .await;

        let inputs = sync_whitelist(&endpoint, "admin", "secret")
            .await
            .expect("a whitelist");
        assert_eq!(inputs, ["Input1", "Input2"]);
        let _ = server.await;
    }

    /// A listing with no configuration in it stops there - the second request
    /// is never made.
    #[tokio::test]
    async fn a_listing_without_a_configuration_stops_the_sync() {
        let (endpoint, server) = stub_sequence(vec![Reply::Body(
            b"Emergency.LoxCC\nMusic.json\n".to_vec(),
        )])
        .await;

        let error = load_miniserver_config(&endpoint, "admin", "secret")
            .await
            .unwrap_err();
        assert!(matches!(error, SyncError::NoConfigFiles), "{error}");

        let requests = server.await.expect("the stub");
        assert_eq!(requests.len(), 1, "the download should not have happened");
    }

    /// An error status on the listing is reported and stops the sync.
    #[tokio::test]
    async fn an_unauthorized_listing_stops_the_sync() {
        let (endpoint, server) = stub_sequence(vec![Reply::Status(401)]).await;

        let error = load_miniserver_config(&endpoint, "admin", "wrong")
            .await
            .unwrap_err();
        assert!(matches!(error, SyncError::Status { status: 401, .. }), "{error}");
        let _ = server.await;
    }

    /// The Miniserver answers some requests with a JSON error body and status
    /// 200, so "not a configuration" has to be told apart from "corrupt".
    #[tokio::test]
    async fn an_error_body_served_with_200_is_reported_as_such() {
        let (endpoint, server) = stub_sequence(vec![
            Reply::Body(LISTING.as_bytes().to_vec()),
            Reply::Body(br#"{"LL":{"control":"dev/fsget","Code":"403"}}"#.to_vec()),
        ])
        .await;

        let error = load_miniserver_config(&endpoint, "admin", "secret")
            .await
            .unwrap_err();
        assert!(
            error.to_string().starts_with("Unexpected configuration payload"),
            "{error}"
        );
        let _ = server.await;
    }

    /// The operator's own configuration, if it is on this machine.
    ///
    /// Deliberately not in the repository - it names their rooms and devices -
    /// so this is a local smoke test rather than CI coverage. It is also the
    /// only input that has ever exercised what the firmware actually writes: a
    /// 2.5 MB document with a BOM and CRLF line endings.
    #[test]
    fn the_real_configuration_yields_its_inputs() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config/sps0.LoxCC");
        let Ok(raw) = std::fs::read(&path) else {
            return;
        };
        let config_xml = decompress_loxcc(&raw).expect("a container");
        assert!(
            config_xml.starts_with(b"\xef\xbb\xbf<?xml version="),
            "the fixture should start with a BOM"
        );
        let inputs = extract_inputs(&config_xml).expect("input names");
        assert!(
            !inputs.is_empty(),
            "a real configuration should name some inputs"
        );
    }

}

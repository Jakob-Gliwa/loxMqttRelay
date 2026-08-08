//! The two GETs, and picking a file out of the listing.
//!
//! Built on hyper's connection API rather than on a client library. There are
//! exactly two requests, both plaintext, both to a device on the local network -
//! so what a full client would add is a TLS stack the relay already has one of,
//! and the project keeps exactly one of those on purpose.

use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Empty, Limited};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use super::{Endpoint, SyncError};

/// Per request, not per sync.
///
/// `aiohttp.ClientTimeout(total=30)` was set on the session and therefore
/// applied to each `session.get(...)` separately, connect through body. Wrapping
/// the whole sync in one budget instead would silently halve what the download
/// gets after the listing has used its share.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A ceiling on what will be read into memory.
///
/// aiohttp's `resp.read()` had none. A real configuration is well under a
/// megabyte, and the timeout already bounds this in practice - this is for the
/// case where something else entirely is on the other end of the socket.
const MAX_BODY: usize = 64 << 20;

/// GET one path, with the standard budget.
pub(super) async fn get(
    endpoint: &Endpoint,
    path: &str,
    authorization: &str,
) -> Result<Vec<u8>, SyncError> {
    get_with_timeout(endpoint, path, authorization, REQUEST_TIMEOUT).await
}

/// The same, with the budget spelled out. The seam the timeout test needs.
pub(super) async fn get_with_timeout(
    endpoint: &Endpoint,
    path: &str,
    authorization: &str,
    budget: Duration,
) -> Result<Vec<u8>, SyncError> {
    let url = format!("{}{path}", endpoint.base_url());
    match tokio::time::timeout(budget, request(endpoint, path, authorization, &url)).await {
        Ok(result) => result,
        Err(_) => Err(SyncError::Timeout { url }),
    }
}

async fn request(
    endpoint: &Endpoint,
    path: &str,
    authorization: &str,
    url: &str,
) -> Result<Vec<u8>, SyncError> {
    let transport = |detail: String| SyncError::Transport {
        url: url.to_owned(),
        detail,
    };

    let stream = TcpStream::connect((endpoint.host.as_str(), endpoint.dialled_port()))
        .await
        .map_err(|e| transport(e.to_string()))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| transport(e.to_string()))?;
    // One connection per request, dropped when this task ends. aiohttp reused
    // the session's; two requests a few minutes apart make that invisible, and
    // Miniservers are not reliable about keep-alive anyway.
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let request = hyper::Request::builder()
        .uri(path)
        .header(hyper::header::HOST, endpoint.authority())
        .header(hyper::header::AUTHORIZATION, authorization)
        .body(Empty::<Bytes>::new())
        .map_err(|e| transport(e.to_string()))?;

    let response = sender
        .send_request(request)
        .await
        .map_err(|e| transport(e.to_string()))?;

    // `raise_for_status()` fired at 400 and above, and not on a redirect. The
    // low-level client follows no redirects either way; the Miniserver does not
    // redirect /dev/fs*.
    let status = response.status().as_u16();
    if status >= 400 {
        return Err(SyncError::Status {
            url: url.to_owned(),
            status,
        });
    }

    let body = Limited::new(response.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|e| transport(format!("the body could not be read: {e}")))?;
    Ok(body.to_bytes().to_vec())
}

/// The `Authorization` header for HTTP Basic.
///
/// latin-1, and an error above U+00FF rather than a fallback to UTF-8. That is
/// what `aiohttp.BasicAuth` did, and quietly sending different bytes instead
/// would show up at the Miniserver as a wrong password - which is a much harder
/// thing to work out than being told the password cannot be sent.
pub(super) fn basic_auth(user: &str, password: &str) -> Result<String, SyncError> {
    let mut raw = Vec::with_capacity(user.len() + password.len() + 1);
    for (field, text) in [("username", user), ("password", password)] {
        if field == "password" {
            raw.push(b':');
        }
        for c in text.chars() {
            match u8::try_from(u32::from(c)) {
                Ok(byte) => raw.push(byte),
                Err(_) => {
                    return Err(SyncError::Credentials(format!(
                        "the {field} contains {c:?}, which HTTP Basic cannot carry"
                    )));
                }
            }
        }
    }
    Ok(format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(&raw)
    ))
}

/// The most recent configuration file in a `/prog` directory listing.
///
/// There is no fixed-name pointer to the active configuration (confirmed with
/// Loxone), so the newest is picked by `(version, timestamp)`. Matched over the
/// raw bytes: the listing has no declared charset, aiohttp's `.text()` guessed
/// one, and the pattern is pure ASCII - so not decoding at all is one fewer
/// thing that can differ.
pub(super) fn select_newest_config(listing: &[u8]) -> Result<String, SyncError> {
    // `[0-9]` and not `\d`, so the question of what counts as a digit in Unicode
    // never arises. `Emergency.LoxCC` and the other files in /prog do not match.
    static PATTERN: &str = r"sps_([0-9]+)_([0-9]+)\.(?:zip|LoxCC)";
    let pattern = regex::bytes::Regex::new(PATTERN).expect("a literal pattern");

    let best = pattern
        .captures_iter(listing)
        .map(|caps| {
            let whole = caps.get(0).expect("group 0").as_bytes();
            let version = caps.get(1).expect("group 1").as_bytes();
            let timestamp = caps.get(2).expect("group 2").as_bytes();
            // Python compared `(int(v), int(ts), name)` - a whole tuple, so the
            // filename is a real third key: on an exact tie ".zip" beats
            // ".LoxCC" because 'z' > 'L'.
            (numeric_key(version), numeric_key(timestamp), whole)
        })
        .max();

    match best {
        Some((_, _, name)) => Ok(String::from_utf8_lossy(name).into_owned()),
        None => Err(SyncError::NoConfigFiles),
    }
}

/// A decimal string ordered as a number, without parsing it.
///
/// `int()` is arbitrary precision and `u64::from_str` is not, so a field with
/// enough digits in it would make the two disagree about which file is newest.
/// For non-negative decimals, comparing `(length, digits)` after stripping
/// leading zeros *is* numeric order - and it cannot overflow.
fn numeric_key(digits: &[u8]) -> (usize, &[u8]) {
    let stripped = match digits.iter().position(|&b| b != b'0') {
        Some(at) => &digits[at..],
        // All zeros: keep one, so "0000" and "0" compare equal.
        None => &digits[digits.len().saturating_sub(1)..],
    };
    (stripped.len(), stripped)
}

#[cfg(test)]
pub(in crate::whitelist) mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    const FW17_LISTING: &str = "\
Emergency.LoxCC
sps_0252_20260430003125.zip
sps_0272_20260727223721.LoxCC
Music.json
";

    #[test]
    fn the_newest_configuration_wins() {
        assert_eq!(
            select_newest_config(FW17_LISTING.as_bytes()).unwrap(),
            "sps_0272_20260727223721.LoxCC"
        );
    }

    #[test]
    fn versions_compare_numerically_and_not_as_text() {
        let listing = b"sps_9_20260101000000.zip\nsps_10_20260101000000.zip\n";
        assert_eq!(
            select_newest_config(listing).unwrap(),
            "sps_10_20260101000000.zip"
        );
        // The same for the timestamp.
        let listing = b"sps_1_9.zip\nsps_1_10.zip\n";
        assert_eq!(select_newest_config(listing).unwrap(), "sps_1_10.zip");
    }

    #[test]
    fn leading_zeros_do_not_change_the_order() {
        let listing = b"sps_0252_20260430003125.zip\nsps_252_20260430003124.zip\n";
        // Same version; the first has the later timestamp.
        assert_eq!(
            select_newest_config(listing).unwrap(),
            "sps_0252_20260430003125.zip"
        );
    }

    /// On an exact tie the filename decides, and ".zip" wins.
    ///
    /// Python's `max` compared the whole `(version, timestamp, name)` tuple and
    /// fell through to the name, where 'z' sorts after 'L'. A fresh upload puts
    /// both files in /prog seconds apart, so this is reachable.
    #[test]
    fn a_dead_tie_is_broken_by_the_filename() {
        let listing = b"sps_1_2.LoxCC\nsps_1_2.zip\n";
        assert_eq!(select_newest_config(listing).unwrap(), "sps_1_2.zip");
    }

    #[test]
    fn an_absurdly_long_number_does_not_overflow() {
        let huge = "9".repeat(25);
        let listing = format!("sps_1_2.zip\nsps_{huge}_1.zip\n");
        assert_eq!(
            select_newest_config(listing.as_bytes()).unwrap(),
            format!("sps_{huge}_1.zip")
        );
    }

    #[test]
    fn a_listing_without_candidates_is_an_error() {
        assert!(matches!(
            select_newest_config(b"Emergency.LoxCC\nMusic.json\n"),
            Err(SyncError::NoConfigFiles)
        ));
        assert!(matches!(
            select_newest_config(b""),
            Err(SyncError::NoConfigFiles)
        ));
    }

    #[test]
    fn credentials_are_encoded_the_way_aiohttp_encoded_them() {
        assert_eq!(basic_auth("admin", "secret").unwrap(), "Basic YWRtaW46c2VjcmV0");
        // latin-1: 'ü' is one byte, 0xFC.
        let header = basic_auth("u", "ü").unwrap();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(header.trim_start_matches("Basic "))
            .unwrap();
        assert_eq!(raw, b"u:\xfc");
    }

    #[test]
    fn a_credential_outside_latin1_is_refused() {
        assert!(matches!(
            basic_auth("u", "pass\u{1f600}"),
            Err(SyncError::Credentials(_))
        ));
    }

    // -- the request path ---------------------------------------------------

    pub(in crate::whitelist) enum Reply {
        Body(Vec<u8>),
        #[allow(dead_code)]
        Status(u16),
        /// Accept the connection and never answer.
        Silent,
    }

    /// A hand-written HTTP/1.1 server that answers a fixed sequence.
    ///
    /// One reply per connection, and the client opens one connection per
    /// request, so a two-element sequence is a listing followed by a download.
    /// Written out rather than mocked at the `get()` boundary because what is
    /// under test here *is* the wiring - the pure functions above are covered on
    /// their own.
    ///
    /// The join handle yields the request lines it saw, so a test can assert on
    /// the paths and headers that were actually sent.
    pub(in crate::whitelist) async fn stub_sequence(
        replies: Vec<Reply>,
    ) -> (Endpoint, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for reply in replies {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                // Read to the end of the headers; there is no request body.
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                seen.push(String::from_utf8_lossy(&request).into_owned());

                match reply {
                    Reply::Silent => {
                        // Hold the connection open so the caller's budget is
                        // what ends the request.
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    Reply::Status(status) => {
                        let head =
                            format!("HTTP/1.1 {status} Nope\r\nContent-Length: 0\r\n\r\n");
                        let _ = socket.write_all(head.as_bytes()).await;
                    }
                    Reply::Body(body) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                    }
                }
            }
            seen
        });
        (
            Endpoint::new("127.0.0.1", port).expect("an address"),
            handle,
        )
    }

    /// The single-reply case, which most tests want.
    async fn stub(reply: Reply) -> (Endpoint, tokio::task::JoinHandle<Vec<String>>) {
        stub_sequence(vec![reply]).await
    }

    #[tokio::test]
    async fn a_body_comes_back_whole() {
        let (endpoint, server) = stub(Reply::Body(FW17_LISTING.as_bytes().to_vec())).await;
        let body = get(&endpoint, "/dev/fslist/prog/", "Basic x")
            .await
            .expect("a listing");
        assert_eq!(body, FW17_LISTING.as_bytes());
        server.await.expect("the stub");
    }

    #[tokio::test]
    async fn the_credentials_reach_the_miniserver() {
        let (endpoint, server) = stub(Reply::Body(b"ok".to_vec())).await;
        let authorization = basic_auth("admin", "secret").unwrap();
        get(&endpoint, "/dev/fsget/prog/x", &authorization)
            .await
            .expect("a body");
        let seen = server.await.expect("the stub");
        let request = &seen[0];
        assert!(request.contains("GET /dev/fsget/prog/x "), "{request}");
        assert!(
            request.contains("authorization: Basic YWRtaW46c2VjcmV0"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn an_error_status_is_reported_with_the_status() {
        let (endpoint, server) = stub(Reply::Status(401)).await;
        let error = get(&endpoint, "/dev/fslist/prog/", "Basic x")
            .await
            .unwrap_err();
        assert!(matches!(error, SyncError::Status { status: 401, .. }), "{error}");
        server.await.expect("the stub");
    }

    /// A server that never answers ends the request, rather than the sync.
    #[tokio::test]
    async fn a_request_that_hangs_gives_up_on_its_own_budget() {
        let (endpoint, _server) = stub(Reply::Silent).await;
        let error = get_with_timeout(
            &endpoint,
            "/dev/fslist/prog/",
            "Basic x",
            Duration::from_millis(120),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SyncError::Timeout { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_server_that_is_not_there_is_reported() {
        // Bind and drop, so the port is almost certainly closed.
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let endpoint = Endpoint::new("127.0.0.1", port).expect("an address");
        let error = get(&endpoint, "/dev/fslist/prog/", "Basic x")
            .await
            .unwrap_err();
        assert!(matches!(error, SyncError::Transport { .. }), "{error}");
    }
}

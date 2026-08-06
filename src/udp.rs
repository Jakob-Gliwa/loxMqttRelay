//! UDP ingress: a datagram from the Miniserver in, an MQTT publish out.
//!
//! Nothing on this path touches Python. The socket, the parser and the source
//! filter all live here, and the publish goes straight to [`MqttShared`], so a
//! datagram never needs the GIL.
//!
//! Where the wording below says "Python did X", it refers to the asyncio
//! implementation this replaces: the parser is a faithful port, quirks
//! included, because the Miniserver's message format is whatever that parser
//! accepted.

use std::borrow::Cow;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{Level, debug, error, info, log_enabled, warn};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::{future_into_py, get_runtime};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use crate::mqtt::{MqttClient, MqttShared};
use crate::util::{lock_recover, loggable};

/// Upper bound for the "warn once per sender" bookkeeping, so a flood of
/// spoofed source addresses cannot grow the set (or the log) without limit.
const MAX_TRACKED_REJECTED_SOURCES: usize = 64;

/// The largest payload a UDP datagram can carry, so nothing is ever truncated.
const MAX_DATAGRAM: usize = 65_535;

/// How long to wait between attempts to resolve a configured sender that did
/// not resolve at startup. Every datagram is dropped in the meantime, so this
/// trades a slow recovery for not hammering a DNS server that is already down.
const SOURCE_RETRY_INTERVAL: Duration = Duration::from_secs(300);

type UserProperties = Vec<(String, String)>;

// ---------------------------------------------------------------------------
// Text handling
// ---------------------------------------------------------------------------

/// `bytes.decode('utf-8', errors='ignore')`.
///
/// Not `String::from_utf8_lossy`: that substitutes U+FFFD, which would put a
/// character into the topic or the payload that the Miniserver never sent.
/// `errors='ignore'` drops the offending bytes instead, and so does this.
fn decode_utf8_ignore(data: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(data) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => {
            let mut out = String::with_capacity(data.len());
            let mut rest = data;
            loop {
                match std::str::from_utf8(rest) {
                    Ok(tail) => {
                        out.push_str(tail);
                        break;
                    }
                    Err(e) => {
                        let valid = e.valid_up_to();
                        if let Ok(head) = std::str::from_utf8(&rest[..valid]) {
                            out.push_str(head);
                        }
                        match e.error_len() {
                            Some(bad) => rest = &rest[valid + bad..],
                            // Truncated sequence at the very end - nothing follows.
                            None => break,
                        }
                    }
                }
            }
            Cow::Owned(out)
        }
    }
}

/// Python's `str.isspace()`, which is what `str.split()` and `str.strip()` use.
///
/// `char::is_whitespace` is the Unicode White_Space property and leaves out the
/// four ASCII separators U+001C..U+001F that Python counts as space.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// `str.split()` with no argument: runs of whitespace, no empty tokens.
fn py_split(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_py_space).filter(|token| !token.is_empty())
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// A datagram taken apart, borrowing from the decoded message where it can.
struct ParsedMessage<'a> {
    retain: bool,
    topic: Cow<'a, str>,
    payload: Cow<'a, str>,
    user_properties: Option<UserProperties>,
}

/// Determine the command and return `(retain, rest)`.
///
/// If the first word (case-insensitive) is `publish`/`retain` it is used as the
/// command and stripped off. Otherwise the command defaults to publish and the
/// whole stripped message is the rest. `None` means there is nothing usable.
fn parse_command(udpmsg: &str) -> Option<(bool, &str)> {
    let msg = py_strip(udpmsg);
    if msg.is_empty() {
        warn!("Empty UDP message");
        return None;
    }

    // Python's msg.split(None, 1): the separator run belongs to neither side.
    let (first_token, tail) = match msg.find(is_py_space) {
        Some(i) => (&msg[..i], Some(py_strip(&msg[i..]))),
        None => (msg, None),
    };

    let is_retain = first_token.eq_ignore_ascii_case("retain");
    let rest = if is_retain || first_token.eq_ignore_ascii_case("publish") {
        match tail {
            Some(tail) => tail,
            None => {
                error!("Missing topic/payload after command: {}", loggable(msg));
                return None;
            }
        }
    } else {
        msg
    };

    if rest.is_empty() {
        error!("No topic/message after command: {}", loggable(msg));
        return None;
    }

    Some((is_retain, rest))
}

/// Parse the content of a `[...]` block into key/value pairs.
///
/// Pairs are separated by `;`, key and value by the first `=`. A pair counts
/// only with a `=` and a non-empty key; empty values are allowed. `None` means
/// not a single valid pair was found, and the block is then *not* treated as
/// user properties at all.
///
/// The second half of the result counts the segments that were thrown away.
/// It is up to the caller to report them, and to report them once: a datagram
/// may carry tens of thousands of `;` and a line each would bury every other
/// message in the log - including for a block that is not a property block at
/// all and whose brackets simply belong to the topic.
fn parse_user_properties(block_content: &str) -> (Option<UserProperties>, usize) {
    let mut properties: UserProperties = Vec::new();
    let mut discarded = 0usize;
    for segment in block_content.split(';') {
        let Some((key, value)) = segment.split_once('=') else {
            if !py_strip(segment).is_empty() {
                discarded += 1;
            }
            continue;
        };
        let key = py_strip(key);
        if key.is_empty() {
            discarded += 1;
            continue;
        }
        properties.push((key.to_owned(), value.to_owned()));
    }

    ((!properties.is_empty()).then_some(properties), discarded)
}

/// Peel off a leading `[...]` block when it holds at least one valid pair.
///
/// Otherwise the input comes back untouched and the `[` is just the start of a
/// topic, which is how a topic containing brackets keeps working.
fn extract_property_block(rest: &str) -> (Option<UserProperties>, &str) {
    if !rest.starts_with('[') {
        return (None, rest);
    }
    let Some(close_index) = rest.find(']') else {
        return (None, rest);
    };

    let block = &rest[1..close_index];
    let (properties, discarded) = parse_user_properties(block);
    let Some(properties) = properties else {
        return (None, rest);
    };

    let remaining = py_strip(&rest[close_index + 1..]);
    if remaining.is_empty() {
        error!("Property block without topic/payload: {}", loggable(rest));
        return (None, rest);
    }

    if discarded > 0 {
        warn!(
            "Ignored {discarded} malformed user property segment(s) (no '=' or an empty key) in \
             '{}'",
            loggable(block)
        );
    }

    (Some(properties), remaining)
}

/// Split the remainder into topic and payload.
///
/// A `{` anywhere means the payload is JSON and starts there. Without one, two
/// tokens are simply topic and payload; with more, the topic grows greedily
/// while a token either contains a slash or sits between two that do. That rule
/// is what lets a Loxone topic contain spaces (`Rollo Gallerie links/set`)
/// without quoting.
fn parse_topic_payload(rest: &str) -> Option<(Cow<'_, str>, Cow<'_, str>)> {
    if let Some(brace_index) = rest.find('{') {
        let topic_part = rest[..brace_index].trim_end_matches(is_py_space);
        let payload_part = py_strip(&rest[brace_index..]);

        if topic_part.is_empty() || payload_part.is_empty() {
            error!(
                "Invalid format - topic or payload empty: {}",
                loggable(rest)
            );
            return None;
        }
        return Some((Cow::Borrowed(topic_part), Cow::Borrowed(payload_part)));
    }

    let tokens: Vec<&str> = py_split(rest).collect();
    if tokens.len() < 2 {
        error!(
            "Invalid format - need at least topic + payload: {}",
            loggable(rest)
        );
        return None;
    }
    if tokens.len() == 2 {
        return Some((Cow::Borrowed(tokens[0]), Cow::Borrowed(tokens[1])));
    }

    let has_slash = |token: &str| token.contains('/');
    let n = tokens.len();
    let mut i = 1;
    // Stop at the second-to-last token: the last one has to be payload.
    while i < n - 1 {
        let keep = has_slash(tokens[i]) || (has_slash(tokens[i - 1]) && has_slash(tokens[i + 1]));
        if !keep {
            break;
        }
        i += 1;
    }

    let topic_str = tokens[..i].join(" ");
    let payload_str = tokens[i..].join(" ");
    if topic_str.is_empty() || payload_str.is_empty() {
        error!(
            "Invalid format - empty topic or payload: {}",
            loggable(rest)
        );
        return None;
    }

    Some((Cow::Owned(topic_str), Cow::Owned(payload_str)))
}

/// Full MQTT 5 parse: command, optional property block, topic and payload.
fn parse_udp_message(udpmsg: &str) -> Option<ParsedMessage<'_>> {
    let (retain, rest) = parse_command(udpmsg)?;
    let (user_properties, rest) = extract_property_block(rest);
    let (topic, payload) = parse_topic_payload(rest)?;
    Some(ParsedMessage {
        retain,
        topic,
        payload,
        user_properties,
    })
}

// ---------------------------------------------------------------------------
// Allowed senders
// ---------------------------------------------------------------------------

/// Strip an optional port from a configured address.
fn host_part(address: &str) -> &str {
    let address = py_strip(address);
    if address.starts_with('[') {
        // [::1]:80
        if let Some(closing_bracket) = address.find(']') {
            return &address[1..closing_bracket];
        }
    }
    if address.parse::<IpAddr>().is_ok() {
        return address;
    }
    // A bare IPv6 address contains several colons and carries no port here.
    if address.matches(':').count() == 1 {
        return address.split(':').next().unwrap_or(address);
    }
    address
}

/// Resolve a host (IP literal or DNS name) to its numeric addresses.
///
/// Kept as addresses rather than their text form: that is one less allocation
/// per datagram in the filter, and `::1` and `0:0:0:0:0:0:0:1` are then the
/// same sender rather than two strings that happen to disagree.
///
/// IPv6 results are rejected rather than added to the allowlist: the UDP
/// listener binds `0.0.0.0` only (see [`UdpListener::start`]), so a sender
/// address that resolved to IPv6 can never actually show up on that socket. An
/// allowlist that quietly included it would look configured while staying
/// deaf forever - clearer to say so at resolution time and treat the host as
/// unresolved, which is what falls out of dropping those addresses here: the
/// existing "no usable sender address" handling and background retry take
/// over from there.
fn resolve_host(host: &str) -> HashSet<IpAddr> {
    if host.is_empty() {
        return HashSet::new();
    }
    match (host, 0u16).to_socket_addrs() {
        Ok(addrs) => {
            let (v4, v6): (HashSet<IpAddr>, HashSet<IpAddr>) =
                addrs.map(|addr| addr.ip()).partition(IpAddr::is_ipv4);
            if !v6.is_empty() {
                error!(
                    "'{}' resolved to IPv6 address(es) {} - the UDP listener only binds \
                     IPv4, so datagrams from these can never arrive; they are ignored. \
                     Configure an IPv4 address, or a hostname with an A record, instead",
                    loggable(host),
                    sorted_listing(v6.into_iter())
                );
            }
            v4
        }
        Err(e) => {
            // `udp_allowed_sources` is writable over MQTT, so even a configured
            // host is foreign text.
            error!(
                "Cannot resolve configured UDP sender '{}', ignoring it: {e}",
                loggable(host)
            );
            HashSet::new()
        }
    }
}

fn in_prefix(addr: &[u8], net: &[u8], prefix: u32) -> bool {
    let full = (prefix / 8) as usize;
    if addr[..full] != net[..full] {
        return false;
    }
    let remainder = prefix % 8;
    if remainder == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - remainder);
    (addr[full] & mask) == (net[full] & mask)
}

/// Whether an address is anything other than a public, globally routed one.
///
/// Deliberately not `Ipv4Addr::is_private`, which is RFC 1918 only: loopback and
/// link-local have to count too, or a relay talking to a Miniserver on
/// `127.0.0.1` would be warned about a public address that is nothing of the
/// sort. The table is Python's `ipaddress.IPv4Address.is_private` (3.13 and
/// later, including the `192.0.0.9`/`192.0.0.10` exceptions) with one addition:
/// the CGNAT shared address space `100.64.0.0/10`, for which Python reports
/// neither `is_private` nor `is_global`. It is not a DynDNS answer either, which
/// is the mistake the only caller warns about, so it belongs on this side.
fn is_non_public_v4(addr: Ipv4Addr) -> bool {
    const NETS: &[(Ipv4Addr, u32)] = &[
        (Ipv4Addr::new(0, 0, 0, 0), 8),
        (Ipv4Addr::new(10, 0, 0, 0), 8),
        (Ipv4Addr::new(100, 64, 0, 0), 10),
        (Ipv4Addr::new(127, 0, 0, 0), 8),
        (Ipv4Addr::new(169, 254, 0, 0), 16),
        (Ipv4Addr::new(172, 16, 0, 0), 12),
        (Ipv4Addr::new(192, 0, 0, 0), 24),
        (Ipv4Addr::new(192, 0, 2, 0), 24),
        (Ipv4Addr::new(192, 168, 0, 0), 16),
        (Ipv4Addr::new(198, 18, 0, 0), 15),
        (Ipv4Addr::new(198, 51, 100, 0), 24),
        (Ipv4Addr::new(203, 0, 113, 0), 24),
        (Ipv4Addr::new(240, 0, 0, 0), 4),
        (Ipv4Addr::new(255, 255, 255, 255), 32),
    ];
    // Globally reachable despite sitting inside 192.0.0.0/24 (PCP anycast).
    const EXCEPTIONS: &[Ipv4Addr] = &[Ipv4Addr::new(192, 0, 0, 9), Ipv4Addr::new(192, 0, 0, 10)];

    if EXCEPTIONS.contains(&addr) {
        return false;
    }
    let octets = addr.octets();
    NETS.iter()
        .any(|(net, prefix)| in_prefix(&octets, &net.octets(), *prefix))
}

/// [`is_non_public_v4`]'s counterpart: Python's `ipaddress.IPv6Address.is_private`,
/// including its delegation for IPv4-mapped addresses.
fn is_non_public_v6(addr: Ipv6Addr) -> bool {
    const NETS: &[(Ipv6Addr, u32)] = &[
        (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1), 128),
        (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 128),
        (Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96),
        (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
        (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
        (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
        (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
        (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
        (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    ];
    const EXCEPTIONS: &[(Ipv6Addr, u32)] = &[
        (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 1), 128),
        (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 2), 128),
        (Ipv6Addr::new(0x2001, 3, 0, 0, 0, 0, 0, 0), 32),
        (Ipv6Addr::new(0x2001, 4, 0x112, 0, 0, 0, 0, 0), 48),
        (Ipv6Addr::new(0x2001, 0x20, 0, 0, 0, 0, 0, 0), 28),
        (Ipv6Addr::new(0x2001, 0x30, 0, 0, 0, 0, 0, 0), 28),
    ];

    if let Some(mapped) = addr.to_ipv4_mapped() {
        return is_non_public_v4(mapped);
    }
    let octets = addr.octets();
    if EXCEPTIONS
        .iter()
        .any(|(net, prefix)| in_prefix(&octets, &net.octets(), *prefix))
    {
        return false;
    }
    NETS.iter()
        .any(|(net, prefix)| in_prefix(&octets, &net.octets(), *prefix))
}

fn is_non_public(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => is_non_public_v4(v4),
        IpAddr::V6(v6) => is_non_public_v6(v6),
    }
}

/// Warn when an allowed sender is a public address.
///
/// A DynDNS entry (Loxone Cloud DNS, No-IP, ...) resolves to the WAN address of
/// the internet connection, while the Miniserver sends its datagrams from its
/// local address - so such a configuration drops everything.
fn warn_about_public_addresses(addresses: &HashSet<IpAddr>) {
    let public: Vec<IpAddr> = addresses
        .iter()
        .copied()
        .filter(|addr| !is_non_public(*addr))
        .collect();
    if public.is_empty() {
        return;
    }
    warn!(
        "Allowed UDP sender(s) {} are public addresses. The Miniserver sends UDP from its local \
         network address, so its datagrams would be dropped - configure its local address instead \
         of a DynDNS entry.",
        sorted_listing(public.iter().copied())
    );
}

/// Addresses as one comma-separated line, ordered like Python's `sorted()` did:
/// by their text form, not numerically.
fn sorted_listing(addresses: impl Iterator<Item = IpAddr>) -> String {
    let mut sorted: Vec<String> = addresses.map(|addr| addr.to_string()).collect();
    sorted.sort_unstable();
    sorted.join(", ")
}

/// Every sender the configuration names: the Miniserver plus the extras.
fn configured_sources(miniserver_ip: &str, extra_sources: &[String]) -> Vec<String> {
    let mut sources = Vec::with_capacity(1 + extra_sources.len());
    sources.push(miniserver_ip.to_owned());
    sources.extend(extra_sources.iter().cloned());
    sources
}

fn resolve_sources(
    sources: &[String],
    resolve: &dyn Fn(&str) -> HashSet<IpAddr>,
) -> HashSet<IpAddr> {
    sources
        .iter()
        .flat_map(|source| resolve(host_part(source)))
        .collect()
}

fn announce_allowed(allowed: &HashSet<IpAddr>) {
    info!(
        "UDP-IN accepts datagrams from {}",
        sorted_listing(allowed.iter().copied())
    );
    warn_about_public_addresses(allowed);
}

/// Senders to accept while the filter is on; empty means nothing resolved yet.
///
/// `resolve` is a parameter so the tests can decide what a name resolves to
/// without a DNS server in the loop.
fn configure_source_filter(
    miniserver_ip: &str,
    extra_sources: &[String],
    filter_enabled: bool,
    resolve: &dyn Fn(&str) -> HashSet<IpAddr>,
) -> HashSet<IpAddr> {
    if !filter_enabled {
        warn!(
            "UDP source filtering is switched off (udp_source_filter_enabled = false) - every \
             host that can reach the UDP port can publish to MQTT"
        );
        return HashSet::new();
    }

    let allowed = resolve_sources(&configured_sources(miniserver_ip, extra_sources), resolve);
    if allowed.is_empty() {
        error!(
            "No usable sender address configured (miniserver_ip='{miniserver_ip}') - UDP \
             datagrams are dropped until one of the configured names resolves"
        );
    } else {
        announce_allowed(&allowed);
    }
    allowed
}

/// Look the configured senders up again until one of them resolves.
///
/// Started only when the filter is on and startup resolution produced nothing.
/// Every datagram is dropped in that state, so without this a DNS server that
/// happened to be away at boot would leave the relay deaf until it is
/// restarted. One success is enough: the addresses are then in place and this
/// stops.
async fn resolve_sources_in_background(sources: Vec<String>, found: mpsc::Sender<HashSet<IpAddr>>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(SOURCE_RETRY_INTERVAL) => {}
            // The receive loop is gone, so the relay is shutting down and there
            // is nobody left to hand an address to.
            _ = found.closed() => return,
        }

        let attempt = sources.clone();
        let Ok(allowed) =
            tokio::task::spawn_blocking(move || resolve_sources(&attempt, &resolve_host)).await
        else {
            // The resolver panicked or the runtime is going away; retrying with
            // the same input would only repeat it.
            return;
        };
        if !allowed.is_empty() {
            let _ = found.send(allowed).await;
            return;
        }
    }
}

/// IPv4 default gateway from `/proc/net/route`, or `None` where that cannot be
/// read (any non-Linux host).
///
/// Inside a Docker bridge network this is the address datagrams appear to come
/// from whenever the userland proxy forwards them instead of iptables DNAT.
fn container_gateway() -> Option<IpAddr> {
    parse_default_gateway(&std::fs::read_to_string("/proc/net/route").ok()?)
}

fn parse_default_gateway(route_table: &str) -> Option<IpAddr> {
    for line in route_table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() > 2 && fields[1] == "00000000" {
            let gateway = u32::from_str_radix(fields[2], 16).ok()?;
            if gateway != 0 {
                // The table stores the address little-endian.
                return Some(IpAddr::V4(Ipv4Addr::from(gateway.swap_bytes())));
            }
        }
    }
    None
}

/// Decides which senders get through, and does the talking about the ones that
/// do not.
///
/// `enabled` is kept apart from `allowed` on purpose. The two used to be one
/// and the same - an empty set meant "let everyone through" - so a filter that
/// was switched off and a filter whose addresses could not be resolved were
/// indistinguishable, and a failure quietly picked the least safe policy.
/// Empty now means "no address to compare against yet", and that drops
/// datagrams rather than waving them through.
struct SourceFilter {
    enabled: bool,
    allowed: HashSet<IpAddr>,
    allowed_listing: String,
    gateway: Option<IpAddr>,
    gateway_warning_logged: bool,
    rejected: HashSet<IpAddr>,
}

impl SourceFilter {
    fn new(enabled: bool, allowed: HashSet<IpAddr>, gateway: Option<IpAddr>) -> Self {
        Self {
            enabled,
            allowed_listing: sorted_listing(allowed.iter().copied()),
            allowed,
            gateway,
            gateway_warning_logged: false,
            rejected: HashSet::new(),
        }
    }

    /// Whether the filter is on but has nothing to compare against, i.e. every
    /// datagram is currently dropped and another resolution attempt is worth
    /// making.
    fn awaits_addresses(&self) -> bool {
        self.enabled && self.allowed.is_empty()
    }

    /// Take on the addresses a later resolution attempt produced.
    fn adopt(&mut self, allowed: HashSet<IpAddr>) {
        self.allowed_listing = sorted_listing(allowed.iter().copied());
        self.allowed = allowed;
        // The senders turned away so far were turned away for want of an
        // address, so they deserve a fresh hearing in the log under the new one.
        self.rejected.clear();
        announce_allowed(&self.allowed);
    }

    fn allows(&mut self, source: IpAddr) -> bool {
        if !self.enabled || self.allowed.contains(&source) {
            return true;
        }

        if self.gateway == Some(source) {
            if !self.gateway_warning_logged {
                self.gateway_warning_logged = true;
                warn!(
                    "UDP datagrams arrive from the container gateway {source} instead of the \
                     Miniserver address, so Docker hides the real sender and source filtering \
                     cannot take effect. Accepting them anyway - restrict the UDP port on the \
                     host firewall or run the container with network_mode: host."
                );
            }
            return true;
        }

        if !self.rejected.contains(&source) && self.rejected.len() < MAX_TRACKED_REJECTED_SOURCES {
            self.rejected.insert(source);
            if self.allowed.is_empty() {
                warn!(
                    "Dropped UDP datagram from {source} - no configured sender address could be \
                     resolved yet, so nothing may publish via UDP"
                );
            } else {
                warn!(
                    "Dropped UDP datagram from {source} - only {} may publish via UDP",
                    self.allowed_listing
                );
            }
        } else {
            debug!("Dropped UDP datagram from {source}");
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------

/// Render properties the way the Python logger did, for the debug line only.
fn format_properties(properties: &Option<UserProperties>) -> String {
    match properties {
        None => "None".to_owned(),
        Some(list) => {
            let pairs: Vec<String> = list
                .iter()
                .map(|(key, value)| format!("('{}', '{}')", loggable(key), loggable(value)))
                .collect();
            format!("[{}]", pairs.join(", "))
        }
    }
}

/// Parse one datagram and hand the result to MQTT.
///
/// The publish is spawned rather than awaited: it can block on the client's
/// command channel, and a socket that stops being read is a socket that loses
/// datagrams in the kernel buffer. The asyncio version spawned a task per
/// datagram for the same reason.
fn handle_datagram(shared: &Arc<MqttShared>, data: &[u8], addr: SocketAddr) {
    let msg = decode_utf8_ignore(data);
    // DEBUG, not INFO: the datagram carries whatever the Miniserver sends, and
    // that is nobody's business at the default level.
    debug!("UDP IN: {addr}: {}", loggable(&msg));

    let Some(parsed) = parse_udp_message(&msg) else {
        return;
    };
    let ParsedMessage {
        retain,
        topic,
        payload,
        user_properties,
    } = parsed;

    if log_enabled!(Level::Debug) {
        debug!(
            "Publishing{}: '{}'='{}' properties={}",
            if retain { " (retain)" } else { "" },
            loggable(&topic),
            loggable(&payload),
            format_properties(&user_properties)
        );
    }

    let shared = Arc::clone(shared);
    let topic = topic.into_owned();
    let payload = payload.into_owned();
    let properties = user_properties.unwrap_or_default();
    // The sender travels with the publish so that a loss is reported once, with
    // everything in it: who sent the datagram, what was in it and why it is
    // gone. Nothing retries it - QoS 0, no outbox - so that line is all that is
    // left of the command the Miniserver sent.
    get_runtime().spawn(async move {
        // The reason is not read here: record_drop has already logged it.
        let _ = shared
            .publish(&topic, payload.as_bytes(), retain, properties, Some(addr))
            .await;
    });
}

async fn serve(
    socket: UdpSocket,
    shared: Arc<MqttShared>,
    mut filter: SourceFilter,
    mut late_addresses: mpsc::Receiver<HashSet<IpAddr>>,
    shutdown: Arc<Notify>,
) {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        tokio::select! {
            _ = shutdown.notified() => break,
            // `Some(..)` rather than a plain binding: once the background
            // resolver is done its sender is dropped, and matching on `Some`
            // is what retires this branch instead of letting a closed channel
            // complete the select over and over.
            Some(allowed) = late_addresses.recv() => filter.adopt(allowed),
            received = socket.recv_from(&mut buf) => match received {
                Ok((len, addr)) => {
                    if filter.allows(addr.ip()) {
                        handle_datagram(&shared, &buf[..len], addr);
                    }
                }
                Err(e) => {
                    // A datagram socket survives most errors (an ICMP unreachable
                    // for an earlier send, say), so this reports and keeps going
                    // rather than taking the relay's only inbound path down.
                    warn!("UDP receive failed: {e}");
                }
            },
        }
    }
    info!("UDP-IN stopped");
}

/// One run of the receive loop: what ends it, and what to join afterwards.
///
/// The `Notify` belongs to the run rather than to the server because [`stop`]
/// can leave a permit behind (see there), and a permit outliving its run would
/// end the next one on its first poll.
///
/// [`stop`]: UdpServer::stop
struct Running {
    shutdown: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Running {
    /// Whether the receive loop is still on the socket. A run that ended on its
    /// own leaves an entry behind that nothing needs to stop.
    fn is_live(&self) -> bool {
        !self.task.is_finished()
    }
}

/// The relay's UDP listener, owned by Python for its lifetime.
///
/// Construct it alongside the MQTT client, then [`UdpServer::start`] once the
/// broker connection is up and [`UdpServer::stop`] on shutdown.
#[pyclass]
pub struct UdpServer {
    shared: Arc<MqttShared>,
    port: u16,
    miniserver_ip: String,
    allowed_sources: Vec<String>,
    filter_enabled: bool,
    /// Shared with the `start` future, which is what actually spawns the loop.
    running: Arc<Mutex<Option<Running>>>,
}

#[pymethods]
impl UdpServer {
    #[new]
    #[pyo3(text_signature = "(self, global_config, mqtt_client)")]
    fn new(global_config: &Bound<'_, PyAny>, mqtt_client: PyRef<'_, MqttClient>) -> PyResult<Self> {
        let udp = global_config.getattr("udp")?;
        let miniserver = global_config.getattr("miniserver")?;

        Ok(Self {
            shared: mqtt_client.shared(),
            port: udp.getattr("udp_in_port")?.extract()?,
            miniserver_ip: miniserver.getattr("miniserver_ip")?.extract()?,
            allowed_sources: udp.getattr("udp_allowed_sources")?.extract()?,
            filter_enabled: udp.getattr("udp_source_filter_enabled")?.extract()?,
            running: Arc::new(Mutex::new(None)),
        })
    }

    /// Bind the socket and start accepting datagrams.
    ///
    /// Neither a failed bind nor a failed filter setup is logged and shrugged
    /// off. Without UDP the relay has no inbound path from the Miniserver, so
    /// starting anyway would be a relay that looks healthy and forwards
    /// nothing; and a relay that answers a panic in the filter by accepting
    /// every sender is worse than one that refuses to start.
    #[pyo3(text_signature = "(self)")]
    fn start<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let shared = Arc::clone(&self.shared);
        let slot = Arc::clone(&self.running);
        let port = self.port;
        let miniserver_ip = self.miniserver_ip.clone();
        let allowed_sources = self.allowed_sources.clone();
        let filter_enabled = self.filter_enabled;

        future_into_py(py, async move {
            // Binding first would report this as "address already in use", which
            // says nothing about the caller having started the server twice.
            if lock_recover(&slot).as_ref().is_some_and(Running::is_live) {
                return Err(PyRuntimeError::new_err(format!(
                    "UDP-IN is already listening on port {port}"
                )));
            }

            // 0.0.0.0 rather than dual-stack on purpose: an IPv4 peer on a
            // dual-stack socket shows up as ::ffff:192.168.1.10 and would never
            // match a configured address.
            let socket = UdpSocket::bind(("0.0.0.0", port))
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("Cannot bind UDP port {port}: {e}")))?;

            let sources = configured_sources(&miniserver_ip, &allowed_sources);

            // Name resolution blocks, and it happens once, at startup.
            let filter = tokio::task::spawn_blocking(move || {
                let gateway = container_gateway();
                let allowed = configure_source_filter(
                    &miniserver_ip,
                    &allowed_sources,
                    filter_enabled,
                    &resolve_host,
                );
                SourceFilter::new(filter_enabled, allowed, gateway)
            })
            .await
            .map_err(|e| {
                PyRuntimeError::new_err(format!("Could not set up UDP source filtering: {e}"))
            })?;

            let (found, late_addresses) = mpsc::channel(1);
            if filter.awaits_addresses() {
                warn!(
                    "UDP-IN drops every datagram until a configured sender resolves; retrying \
                     every {} seconds",
                    SOURCE_RETRY_INTERVAL.as_secs()
                );
                get_runtime().spawn(resolve_sources_in_background(sources, found));
            }

            let shutdown = Arc::new(Notify::new());
            let task = get_runtime().spawn(serve(
                socket,
                shared,
                filter,
                late_addresses,
                Arc::clone(&shutdown),
            ));
            if let Some(previous) = lock_recover(&slot).replace(Running { shutdown, task }) {
                // Only reachable if two starts raced past the check above, and
                // then the bind would have failed. Abort rather than drop: a
                // dropped handle detaches its task instead of ending it.
                previous.task.abort();
            }
            info!("UDP-IN listening on port {port}");
            Ok(())
        })
    }

    /// Close the socket and wait for the receive loop to finish.
    ///
    /// Leaves the server startable again: what ends the loop is taken out of
    /// the slot here, so the next [`start`] gets its own.
    ///
    /// [`start`]: UdpServer::start
    #[pyo3(text_signature = "(self)")]
    fn stop<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let running = lock_recover(&self.running).take();
        if let Some(run) = &running {
            // `notify_waiters` wakes a loop that is already parked on
            // `notified()` but stores nothing; `notify_one` covers the window
            // where the task is spawned and not yet polled, by leaving a permit
            // its first poll consumes. That permit can outlive the loop, and
            // dropping the run here is what keeps it from reaching the next one.
            run.shutdown.notify_waiters();
            run.shutdown.notify_one();
        }

        future_into_py(py, async move {
            if let Some(run) = running
                && let Err(e) = run.task.await
                && !e.is_cancelled()
            {
                warn!("UDP receive loop ended abnormally: {e}");
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parse result in the shape the table below reads best in: the command
    /// as a word, owned strings, properties as plain pairs.
    type Parsed = (&'static str, String, String, Option<Vec<(String, String)>>);

    fn parse(msg: &str) -> Option<Parsed> {
        parse_udp_message(msg).map(|m| {
            (
                if m.retain { "retain" } else { "publish" },
                m.topic.into_owned(),
                m.payload.into_owned(),
                m.user_properties,
            )
        })
    }

    fn expect(command: &'static str, topic: &str, payload: &str) -> Option<Parsed> {
        Some((command, topic.to_owned(), payload.to_owned(), None))
    }

    /// Captures what the crate logs, so a test can assert on a log line that is
    /// the whole point of the code under it.
    ///
    /// `log` allows one global logger per process, so exactly one test may use
    /// this. Under nextest that is a process of its own anyway; under plain
    /// `cargo test` the single installation still holds.
    struct Recorder;

    static RECORDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    impl log::Log for Recorder {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            if let Ok(mut lines) = RECORDED.lock() {
                lines.push(format!("{} {}", record.level(), record.args()));
            }
        }
        fn flush(&self) {}
    }

    fn props(pairs: &[(&str, &str)]) -> Option<Vec<(String, String)>> {
        Some(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    // -----------------------------------------------------------------------
    // Parser: messages without a property block
    // -----------------------------------------------------------------------

    #[test]
    fn command_is_recognised_and_defaults_to_publish() {
        assert_eq!(
            parse("publish topic1 message1"),
            expect("publish", "topic1", "message1")
        );
        assert_eq!(
            parse("retain topic2 message2"),
            expect("retain", "topic2", "message2")
        );
        assert_eq!(
            parse("topic3 message3"),
            expect("publish", "topic3", "message3")
        );
    }

    #[test]
    fn the_command_word_is_case_insensitive() {
        assert_eq!(
            parse("RETAIN topic4 message4"),
            expect("retain", "topic4", "message4")
        );
        assert_eq!(
            parse("Retain topic5 message5"),
            expect("retain", "topic5", "message5")
        );
    }

    #[test]
    fn a_payload_may_contain_spaces() {
        assert_eq!(
            parse("publish a/b c/d message with spaces"),
            expect("publish", "a/b c/d", "message with spaces")
        );
        assert_eq!(
            parse("a/b c/d message with spaces"),
            expect("publish", "a/b c/d", "message with spaces")
        );
        assert_eq!(
            parse("publish topic6 message with spaces"),
            expect("publish", "topic6", "message with spaces")
        );
        assert_eq!(
            parse("topic7 message with spaces"),
            expect("publish", "topic7", "message with spaces")
        );
    }

    #[test]
    fn a_message_without_a_payload_is_rejected() {
        assert_eq!(parse("single"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("   "), None);
        assert_eq!(parse("publish"), None);
        assert_eq!(parse("retain"), None);
    }

    #[test]
    fn slashes_in_the_payload_do_not_confuse_the_split() {
        assert_eq!(
            parse("publish topic/with/slashes message/with/slashes"),
            expect("publish", "topic/with/slashes", "message/with/slashes")
        );
        assert_eq!(
            parse("publish test/topic/path message/with/slashes"),
            expect("publish", "test/topic/path", "message/with/slashes")
        );
        assert_eq!(
            parse("test/topic/path message/with/slashes"),
            expect("publish", "test/topic/path", "message/with/slashes")
        );
    }

    #[test]
    fn surrounding_whitespace_is_stripped() {
        assert_eq!(
            parse("  publish  a/path with/spaces  message8  "),
            expect("publish", "a/path with/spaces", "message8")
        );
        assert_eq!(
            parse("  a/path with/spaces  message9  "),
            expect("publish", "a/path with/spaces", "message9")
        );
        assert_eq!(
            parse("  publish  topic8  message8  "),
            expect("publish", "topic8", "message8")
        );
        assert_eq!(
            parse("  topic9  message9  "),
            expect("publish", "topic9", "message9")
        );
    }

    #[test]
    fn the_separators_are_pythons_and_not_rusts() {
        // Enumerated rather than derived, because the whole point is that this
        // set is not `char::is_whitespace`: the four ASCII separators below are
        // missing from it, and a Miniserver that sends one would otherwise get
        // a topic with a control character embedded in it.
        const PY_SPACE: &[char] = &[
            '\u{9}', '\u{a}', '\u{b}', '\u{c}', '\u{d}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}',
            '\u{20}', '\u{85}', '\u{a0}', '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}',
            '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}',
            '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}',
        ];
        for c in PY_SPACE {
            assert!(is_py_space(*c), "U+{:04X} splits in Python", *c as u32);
            assert_eq!(
                parse(&format!("publish{c}topic{c}payload")),
                expect("publish", "topic", "payload"),
                "U+{:04X} should separate",
                *c as u32
            );
        }
        // Not whitespace to Python either, so it stays inside the payload.
        assert_eq!(
            parse("publish topic pay\u{200b}load"),
            expect("publish", "topic", "pay\u{200b}load")
        );
    }

    #[test]
    fn a_topic_may_contain_spaces_between_slashed_tokens() {
        // The case the greedy rule exists for: a Loxone control named with
        // spaces, unquoted, followed by its value.
        assert_eq!(
            parse("zigbee2mqtt/Rollo Gallerie links/set 100"),
            expect("publish", "zigbee2mqtt/Rollo Gallerie links/set", "100")
        );
        assert_eq!(parse("a/b c/d e f"), expect("publish", "a/b c/d", "e f"));
    }

    #[test]
    fn a_brace_starts_the_payload() {
        assert_eq!(
            parse(r#"publish test/complex topic {"key":"value"}"#),
            expect("publish", "test/complex topic", r#"{"key":"value"}"#)
        );
        assert_eq!(
            parse(r#"publish test/topic {"key":"value"}"#),
            expect("publish", "test/topic", r#"{"key":"value"}"#)
        );
        assert_eq!(
            parse(r#"publish test/complex/topic {"key": "value"}"#),
            expect("publish", "test/complex/topic", r#"{"key": "value"}"#)
        );
        assert_eq!(
            parse(r#"publish test/topic {"key": "value with spaces"}"#),
            expect("publish", "test/topic", r#"{"key": "value with spaces"}"#)
        );
        assert_eq!(
            parse(r#"publish test/complex topic {"key": "value"}"#),
            expect("publish", "test/complex topic", r#"{"key": "value"}"#)
        );
        assert_eq!(
            parse(r#"retain test/complex/topic {"number":42}"#),
            expect("retain", "test/complex/topic", r#"{"number":42}"#)
        );
        assert_eq!(
            parse(r#"retain test/complex topic {"number":42}"#),
            expect("retain", "test/complex topic", r#"{"number":42}"#)
        );
        assert_eq!(
            parse(r#"retain test/topic {"number":42}"#),
            expect("retain", "test/topic", r#"{"number":42}"#)
        );
        assert_eq!(
            parse(r#"a/b/c/d/set {"action":"toggle"}"#),
            expect("publish", "a/b/c/d/set", r#"{"action":"toggle"}"#)
        );
        assert_eq!(
            parse(r#"a/b c d/set {"action":"toggle"}"#),
            expect("publish", "a/b c d/set", r#"{"action":"toggle"}"#)
        );
        assert_eq!(
            parse(r#"publish Home/Automation/Light Control {"mode": "auto on"}"#),
            expect(
                "publish",
                "Home/Automation/Light Control",
                r#"{"mode": "auto on"}"#
            )
        );
        assert_eq!(
            parse(r#"publish Home/Automation/Light/Control {"mode": "auto on"}"#),
            expect(
                "publish",
                "Home/Automation/Light/Control",
                r#"{"mode": "auto on"}"#
            )
        );
    }

    #[test]
    fn a_brace_with_nothing_before_it_is_rejected() {
        assert_eq!(parse(r#"publish {"key":"value"}"#), None);
        assert_eq!(parse(r#"{"key":"value"}"#), None);
    }

    // -----------------------------------------------------------------------
    // Parser: the '[key=value]' block
    // -----------------------------------------------------------------------

    #[test]
    fn user_properties_are_taken_from_a_leading_block() {
        assert_eq!(
            parse("publish [source=loxone] home/light on"),
            Some((
                "publish",
                "home/light".to_owned(),
                "on".to_owned(),
                props(&[("source", "loxone")])
            ))
        );
        assert_eq!(
            parse("publish [source=loxone;room=kitchen] home/light on"),
            Some((
                "publish",
                "home/light".to_owned(),
                "on".to_owned(),
                props(&[("source", "loxone"), ("room", "kitchen")])
            ))
        );
        assert_eq!(
            parse("[unit=celsius] home/temp 22.5"),
            Some((
                "publish",
                "home/temp".to_owned(),
                "22.5".to_owned(),
                props(&[("unit", "celsius")])
            ))
        );
        assert_eq!(
            parse("retain [origin=ms1] home/status online"),
            Some((
                "retain",
                "home/status".to_owned(),
                "online".to_owned(),
                props(&[("origin", "ms1")])
            ))
        );
    }

    #[test]
    fn a_property_value_may_be_empty_or_contain_equals_and_spaces() {
        assert_eq!(
            parse("publish [flag=] home/light on"),
            Some((
                "publish",
                "home/light".to_owned(),
                "on".to_owned(),
                props(&[("flag", "")])
            ))
        );
        assert_eq!(
            parse("publish [token=a=b=c] home/light on"),
            Some((
                "publish",
                "home/light".to_owned(),
                "on".to_owned(),
                props(&[("token", "a=b=c")])
            ))
        );
        assert_eq!(
            parse("publish [note=hello world] home/light on"),
            Some((
                "publish",
                "home/light".to_owned(),
                "on".to_owned(),
                props(&[("note", "hello world")])
            ))
        );
    }

    #[test]
    fn duplicate_property_keys_are_kept() {
        assert_eq!(
            parse("publish [tag=a;tag=b] home/light on"),
            Some((
                "publish",
                "home/light".to_owned(),
                "on".to_owned(),
                props(&[("tag", "a"), ("tag", "b")])
            ))
        );
    }

    #[test]
    fn a_property_block_combines_with_a_json_payload() {
        assert_eq!(
            parse(r#"publish [source=loxone] home/thermostat {"mode": "heat"}"#),
            Some((
                "publish",
                "home/thermostat".to_owned(),
                r#"{"mode": "heat"}"#.to_owned(),
                props(&[("source", "loxone")])
            ))
        );
    }

    #[test]
    fn a_block_without_a_valid_pair_stays_part_of_the_topic() {
        // Nothing in the block parses, so it was never a property block - the
        // brackets belong to whoever sent them.
        assert_eq!(
            parse("publish [foo] home/light on"),
            expect("publish", "[foo] home/light", "on")
        );
        assert_eq!(
            parse("publish [] home/light on"),
            expect("publish", "[] home/light", "on")
        );
        assert_eq!(
            parse("publish [=x] home/light on"),
            expect("publish", "[=x] home/light", "on")
        );
        // A whitespace-only block leaves '[' and ']' as separate tokens, and the
        // greedy rule then stops at the very first one.
        assert_eq!(
            parse("publish [   ] home/light on"),
            expect("publish", "[", "] home/light on")
        );
    }

    #[test]
    fn invalid_segments_are_skipped_and_the_valid_ones_kept() {
        assert_eq!(
            parse("publish [foo;room=kitchen] home/light on"),
            Some((
                "publish",
                "home/light".to_owned(),
                "on".to_owned(),
                props(&[("room", "kitchen")])
            ))
        );
    }

    #[test]
    fn a_property_block_with_nothing_after_it_is_not_a_property_block() {
        // Falls back to treating the brackets as text, which then fails to
        // produce a topic and a payload.
        assert_eq!(parse("publish [a=b]"), None);
    }

    // -----------------------------------------------------------------------
    // Decoding
    // -----------------------------------------------------------------------

    #[test]
    fn invalid_utf8_is_dropped_rather_than_replaced() {
        // What `errors='ignore'` does: the bad byte disappears, and no U+FFFD
        // is invented in its place.
        assert_eq!(decode_utf8_ignore(b"topic v\xffalue"), "topic value");
        assert_eq!(decode_utf8_ignore(b"\xff\xfe"), "");
        assert_eq!(decode_utf8_ignore("gültig".as_bytes()), "gültig");
        // A sequence cut off at the end is dropped too.
        assert_eq!(decode_utf8_ignore(b"ok\xc3"), "ok");
    }

    #[test]
    fn a_datagram_with_a_broken_byte_still_parses() {
        let msg = decode_utf8_ignore(b"publish home/light o\xffn");
        assert_eq!(parse(&msg), expect("publish", "home/light", "on"));
    }

    // -----------------------------------------------------------------------
    // Address handling
    // -----------------------------------------------------------------------

    #[test]
    fn a_port_is_stripped_from_a_configured_address() {
        assert_eq!(host_part("192.168.1.10"), "192.168.1.10");
        assert_eq!(host_part("192.168.1.10:80"), "192.168.1.10");
        assert_eq!(host_part("miniserver.local:8080"), "miniserver.local");
        assert_eq!(host_part("miniserver.local"), "miniserver.local");
        assert_eq!(host_part("[::1]:80"), "::1");
        assert_eq!(host_part("  192.168.1.10  "), "192.168.1.10");
        // A bare IPv6 address has several colons and no port to strip.
        assert_eq!(host_part("fd00::1"), "fd00::1");
    }

    #[test]
    fn resolve_host_rejects_ipv6() {
        // The listener binds IPv4 only, so an IPv6 literal must not end up in
        // the allowlist looking like a match that can never happen.
        assert_eq!(resolve_host("2606:4700::1"), HashSet::new());
        assert_eq!(resolve_host("::1"), HashSet::new());
        // A resolvable IPv4 address is unaffected.
        assert_eq!(resolve_host("192.168.1.10"), HashSet::from([ip("192.168.1.10")]));
    }

    #[test]
    fn non_public_covers_more_than_rfc1918() {
        for address in [
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.10",
            "127.0.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.10",
        ] {
            assert!(is_non_public(ip(address)), "{address} should be non-public");
        }

        for address in ["84.1.2.3", "8.8.8.8", "172.32.0.1", "192.0.0.9", "2606:4700::1"] {
            assert!(!is_non_public(ip(address)), "{address} should be public");
        }
    }

    #[test]
    fn the_default_gateway_is_read_little_endian() {
        // The line Docker's bridge network produces: destination 00000000,
        // gateway 010011AC = 172.17.0.1.
        let table = "Iface\tDestination\tGateway\tFlags\n\
                     eth0\t00000000\t010011AC\t0003\n\
                     eth0\t000011AC\t00000000\t0001\n";
        assert_eq!(parse_default_gateway(table), Some(ip("172.17.0.1")));

        // No default route at all.
        let table = "Iface\tDestination\tGateway\tFlags\n\
                     eth0\t000011AC\t00000000\t0001\n";
        assert_eq!(parse_default_gateway(table), None);
    }

    // -----------------------------------------------------------------------
    // Source filter
    // -----------------------------------------------------------------------

    fn ip(address: &str) -> IpAddr {
        address.parse().expect("test address must parse")
    }

    fn fixed_resolver(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> HashSet<IpAddr> {
        move |host: &str| {
            pairs
                .iter()
                .filter(|(name, _)| *name == host)
                .map(|(_, addr)| ip(addr))
                .collect()
        }
    }

    fn filter_for(
        miniserver_ip: &str,
        extra: &[String],
        enabled: bool,
        pairs: &'static [(&'static str, &'static str)],
        gateway: Option<&str>,
    ) -> SourceFilter {
        let allowed =
            configure_source_filter(miniserver_ip, extra, enabled, &fixed_resolver(pairs));
        SourceFilter::new(enabled, allowed, gateway.map(ip))
    }

    #[test]
    fn only_the_configured_miniserver_gets_through() {
        let mut filter = filter_for(
            "192.168.1.10",
            &[],
            true,
            &[("192.168.1.10", "192.168.1.10")],
            None,
        );
        assert!(filter.allows(ip("192.168.1.10")));
        assert!(!filter.allows(ip("192.168.1.99")));
    }

    #[test]
    fn additional_allowed_sources_are_accepted() {
        let mut filter = filter_for(
            "192.168.1.10",
            &["192.168.1.50".to_owned()],
            true,
            &[
                ("192.168.1.10", "192.168.1.10"),
                ("192.168.1.50", "192.168.1.50"),
            ],
            None,
        );
        assert!(filter.allows(ip("192.168.1.50")));
        assert!(!filter.allows(ip("192.168.1.51")));
    }

    #[test]
    fn a_hostname_is_matched_through_its_resolved_address() {
        let mut filter = filter_for(
            "miniserver.local",
            &[],
            true,
            &[("miniserver.local", "192.168.1.10")],
            None,
        );
        assert!(filter.allows(ip("192.168.1.10")));
        assert!(!filter.allows(ip("192.168.1.99")));
    }

    #[test]
    fn switching_the_filter_off_accepts_everyone() {
        let mut filter = filter_for(
            "192.168.1.10",
            &[],
            false,
            &[("192.168.1.10", "192.168.1.10")],
            None,
        );
        assert!(filter.allows(ip("203.0.113.7")));
    }

    #[test]
    fn an_unresolvable_address_drops_everything_rather_than_nothing() {
        // The filter was asked for, so a name that will not resolve must not
        // turn into "accept every sender" behind the operator's back.
        let mut filter = filter_for("does-not-resolve", &[], true, &[], None);
        assert!(filter.awaits_addresses());
        assert!(!filter.allows(ip("192.168.1.99")));
    }

    #[test]
    fn a_late_resolution_puts_the_filter_to_work() {
        let mut filter = filter_for("does-not-resolve", &[], true, &[], None);
        assert!(!filter.allows(ip("192.168.1.10")));

        filter.adopt(HashSet::from([ip("192.168.1.10")]));

        assert!(!filter.awaits_addresses());
        assert!(filter.allows(ip("192.168.1.10")));
        assert!(!filter.allows(ip("192.168.1.99")));
    }

    #[test]
    fn a_disabled_filter_never_waits_for_an_address() {
        let filter = filter_for("does-not-resolve", &[], false, &[], None);
        assert!(!filter.awaits_addresses());
    }

    #[test]
    fn a_source_that_resolves_late_is_looked_up_from_the_whole_configuration() {
        let sources = configured_sources("miniserver.local", &["192.168.1.50".to_owned()]);
        assert_eq!(sources, ["miniserver.local", "192.168.1.50"]);

        let resolved = resolve_sources(
            &sources,
            &fixed_resolver(&[
                ("miniserver.local", "192.168.1.10"),
                ("192.168.1.50", "192.168.1.50"),
            ]),
        );
        assert_eq!(
            resolved,
            HashSet::from([ip("192.168.1.10"), ip("192.168.1.50")])
        );
    }

    #[test]
    fn the_container_gateway_is_accepted_despite_the_filter() {
        let mut filter = filter_for(
            "192.168.1.10",
            &[],
            true,
            &[("192.168.1.10", "192.168.1.10")],
            Some("172.17.0.1"),
        );
        assert!(filter.allows(ip("172.17.0.1")));
        assert!(filter.allows(ip("172.17.0.1")));
        assert!(filter.gateway_warning_logged);
        // Everything else is still checked.
        assert!(!filter.allows(ip("192.168.1.99")));
    }

    #[test]
    fn the_rejected_source_bookkeeping_is_bounded() {
        let mut filter = filter_for(
            "192.168.1.10",
            &[],
            true,
            &[("192.168.1.10", "192.168.1.10")],
            None,
        );
        for i in 0..(MAX_TRACKED_REJECTED_SOURCES + 50) {
            assert!(!filter.allows(ip(&format!("10.9.{}.{}", i / 256, i % 256))));
        }
        assert_eq!(filter.rejected.len(), MAX_TRACKED_REJECTED_SOURCES);
    }

    // -----------------------------------------------------------------------
    // Datagram to publish
    // -----------------------------------------------------------------------

    /// A datagram the broker never took must leave a trace naming the sender.
    ///
    /// Nothing retries it - QoS 0, no outbox - so this log line is all that is
    /// left of the command the Miniserver sent. It went missing once already
    /// (#23), when publish reported success while disconnected. One line, not
    /// two: the sender is handed to the publish so the loss is reported once,
    /// with everything in it.
    #[test]
    fn a_datagram_that_never_reaches_the_broker_is_reported() {
        log::set_logger(&Recorder).expect("only one test may install the logger");
        log::set_max_level(log::LevelFilter::Debug);

        // No client was ever set on it, so every publish is dropped.
        let shared = Arc::new(MqttShared::new());
        let addr: SocketAddr = "192.168.1.10:4711".parse().unwrap();
        handle_datagram(&shared, b"retain home/status online", addr);

        // The publish is spawned, so give it a moment to land.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let line = loop {
            let found = RECORDED
                .lock()
                .unwrap()
                .iter()
                .find(|line| line.contains("Dropped MQTT publish"))
                .cloned();
            match found {
                Some(line) => break line,
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                None => panic!("no drop was reported: {:?}", RECORDED.lock().unwrap()),
            }
        };

        assert!(line.starts_with("WARN "), "must be visible by default: {line}");
        for expected in [
            "192.168.1.10:4711",
            "broker not connected",
            "'home/status'='online'",
        ] {
            assert!(line.contains(expected), "{expected:?} missing from {line:?}");
        }

        // And exactly once: the loss used to be announced by the publish and
        // again by the caller, so an operator had to correlate two lines.
        let reported = RECORDED
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with("WARN ") && line.contains("home/status"))
            .count();
        assert_eq!(reported, 1, "the loss must be reported once, not twice");
    }

    #[test]
    fn a_port_on_the_miniserver_address_does_not_break_the_filter() {
        let mut filter = filter_for(
            "192.168.1.10:80",
            &[],
            true,
            &[("192.168.1.10", "192.168.1.10")],
            None,
        );
        assert!(filter.allows(ip("192.168.1.10")));
    }
}

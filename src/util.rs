//! Habits the whole crate shares: how a poisoned lock is answered, how foreign
//! text is put into a log line, and one definition of whitespace that differs
//! from Rust's default in a way that is observable.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard};

/// Whitespace, as the UDP message format and the configuration file define it.
///
/// Unicode's White_Space property - what `char::is_whitespace` answers - leaves
/// out the four ASCII separators U+001C..U+001F, and both formats count them.
/// The difference is observable twice: which bytes a datagram is split on, and
/// whether a `base_topic` made of them reads as blank.
pub(crate) fn is_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// Trim [`is_space`] from both ends.
pub(crate) fn strip_space(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// How much of a foreign string may reach a log line.
///
/// A UDP datagram carries up to 64 KiB and a config response is the whole
/// configuration; either would push everything else out of a log file.
pub(crate) const LOG_VALUE_LIMIT: usize = 256;

/// Lock, and carry on with the data that is there if a previous holder panicked.
///
/// Every mutex in this crate guards a cache or a diagnostic ring. Panicking on
/// a poisoned one would take the relay down over a stale cache entry, and
/// silently handing back nothing would turn the poisoning into missing data -
/// in the case of the shape cache into plans that keep applying filter verdicts
/// which have since been replaced.
pub(crate) fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A topic, payload or broker message as it may appear in a log line.
///
/// Control characters are escaped and the text is cut at [`LOG_VALUE_LIMIT`].
/// Both matter because all of it comes from outside: a payload with a newline
/// and a fake timestamp in it would otherwise read as a log line of its own.
pub(crate) fn loggable(text: &str) -> Cow<'_, str> {
    let head = head_of(text, LOG_VALUE_LIMIT);
    let truncated = head.len() < text.len();
    if !truncated && !head.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(head.len() + 24);
    for c in head.chars() {
        push_escaped(&mut out, c);
    }
    if truncated {
        let _ = write!(out, "... ({} bytes total)", text.len());
    }
    Cow::Owned(out)
}

/// [`loggable`] for a payload that need not be UTF-8.
pub(crate) fn loggable_bytes(bytes: &[u8]) -> Cow<'_, str> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return loggable(text);
    }
    let head = &bytes[..bytes.len().min(LOG_VALUE_LIMIT)];
    let mut out = String::with_capacity(head.len() + 24);
    for &b in head {
        // Binary, so the bytes are shown as bytes rather than decoded into
        // characters the sender never wrote.
        if b.is_ascii_graphic() || b == b' ' {
            out.push(b as char);
        } else {
            let _ = write!(out, "\\x{b:02x}");
        }
    }
    if head.len() < bytes.len() {
        let _ = write!(out, "... ({} bytes total)", bytes.len());
    }
    Cow::Owned(out)
}

/// At most `limit` bytes, cut on a character boundary.
fn head_of(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn push_escaped(out: &mut String, c: char) {
    match c {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        c if c.is_control() => {
            let _ = write!(out, "\\x{:02x}", c as u32);
        }
        c => out.push(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harmless_text_is_handed_back_untouched() {
        assert!(matches!(loggable("home/light on"), Cow::Borrowed(_)));
        assert_eq!(loggable("Rollo Gallerie links/set"), "Rollo Gallerie links/set");
        // Non-ASCII is not a control character and stays readable.
        assert_eq!(loggable("gültig"), "gültig");
    }

    #[test]
    fn control_characters_cannot_forge_a_log_line() {
        assert_eq!(
            loggable("on\nINFO [x] all good"),
            "on\\nINFO [x] all good"
        );
        assert_eq!(loggable("a\r\nb"), "a\\r\\nb");
        assert_eq!(loggable("esc\u{1b}[2Kx"), "esc\\x1b[2Kx");
        // C1 range counts too - a terminal acts on those as well.
        assert_eq!(loggable("a\u{85}b"), "a\\x85b");
    }

    #[test]
    fn long_values_are_cut_and_say_so() {
        let long = "x".repeat(LOG_VALUE_LIMIT + 10);
        let cut = loggable(&long);
        assert!(cut.starts_with(&"x".repeat(LOG_VALUE_LIMIT)));
        assert!(cut.ends_with(&format!("... ({} bytes total)", long.len())));
    }

    #[test]
    fn a_multibyte_character_is_not_cut_in_half() {
        // 'ä' is two bytes, so the limit falls inside the last one.
        let long = "ä".repeat(LOG_VALUE_LIMIT);
        let cut = loggable(&long);
        assert!(cut.starts_with(&"ä".repeat(LOG_VALUE_LIMIT / 2)));
    }

    #[test]
    fn binary_payloads_are_shown_as_bytes() {
        assert_eq!(loggable_bytes(b"on"), "on");
        assert_eq!(loggable_bytes(b"\xff\x00ok"), "\\xff\\x00ok");
    }
}

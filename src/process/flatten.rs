//! The DOM route, and the value helpers both routes share.
//!
//! This is the slow but always-correct half of the flattening: build a
//! `serde_json::Value`, walk it, and hand back every `(target topic, value)`
//! pair it yields. Every document the plan route in [`super::shape`] refuses
//! ends up here, so this is also the definition the plan route is checked
//! against.

use std::borrow::Cow;

use serde_json::Value;

/// Convert a known boolean string to "1"/"0", or None if unrecognized.
pub(crate) fn convert_boolean_str(input: &str) -> Option<&'static str> {
    match input {
        "true" | "yes" | "on" | "enabled" | "enable" | "1" | "check" | "checked" | "select"
        | "selected" => Some("1"),
        "false" | "no" | "off" | "disabled" | "disable" | "0" => Some("0"),
        _ => None,
    }
}

/// Apply the boolean mapping without a cache.
///
/// Trim, lowercase, look up the keyword table, hand back the original when
/// nothing matches. Called per leaf on both routes, where a mutex and two
/// allocations per value would eat most of what the plan route saves.
///
/// Short ASCII values lowercase into a stack buffer; everything else falls
/// through to the allocating route. The buffer size is a speed knob only: an
/// oversized value takes the slow path, it is never declared a non-match, so
/// the constant cannot silently disagree with the keyword table.
pub(crate) fn convert_bool_value(val: &str) -> Cow<'_, str> {
    if val.is_empty() {
        return Cow::Borrowed(val);
    }
    let trimmed = val.trim();
    const BUF: usize = 16;
    if trimmed.is_ascii() && trimmed.len() <= BUF {
        let mut buf = [0u8; BUF];
        let bytes = trimmed.as_bytes();
        for (slot, b) in buf.iter_mut().zip(bytes) {
            *slot = b.to_ascii_lowercase();
        }
        if let Ok(s) = std::str::from_utf8(&buf[..bytes.len()])
            && let Some(mapped) = convert_boolean_str(s)
        {
            return Cow::Borrowed(mapped);
        }
        return Cow::Borrowed(val);
    }
    match convert_boolean_str(&trimmed.to_lowercase()) {
        Some(mapped) => Cow::Borrowed(mapped),
        None => Cow::Borrowed(val),
    }
}

/// Render a JSON number the way `serde_json::Value` would.
///
/// Returns `None` for anything whose canonical form is not obvious from the
/// source text (leading zeros, `-0`, integers too wide for i64/u64). The caller
/// then falls back to the DOM path, so a miss costs speed but never accuracy.
pub(crate) fn number_value(text: &str) -> Option<Cow<'_, str>> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let is_integer = !bytes.iter().any(|&c| c == b'.' || c == b'e' || c == b'E');
    if is_integer {
        let (negative, digits) = match bytes.split_first() {
            Some((b'-', rest)) => (true, rest),
            _ => (false, bytes),
        };
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        // Leading zeros are invalid JSON and "-0" round-trips as "-0.0".
        if (digits.len() > 1 && digits[0] == b'0') || (negative && digits == b"0") {
            return None;
        }
        let fits = if negative {
            text.parse::<i64>().is_ok()
        } else {
            text.parse::<u64>().is_ok()
        };
        if fits {
            return Some(Cow::Borrowed(text));
        }
        // Too wide for an integer: serde_json widens to f64, so do the same.
    }
    let parsed: f64 = text.parse().ok()?;
    // Number::from_f64 rejects inf/NaN and is the very serializer the DOM path
    // uses, so the rendering cannot drift apart.
    Some(Cow::Owned(serde_json::Number::from_f64(parsed)?.to_string()))
}

/// `topic.replace('/', "_").replace('%', "_")` in one pass and one allocation.
///
/// Both separators and their replacement are ASCII, so the result is exactly as
/// long as the input and the segments between them can be copied wholesale. The
/// obvious `chars().map().collect()` cannot know that: `Chars::size_hint` only
/// promises a quarter of the byte length, so it reserves too little and grows
/// the string again while filling it.
pub(crate) fn normalize_topic_str(topic: &str) -> String {
    if !topic.as_bytes().iter().any(|&c| c == b'/' || c == b'%') {
        return topic.to_string();
    }
    let mut out = String::with_capacity(topic.len());
    let mut rest = topic;
    while let Some(at) = rest.find(['/', '%']) {
        out.push_str(&rest[..at]);
        out.push('_');
        rest = &rest[at + 1..];
    }
    out.push_str(rest);
    out
}

/// Flatten a serde_json `Value` into `key/value` pairs using '/' as separator.
fn flatten_json(obj: &Value, prefix: &str, acc: &mut Vec<(String, String)>) {
    match obj {
        Value::Object(map) => {
            for (k, v) in map {
                let new_key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}/{k}")
                };
                push_leaf(v, new_key, acc);
            }
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                let new_key = if prefix.is_empty() {
                    i.to_string()
                } else {
                    format!("{prefix}/{i}")
                };
                push_leaf(item, new_key, acc);
            }
        }
        _ => {}
    }
}

fn push_leaf(value: &Value, key: String, acc: &mut Vec<(String, String)>) {
    match value {
        Value::Object(_) | Value::Array(_) => flatten_json(value, &key, acc),
        Value::String(s) => acc.push((key, s.clone())),
        Value::Number(num) => acc.push((key, num.to_string())),
        Value::Bool(b) => acc.push((key, b.to_string())),
        Value::Null => acc.push((key, "null".to_string())),
    }
}

/// Every `(target topic, value)` pair a message yields, filters not applied.
///
/// A payload that is not a JSON object - or any payload at all with
/// `expand_json` off - is forwarded whole under the topic it arrived on.
pub(crate) fn dom_targets(topic: &str, message: &str, expand_json: bool) -> Vec<(String, String)> {
    if !expand_json {
        return vec![(topic.to_string(), message.to_string())];
    }
    match serde_json::from_str::<Value>(message) {
        Ok(json_val) if json_val.is_object() => {
            let mut flat = Vec::new();
            flatten_json(&json_val, "", &mut flat);
            flat.into_iter()
                .map(|(k, v)| (format!("{topic}/{k}"), v))
                .collect()
        }
        _ => vec![(topic.to_string(), message.to_string())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyword_table_maps_both_ways() {
        for on in [
            "true", "yes", "on", "enabled", "enable", "1", "check", "checked", "select", "selected",
        ] {
            assert_eq!(convert_bool_value(on), "1", "{on}");
        }
        for off in ["false", "no", "off", "disabled", "disable", "0"] {
            assert_eq!(convert_bool_value(off), "0", "{off}");
        }
    }

    #[test]
    fn an_unknown_value_is_handed_back_untouched() {
        assert_eq!(convert_bool_value("23.5"), "23.5");
        assert_eq!(convert_bool_value("Rollo"), "Rollo");
        assert_eq!(convert_bool_value(""), "");
    }

    /// Case and surrounding whitespace are normalized away, but only for the
    /// lookup - a non-match keeps the value exactly as it arrived.
    #[test]
    fn case_and_whitespace_do_not_hide_a_keyword() {
        assert_eq!(convert_bool_value(" TRUE "), "1");
        assert_eq!(convert_bool_value("\tOff\n"), "0");
        assert_eq!(convert_bool_value("  Rollo  "), "  Rollo  ");
    }

    /// A value longer than the stack buffer must take the allocating route
    /// rather than being declared a non-match.
    #[test]
    fn a_long_keyword_still_matches() {
        // 8 chars, but with padding it exceeds the 16-byte buffer.
        let padded = format!("{:>20}", "selected");
        assert_eq!(convert_bool_value(&padded), "1");
    }

    #[test]
    fn separators_become_underscores() {
        assert_eq!(normalize_topic_str("dev/a%b/temp"), "dev_a_b_temp");
        assert_eq!(normalize_topic_str("plain"), "plain");
        assert_eq!(normalize_topic_str("//"), "__");
    }

    #[test]
    fn numbers_render_the_way_serde_would() {
        assert_eq!(number_value("42").as_deref(), Some("42"));
        assert_eq!(number_value("-7").as_deref(), Some("-7"));
        assert_eq!(number_value("12.5").as_deref(), Some("12.5"));
        // Refused, so the DOM route decides: it would render these differently.
        assert!(number_value("007").is_none());
        assert!(number_value("-0").is_none());
        assert!(number_value("nope").is_none());
    }

    #[test]
    fn nested_json_flattens_onto_slash_separated_topics() {
        let flat = dom_targets("dev/x", r#"{"a":{"b":1},"c":[true,"s"]}"#, true);
        assert_eq!(
            flat,
            vec![
                ("dev/x/a/b".to_string(), "1".to_string()),
                ("dev/x/c/0".to_string(), "true".to_string()),
                ("dev/x/c/1".to_string(), "s".to_string()),
            ]
        );
    }

    #[test]
    fn a_non_object_payload_is_forwarded_whole() {
        assert_eq!(
            dom_targets("dev/x", "just text", true),
            vec![("dev/x".to_string(), "just text".to_string())]
        );
        // A JSON array is not an object either, so it stays as it stands.
        assert_eq!(
            dom_targets("dev/x", "[1,2]", true),
            vec![("dev/x".to_string(), "[1,2]".to_string())]
        );
    }

    #[test]
    fn expand_json_off_forwards_the_raw_message() {
        let raw = r#"{"a":1}"#;
        assert_eq!(
            dom_targets("dev/x", raw, false),
            vec![("dev/x".to_string(), raw.to_string())]
        );
    }
}

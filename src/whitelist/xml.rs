//! The virtual input names, out of the configuration XML.
//!
//! One fixed query is ever run against this document:
//! `//C[@Type='VirtualInCaption']//C/@Title`. That is small enough to scan for
//! rather than parse into a tree, which matters twice over. A 2.5 MB
//! configuration would become an order of magnitude more memory as a DOM, and -
//! more importantly - a scanner can stop at the first thing it does not
//! understand and keep what it already has. lxml read these files with
//! `recover=True` for exactly that reason: Miniserver configurations turn up
//! with duplicate attributes and unclosed elements, and a strict parser that
//! refuses the whole document would leave the relay with no whitelist at all.
//!
//! # Which Python did this replace?
//!
//! There were two implementations, and they did not agree. `extract_inputs`
//! used pygixml and a real XPath; `_extract_inputs_lxml` walked the tree by hand
//! and, on *nested* `VirtualInCaption` elements, emitted the inner titles twice.
//! The existing test compared `set(inputs)`, so the difference never showed.
//! pygixml is what ran in production and in the image, so the XPath node-set
//! semantics are what is reproduced here: every `C` below any
//! `VirtualInCaption` contributes its `Title` exactly once, in document order.

use log::warn;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use super::SyncError;

/// An element that is currently open.
struct Frame {
    name: Box<[u8]>,
    /// Whether this element is itself a `VirtualInCaption`.
    vic: bool,
}

/// Every virtual input name the configuration declares, in document order.
pub(crate) fn extract_inputs(config_xml: &[u8]) -> Result<Vec<String>, SyncError> {
    // The declared encoding is deliberately ignored, because the Python path
    // ignored it: `config_xml.decode("utf-8", errors="replace")` is exactly
    // this. A leading BOM - which the real configurations carry - arrives as a
    // text event before the root and is skipped like any other text.
    let text = String::from_utf8_lossy(config_xml);
    let mut reader = Reader::from_str(&text);
    let config = reader.config_mut();
    // Recovery, spelled out. A mismatched or stray end tag must not abandon the
    // document, and a bare '&' in a title must not either.
    config.check_end_names = false;
    config.allow_unmatched_ends = true;
    config.check_comments = false;
    // Empty elements are handled explicitly below; expanding them into a
    // start/end pair would push them onto the stack for no reason.
    config.expand_empty_elements = false;

    let mut open: Vec<Frame> = Vec::with_capacity(32);
    let mut vic_open = 0usize;
    let mut saw_element = false;
    let mut titles: Vec<String> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                saw_element = true;
                let is_c = e.name().as_ref() == b"C";
                // Emit before pushing this element's own flag. That is what
                // makes `//C[...]//C` a strict descendant axis: a
                // VirtualInCaption never contributes its own Title, only its
                // descendants'.
                if is_c && vic_open > 0 {
                    push_title(&mut titles, &e);
                }
                let vic = is_c && attribute(&e, b"Type").as_deref() == Some("VirtualInCaption");
                if vic {
                    vic_open += 1;
                }
                open.push(Frame {
                    name: e.name().as_ref().into(),
                    vic,
                });
            }
            Ok(Event::Empty(e)) => {
                saw_element = true;
                // No descendants, so it never enters the stack - even when it is
                // itself a VirtualInCaption.
                if e.name().as_ref() == b"C" && vic_open > 0 {
                    push_title(&mut titles, &e);
                }
            }
            Ok(Event::End(e)) => {
                // Close the innermost element of that name and discard whatever
                // was left open inside it. This is the shape libxml2's recovery
                // takes, and it is what lets an unclosed element cost only its
                // own subtree rather than the rest of the file.
                let name = e.name();
                if let Some(at) = open.iter().rposition(|frame| *frame.name == *name.as_ref()) {
                    vic_open -= open[at..].iter().filter(|frame| frame.vic).count();
                    open.truncate(at);
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                // Salvage, as `recover=True` did. Whatever was read before this
                // point is still a valid part of the whitelist, and refusing all
                // of it would fail closed and stop every forward.
                warn!(
                    "Configuration XML could not be read past byte {}: {e}; \
                     keeping the {} input(s) found so far",
                    reader.buffer_position(),
                    titles.len()
                );
                break;
            }
        }
    }

    // A scanner would happily report "no inputs" for a document that is not XML
    // at all - a Miniserver error page, say, or the JSON body it answers some
    // requests with. Both Python parsers raised there, and the distinction is
    // worth keeping: no inputs found is a configuration question, not a
    // document that was never parsed.
    if !saw_element {
        return Err(SyncError::Xml(
            "the configuration has no XML document element".to_owned(),
        ));
    }
    Ok(titles)
}

fn push_title(titles: &mut Vec<String>, element: &BytesStart<'_>) {
    if let Some(title) = attribute(element, b"Title")
        && !title.is_empty()
    {
        // Python tested `if title:`, so a title of one space is kept and only an
        // empty one is dropped.
        titles.push(title);
    }
}

/// One attribute's value, normalized the way an XML parser normalizes it.
///
/// Two things quick-xml leaves to the caller that both libxml2 and pugixml do
/// themselves, and that are therefore parity requirements rather than polish:
///
/// * A repeated attribute is not an error, and the *first* one wins. quick-xml's
///   attribute iterator checks for duplicates by default and fails the element;
///   lxml's `.get()` and pugixml's `.attribute()` both simply take the first.
/// * A literal tab, newline or carriage return inside an attribute value becomes
///   a single space - and a CRLF becomes one space, not two - while the same
///   character written as `&#10;` survives. So the normalization happens on the
///   raw text, before unescaping.
fn attribute(element: &BytesStart<'_>, wanted: &[u8]) -> Option<String> {
    let mut attributes = element.attributes();
    attributes.with_checks(false);
    for attribute in attributes.flatten() {
        if attribute.key.as_ref() != wanted {
            continue;
        }
        let raw = String::from_utf8_lossy(&attribute.value);
        let normalized = normalize_whitespace(&raw);
        return Some(match quick_xml::escape::unescape(&normalized) {
            Ok(value) => value.into_owned(),
            // An entity we do not know is not worth losing the input over; the
            // recovery parsers left such text as it stood.
            Err(_) => normalized.into_owned(),
        });
    }
    None
}

/// `\r\n`, `\r`, `\n` and `\t` each become one space.
fn normalize_whitespace(raw: &str) -> std::borrow::Cow<'_, str> {
    if !raw.contains(['\r', '\n', '\t']) {
        return std::borrow::Cow::Borrowed(raw);
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                // CRLF is one line break and therefore one space.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(' ');
            }
            '\n' | '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(xml: &str) -> Vec<String> {
        extract_inputs(xml.as_bytes()).expect("a document")
    }

    #[test]
    fn titles_come_from_the_descendants_of_a_virtual_in_caption() {
        let xml = r#"
            <ControlList>
              <C Type="Other"><C Title="ignored"/></C>
              <C Type="VirtualInCaption">
                <C Title="Input1"/>
                <C Title="Input2"/>
                <C Type="Other"/>
                <C Title="Input3"/>
              </C>
            </ControlList>"#;
        assert_eq!(inputs(xml), ["Input1", "Input2", "Input3"]);
    }

    #[test]
    fn a_config_without_virtual_inputs_yields_nothing() {
        assert!(inputs("<ControlList><C Title=\"x\"/></ControlList>").is_empty());
    }

    /// A `VirtualInCaption` does not contribute its own `Title`, only its
    /// descendants'. The axis is `//C[...]//C`, not `//C[...]`.
    #[test]
    fn a_virtual_in_caption_does_not_contribute_its_own_title() {
        let xml = r#"<R><C Type="VirtualInCaption" Title="self"><C Title="child"/></C></R>"#;
        assert_eq!(inputs(xml), ["child"]);
    }

    /// The test that pins which Python implementation this reproduces.
    ///
    /// pygixml - the one that actually ran - yields each title once. The lxml
    /// fallback walked the tree recursively and emitted the inner ones twice
    /// (`["A", "B", "B"]`). Both were verified against the real modules before
    /// this was written.
    #[test]
    fn a_nested_virtual_in_caption_does_not_repeat_its_titles() {
        let xml = r#"<R>
            <C Type="VirtualInCaption">
              <C Title="A"/>
              <C Type="VirtualInCaption"><C Title="B"/></C>
            </C></R>"#;
        assert_eq!(inputs(xml), ["A", "B"]);
    }

    #[test]
    fn a_repeated_attribute_keeps_the_first() {
        let xml = r#"<R><C Type="VirtualInCaption"><C Title="first" Title="second"/></C></R>"#;
        assert_eq!(inputs(xml), ["first"]);

        // Also on the Type that opens the block: a duplicate there must not
        // make the element stop counting as a VirtualInCaption.
        let xml = r#"<R><C Type="VirtualInCaption" Type="Other"><C Title="x"/></C></R>"#;
        assert_eq!(inputs(xml), ["x"]);
    }

    /// An unclosed element costs its own subtree and nothing more.
    #[test]
    fn an_unclosed_element_still_yields_its_siblings() {
        let xml = r#"<R><C Type="VirtualInCaption">
            <C Title="Input1">
            <C Title="Input2"/>
        </C></R>"#;
        assert_eq!(inputs(xml), ["Input1", "Input2"]);
    }

    #[test]
    fn a_stray_end_tag_is_ignored() {
        let xml = r#"<R></Nonsense><C Type="VirtualInCaption"><C Title="x"/></C></R>"#;
        assert_eq!(inputs(xml), ["x"]);
    }

    /// The real configurations start with a UTF-8 BOM and use CRLF.
    #[test]
    fn a_byte_order_mark_is_not_content() {
        let xml = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n\
                   <R><C Type=\"VirtualInCaption\"><C Title=\"InputWithBOM\"/></C></R>";
        assert_eq!(inputs(xml), ["InputWithBOM"]);
    }

    /// Verified against pygixml and lxml, both of which agree on every row.
    #[test]
    fn attribute_values_are_normalized_the_way_the_parsers_normalize_them() {
        let one = |title: &str| {
            let xml = format!(r#"<R><C Type="VirtualInCaption"><C Title="{title}"/></C></R>"#);
            inputs(&xml).into_iter().next().expect("a title")
        };
        // A literal separator becomes one space...
        assert_eq!(one("a\tb"), "a b");
        assert_eq!(one("a\nb"), "a b");
        assert_eq!(one("a\r\nb"), "a b", "CRLF is one line break, so one space");
        assert_eq!(one("a\rb"), "a b");
        // ...but the same character written as a reference survives.
        assert_eq!(one("a&#10;b"), "a\nb");
        assert_eq!(one("a&amp;b"), "a&b");
        assert_eq!(one("a&lt;b"), "a<b");
    }

    /// Python tested `if title:`, so only a genuinely empty one is dropped.
    #[test]
    fn a_title_of_one_space_is_kept_but_an_empty_one_is_not() {
        let xml = r#"<R><C Type="VirtualInCaption"><C Title=""/><C Title=" "/></C></R>"#;
        assert_eq!(inputs(xml), [" "]);
    }

    #[test]
    fn titles_are_taken_literally() {
        let xml = r#"<R><C Type="VirtualInCaption">
            <C Title="Küche/Licht"/><C Title="a b_c"/><C Title="100%"/>
        </C></R>"#;
        assert_eq!(inputs(xml), ["Küche/Licht", "a b_c", "100%"]);
    }

    /// Not-XML is an error, not an empty whitelist.
    ///
    /// A scanner would otherwise report "no inputs" for the JSON error body the
    /// Miniserver answers some requests with, and an empty whitelist is
    /// fail-closed: it stops every forward.
    #[test]
    fn a_payload_that_is_not_xml_is_an_error() {
        assert!(extract_inputs(b"Invalid XML").is_err());
        assert!(extract_inputs(br#"{"LL":{"control":"dev/fsget","Code":"403"}}"#).is_err());
        assert!(extract_inputs(b"").is_err());
    }

    /// A document cut off mid-element keeps everything read before the cut.
    #[test]
    fn truncated_xml_keeps_what_was_read() {
        let xml = r#"<R><C Type="VirtualInCaption"><C Title="Input1"/><C Title="Inp"#;
        assert_eq!(inputs(xml), ["Input1"]);
    }

    /// `errors="replace"` in Python, `from_utf8_lossy` here.
    #[test]
    fn a_non_utf8_byte_becomes_a_replacement_character() {
        let mut xml = br#"<R><C Type="VirtualInCaption"><C Title="a"#.to_vec();
        xml.push(0xff);
        xml.extend_from_slice(br#"b"/></C></R>"#);
        assert_eq!(extract_inputs(&xml).unwrap(), ["a\u{fffd}b"]);
    }
}

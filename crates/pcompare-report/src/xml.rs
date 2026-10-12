//! XML output for text and folder reports, meant for scripts and other tools
//! to read. Both documents are UTF-8, use no namespace and carry
//! `version="1"` on the root element; attributes that don't apply are left
//! out rather than written empty.
//!
//! Text comparison (`TextReportFormat::Xml`):
//!
//! ```xml
//! <textReport version="1" generator="PCompare" title="…" produced="…">
//!   <options layout="sideBySide" ignoreUnimportant="true" context="3" differencesOnly="false"/>
//!   <comparison name="…">            <!-- one per file pair; name only in multi-file reports -->
//!     <left path="…" lines="8"/>
//!     <right path="…" lines="9"/>
//!     <summary differences="2" changed="1" leftOnly="0" rightOnly="1" unimportant="0"/>
//!     <!-- one per run of differing lines; kind is changed | leftOnly | rightOnly | unimportant.
//!          A side with no lines (count 0) gives the line its text would follow, as in a patch. -->
//!     <difference index="1" kind="changed" leftStart="3" leftCount="1" rightStart="3" rightCount="1">
//!       <line side="left" n="3">three</line>
//!       <line side="right" n="3">THREE!</line>
//!     </difference>
//!     <bookmark side="left" line="3">optional comment</bookmark>
//!   </comparison>
//! </textReport>
//! ```
//!
//! Folder comparison (`FolderReportFormat::Xml`): one `<entry>` per listed row
//! in tree order (`depth` gives the nesting), with the side's metadata when
//! it exists, the bookmark comment, and — when saved with per-file reports —
//! the file's text comparison as a `<comparison>` element shaped like the
//! one above (or `<comparison skipped="reason"/>`):
//!
//! ```xml
//! <folderReport version="1" generator="PCompare" title="…" produced="…">
//!   <left root="…"/>
//!   <right root="…"/>
//!   <options layout="detailed" filter="all" nameFilter="…"/>
//!   <summary same="1" similar="0" different="1" leftOnly="1" rightOnly="1" errors="0" ignored="0"/>
//!   <entries count="5">
//!     <entry path="src/b.rs" name="b.rs" type="file" depth="1" status="different"
//!            label="Left newer" newer="left" ignored="true" leftPath="…" error="…">
//!       <left size="1200" modified="2026-01-01 10:00:00"/>
//!       <right size="1100"/>
//!       <comment>…</comment>
//!       <comparison>…</comparison>
//!     </entry>
//!   </entries>
//! </folderReport>
//! ```
//!
//! `status` is one of same, similar, different, leftOnly, rightOnly,
//! typeMismatch, error; `label` is the status as the HTML report words it.

/// The XML declaration every report starts with.
pub(crate) const DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";

/// Characters XML 1.0 can't hold at all (most C0 controls, lone surrogates
/// can't occur in a `str`) become U+FFFD.
fn allowed(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || (c >= ' ' && c != '\u{FFFE}' && c != '\u{FFFF}')
}

/// Escapes element content.
pub(crate) fn text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // A bare CR would be normalised away by parsers.
            '\r' => out.push_str("&#13;"),
            c if allowed(c) => out.push(c),
            _ => out.push('\u{FFFD}'),
        }
    }
    out
}

/// Escapes a double-quoted attribute value, keeping tabs and line breaks
/// (which parsers would otherwise turn into spaces).
pub(crate) fn attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            c if allowed(c) => out.push(c),
            _ => out.push('\u{FFFD}'),
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use quick_xml::events::Event;
    use quick_xml::{Reader, XmlVersion};

    /// Parses `doc` completely, failing on malformed markup, unbalanced
    /// tags or unknown entities, and returns every element name in order.
    pub(crate) fn parse(doc: &str) -> Vec<String> {
        let mut reader = Reader::from_str(doc);
        reader.config_mut().check_end_names = true;
        let mut names = Vec::new();
        let mut depth = 0usize;
        let element = |e: &quick_xml::events::BytesStart, names: &mut Vec<String>| {
            for a in e.attributes() {
                a.expect("attribute")
                    .normalized_value(XmlVersion::Explicit1_0)
                    .expect("attribute value");
            }
            names.push(e.name().as_ref().to_owned());
        };
        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    depth += 1;
                    element(&e, &mut names);
                }
                Ok(Event::Empty(e)) => element(&e, &mut names),
                Ok(Event::End(_)) => depth -= 1,
                Ok(Event::GeneralRef(r)) => {
                    if r.is_char_ref() {
                        assert!(r.resolve_char_ref().expect("char ref").is_some());
                    } else {
                        let name: &str = &r;
                        assert!(
                            ["amp", "lt", "gt", "quot", "apos"].contains(&name),
                            "unknown entity &{name};"
                        );
                    }
                }
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("malformed XML at {}: {e}\n{doc}", reader.buffer_position()),
            }
        }
        assert_eq!(depth, 0, "unclosed elements\n{doc}");
        names
    }

    #[test]
    fn escaping_round_trips_through_a_parser() {
        let nasty = "a<b>&\"'\tc\r\n\u{1}d";
        let doc = format!("{DECLARATION}<r v=\"{}\">{}</r>", attr(nasty), text(nasty));
        assert_eq!(parse(&doc), ["r"]);
        assert!(attr(nasty).contains("&#9;") && text(nasty).contains('\u{FFFD}'));
    }
}

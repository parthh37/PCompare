//! JSON and XML pretty-printers, so files that differ only in layout (or,
//! for JSON, key order) compare as equal.

use quick_xml::events::Event;
use quick_xml::{Reader, Writer};
use serde_json::{Map, Value};

/// JSON re-indented by two spaces with object keys sorted.
pub fn json_pretty(text: &str) -> Result<String, String> {
    let value: Value =
        serde_json::from_str(text.trim_start_matches('\u{FEFF}')).map_err(|e| format!("invalid JSON: {e}"))?;
    let mut out = serde_json::to_string_pretty(&sorted(value)).map_err(|e| e.to_string())?;
    out.push('\n');
    Ok(out)
}

/// Rebuilds objects in key order (whether or not serde_json keeps insertion order).
fn sorted(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect::<Map<_, _>>())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// XML re-indented by two spaces; whitespace-only text between elements is
/// dropped, everything else (comments, CDATA, processing instructions) kept.
pub fn xml_pretty(text: &str) -> Result<String, String> {
    let mut reader = Reader::from_str(text.trim_start_matches('\u{FEFF}'));
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("invalid XML at byte {}: {e}", reader.buffer_position()))?;
        match event {
            Event::Eof => break,
            Event::Text(t) if t.iter().all(u8::is_ascii_whitespace) => {}
            e => writer.write_event(e).map_err(|e| e.to_string())?,
        }
    }
    let mut out = String::from_utf8(writer.into_inner()).map_err(|e| e.to_string())?;
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_sorted_and_indented() {
        let a = json_pretty(r#"{"b":1,"a":{"z":[1,{"y":2,"x":3}],"c":null}}"#).unwrap();
        assert_eq!(
            a,
            "{\n  \"a\": {\n    \"c\": null,\n    \"z\": [\n      1,\n      {\n        \"x\": 3,\n        \"y\": 2\n      }\n    ]\n  },\n  \"b\": 1\n}\n"
        );
        // Layout and key order no longer matter.
        let b = json_pretty("{ \"a\" : { \"z\" : [ 1, { \"x\":3, \"y\":2 } ], \"c\": null },\n \"b\": 1 }").unwrap();
        assert_eq!(a, b);
        assert!(json_pretty("{oops").unwrap_err().starts_with("invalid JSON"));
    }

    #[test]
    fn xml_indented() {
        let x = xml_pretty("<?xml version=\"1.0\"?><a x=\"1\"><b>t &amp; u</b><!--c--><c/></a>").unwrap();
        assert_eq!(
            x,
            "<?xml version=\"1.0\"?>\n<a x=\"1\">\n  <b>t &amp; u</b>\n  <!--c-->\n  <c/>\n</a>\n"
        );
        let y = xml_pretty("<?xml version=\"1.0\"?>\n<a x=\"1\">\n    <b>t &amp; u</b>  <!--c-->\n<c/></a>").unwrap();
        assert_eq!(x, y);
        assert!(xml_pretty("<a><b></a>").is_err());
    }
}

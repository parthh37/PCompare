//! HTML → plain text: markup stripped, block elements on their own lines,
//! table cells separated by tabs, list items bulleted, entities decoded, and
//! whitespace collapsed as a browser would (except inside `<pre>`).

/// Elements that start and end a line.
const BLOCKS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "dd",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tbody",
    "thead",
    "tfoot",
    "tr",
    "ul",
    "caption",
    "summary",
    "details",
];

/// Elements whose content is not shown.
const HIDDEN: &[&str] = &[
    "script", "style", "head", "title", "noscript", "template", "svg", "object", "iframe",
];

pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    // Inside <pre> (depth) whitespace is kept.
    let mut pre = 0usize;
    // Pending collapsed whitespace between words.
    let mut space = false;
    // Cells already written on the current table row.
    let mut cells = 0usize;
    let bytes = html.as_bytes();
    let mut i = 0;
    let newline = |out: &mut String| {
        while out.ends_with(' ') {
            out.pop();
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    };
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Comments, doctypes, and CDATA.
            if html[i..].starts_with("<!--") {
                i = html[i + 4..].find("-->").map_or(bytes.len(), |e| i + 4 + e + 3);
                continue;
            }
            let Some(end) = tag_end(bytes, i) else {
                // A stray '<' is text.
                push_text(&mut out, "<", pre > 0, &mut space);
                i += 1;
                continue;
            };
            let tag = &html[i + 1..end];
            i = end + 1;
            if tag.starts_with('!') || tag.starts_with('?') {
                continue;
            }
            let closing = tag.starts_with('/');
            let name: String = tag
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            if !closing && HIDDEN.contains(&name.as_str()) && !tag.ends_with('/') {
                // Skip to the matching close tag.
                let close = format!("</{name}");
                let rest = html[i..].to_ascii_lowercase();
                i = match rest.find(&close) {
                    Some(p) => html[i + p..].find('>').map_or(bytes.len(), |e| i + p + e + 1),
                    None => bytes.len(),
                };
                continue;
            }
            match name.as_str() {
                "br" => {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push('\n');
                    space = false;
                }
                "pre" => {
                    newline(&mut out);
                    pre = if closing { pre.saturating_sub(1) } else { pre + 1 };
                    space = false;
                }
                "tr" => {
                    newline(&mut out);
                    cells = 0;
                    space = false;
                }
                "td" | "th" if !closing => {
                    if cells > 0 {
                        while out.ends_with(' ') {
                            out.pop();
                        }
                        out.push('\t');
                    }
                    cells += 1;
                    space = false;
                }
                "li" if !closing => {
                    newline(&mut out);
                    out.push_str("• ");
                    space = false;
                }
                "img" if !closing => {
                    if let Some(alt) = attribute(tag, "alt").filter(|a| !a.trim().is_empty()) {
                        push_text(&mut out, &format!("[{}]", decode_entities(&alt)), false, &mut space);
                    }
                }
                n if BLOCKS.contains(&n) => {
                    newline(&mut out);
                    space = false;
                }
                _ => {}
            }
            continue;
        }
        let next = html[i..].find('<').map_or(bytes.len(), |p| i + p);
        push_text(&mut out, &decode_entities(&html[i..next]), pre > 0, &mut space);
        i = next;
    }
    while out.ends_with([' ', '\n']) {
        out.pop();
    }
    out.push('\n');
    out
}

/// Index of the `>` closing the tag that starts at `start`, honouring quotes.
fn tag_end(b: &[u8], start: usize) -> Option<usize> {
    if !b
        .get(start + 1)
        .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, b'/' | b'!' | b'?'))
    {
        return None;
    }
    let mut quote = None;
    for (j, &c) in b.iter().enumerate().skip(start + 1) {
        match (quote, c) {
            (None, b'"' | b'\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, b'>') => return Some(j),
            _ => {}
        }
    }
    None
}

/// The value of `name="..."` in a tag's source.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find(name) {
        let at = from + p;
        from = at + name.len();
        let before_ok = lower[..at].ends_with(|c: char| c.is_whitespace());
        let rest = tag[from..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let rest = rest[1..].trim_start();
        return Some(match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or("").to_string(),
            _ => rest
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or("")
                .to_string(),
        });
    }
    None
}

fn push_text(out: &mut String, text: &str, keep_space: bool, space: &mut bool) {
    if keep_space {
        out.push_str(text);
        return;
    }
    for c in text.chars() {
        if c.is_whitespace() && c != '\u{A0}' {
            *space = true;
        } else {
            if *space && !out.is_empty() && !out.ends_with(['\n', '\t', ' ']) {
                out.push(' ');
            }
            *space = false;
            out.push(if c == '\u{A0}' { ' ' } else { c });
        }
    }
}

const ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    ("nbsp", "\u{A0}"),
    ("copy", "©"),
    ("reg", "®"),
    ("trade", "™"),
    ("hellip", "…"),
    ("mdash", "—"),
    ("ndash", "–"),
    ("lsquo", "‘"),
    ("rsquo", "’"),
    ("ldquo", "“"),
    ("rdquo", "”"),
    ("laquo", "«"),
    ("raquo", "»"),
    ("bull", "•"),
    ("middot", "·"),
    ("deg", "°"),
    ("euro", "€"),
    ("pound", "£"),
    ("yen", "¥"),
    ("cent", "¢"),
    ("sect", "§"),
    ("para", "¶"),
    ("times", "×"),
    ("divide", "÷"),
    ("plusmn", "±"),
    ("larr", "←"),
    ("rarr", "→"),
    ("uarr", "↑"),
    ("darr", "↓"),
    ("shy", ""),
    ("zwj", "\u{200D}"),
    ("zwnj", "\u{200C}"),
];

/// Decodes `&name;`, `&#n;` and `&#xh;`; unknown references stay as written.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let end = rest[1..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '#')
            .map(|e| e + 1);
        let decoded = end.filter(|&e| rest[e..].starts_with(';')).and_then(|e| {
            let name = &rest[1..e];
            let ch = if let Some(num) = name.strip_prefix('#') {
                let n = match num.strip_prefix(['x', 'X']) {
                    Some(h) => u32::from_str_radix(h, 16).ok(),
                    None => num.parse().ok(),
                };
                n.and_then(char::from_u32).map(String::from)
            } else {
                ENTITIES.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
            };
            ch.map(|c| (c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push_str(&c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markup_and_lays_out_blocks() {
        let html = r#"<!DOCTYPE html><html><head><title>T</title><style>p{color:red}</style>
<script>if (a < b) alert("x")</script></head>
<body><h1>Title</h1><p>Hello   <b>bold</b>
world&nbsp;&amp;&#33;&#x41; &unknown; a < b</p><!-- comment <p>no</p> -->
<ul><li>one</li><li>two</li></ul>
<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td> 2 </td></tr></table>
<pre>  keep
   this</pre>line<br>break <img src="x.png" alt="logo"></body></html>"#;
        assert_eq!(
            html_to_text(html),
            "Title\nHello bold world &!A &unknown; a < b\n• one\n• two\nA\tB\n1\t2\n  keep\n   this\nline\nbreak [logo]\n"
        );
    }

    #[test]
    fn attribute_values() {
        assert_eq!(
            attribute(r#"img data-alt="no" alt='yes' "#, "alt").as_deref(),
            Some("yes")
        );
        assert_eq!(attribute("img alt=bare>", "alt").as_deref(), Some("bare"));
        assert_eq!(attribute("img src=x", "alt"), None);
    }
}

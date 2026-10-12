//! Rich Text Format → plain text.
//!
//! A small tokenizer over groups and control words: destinations that hold no
//! document text (font and colour tables, pictures, field instructions, `\*`
//! groups...) are skipped, `\par` / `\line` / `\tab` / `\cell` map to breaks
//! and tabs, `\'hh` bytes decode in the document's ANSI code page, and `\uN`
//! characters skip their `\ucN` fallback characters.

use encoding_rs::Encoding;

/// Destinations whose content is not document text.
const SKIP_DESTINATIONS: &[&str] = &[
    "fonttbl",
    "colortbl",
    "stylesheet",
    "info",
    "pict",
    "object",
    "objdata",
    "fldinst",
    "themedata",
    "colorschememapping",
    "latentstyles",
    "datastore",
    "xmlnstbl",
    "listtable",
    "listoverridetable",
    "rsidtbl",
    "generator",
    "filetbl",
    "revtbl",
    "pgdsctbl",
    "header",
    "headerl",
    "headerr",
    "headerf",
    "footer",
    "footerl",
    "footerr",
    "footerf",
    "bkmkstart",
    "bkmkend",
    "nonshppict",
    "shppict",
    "private",
];

#[derive(Clone, Copy)]
struct Group {
    skip: bool,
    /// Fallback characters after each `\uN` (`\ucN`).
    uc: usize,
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    out: String,
    /// `\'hh` bytes awaiting decoding (multi-byte code pages need them together).
    pending: Vec<u8>,
    codepage: &'static Encoding,
    /// Fallback characters still to drop after a `\uN`.
    skip_chars: usize,
}

pub fn rtf_to_text(bytes: &[u8]) -> Result<String, String> {
    if !bytes.trim_ascii_start().starts_with(b"{\\rtf") {
        return Err("not an RTF document".into());
    }
    let mut p = Parser {
        src: bytes,
        pos: 0,
        out: String::new(),
        pending: Vec::new(),
        codepage: encoding_rs::WINDOWS_1252,
        skip_chars: 0,
    };
    p.run();
    Ok(p.out)
}

impl Parser<'_> {
    fn flush(&mut self) {
        if !self.pending.is_empty() {
            let text = self.codepage.decode_without_bom_handling(&self.pending).0.into_owned();
            self.out.push_str(&text);
            self.pending.clear();
        }
    }

    fn emit(&mut self, g: Group, s: &str) {
        if g.skip {
            return;
        }
        self.flush();
        self.out.push_str(s);
    }

    /// Drops one pending `\uN` fallback character; true if one was dropped.
    fn take_fallback(&mut self) -> bool {
        if self.skip_chars > 0 {
            self.skip_chars -= 1;
            true
        } else {
            false
        }
    }

    fn run(&mut self) {
        let mut stack: Vec<Group> = Vec::new();
        let mut g = Group { skip: false, uc: 1 };
        // True right after `{`: the next control word may name a destination.
        let mut group_start = false;
        while self.pos < self.src.len() {
            let c = self.src[self.pos];
            self.pos += 1;
            match c {
                b'{' => {
                    self.skip_chars = 0;
                    stack.push(g);
                    group_start = true;
                }
                b'}' => {
                    self.skip_chars = 0;
                    g = stack.pop().unwrap_or(g);
                    group_start = false;
                }
                b'\\' => {
                    let at_start = group_start;
                    group_start = false;
                    self.control(&mut g, at_start);
                }
                b'\r' | b'\n' => {}
                _ => {
                    group_start = false;
                    if self.take_fallback() || g.skip {
                        continue;
                    }
                    // Plain bytes are in the ANSI code page too.
                    self.pending.push(c);
                }
            }
        }
        self.flush();
    }

    fn control(&mut self, g: &mut Group, at_group_start: bool) {
        let Some(&c) = self.src.get(self.pos) else { return };
        if !c.is_ascii_alphabetic() {
            self.pos += 1;
            match c {
                b'\'' => {
                    let hex = self
                        .src
                        .get(self.pos..self.pos + 2)
                        .and_then(|h| std::str::from_utf8(h).ok());
                    if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                        self.pos += 2;
                        if !self.take_fallback() && !g.skip {
                            self.pending.push(b);
                        }
                    }
                }
                b'*' => g.skip = true,
                b'~' => self.emit(*g, "\u{A0}"),
                b'_' => self.emit(*g, "-"),
                b'-' => {}
                b'\r' | b'\n' => self.emit(*g, "\n"),
                b'\\' | b'{' | b'}' if !self.take_fallback() => {
                    self.emit(*g, std::str::from_utf8(&[c]).unwrap_or(""));
                }
                _ => {}
            }
            return;
        }
        let start = self.pos;
        while self.src.get(self.pos).is_some_and(u8::is_ascii_alphabetic) {
            self.pos += 1;
        }
        let word = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("");
        let num_start = self.pos;
        if self.src.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        while self.src.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        let param: Option<i32> = std::str::from_utf8(&self.src[num_start..self.pos])
            .ok()
            .and_then(|n| n.parse().ok());
        // One space delimits the control word and is not text.
        if self.src.get(self.pos) == Some(&b' ') {
            self.pos += 1;
        }

        if at_group_start && SKIP_DESTINATIONS.contains(&word) {
            g.skip = true;
            return;
        }
        match word {
            "ansicpg" => {
                if let Some(enc) = param.and_then(codepage) {
                    self.flush();
                    self.codepage = enc;
                }
            }
            "mac" => self.codepage = encoding_rs::MACINTOSH,
            "uc" => g.uc = param.unwrap_or(1).max(0) as usize,
            "u" => {
                if let Some(n) = param {
                    // Negative values are UTF-16 units written as signed 16-bit numbers.
                    let unit = n as i16 as u16;
                    if !g.skip {
                        self.flush();
                        if let Some(ch) = char::from_u32(unit as u32) {
                            self.out.push(ch);
                        } else {
                            // A surrogate half: combine with a following low half.
                            self.out.push_str(&String::from_utf16_lossy(&[unit]));
                        }
                    }
                    self.skip_chars = g.uc;
                }
            }
            "par" | "line" | "sect" | "page" | "row" => self.emit(*g, "\n"),
            "tab" | "cell" => self.emit(*g, "\t"),
            "emdash" => self.emit(*g, "\u{2014}"),
            "endash" => self.emit(*g, "\u{2013}"),
            "bullet" => self.emit(*g, "\u{2022}"),
            "lquote" => self.emit(*g, "\u{2018}"),
            "rquote" => self.emit(*g, "\u{2019}"),
            "ldblquote" => self.emit(*g, "\u{201C}"),
            "rdblquote" => self.emit(*g, "\u{201D}"),
            "emspace" | "enspace" | "qmspace" => self.emit(*g, " "),
            "bin" => {
                // Raw binary data of `param` bytes.
                self.pos = (self.pos + param.unwrap_or(0).max(0) as usize).min(self.src.len());
            }
            _ => {}
        }
    }
}

/// The encoding for a Windows code page number.
fn codepage(cp: i32) -> Option<&'static Encoding> {
    let label = match cp {
        65001 => "utf-8",
        874 => "windows-874",
        932 => "shift_jis",
        936 => "gbk",
        949 => "euc-kr",
        950 => "big5",
        1250..=1258 => return Encoding::for_label(format!("windows-{cp}").as_bytes()),
        10000 => "macintosh",
        20866 => "koi8-r",
        28591..=28605 => return Encoding::for_label(format!("iso-8859-{}", cp - 28590).as_bytes()),
        _ => return None,
    };
    Encoding::for_label(label.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rtf: &str) -> String {
        rtf_to_text(rtf.as_bytes()).unwrap()
    }

    #[test]
    fn paragraphs_tabs_and_escapes() {
        let rtf = r"{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0\fswiss Helvetica;}}{\colortbl;\red255\green0\blue0;}
\f0\fs24 Hello \b bold\b0  world\par
a\tab b\line c \{braces\} back\\slash\par
caf\'e9 \emdash  done\par}";
        assert_eq!(
            text(rtf),
            "Hello bold world\na\tb\nc {braces} back\\slash\ncafé \u{2014} done\n"
        );
    }

    #[test]
    fn unicode_with_fallbacks_and_skipped_groups() {
        let rtf = r"{\rtf1\ansi{\*\generator Riched20;}{\info{\title T}}\uc1\u8364?euro \u-3913?x{\*\unknown hidden}{\pict 89504e47}
{\field{\*\fldinst HYPERLINK x}{\fldrslt link}}\par}";
        assert_eq!(text(rtf), "\u{20AC}euro \u{F0B7}xlink\n");
    }

    #[test]
    fn code_page_bytes_decode_together() {
        // Shift-JIS "日本" as \'hh pairs.
        let rtf = r"{\rtf1\ansi\ansicpg932 \'93\'fa\'96\'7b\par}";
        assert_eq!(text(rtf), "日本\n");
        assert!(rtf_to_text(b"plain").is_err());
    }
}

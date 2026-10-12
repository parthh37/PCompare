//! Delphi / C++Builder binary form files (TPF0 streams) → the textual DFM
//! form Delphi itself writes (`object Form1: TForm1 ... end`).

/// The signature of a binary component stream.
const SIGNATURE: &[u8] = b"TPF0";

// TValueType tags.
const VA_NULL: u8 = 0;
const VA_LIST: u8 = 1;
const VA_INT8: u8 = 2;
const VA_INT16: u8 = 3;
const VA_INT32: u8 = 4;
const VA_EXTENDED: u8 = 5;
const VA_STRING: u8 = 6;
const VA_IDENT: u8 = 7;
const VA_FALSE: u8 = 8;
const VA_TRUE: u8 = 9;
const VA_BINARY: u8 = 10;
const VA_SET: u8 = 11;
const VA_LSTRING: u8 = 12;
const VA_NIL: u8 = 13;
const VA_COLLECTION: u8 = 14;
const VA_SINGLE: u8 = 15;
const VA_CURRENCY: u8 = 16;
const VA_DATE: u8 = 17;
const VA_WSTRING: u8 = 18;
const VA_INT64: u8 = 19;
const VA_UTF8STRING: u8 = 20;
const VA_DOUBLE: u8 = 21;

// Object prefix flags (TFilerFlags).
const FF_INHERITED: u8 = 1;
const FF_CHILD_POS: u8 = 2;
const FF_INLINE: u8 = 4;

/// Where the TPF0 stream starts: at 0, or after the 16-bit resource header
/// (`FF 0A 00 name...`) that binary .dfm files on disk carry.
fn stream_start(bytes: &[u8]) -> Option<usize> {
    if bytes.starts_with(SIGNATURE) {
        return Some(0);
    }
    if bytes.starts_with(&[0xFF, 0x0A, 0x00]) {
        let head = &bytes[..bytes.len().min(512)];
        return head.windows(4).position(|w| w == SIGNATURE);
    }
    None
}

pub fn is_binary_dfm(bytes: &[u8]) -> bool {
    stream_start(bytes).is_some()
}

pub fn dfm_to_text(bytes: &[u8]) -> Result<String, String> {
    let start = stream_start(bytes).ok_or("not a binary Delphi form")?;
    let mut r = Reader {
        b: bytes,
        pos: start + SIGNATURE.len(),
        out: String::new(),
    };
    r.object(0)?;
    Ok(r.out)
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
    out: String,
}

fn truncated() -> String {
    "the form data is truncated or damaged".into()
}

impl Reader<'_> {
    fn bytes(&mut self, n: usize) -> Result<&[u8], String> {
        let s = self.b.get(self.pos..self.pos + n).ok_or_else(truncated)?;
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }
    fn peek(&self) -> Result<u8, String> {
        self.b.get(self.pos).copied().ok_or_else(truncated)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        Ok(self.bytes(N)?.try_into().unwrap_or([0; N]))
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    /// A ShortString: a length byte, then (UTF-8 or ANSI) bytes.
    fn short_str(&mut self) -> Result<String, String> {
        let n = self.u8()? as usize;
        Ok(ansi_or_utf8(self.bytes(n)?))
    }
    /// True (and consumed) at a list terminator.
    fn end_of_list(&mut self) -> Result<bool, String> {
        if self.peek()? == VA_NULL {
            self.pos += 1;
            return Ok(true);
        }
        Ok(false)
    }
    fn integer(&mut self) -> Result<i64, String> {
        match self.u8()? {
            VA_INT8 => Ok(self.u8()? as i8 as i64),
            VA_INT16 => Ok(i16::from_le_bytes(self.array()?) as i64),
            VA_INT32 => Ok(self.i32()? as i64),
            t => Err(format!("expected an integer, found value type {t}")),
        }
    }
    fn indent(&mut self, level: usize) {
        self.out.extend(std::iter::repeat_n("  ", level));
    }

    fn object(&mut self, level: usize) -> Result<(), String> {
        let mut flags = 0;
        let mut position = None;
        if self.peek()? & 0xF0 == 0xF0 {
            flags = self.u8()? & 0x0F;
            if flags & FF_CHILD_POS != 0 {
                position = Some(self.integer()?);
            }
        }
        let class = self.short_str()?;
        let name = self.short_str()?;
        self.indent(level);
        self.out.push_str(if flags & FF_INHERITED != 0 {
            "inherited "
        } else if flags & FF_INLINE != 0 {
            "inline "
        } else {
            "object "
        });
        if !name.is_empty() {
            self.out.push_str(&name);
            self.out.push_str(": ");
        }
        self.out.push_str(&class);
        if let Some(p) = position {
            self.out.push_str(&format!(" [{p}]"));
        }
        self.out.push('\n');
        while !self.end_of_list()? {
            self.property(level + 1)?;
        }
        while !self.end_of_list()? {
            self.object(level + 1)?;
        }
        self.indent(level);
        self.out.push_str("end\n");
        Ok(())
    }

    fn property(&mut self, level: usize) -> Result<(), String> {
        let name = self.short_str()?;
        self.indent(level);
        self.out.push_str(&name);
        self.out.push_str(" = ");
        self.value(level)?;
        self.out.push('\n');
        Ok(())
    }

    fn value(&mut self, level: usize) -> Result<(), String> {
        let tag = self.u8()?;
        match tag {
            VA_LIST => {
                self.out.push_str("(\n");
                while !self.end_of_list()? {
                    self.indent(level + 1);
                    self.value(level + 1)?;
                    self.out.push('\n');
                }
                self.indent(level);
                self.out.push(')');
            }
            VA_INT8 | VA_INT16 | VA_INT32 => {
                self.pos -= 1;
                let n = self.integer()?;
                self.out.push_str(&n.to_string());
            }
            VA_INT64 => {
                let n = i64::from_le_bytes(self.array()?);
                self.out.push_str(&n.to_string());
            }
            VA_EXTENDED => {
                let v = extended(self.array()?);
                self.out.push_str(&float(v));
            }
            VA_DOUBLE => {
                let v = f64::from_le_bytes(self.array()?);
                self.out.push_str(&float(v));
            }
            VA_SINGLE => {
                let v = f32::from_le_bytes(self.array()?) as f64;
                self.out.push_str(&format!("{}s", float(v)));
            }
            VA_CURRENCY => {
                let v = i64::from_le_bytes(self.array()?) as f64 / 10_000.0;
                self.out.push_str(&format!("{}c", float(v)));
            }
            VA_DATE => {
                let v = f64::from_le_bytes(self.array()?);
                self.out.push_str(&format!("{}d", float(v)));
            }
            VA_STRING => {
                let s = self.short_str()?;
                self.out.push_str(&quote(&s));
            }
            VA_LSTRING => {
                let n = self.i32()?.max(0) as usize;
                let s = ansi_or_utf8(self.bytes(n)?);
                self.out.push_str(&quote(&s));
            }
            VA_UTF8STRING => {
                let n = self.i32()?.max(0) as usize;
                let s = String::from_utf8_lossy(self.bytes(n)?).into_owned();
                self.out.push_str(&quote(&s));
            }
            VA_WSTRING => {
                let n = self.i32()?.max(0) as usize;
                let units: Vec<u16> = self
                    .bytes(n * 2)?
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&c| u16::from_le_bytes(c))
                    .collect();
                self.out.push_str(&quote(&String::from_utf16_lossy(&units)));
            }
            VA_IDENT => {
                let s = self.short_str()?;
                self.out.push_str(&s);
            }
            VA_FALSE => self.out.push_str("False"),
            VA_TRUE => self.out.push_str("True"),
            VA_NIL => self.out.push_str("nil"),
            VA_NULL => self.out.push_str("Null"),
            VA_SET => {
                let mut items = Vec::new();
                loop {
                    let s = self.short_str()?;
                    if s.is_empty() {
                        break;
                    }
                    items.push(s);
                }
                self.out.push_str(&format!("[{}]", items.join(", ")));
            }
            VA_BINARY => {
                let n = self.i32()?.max(0) as usize;
                let data = self.bytes(n)?.to_vec();
                self.out.push_str("{\n");
                for chunk in data.chunks(32) {
                    self.indent(level + 1);
                    for b in chunk {
                        self.out.push_str(&format!("{b:02X}"));
                    }
                    self.out.push('\n');
                }
                self.indent(level);
                self.out.push('}');
            }
            VA_COLLECTION => {
                self.out.push_str("<\n");
                while !self.end_of_list()? {
                    self.indent(level + 1);
                    self.out.push_str("item");
                    if matches!(self.peek()?, VA_INT8 | VA_INT16 | VA_INT32) {
                        let i = self.integer()?;
                        self.out.push_str(&format!(" [{i}]"));
                    }
                    self.out.push('\n');
                    if self.u8()? != VA_LIST {
                        return Err(truncated());
                    }
                    while !self.end_of_list()? {
                        self.property(level + 2)?;
                    }
                    self.indent(level + 1);
                    self.out.push_str("end\n");
                }
                self.indent(level);
                self.out.push('>');
            }
            t => return Err(format!("unknown value type {t} at byte {}", self.pos - 1)),
        }
        Ok(())
    }
}

/// Names and short strings are UTF-8 since Delphi 2009, ANSI before.
fn ansi_or_utf8(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_owned(),
        Err(_) => encoding_rs::WINDOWS_1252.decode_without_bom_handling(b).0.into_owned(),
    }
}

/// An 80-bit x87 extended float as f64.
fn extended(b: [u8; 10]) -> f64 {
    let mantissa = u64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8]));
    let se = u16::from_le_bytes([b[8], b[9]]);
    let exp = (se & 0x7FFF) as i32;
    if exp == 0 && mantissa == 0 {
        return 0.0;
    }
    let v = mantissa as f64 * 2f64.powi(exp - 16383 - 63);
    if se & 0x8000 != 0 {
        -v
    } else {
        v
    }
}

fn float(v: f64) -> String {
    format!("{v}")
}

/// A Delphi string literal: printable runs in quotes (quotes doubled),
/// control characters as `#n`.
fn quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    let mut out = String::new();
    let mut open = false;
    for c in s.chars() {
        if (c as u32) < 0x20 {
            if open {
                out.push('\'');
                open = false;
            }
            out.push_str(&format!("#{}", c as u32));
        } else {
            if !open {
                out.push('\'');
                open = true;
            }
            if c == '\'' {
                out.push('\'');
            }
            out.push(c);
        }
    }
    if open {
        out.push('\'');
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn s(out: &mut Vec<u8>, v: &str) {
        out.push(v.len() as u8);
        out.extend(v.as_bytes());
    }

    /// A small form: properties of most value types, a nested child and a collection.
    pub fn sample() -> Vec<u8> {
        let mut b = b"TPF0".to_vec();
        s(&mut b, "TForm1");
        s(&mut b, "Form1");
        s(&mut b, "Left");
        b.extend([VA_INT16, 0x2C, 0x01]); // 300
        s(&mut b, "Caption");
        b.push(VA_STRING);
        s(&mut b, "It's");
        s(&mut b, "Hint");
        b.push(VA_WSTRING);
        let w: Vec<u16> = "a\r\nb".encode_utf16().collect();
        b.extend((w.len() as i32).to_le_bytes());
        w.iter().for_each(|u| b.extend(u.to_le_bytes()));
        s(&mut b, "Visible");
        b.push(VA_FALSE);
        s(&mut b, "Color");
        b.push(VA_IDENT);
        s(&mut b, "clBtnFace");
        s(&mut b, "BorderIcons");
        b.push(VA_SET);
        s(&mut b, "biSystemMenu");
        s(&mut b, "biMinimize");
        b.push(0);
        s(&mut b, "Scale");
        b.push(VA_DOUBLE);
        b.extend(1.5f64.to_le_bytes());
        s(&mut b, "Ratio");
        b.push(VA_EXTENDED);
        // 2.5 = 1.01b * 2^1: mantissa 0xA000..., exponent 16384.
        b.extend(0xA000_0000_0000_0000u64.to_le_bytes());
        b.extend(16384u16.to_le_bytes());
        s(&mut b, "Strings");
        b.push(VA_LIST);
        b.push(VA_STRING);
        s(&mut b, "one");
        b.push(VA_INT8);
        b.push(0xFF); // -1
        b.push(0);
        s(&mut b, "Data");
        b.push(VA_BINARY);
        b.extend(3i32.to_le_bytes());
        b.extend([0xDE, 0xAD, 0x01]);
        s(&mut b, "Columns");
        b.push(VA_COLLECTION);
        b.push(VA_LIST);
        s(&mut b, "Width");
        b.extend([VA_INT8, 64]);
        b.push(0); // end of item properties
        b.extend([VA_INT8, 7]); // item index
        b.push(VA_LIST);
        b.push(0);
        b.push(0); // end of collection
        b.push(0); // end of properties
                   // A child at a fixed position, inherited.
        b.extend([0xF0 | FF_INHERITED | FF_CHILD_POS, VA_INT8, 2]);
        s(&mut b, "TButton");
        s(&mut b, "OK");
        s(&mut b, "Default");
        b.push(VA_TRUE);
        b.push(0); // end of child properties
        b.push(0); // end of child children
        b.push(0); // end of children
        b
    }

    const EXPECTED: &str = "object Form1: TForm1
  Left = 300
  Caption = 'It''s'
  Hint = 'a'#13#10'b'
  Visible = False
  Color = clBtnFace
  BorderIcons = [biSystemMenu, biMinimize]
  Scale = 1.5
  Ratio = 2.5
  Strings = (
    'one'
    -1
  )
  Data = {
    DEAD01
  }
  Columns = <
    item
      Width = 64
    end
    item [7]
    end
  >
  inherited OK: TButton [2]
    Default = True
  end
end
";

    #[test]
    fn converts_binary_form_to_text() {
        assert_eq!(dfm_to_text(&sample()).unwrap(), EXPECTED);
    }

    #[test]
    fn skips_the_resource_header() {
        let mut bytes = vec![0xFF, 0x0A, 0x00];
        bytes.extend(b"TFORM1\0");
        bytes.extend([0x30, 0x10]);
        let body = sample();
        bytes.extend((body.len() as u32).to_le_bytes());
        bytes.extend(&body);
        assert!(is_binary_dfm(&bytes));
        assert_eq!(dfm_to_text(&bytes).unwrap(), EXPECTED);
    }

    #[test]
    fn truncated_streams_fail_cleanly() {
        let full = sample();
        for cut in [5, 20, full.len() - 1] {
            assert!(dfm_to_text(&full[..cut]).is_err(), "cut at {cut}");
        }
        assert!(!is_binary_dfm(b"object Form1: TForm1"));
    }
}

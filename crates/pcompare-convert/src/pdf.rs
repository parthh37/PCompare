//! PDF text extraction (via `pdf-extract`), one page after another.

/// The text of every page. Scanned (image-only) PDFs yield no text.
pub fn pdf_to_text(bytes: &[u8]) -> Result<String, String> {
    if !bytes.starts_with(b"%PDF") && !bytes.get(..1024).is_some_and(|head| find(head, b"%PDF").is_some()) {
        return Err("not a PDF file".into());
    }
    // The extractor panics on some malformed files; report those as errors.
    let result = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
    match result {
        Ok(Ok(text)) => Ok(text.replace('\u{0C}', "\n")),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("the PDF could not be read (unsupported or damaged)".into()),
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A one-page PDF showing each of `lines` in Helvetica, with a correct xref.
    pub fn pdf(lines: &[&str]) -> Vec<u8> {
        let mut content = String::from("BT /F1 12 Tf 72 720 Td 14 TL\n");
        for l in lines {
            content.push_str(&format!("({}) Tj T*\n", l.replace('(', "\\(").replace(')', "\\)")));
        }
        content.push_str("ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_string(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{body}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for o in offsets {
            out.extend(format!("{o:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        out
    }

    #[test]
    fn extracts_page_text() {
        let text = pdf_to_text(&pdf(&["Hello PDF", "Second line"])).unwrap();
        let words: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(words, ["Hello", "PDF", "Second", "line"], "{text:?}");
    }

    #[test]
    fn rejects_non_pdf_and_damaged_files() {
        assert_eq!(pdf_to_text(b"hello").unwrap_err(), "not a PDF file");
        assert!(pdf_to_text(b"%PDF-1.4\ngarbage").is_err());
    }
}

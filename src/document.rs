//! Accepted upload formats and how each one reaches the model.
//!
//! PDFs go to Claude, GPT and Gemini natively. Spreadsheets (xlsx, xlsm, xlsb,
//! xls, ods) and Word documents (docx) are converted to plain text here, because
//! no provider accepts them as binary parts. Providers without file input
//! (DeepSeek, Z.ai, Xiaomi) also get PDFs as text, via the PDF's text layer.

use std::io::{Cursor, Read};

use calamine::{Data, Reader as _};
use quick_xml::events::Event;

/// What the upload was recognised as. Surfaced in `x-genparse-file-type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    Pdf,
    Spreadsheet,
    Docx,
}

impl DocumentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DocumentKind::Pdf => "pdf",
            DocumentKind::Spreadsheet => "spreadsheet",
            DocumentKind::Docx => "docx",
        }
    }
}

/// The payload handed to a provider.
pub enum Document {
    /// Raw PDF bytes, base64 without line breaks.
    Pdf { base64: String },
    /// Plain-text rendering of a non-PDF upload.
    Text { text: String },
}

const ZIP_MAGIC: &[u8] = b"PK\x03\x04";
const OLE_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

fn extension(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Work out the format from the bytes, falling back to the filename only where
/// the bytes are ambiguous (an OLE container may be .xls or .doc).
pub fn detect(bytes: &[u8], filename: &str) -> Result<DocumentKind, String> {
    let ext = extension(filename);
    if bytes.starts_with(b"%PDF") {
        return Ok(DocumentKind::Pdf);
    }
    if bytes.starts_with(ZIP_MAGIC) {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| format!("file is a zip container but could not be opened: {e}"))?;
        if zip.by_name("xl/workbook.xml").is_ok() {
            return Ok(DocumentKind::Spreadsheet);
        }
        if zip.by_name("word/document.xml").is_ok() {
            return Ok(DocumentKind::Docx);
        }
        if ext == "ods" || zip_mimetype(&mut zip).is_some_and(|m| m.contains("spreadsheet")) {
            return Ok(DocumentKind::Spreadsheet);
        }
        return Err("zip container is not a .xlsx, .docx or .ods file".into());
    }
    if bytes.starts_with(OLE_MAGIC) {
        return match ext.as_str() {
            "xls" => Ok(DocumentKind::Spreadsheet),
            "doc" => Err("legacy .doc is not supported; save the file as .docx".into()),
            _ => Err("legacy Office binary file; only .xls is supported, or save as .docx".into()),
        };
    }
    match ext.as_str() {
        "xlsb" => Ok(DocumentKind::Spreadsheet),
        _ => Err("unsupported file type; upload a PDF, spreadsheet (xlsx/xls/ods) or .docx".into()),
    }
}

fn zip_mimetype(zip: &mut zip::ZipArchive<Cursor<&[u8]>>) -> Option<String> {
    let mut f = zip.by_name("mimetype").ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

/// Produce the provider payload for a detected upload. `pdf_as_text` is set
/// for providers that cannot take PDF bytes; the PDF's text layer is used.
pub fn convert(
    kind: DocumentKind,
    bytes: &[u8],
    max_text_chars: usize,
    pdf_as_text: bool,
) -> Result<Document, ConvertError> {
    let text = match kind {
        DocumentKind::Pdf if !pdf_as_text => {
            use base64::prelude::*;
            return Ok(Document::Pdf {
                base64: BASE64_STANDARD.encode(bytes),
            });
        }
        DocumentKind::Pdf => pdf_to_text(bytes)?,
        DocumentKind::Spreadsheet => spreadsheet_to_text(bytes)?,
        DocumentKind::Docx => docx_to_text(bytes)?,
    };
    let text = tidy(&text);
    if text.trim().is_empty() {
        let what = match kind {
            DocumentKind::Pdf => {
                "the PDF has no text layer (scanned?); use a PDF-capable model such as Claude, GPT or Gemini"
            }
            _ => "no text found in the document",
        };
        return Err(ConvertError::Unreadable(what.into()));
    }
    let chars = text.chars().count();
    if chars > max_text_chars {
        return Err(ConvertError::TooLarge {
            chars,
            max_text_chars,
        });
    }
    Ok(Document::Text { text })
}

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("could not read the document: {0}")]
    Unreadable(String),
    #[error(
        "document text is {chars} characters, over the {max_text_chars} limit (extraction.max_text_chars)"
    )]
    TooLarge { chars: usize, max_text_chars: usize },
}

/// Text layer of a PDF. `pdf-extract` can panic on malformed files, so the
/// call is fenced and a panic becomes an ordinary error.
fn pdf_to_text(bytes: &[u8]) -> Result<String, ConvertError> {
    let result = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
    match result {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(ConvertError::Unreadable(format!(
            "PDF text extraction failed: {e}"
        ))),
        Err(_) => Err(ConvertError::Unreadable(
            "PDF text extraction crashed on this file".into(),
        )),
    }
}

fn spreadsheet_to_text(bytes: &[u8]) -> Result<String, ConvertError> {
    let mut wb = calamine::open_workbook_auto_from_rs(Cursor::new(bytes))
        .map_err(|e| ConvertError::Unreadable(e.to_string()))?;
    let mut out = String::new();
    for (name, range) in wb.worksheets() {
        if range.is_empty() {
            continue;
        }
        out.push_str(&format!("# Sheet: {name}\n"));
        for row in range.rows() {
            let cells: Vec<String> = row.iter().map(cell_text).collect();
            let last = cells.iter().rposition(|c| !c.is_empty());
            let Some(last) = last else { continue };
            out.push_str(&cells[..=last].join("\t"));
            out.push('\n');
        }
        out.push('\n');
    }
    Ok(out)
}

fn cell_text(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::DateTime(dt) => match dt.as_datetime() {
            Some(t) if t.time() == chrono_midnight() => t.format("%Y-%m-%d").to_string(),
            Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
            None => dt.to_string(),
        },
        Data::Error(e) => format!("#{e}"),
        other => other.to_string().replace(['\t', '\n'], " "),
    }
}

fn chrono_midnight() -> chrono::NaiveTime {
    chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("midnight is a valid time")
}

fn docx_to_text(bytes: &[u8]) -> Result<String, ConvertError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| ConvertError::Unreadable(e.to_string()))?;
    let mut xml = Vec::new();
    zip.by_name("word/document.xml")
        .map_err(|_| ConvertError::Unreadable("word/document.xml missing".into()))?
        .read_to_end(&mut xml)
        .map_err(|e| ConvertError::Unreadable(e.to_string()))?;
    word_xml_to_text(&xml)
}

/// Walk WordprocessingML: text lives in `w:t` runs, paragraphs become lines,
/// table cells are separated by tabs. Field codes (`w:instrText`) and tracked
/// deletions (`w:delText`) are never `w:t`, so they drop out naturally.
fn word_xml_to_text(xml: &[u8]) -> Result<String, ConvertError> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut in_text = false;
    loop {
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| ConvertError::Unreadable(format!("bad document.xml: {e}")))?;
        match ev {
            Event::Eof => break,
            Event::Start(e) if e.local_name().as_ref() == "t" => in_text = true,
            Event::End(e) => match e.local_name().as_ref() {
                "t" => in_text = false,
                "p" => out.push('\n'),
                "tc" => {
                    if out.ends_with('\n') {
                        out.pop();
                    }
                    out.push('\t');
                }
                "tr" => {
                    if out.ends_with('\t') {
                        out.pop();
                    }
                    out.push('\n');
                }
                _ => {}
            },
            Event::Empty(e) => match e.local_name().as_ref() {
                "tab" => out.push('\t'),
                "br" | "cr" => out.push('\n'),
                _ => {}
            },
            Event::Text(t) if in_text => out.push_str(&t.xml10_content()),
            Event::GeneralRef(r) if in_text => {
                let name = r.xml10_content();
                let resolved = match name.as_ref() {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => r.resolve_char_ref().ok().flatten(),
                };
                match resolved {
                    Some(c) => out.push(c),
                    None => out.push_str(&format!("&{name};")),
                }
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Collapse runs of blank lines and strip trailing whitespace per line.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn make_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, body) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    const DOC_XML: &str = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
<w:p><w:r><w:t>Invoice </w:t></w:r><w:r><w:t>INV-7</w:t></w:r></w:p>
<w:p><w:r><w:fldChar w:fldCharType="begin"/><w:instrText>PAGE</w:instrText><w:t>Total:</w:t><w:tab/><w:t>12.50 &amp; tax</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
<w:p><w:del><w:r><w:delText>gone</w:delText></w:r></w:del></w:p>
</w:body></w:document>"#;

    #[test]
    fn docx_text() {
        let docx = make_zip(&[
            ("word/document.xml", DOC_XML),
            ("[Content_Types].xml", "<Types/>"),
        ]);
        assert_eq!(detect(&docx, "x.docx").unwrap(), DocumentKind::Docx);
        let Document::Text { text } = convert(DocumentKind::Docx, &docx, 10_000, false).unwrap()
        else {
            panic!("expected text");
        };
        assert_eq!(text, "Invoice INV-7\nTotal:\t12.50 & tax\nA\tB\n");
    }

    #[test]
    fn xlsx_text() {
        let xlsx = make_zip(&[
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Items" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>item</t></is></c><c r="B1" t="inlineStr"><is><t>qty</t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>widget</t></is></c><c r="B2"><v>3</v></c></row></sheetData></worksheet>"#,
            ),
        ]);
        assert_eq!(detect(&xlsx, "x.xlsx").unwrap(), DocumentKind::Spreadsheet);
        let Document::Text { text } =
            convert(DocumentKind::Spreadsheet, &xlsx, 10_000, false).unwrap()
        else {
            panic!("expected text");
        };
        assert_eq!(text, "# Sheet: Items\nitem\tqty\nwidget\t3\n");
    }

    #[test]
    fn detection_edges() {
        assert_eq!(detect(b"%PDF-1.7 ...", "a.pdf").unwrap(), DocumentKind::Pdf);
        assert!(detect(b"hello", "a.txt").is_err());
        let mut ole = OLE_MAGIC.to_vec();
        ole.extend_from_slice(&[0; 16]);
        assert_eq!(detect(&ole, "old.xls").unwrap(), DocumentKind::Spreadsheet);
        assert!(detect(&ole, "old.doc").unwrap_err().contains(".docx"));
        let other_zip = make_zip(&[("readme.txt", "hi")]);
        assert!(detect(&other_zip, "a.zip").is_err());
    }

    #[test]
    fn pdf_text_layer() {
        let pdf = include_bytes!("../tests/fixtures/minimal.pdf");
        assert_eq!(detect(pdf, "minimal.pdf").unwrap(), DocumentKind::Pdf);
        assert!(matches!(
            convert(DocumentKind::Pdf, pdf, 10_000, false).unwrap(),
            Document::Pdf { .. }
        ));
        let Document::Text { text } = convert(DocumentKind::Pdf, pdf, 10_000, true).unwrap() else {
            panic!("expected text");
        };
        assert!(text.contains("INV-42"), "got {text:?}");
    }

    #[test]
    fn text_limit() {
        let docx = make_zip(&[("word/document.xml", DOC_XML)]);
        assert!(matches!(
            convert(DocumentKind::Docx, &docx, 5, false),
            Err(ConvertError::TooLarge { .. })
        ));
    }
}

//! Uploaded files → [`SourceDoc`]s: an xlsx becomes one sheet doc
//! per tab (headers + rows), a docx becomes markdown with its
//! heading styles kept, a PDF becomes its text. Everything here is untrusted:
//! the size is capped before decoding and every decompression is bounded.

use std::io::{Cursor, Read};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use calamine::{Reader, Xlsx};
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use sun_core::{Error, Timestamp};

use super::{DocContent, MAX_CONTENT_BYTES, SourceDoc};

/// A file as an upload sends it; its kind comes from `external_id`'s extension.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    /// File name or path, ending in `.xlsx`, `.docx` or `.pdf`.
    pub external_id: String,
    /// Title, for citations.
    pub title: String,
    /// Version label.
    pub version: String,
    /// When the file last changed (RFC 3339).
    #[serde(default)]
    pub updated_at: Option<Timestamp>,
    /// Link to the original.
    #[serde(default)]
    pub url: Option<String>,
    /// The bytes, base64 (standard alphabet).
    pub content_base64: String,
}

/// File extensions [`convert`] reads.
pub const FILE_EXTENSIONS: &[&str] = &["xlsx", "docx", "pdf"];

/// Converts an uploaded file into the docs to ingest.
pub fn convert(file: SourceFile) -> Result<Vec<SourceDoc>, Error> {
    let name = file.external_id.to_lowercase();
    let extension = name.rsplit_once('.').map(|(_, e)| e).unwrap_or_default();
    if !FILE_EXTENSIONS.contains(&extension) {
        return Err(invalid(format!(
            "{}: upload .xlsx, .docx or .pdf files, or send markdown",
            file.external_id
        )));
    }
    if file.content_base64.len() > MAX_CONTENT_BYTES / 3 * 4 + 4 {
        return Err(invalid(format!(
            "{} is larger than {MAX_CONTENT_BYTES} bytes",
            file.external_id
        )));
    }
    let bytes = BASE64
        .decode(file.content_base64.trim())
        .map_err(|e| invalid(format!("{}: bad base64: {e}", file.external_id)))?;
    let fail = |e: String| invalid(format!("{}: {e}", file.external_id));
    let doc = |external_id: String, title: String, content| SourceDoc {
        external_id,
        title,
        version: file.version.clone(),
        updated_at: file.updated_at,
        url: file.url.clone(),
        acl: Vec::new(),
        content,
    };
    match extension {
        "xlsx" => {
            let sheets = xlsx(&bytes).map_err(fail)?;
            Ok(sheets
                .into_iter()
                .map(|(sheet, content)| {
                    doc(
                        format!("{}#{sheet}", file.external_id),
                        format!("{} / {sheet}", file.title),
                        content,
                    )
                })
                .collect())
        }
        "docx" => Ok(vec![doc(
            file.external_id.clone(),
            file.title.clone(),
            DocContent::Markdown(docx(&bytes).map_err(fail)?),
        )]),
        _ => Ok(vec![doc(
            file.external_id.clone(),
            file.title.clone(),
            DocContent::Markdown(pdf(&bytes).map_err(fail)?),
        )]),
    }
}

/// Every non-empty tab: its first non-empty row is the header row.
fn xlsx(bytes: &[u8]) -> Result<Vec<(String, DocContent)>, String> {
    // ponytail: calamine has no decompression cap; the 8 MB input cap bounds
    // the archive, not a crafted zip bomb inside it.
    let mut book = Xlsx::new(Cursor::new(bytes)).map_err(|e| format!("not an xlsx: {e}"))?;
    let mut out = Vec::new();
    for sheet in book.sheet_names() {
        let range = book
            .worksheet_range(&sheet)
            .map_err(|e| format!("sheet {sheet}: {e}"))?;
        let mut rows = range
            .rows()
            .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>())
            .filter(|row| row.iter().any(|c| !c.trim().is_empty()));
        let Some(headers) = rows.next() else { continue };
        let rows: Vec<Vec<String>> = rows.collect();
        out.push((sheet, DocContent::Sheet { headers, rows }));
    }
    if out.is_empty() {
        return Err("the workbook has no data".into());
    }
    Ok(out)
}

/// `word/document.xml` as markdown: paragraphs, `Heading N`/`Title` styles
/// as `#` headings, tabs and breaks kept.
fn docx(bytes: &[u8]) -> Result<String, String> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not a docx: {e}"))?;
    let mut xml = Vec::new();
    archive
        .by_name("word/document.xml")
        .map_err(|e| format!("not a docx: {e}"))?
        .take(MAX_CONTENT_BYTES as u64 + 1)
        .read_to_end(&mut xml)
        .map_err(|e| format!("not a docx: {e}"))?;
    if xml.len() > MAX_CONTENT_BYTES {
        return Err(format!("the text is larger than {MAX_CONTENT_BYTES} bytes"));
    }
    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    let (mut out, mut paragraph, mut level) = (String::new(), String::new(), 0usize);
    let mut in_text = false;
    let mut buf = Vec::new();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| format!("bad document.xml: {e}"))?;
        match event {
            Event::Start(e) if e.name().as_ref() == b"w:t" => in_text = true,
            Event::End(e) if e.name().as_ref() == b"w:t" => in_text = false,
            Event::Text(t) if in_text => {
                paragraph.push_str(&t.decode().map_err(|e| e.to_string())?);
            }
            Event::GeneralRef(r) if in_text => {
                let name = r.decode().map_err(|e| e.to_string())?;
                if let Some(c) = r.resolve_char_ref().map_err(|e| e.to_string())? {
                    paragraph.push(c);
                } else if let Some(s) = quick_xml::escape::resolve_predefined_entity(&name) {
                    paragraph.push_str(s);
                }
            }
            Event::Empty(e) => match e.name().as_ref() {
                b"w:tab" => paragraph.push('\t'),
                b"w:br" => paragraph.push('\n'),
                b"w:pStyle" => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            level = heading_level(&String::from_utf8_lossy(&attr.value));
                        }
                    }
                }
                _ => {}
            },
            Event::End(e) if e.name().as_ref() == b"w:p" => {
                let text = paragraph.trim();
                if !text.is_empty() {
                    if level > 0 {
                        out.push_str(&"#".repeat(level));
                        out.push(' ');
                    }
                    out.push_str(text);
                    out.push_str("\n\n");
                }
                (paragraph, level) = (String::new(), 0);
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if out.trim().is_empty() {
        return Err("the document has no text".into());
    }
    Ok(out)
}

/// `Title` → 1, `Heading2` / `heading 2` → 2, anything else → 0.
fn heading_level(style: &str) -> usize {
    let style = style.to_lowercase().replace(' ', "");
    if style == "title" {
        return 1;
    }
    style
        .strip_prefix("heading")
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| (1..=6).contains(n))
        .unwrap_or(0)
}

/// Every page's text layer, pages separated by a blank line.
fn pdf(bytes: &[u8]) -> Result<String, String> {
    let doc = lopdf::Document::load_mem(bytes).map_err(|e| format!("not a PDF: {e}"))?;
    if doc.is_encrypted() {
        return Err("the PDF is encrypted".into());
    }
    let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
    let text = doc
        .extract_text_with_limit(&pages, MAX_CONTENT_BYTES)
        .map_err(|e| format!("can't read the PDF's text: {e}"))?;
    if text.trim().is_empty() {
        return Err("the PDF has no text layer (a scan?)".into());
    }
    Ok(text)
}

fn invalid(message: String) -> Error {
    Error::InvalidInput(message)
}

/// Test fixtures: tiny valid xlsx, docx and PDF files (used by the engine's
/// tests too).
#[doc(hidden)]
pub mod fixtures {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;

    /// A zip of `(path, text)` entries.
    pub fn zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (path, text) in entries {
            // Writes only fail on I/O, and this is in memory.
            if out.start_file(*path, options).is_ok() {
                let _ = out.write_all(text.as_bytes());
            }
        }
        out.finish().map(Cursor::into_inner).unwrap_or_default()
    }

    /// An xlsx with one tab named `sheet` holding `rows` of inline strings.
    pub fn xlsx(sheet: &str, rows: &[&[&str]]) -> Vec<u8> {
        let mut data = String::new();
        for (r, row) in rows.iter().enumerate() {
            data.push_str(&format!("<row r=\"{}\">", r + 1));
            for (c, cell) in row.iter().enumerate() {
                let col = char::from(b'A' + u8::try_from(c).unwrap_or(0));
                data.push_str(&format!(
                    "<c r=\"{col}{}\" t=\"inlineStr\"><is><t>{cell}</t></is></c>",
                    r + 1
                ));
            }
            data.push_str("</row>");
        }
        let main = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
        let rel = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
        zip(&[
            (
                "[Content_Types].xml",
                "<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
                 <Default Extension=\"xml\" ContentType=\"application/xml\"/></Types>",
            ),
            (
                "_rels/.rels",
                &format!(
                    "<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
                     <Relationship Id=\"rId1\" Type=\"{rel}/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>"
                ),
            ),
            (
                "xl/workbook.xml",
                &format!(
                    "<?xml version=\"1.0\"?><workbook xmlns=\"{main}\" xmlns:r=\"{rel}\"><sheets>\
                     <sheet name=\"{sheet}\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>"
                ),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                &format!(
                    "<?xml version=\"1.0\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
                     <Relationship Id=\"rId1\" Type=\"{rel}/worksheet\" Target=\"worksheets/sheet1.xml\"/></Relationships>"
                ),
            ),
            (
                "xl/worksheets/sheet1.xml",
                &format!(
                    "<?xml version=\"1.0\"?><worksheet xmlns=\"{main}\"><sheetData>{data}</sheetData></worksheet>"
                ),
            ),
        ])
    }

    /// A docx of `(style, text)` paragraphs (`""` = body text).
    pub fn docx(paragraphs: &[(&str, &str)]) -> Vec<u8> {
        let mut body = String::new();
        for (style, text) in paragraphs {
            let props = if style.is_empty() {
                String::new()
            } else {
                format!("<w:pPr><w:pStyle w:val=\"{style}\"/></w:pPr>")
            };
            body.push_str(&format!("<w:p>{props}<w:r><w:t>{text}</w:t></w:r></w:p>"));
        }
        zip(&[(
            "word/document.xml",
            &format!(
                "<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
                 <w:body>{body}</w:body></w:document>"
            ),
        )])
    }

    /// A one-page PDF showing `text` in Helvetica.
    pub fn pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 12 Tf 72 712 Td ({text}) Tj ET");
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_owned(),
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
                .to_owned(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{object}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for offset in offsets {
            out.extend(format!("{offset:010} 00000 n \n").bytes());
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, bytes: &[u8]) -> SourceFile {
        SourceFile {
            external_id: name.into(),
            title: "Spec".into(),
            version: "v1".into(),
            updated_at: None,
            url: None,
            content_base64: BASE64.encode(bytes),
        }
    }

    #[test]
    fn docx_and_pdf_become_markdown() {
        let docx = fixtures::docx(&[
            ("Title", "Checkout"),
            ("Heading2", "Refunds"),
            ("", "Within 5 days &amp; free"),
        ]);
        let docs = convert(file("spec.docx", &docx)).unwrap();
        assert_eq!(
            docs[0].content,
            DocContent::Markdown("# Checkout\n\n## Refunds\n\nWithin 5 days & free\n\n".into())
        );
        let docs = convert(file("prd.pdf", &fixtures::pdf("Refunds take 5 days"))).unwrap();
        let DocContent::Markdown(text) = &docs[0].content else {
            panic!()
        };
        assert!(text.contains("Refunds take 5 days"), "{text:?}");
    }

    #[test]
    fn bad_files_are_refused() {
        assert!(convert(file("a.png", b"x")).is_err());
        assert!(convert(file("a.pdf", b"not a pdf")).is_err());
        assert!(convert(file("a.docx", b"PK nope")).is_err());
        let mut bad = file("a.xlsx", b"");
        bad.content_base64 = "!!".into();
        assert!(convert(bad).unwrap_err().to_string().contains("base64"));
    }
}

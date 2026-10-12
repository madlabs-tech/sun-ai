//! Content gates: checks over a file the step produced, run in the
//! engine. Pure: the caller fetches the text (and resolves links) on the host.

use std::collections::HashSet;
use std::path::{Component, Path};

use serde::Deserialize;
use sun_core::{Result, RunId};
use sun_store::{ContextRepo, EventLog, KnowledgeRepo, Tx};

/// A content gate with its validated params (`gates: [{terms: {file: …}}]`).
/// The `gate` tag is the gate name; unknown params are rejected.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "gate", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Check {
    /// Every listed Markdown heading exists (case-insensitive, any level).
    SectionsPresent { file: String, sections: Vec<String> },
    /// Word and character limits.
    Length {
        file: String,
        min_words: Option<usize>,
        max_words: Option<usize>,
        min_chars: Option<usize>,
        max_chars: Option<usize>,
    },
    /// The file is well-formed `kind` (default: from its extension).
    Format { file: String, kind: Option<Format> },
    /// Every http(s) link answers 2xx/3xx (checked from the host).
    LinksResolve { file: String },
    /// Banned terms are absent and required terms present (case-insensitive),
    /// e.g. a brand glossary.
    Terms {
        file: String,
        #[serde(default)]
        banned: Vec<String>,
        #[serde(default)]
        required: Vec<String>,
    },
    /// Image limits: pixel size (PNG, GIF, JPEG) and file size in bytes.
    ImageSize {
        file: String,
        min_width: Option<u32>,
        max_width: Option<u32>,
        min_height: Option<u32>,
        max_height: Option<u32>,
        max_bytes: Option<u64>,
    },
    /// The file cites at least one knowledge doc the run's context packs
    /// gave: its `doc:<id>`, or its title with its version or
    /// date. Passes when no doc was given. See [`cites`].
    CitesSources { file: String },
}

/// File formats the `format` gate understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Format {
    Markdown,
    Csv,
    Json,
}

impl Check {
    /// Parses `params` of content gate `name`; `Err` is a config error.
    pub(crate) fn parse(name: &str, params: Option<&serde_json::Value>) -> Result<Self, String> {
        let mut object = match params {
            Some(serde_json::Value::Object(map)) => map.clone(),
            Some(_) => return Err(format!("`{name}` params must be a map")),
            None => serde_json::Map::new(),
        };
        object.insert("gate".into(), name.into());
        let check: Self =
            serde_json::from_value(object.into()).map_err(|e| format!("`{name}` params: {e}"))?;
        check_path(check.file())?;
        if let Self::Format { file, kind: None } = &check {
            Format::from_extension(file)?;
        }
        Ok(check)
    }

    /// The file to check, relative to the task worktree.
    pub(crate) fn file(&self) -> &str {
        match self {
            Self::SectionsPresent { file, .. }
            | Self::Length { file, .. }
            | Self::Format { file, .. }
            | Self::LinksResolve { file }
            | Self::Terms { file, .. }
            | Self::ImageSize { file, .. }
            | Self::CitesSources { file } => file,
        }
    }

    /// The host command whose output [`Self::run`] reads: the file itself,
    /// or for an image its byte count and first [`IMAGE_HEAD`] bytes in hex
    /// (the exec reply is text).
    pub(crate) fn command(&self) -> String {
        let file = super::quote(self.file());
        match self {
            Self::ImageSize { .. } => {
                format!("wc -c < {file} && head -c {IMAGE_HEAD} -- {file} | od -An -v -tx1")
            }
            _ => format!("cat -- {file}"),
        }
    }

    /// Runs the check on `text`. `Ok` and `Err` both carry the gate output.
    /// `links_resolve` only extracts here; see [`links`]. `cites_sources`
    /// here has no sources (it passes); the gate runs [`cites`].
    pub(crate) fn run(&self, text: &str) -> Result<String, String> {
        match self {
            Self::SectionsPresent { sections, .. } => {
                let headings: Vec<String> = text.lines().filter_map(heading).collect();
                let missing: Vec<&str> = sections
                    .iter()
                    .filter(|s| !headings.contains(&s.trim().to_lowercase()))
                    .map(String::as_str)
                    .collect();
                if missing.is_empty() {
                    Ok(format!("all {} sections present", sections.len()))
                } else {
                    Err(format!("missing sections: {}", missing.join(", ")))
                }
            }
            Self::Length {
                min_words,
                max_words,
                min_chars,
                max_chars,
                ..
            } => {
                let words = text.split_whitespace().count();
                let chars = text.chars().count();
                let mut problems = Vec::new();
                for (value, unit, min, max) in [
                    (words, "words", *min_words, *max_words),
                    (chars, "chars", *min_chars, *max_chars),
                ] {
                    if let Some(min) = min.filter(|&min| value < min) {
                        problems.push(format!("too short: {value} {unit} < {min}"));
                    }
                    if let Some(max) = max.filter(|&max| value > max) {
                        problems.push(format!("too long: {value} {unit} > {max}"));
                    }
                }
                let summary = format!("{words} words, {chars} chars");
                if problems.is_empty() {
                    Ok(summary)
                } else {
                    Err(format!("{summary}; {}", problems.join("; ")))
                }
            }
            Self::Format { file, kind } => {
                let kind = match kind {
                    Some(kind) => *kind,
                    None => Format::from_extension(file)?,
                };
                kind.validate(text)
            }
            Self::LinksResolve { .. } => Ok(format!("{} links", links(text).len())),
            Self::CitesSources { .. } => cites(text, &[]),
            Self::Terms {
                banned, required, ..
            } => {
                let lower = text.to_lowercase();
                let has = |term: &String| lower.contains(&term.to_lowercase());
                let found: Vec<&str> = banned
                    .iter()
                    .filter(|t| has(t))
                    .map(String::as_str)
                    .collect();
                let absent: Vec<&str> = required
                    .iter()
                    .filter(|t| !has(t))
                    .map(String::as_str)
                    .collect();
                let mut problems = Vec::new();
                if !found.is_empty() {
                    problems.push(format!("banned terms used: {}", found.join(", ")));
                }
                if !absent.is_empty() {
                    problems.push(format!("required terms missing: {}", absent.join(", ")));
                }
                if problems.is_empty() {
                    Ok("terms ok".into())
                } else {
                    Err(problems.join("; "))
                }
            }
            Self::ImageSize {
                file,
                min_width,
                max_width,
                min_height,
                max_height,
                max_bytes,
            } => {
                let (bytes, head) = image_head(text)?;
                let mut problems = Vec::new();
                if let Some(max) = max_bytes.filter(|&max| bytes > max) {
                    problems.push(format!("too big: {bytes} bytes > {max}"));
                }
                let wants_pixels = [min_width, max_width, min_height, max_height]
                    .iter()
                    .any(|limit| limit.is_some());
                let summary = match dimensions(&head) {
                    Some((width, height)) => {
                        for (value, unit, min, max) in [
                            (width, "wide", *min_width, *max_width),
                            (height, "high", *min_height, *max_height),
                        ] {
                            if let Some(min) = min.filter(|&min| value < min) {
                                problems.push(format!("too small: {value} px {unit} < {min}"));
                            }
                            if let Some(max) = max.filter(|&max| value > max) {
                                problems.push(format!("too large: {value} px {unit} > {max}"));
                            }
                        }
                        format!("{width}×{height} px, {bytes} bytes")
                    }
                    None if wants_pixels => {
                        return Err(format!(
                            "`{file}` is not a PNG, GIF or JPEG image Sun can measure"
                        ));
                    }
                    None => format!("{bytes} bytes"),
                };
                if problems.is_empty() {
                    Ok(summary)
                } else {
                    Err(format!("{summary}; {}", problems.join("; ")))
                }
            }
        }
    }
}

/// Bytes of an image the `image_size` gate reads: enough for a PNG or GIF
/// header and a JPEG's first frame header after typical EXIF/ICC segments.
// ponytail: a JPEG whose frame header sits past 64 KiB reads as unmeasurable;
// read more (or add a host file-read frame) if real files need it.
const IMAGE_HEAD: usize = 16 * 1024;

/// Parses [`Check::command`]'s image output: `<byte count>\n<hex bytes…>`.
fn image_head(text: &str) -> Result<(u64, Vec<u8>), String> {
    let mut words = text.split_whitespace();
    let bytes = words
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .ok_or_else(|| format!("image: unexpected output: {}", text.trim()))?;
    let head = words
        .map(|hex| u8::from_str_radix(hex, 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|e| format!("image: unexpected output: {e}"))?;
    Ok((bytes, head))
}

/// Width and height of a PNG, GIF or JPEG from its first bytes.
fn dimensions(head: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| {
        Some(u32::from(u16::from_be_bytes([
            *head.get(i)?,
            *head.get(i + 1)?,
        ])))
    };
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        let be32 = |i: usize| Some(u32::from_be_bytes(head.get(i..i + 4)?.try_into().ok()?));
        return Some((be32(16)?, be32(20)?));
    }
    if head.starts_with(b"GIF8") {
        let le16 = |i: usize| {
            Some(u32::from(u16::from_le_bytes([
                *head.get(i)?,
                *head.get(i + 1)?,
            ])))
        };
        return Some((le16(6)?, le16(8)?));
    }
    if head.starts_with(&[0xFF, 0xD8]) {
        // Walk the segments to the first start-of-frame (SOF0..SOF15 but
        // DHT, JPG and DAC): `FF Cn len(2) precision(1) height(2) width(2)`.
        let mut i = 2;
        while *head.get(i)? == 0xFF {
            let marker = *head.get(i + 1)?;
            if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            i += 2 + usize::try_from(be16(i + 2)?).ok()?;
        }
    }
    None
}

impl Format {
    fn from_extension(file: &str) -> Result<Self, String> {
        let ext = Path::new(file)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase);
        match ext.as_deref() {
            Some("md" | "markdown") => Ok(Self::Markdown),
            Some("csv") => Ok(Self::Csv),
            Some("json") => Ok(Self::Json),
            // ponytail: no image sizes yet; needs bytes, not exec text.
            _ => Err(format!(
                "`format`: can't tell the format of `{file}`; set `kind: markdown | csv | json`"
            )),
        }
    }

    fn validate(self, text: &str) -> Result<String, String> {
        match self {
            Self::Markdown => {
                let fences = text
                    .lines()
                    .filter(|l| l.trim_start().starts_with("```"))
                    .count();
                if text.trim().is_empty() {
                    Err("markdown: the file is empty".into())
                } else if fences % 2 == 1 {
                    Err("markdown: a ``` code fence is never closed".into())
                } else {
                    Ok("valid markdown".into())
                }
            }
            Self::Json => serde_json::from_str::<serde_json::Value>(text)
                .map(|_| "valid json".into())
                .map_err(|e| format!("json: {e}")),
            Self::Csv => {
                let rows = csv_rows(text)?;
                let Some(width) = rows.first().copied() else {
                    return Err("csv: the file is empty".into());
                };
                match rows.iter().position(|&n| n != width) {
                    None => Ok(format!("valid csv: {} rows × {width} columns", rows.len())),
                    Some(i) => Err(format!(
                        "csv: row {} has {} columns, the header has {width}",
                        i + 1,
                        rows[i]
                    )),
                }
            }
        }
    }
}

/// A knowledge doc a run was given, as an output may cite it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Source {
    /// `doc:<id>`.
    pub pointer: String,
    pub title: String,
    pub version: String,
    /// `YYYY-MM-DD` of its last change, when known.
    pub date: Option<String>,
}

/// `cites_sources`: passes when `text` cites at least one of `sources` by
/// pointer, or by title together with its version or date (case-insensitive).
pub(crate) fn cites(text: &str, sources: &[Source]) -> std::result::Result<String, String> {
    if sources.is_empty() {
        return Ok("no knowledge docs were given to this run".into());
    }
    let lower = text.to_lowercase();
    let has = |s: &str| !s.trim().is_empty() && lower.contains(&s.trim().to_lowercase());
    let cited: Vec<&str> = sources
        .iter()
        .filter(|s| {
            has(&s.pointer)
                || (has(&s.title) && (has(&s.version) || s.date.as_deref().is_some_and(has)))
        })
        .map(|s| s.title.as_str())
        .collect();
    if cited.is_empty() {
        let given: Vec<String> = sources
            .iter()
            .map(|s| {
                let date = s
                    .date
                    .as_deref()
                    .map(|d| format!(" · {d}"))
                    .unwrap_or_default();
                format!("{} · {}{date} ({})", s.title, s.version, s.pointer)
            })
            .collect();
        Err(format!(
            "cites none of the {} knowledge docs it was given; cite each doc you relied on \
             with its title and version or date: {}",
            sources.len(),
            given.join("; ")
        ))
    } else {
        Ok(format!(
            "cites {} of {} knowledge docs: {}",
            cited.len(),
            sources.len(),
            cited.join(", ")
        ))
    }
}

/// The knowledge docs (P4) of every context pack `run_id`'s agent steps
/// were given, deduped; docs deleted since are left out.
pub(crate) async fn run_sources(tx: &mut Tx, run_id: RunId) -> Result<Vec<Source>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let events = tx.events_after(Some(run_id), 0, u32::MAX).await?;
    for event in events
        .iter()
        .filter(|e| e.kind == crate::executor::kind::CONTEXT_PACK)
    {
        let Some(id) = event
            .payload
            .get("context_pack_id")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse().ok())
        else {
            continue;
        };
        let pack = tx.get_context_pack(id).await?;
        let items = pack.items.as_array().cloned().unwrap_or_default();
        for item in items
            .iter()
            .filter(|i| i.get("priority").and_then(|p| p.as_u64()) == Some(4))
        {
            let Some(doc_id) = item
                .get("pointer")
                .and_then(|p| p.as_str())
                .and_then(|p| p.strip_prefix("doc:"))
            else {
                continue;
            };
            if !seen.insert(doc_id.to_owned()) {
                continue;
            }
            if let Ok(doc) = tx.get_doc(doc_id).await {
                out.push(Source {
                    pointer: format!("doc:{doc_id}"),
                    title: doc.title,
                    version: doc.version,
                    date: doc.updated_at.map(|at| at.format("%Y-%m-%d").to_string()),
                });
            }
        }
    }
    Ok(out)
}

/// Column count of each CSV record (RFC 4180 quoting; blank lines skipped).
fn csv_rows(text: &str) -> Result<Vec<usize>, String> {
    let mut rows = Vec::new();
    let (mut columns, mut quoted, mut empty) = (1, false, true);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => columns += 1,
            '\n' if !quoted => {
                if !empty || columns > 1 {
                    rows.push(columns);
                }
                (columns, empty) = (1, true);
                continue;
            }
            '\r' if !quoted => continue,
            _ => {}
        }
        empty = false;
    }
    if quoted {
        return Err("csv: a quoted field is never closed".into());
    }
    if !empty || columns > 1 {
        rows.push(columns);
    }
    Ok(rows)
}

/// The lowercase text of a Markdown ATX heading line (`## Risks ##` → `risks`).
fn heading(line: &str) -> Option<String> {
    let rest = line.trim_start().trim_start_matches('#');
    let level = line.trim_start().len() - rest.len();
    if !(1..=6).contains(&level) || !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    Some(rest.trim().trim_end_matches('#').trim().to_lowercase())
}

/// The distinct http(s) URLs in `text`, in order of appearance.
pub(crate) fn links(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(at) = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| rest.find(scheme))
        .min()
    {
        let tail = &rest[at..];
        let end = tail
            .find(|c: char| c.is_whitespace() || "<>\"'`()[]{}|\\^".contains(c))
            .unwrap_or(tail.len());
        let url = tail[..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
        if url.len() > "https://".len() && !out.iter().any(|u| u == url) {
            out.push(url.to_owned());
        }
        rest = &tail[end.max(1)..];
    }
    out
}

/// A gate file must stay inside the worktree (trust boundary: workflow YAML).
fn check_path(file: &str) -> Result<(), String> {
    let path = Path::new(file);
    let inside = !file.is_empty()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if inside {
        Ok(())
    } else {
        Err(format!(
            "`file: {file}` must be a relative path inside the worktree"
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn check(name: &str, params: serde_json::Value) -> Check {
        Check::parse(name, Some(&params)).unwrap()
    }

    #[test]
    fn params_are_validated() {
        assert!(Check::parse("terms", None).is_err(), "file is required");
        let unknown = Check::parse("terms", Some(&json!({"file": "a.md", "bogus": 1})));
        assert!(unknown.unwrap_err().contains("bogus"));
        for bad in ["../etc/passwd", "/etc/passwd", ""] {
            assert!(
                Check::parse("terms", Some(&json!({ "file": bad }))).is_err(),
                "{bad}"
            );
        }
        assert!(Check::parse("format", Some(&json!({"file": "logo.png"}))).is_err());
        assert_eq!(
            check("links_resolve", json!({"file": "./a.md"})).file(),
            "./a.md"
        );
    }

    #[test]
    fn role_templates_have_valid_content_gate_params() {
        for template in sun_workflow::TEMPLATES {
            let workflow = sun_workflow::parse(template.source_yaml).unwrap();
            for step in &workflow.steps {
                if let sun_workflow::StepKind::Gate { gates, .. } = &step.kind {
                    for gate in gates
                        .iter()
                        .filter(|g| sun_workflow::CONTENT_GATES.contains(&g.name.as_str()))
                    {
                        Check::parse(&gate.name, gate.params.as_ref())
                            .unwrap_or_else(|e| panic!("{}: {e}", template.id));
                    }
                }
            }
        }
    }

    #[test]
    fn sections_present() {
        let gate = check(
            "sections_present",
            json!({"file": "a.md", "sections": ["Summary", "Risks"]}),
        );
        assert!(gate.run("# Plan\n## summary ##\ntext\n### Risks\n").is_ok());
        let err = gate.run("# Summary\n#Risks\n").unwrap_err();
        assert_eq!(err, "missing sections: Risks");
    }

    #[test]
    fn length_limits() {
        let gate = check(
            "length",
            json!({"file": "a.md", "min_words": 3, "max_chars": 12}),
        );
        assert!(
            gate.run("one two three")
                .unwrap_err()
                .contains("too long: 13 chars > 12")
        );
        assert!(
            gate.run("one two")
                .unwrap_err()
                .contains("too short: 2 words < 3")
        );
        assert_eq!(gate.run("a b c").unwrap(), "3 words, 5 chars");
    }

    #[test]
    fn formats() {
        let csv = check("format", json!({"file": "a.csv"}));
        assert!(csv.run("a,b\n\"x, y\",2\n\"q\"\"\",3\r\n").is_ok());
        assert!(csv.run("a,b\n1,2,3\n").unwrap_err().contains("row 2 has 3"));
        assert!(csv.run("a,\"b\n").is_err());
        let json_gate = check("format", json!({"file": "out.txt", "kind": "json"}));
        assert!(json_gate.run("{\"a\": 1}").is_ok());
        assert!(json_gate.run("{a: 1}").is_err());
        let md = check("format", json!({"file": "a.md"}));
        assert!(md.run("# T\n```rs\nx\n```\n").is_ok());
        assert!(md.run("```\nx\n").is_err());
        assert!(md.run("  \n").is_err());
    }

    #[test]
    fn terms() {
        let gate = check(
            "terms",
            json!({"file": "a.md", "banned": ["cheap"], "required": ["Sun"]}),
        );
        assert!(gate.run("Meet SUN, the engine.").is_ok());
        let err = gate.run("A cheap engine.").unwrap_err();
        assert_eq!(err, "banned terms used: cheap; required terms missing: Sun");
    }

    /// `command()`'s output for an image of `bytes` whose head is `head`.
    fn image_output(bytes: usize, head: &[u8]) -> String {
        let hex: Vec<String> = head.iter().map(|b| format!("{b:02x}")).collect();
        format!("    {bytes}\n {}\n", hex.join(" "))
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut head = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        head.extend(width.to_be_bytes());
        head.extend(height.to_be_bytes());
        head.extend([8, 6, 0, 0, 0]);
        head
    }

    #[test]
    fn oversize_png_fails() {
        let gate = check(
            "image_size",
            json!({"file": "hero.png", "max_width": 1200, "max_height": 630, "max_bytes": 500_000}),
        );
        assert!(gate.command().contains("'hero.png'"));
        assert_eq!(
            gate.run(&image_output(48_213, &png(1200, 630))).unwrap(),
            "1200×630 px, 48213 bytes"
        );
        assert_eq!(
            gate.run(&image_output(912_000, &png(2400, 630)))
                .unwrap_err(),
            "2400×630 px, 912000 bytes; too big: 912000 bytes > 500000; \
             too large: 2400 px wide > 1200"
        );
        let small = check("image_size", json!({"file": "a.gif", "min_height": 100}));
        let gif = [b"GIF89a".as_slice(), &[0x40, 0x01, 0x20, 0x00]].concat();
        assert_eq!(
            small.run(&image_output(900, &gif)).unwrap_err(),
            "320×32 px, 900 bytes; too small: 32 px high < 100"
        );
        // JPEG: APP0 segment, then SOF0 with height 480, width 640.
        let jpeg = [
            &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00][..],
            &[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0xE0, 0x02, 0x80],
        ]
        .concat();
        assert_eq!(
            small.run(&image_output(10, &jpeg)).unwrap(),
            "640×480 px, 10 bytes"
        );
        assert!(small.run(&image_output(3, b"abc")).is_err(), "not an image");
        let bytes_only = check("image_size", json!({"file": "a.svg", "max_bytes": 5}));
        assert_eq!(bytes_only.run(&image_output(3, b"<sv")).unwrap(), "3 bytes");
    }

    /// An output that relies on a doc must cite it.
    #[test]
    fn uncited_output_fails_cites_sources() {
        let gate = check("cites_sources", json!({"file": "prd.md"}));
        let sources = [Source {
            pointer: "doc:01J".into(),
            title: "Pricing PRD".into(),
            version: "v7".into(),
            date: Some("2026-09-30".into()),
        }];
        let uncited = cites("Plans cost 10 USD.", &sources).unwrap_err();
        assert!(
            uncited.contains("Pricing PRD · v7 · 2026-09-30 (doc:01J)"),
            "{uncited}"
        );
        assert!(cites("Per the pricing prd (v7), plans cost 10 USD.", &sources).is_ok());
        assert!(cites("Source: Pricing PRD · 2026-09-30", &sources).is_ok());
        assert!(cites("See doc:01J.", &sources).is_ok());
        assert!(
            cites("Pricing PRD says so.", &sources).is_err(),
            "title alone"
        );
        assert!(gate.run("no docs given").is_ok());
    }

    #[test]
    fn links_are_extracted() {
        let text = "See [docs](https://a.dev/x?q=1), <http://b.io>. Again https://a.dev/x?q=1.\n\
                    `https://c.dev/y` and https:// alone";
        assert_eq!(
            links(text),
            ["https://a.dev/x?q=1", "http://b.io", "https://c.dev/y"]
        );
    }
}

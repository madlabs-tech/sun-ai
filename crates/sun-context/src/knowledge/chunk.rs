//! Normalize and chunk. Pure: text in, chunks out.
//!
//! - Markdown is split on ATX headings (not inside code fences); each chunk
//!   keeps its heading path. Sections over [`MAX_TOKENS`] split on paragraphs,
//!   then lines, then chars; neighbours under [`MIN_TOKENS`] merge while the
//!   result fits, keeping the headings they share (a headings-only parent
//!   takes its child's path).
//! - Sheets stay markdown tables: rows are grouped up to [`MAX_TOKENS`] and
//!   every chunk repeats the header row, so each one is self-describing.

use sun_store::DocChunk;

use crate::pack::{BYTES_PER_TOKEN, estimate_tokens};

/// Chunks smaller than this merge with the next section when they fit.
pub const MIN_TOKENS: u32 = 400;
/// No chunk is larger than this (except one sheet row that alone is larger).
pub const MAX_TOKENS: u32 = 800;
const MAX_BYTES: usize = MAX_TOKENS as usize * BYTES_PER_TOKEN;

/// Line endings to `\n`, no BOM, no trailing spaces, at most one blank line in a row.
pub(crate) fn normalize_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = false;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim_end();
        if line.is_empty() {
            if blank || out.is_empty() {
                continue;
            }
            blank = true;
        } else {
            blank = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Heading-aware chunks of normalized markdown.
pub(crate) fn chunk_markdown(markdown: &str) -> Vec<DocChunk> {
    let mut chunks: Vec<(Vec<String>, String)> = Vec::new();
    for (path, text) in sections(markdown) {
        for piece in split_to_fit(&text) {
            if let Some((last_path, last)) = chunks.last_mut()
                && estimate_tokens(last) < MIN_TOKENS
                && last.len() + piece.len() <= MAX_BYTES
            {
                // Headings-only parents give way to the child's path;
                // siblings with content keep the headings they share.
                if last
                    .lines()
                    .all(|l| l.trim().is_empty() || heading(l).is_some())
                {
                    last_path.clone_from(&path);
                } else {
                    let shared = last_path
                        .iter()
                        .zip(&path)
                        .take_while(|(a, b)| a == b)
                        .count();
                    last_path.truncate(shared);
                }
                last.push_str(&piece);
            } else {
                chunks.push((path.clone(), piece));
            }
        }
    }
    chunks
        .into_iter()
        .map(|(path, body)| (path, body.trim_end().to_owned()))
        .filter(|(_, body)| !body.is_empty())
        .map(|(path, body)| doc_chunk(path.join(" > "), body))
        .collect()
}

/// A sheet as markdown tables of grouped rows, header repeated in each.
pub(crate) fn chunk_sheet(headers: &[String], rows: &[Vec<String>]) -> Vec<DocChunk> {
    let header = format!(
        "{}|{}\n",
        table_row(headers),
        " --- |".repeat(headers.len().max(1))
    );
    let mut chunks = Vec::new();
    let mut body = header.clone();
    let mut first = 1;
    for (i, row) in rows.iter().enumerate() {
        let line = table_row(row);
        if body.len() > header.len() && body.len() + line.len() > MAX_BYTES {
            chunks.push(doc_chunk(format!("rows {first}-{i}"), body));
            body = header.clone();
            first = i + 1;
        }
        body.push_str(&line);
    }
    if body.len() > header.len() || chunks.is_empty() {
        chunks.push(doc_chunk(format!("rows {first}-{}", rows.len()), body));
    }
    chunks
}

fn table_row(cells: &[String]) -> String {
    let cells: Vec<String> = cells
        .iter()
        .map(|c| c.replace('|', "\\|").replace(['\r', '\n'], " "))
        .collect();
    format!("| {} |\n", cells.join(" | "))
}

fn doc_chunk(heading_path: String, body: String) -> DocChunk {
    DocChunk {
        token_estimate: estimate_tokens(&body),
        heading_path,
        body,
    }
}

/// `(heading path, text)` per heading; text before the first heading has an
/// empty path. Each section's text starts with its heading line.
fn sections(markdown: &str) -> Vec<(Vec<String>, String)> {
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut out = vec![(Vec::new(), String::new())];
    let mut fence: Option<&str> = None;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m));
        match (fence, marker) {
            (Some(open), Some(m)) if open == m => fence = None,
            (None, Some(m)) => fence = Some(m),
            (None, None) => {
                if let Some((level, title)) = heading(line) {
                    stack.retain(|(l, _)| *l < level);
                    stack.push((level, title));
                    out.push((
                        stack.iter().map(|(_, t)| t.clone()).collect(),
                        String::new(),
                    ));
                }
            }
            _ => {}
        }
        if let Some((_, text)) = out.last_mut() {
            text.push_str(line);
            text.push('\n');
        }
    }
    out.retain(|(_, text)| !text.trim().is_empty());
    out
}

/// `# Title` → `(1, "Title")`; up to six `#`, then a space or nothing.
fn heading(line: &str) -> Option<(usize, String)> {
    let level = line.bytes().take_while(|b| *b == b'#').count();
    let rest = &line[level..];
    if !(1..=6).contains(&level) || !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    (!title.is_empty()).then(|| (level, title.to_owned()))
}

/// Pieces of at most [`MAX_BYTES`]: on paragraph, then line, then char boundaries.
fn split_to_fit(text: &str) -> Vec<String> {
    if text.len() <= MAX_BYTES {
        return vec![text.to_owned()];
    }
    for separator in ["\n\n", "\n"] {
        let parts: Vec<&str> = text.split_inclusive(separator).collect();
        if parts.len() > 1 {
            let mut out: Vec<String> = Vec::new();
            for piece in parts.into_iter().flat_map(split_to_fit) {
                match out.last_mut() {
                    Some(last) if last.len() + piece.len() <= MAX_BYTES => last.push_str(&piece),
                    _ => out.push(piece),
                }
            }
            return out;
        }
    }
    let mut out = vec![String::new()];
    for c in text.chars() {
        if out
            .last()
            .is_some_and(|last| last.len() + c.len_utf8() > MAX_BYTES)
        {
            out.push(String::new());
        }
        if let Some(last) = out.last_mut() {
            last.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(word: &str, tokens: usize) -> String {
        format!(
            "{}\n\n",
            format!("{word} ").repeat(tokens * BYTES_PER_TOKEN / 5)
        )
    }

    #[test]
    fn knowledge_chunks_follow_headings_and_size_limits() {
        let md = format!(
            "# PRD\nIntro.\n## Pricing\n{}### IDR\nRp 50.000 per seat.\n```\n# not a heading\n```\n\
             ### USD\n$5 per seat.\n## Roadmap\n{}{}",
            para("cost", 300),
            para("late", 700),
            para("late", 700),
        );
        let chunks = chunk_markdown(&normalize_markdown(&md));
        for c in &chunks {
            assert!(c.token_estimate <= MAX_TOKENS, "{c:?}");
            assert!(!c.heading_path.contains("not a heading"));
        }
        // Small siblings merge under the headings they share.
        assert_eq!(chunks[0].heading_path, "PRD");
        assert!(chunks[0].body.contains("### IDR") && chunks[0].body.contains("$5 per seat."));
        // The oversized section splits on paragraphs and keeps its path.
        let roadmap: Vec<_> = chunks
            .iter()
            .filter(|c| c.heading_path == "PRD > Roadmap")
            .collect();
        assert_eq!(roadmap.len(), 2);
        assert!(roadmap.iter().all(|c| c.token_estimate >= MIN_TOKENS));
    }

    #[test]
    fn knowledge_giant_lines_split_on_char_boundaries() {
        let chunks = chunk_markdown(&"é".repeat(MAX_BYTES));
        assert_eq!(chunks.len(), 2);
        let joined: String = chunks.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(joined, "é".repeat(MAX_BYTES));
    }

    #[test]
    fn knowledge_sheets_repeat_the_header_in_every_chunk() {
        let headers = vec!["plan".to_owned(), "price|idr".to_owned()];
        let rows: Vec<Vec<String>> = (0..200)
            .map(|i| vec![format!("plan {i}"), format!("{} 000", i * 10)])
            .collect();
        let chunks = chunk_sheet(&headers, &rows);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(
                c.body
                    .starts_with("| plan | price\\|idr |\n| --- | --- |\n")
            );
            assert!(c.token_estimate <= MAX_TOKENS);
        }
        assert_eq!(chunks[0].heading_path.split('-').next(), Some("rows 1"));
        assert_eq!(
            chunks.last().map(|c| c.heading_path.ends_with("-200")),
            Some(true)
        );
        let rows_seen: usize = chunks.iter().map(|c| c.body.lines().count() - 2).sum();
        assert_eq!(rows_seen, 200);
    }
}

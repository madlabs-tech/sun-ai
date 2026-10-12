//! Stale and conflicting source flags, rule-based:
//! - **Stale:** a doc last changed more than its source's `max_age_days`
//!   ([`DEFAULT_MAX_AGE_DAYS`] when unset, `0` = never) ago.
//! - **Conflict:** chunks of different docs under the same heading path whose
//!   numbers or dates differ. Sun flags them on the task instead of picking one.
//!
//! [`flag`] is pure (the clock is a parameter).

use std::collections::BTreeSet;

use serde::Serialize;
use sun_core::Timestamp;
use sun_store::ChunkHit;

use super::citation;
use crate::KnowledgeHit;

/// Age after which a doc is flagged stale when its source sets no
/// `max_age_days` in its config.
pub const DEFAULT_MAX_AGE_DAYS: i64 = 180;
/// Differing values quoted per doc in a conflict message.
const MAX_VALUES: usize = 4;

/// What kind of problem a [`KnowledgeFlag`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagKind {
    /// Older than the source's age limit.
    Stale,
    /// Disagrees with another doc on the same heading.
    Conflict,
}

/// A stale or conflicting source, raised on the task instead of choosing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KnowledgeFlag {
    /// Stale or conflict.
    pub kind: FlagKind,
    /// Plain-language finding.
    pub message: String,
    /// The chunks involved: `title > heading · version · date · url`.
    pub citations: Vec<String>,
    /// `doc:<id>` of each, in the same order.
    pub pointers: Vec<String>,
}

/// Flags among `candidates` (ranked chunks) of the docs in `docs` (the
/// selected doc ids, best first): stale docs first, then conflicts.
pub fn flag(candidates: &[ChunkHit], docs: &[String], now: Timestamp) -> Vec<KnowledgeFlag> {
    // Best chunk of each selected doc, in rank order.
    let best: Vec<&ChunkHit> = docs
        .iter()
        .filter_map(|doc| candidates.iter().find(|c| &c.doc_id == doc))
        .collect();
    let mut flags: Vec<KnowledgeFlag> = best.iter().filter_map(|hit| stale(hit, now)).collect();

    // One chunk per (heading, doc), best first, grouped by heading in order.
    let mut groups: Vec<(String, Vec<&ChunkHit>)> = Vec::new();
    for hit in candidates.iter().filter(|c| docs.contains(&c.doc_id)) {
        let Some(key) = heading_key(&hit.heading_path) else {
            continue;
        };
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, hits)) if hits.iter().any(|h| h.doc_id == hit.doc_id) => {}
            Some((_, hits)) => hits.push(hit),
            None => groups.push((key, vec![hit])),
        }
    }
    flags.extend(groups.iter().filter_map(|(_, hits)| conflict(hits)));
    flags
}

/// Every flag of `hits` once, in order: what the pack and the task show.
pub fn unique_flags(hits: &[KnowledgeHit]) -> Vec<KnowledgeFlag> {
    let mut out: Vec<KnowledgeFlag> = Vec::new();
    for flag in hits.iter().flat_map(|h| &h.flags) {
        if !out.contains(flag) {
            out.push(flag.clone());
        }
    }
    out
}

fn stale(hit: &ChunkHit, now: Timestamp) -> Option<KnowledgeFlag> {
    let max_days = hit.max_age_days.unwrap_or(DEFAULT_MAX_AGE_DAYS);
    let updated = hit.updated_at?;
    let age_days = (now - updated).num_days();
    (max_days > 0 && age_days > max_days).then(|| KnowledgeFlag {
        kind: FlagKind::Stale,
        message: format!(
            "{} was last changed on {}, {age_days} days ago (the limit is {max_days} days). \
             Check it is still current.",
            hit.title,
            updated.format("%Y-%m-%d"),
        ),
        citations: vec![citation(hit)],
        pointers: vec![format!("doc:{}", hit.doc_id)],
    })
}

fn conflict(hits: &[&ChunkHit]) -> Option<KnowledgeFlag> {
    let numbered: Vec<(&ChunkHit, BTreeSet<String>)> = hits
        .iter()
        .map(|h| (*h, numbers(&h.body)))
        .filter(|(_, n)| !n.is_empty())
        .collect();
    if numbered.len() < 2 || numbered.iter().all(|(_, n)| *n == numbered[0].1) {
        return None;
    }
    let shared: BTreeSet<&String> = numbered[0]
        .1
        .iter()
        .filter(|v| numbered.iter().all(|(_, n)| n.contains(*v)))
        .collect();
    let says: Vec<String> = numbered
        .iter()
        .map(|(hit, values)| {
            let own: Vec<&str> = values
                .iter()
                .filter(|v| !shared.contains(v))
                .take(MAX_VALUES)
                .map(String::as_str)
                .collect();
            let own = if own.is_empty() {
                "fewer values".to_owned()
            } else {
                own.join(", ")
            };
            format!("{} says {own}", hit.title)
        })
        .collect();
    Some(KnowledgeFlag {
        kind: FlagKind::Conflict,
        message: format!(
            "Sources disagree on \"{}\": {}. Confirm which one is current before using it.",
            heading_tail(&numbered[0].0.heading_path),
            says.join("; "),
        ),
        citations: numbered.iter().map(|(h, _)| citation(h)).collect(),
        pointers: numbered
            .iter()
            .map(|(h, _)| format!("doc:{}", h.doc_id))
            .collect(),
    })
}

/// What "the same heading" means across docs: the path without its top
/// segment (usually the doc's own title), case- and space-folded. Sheet row
/// ranges and headingless chunks never match.
// ponytail: exact path match after dropping the H1; fuzzy heading matching
// (or the decision engine's confidence) when this misses real pairs.
fn heading_key(path: &str) -> Option<String> {
    if path.is_empty() || path.starts_with("rows ") {
        return None;
    }
    let folded: Vec<&str> = heading_tail(path).split_whitespace().collect();
    Some(folded.join(" ").to_lowercase())
}

/// `path` without its top segment, unless it has only one.
fn heading_tail(path: &str) -> &str {
    path.split_once(" > ").map_or(path, |(_, tail)| tail)
}

/// Numbers and dates in `text`: digit runs joined by `. , : / -` (so
/// `99.000`, `1,200`, `2026-01-05` stay whole). Digits glued to a letter
/// (`h1`, `S3`, `v2`) don't count.
fn numbers(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut current = String::new();
    let mut previous = ' ';
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let next_is_digit = chars.peek().is_some_and(char::is_ascii_digit);
        let starts = c.is_ascii_digit() && !previous.is_alphanumeric();
        let continues = !current.is_empty()
            && (c.is_ascii_digit() || matches!(c, '.' | ',' | ':' | '/' | '-') && next_is_digit);
        if starts || continues {
            current.push(c);
        } else if !current.is_empty() {
            if !c.is_alphabetic() {
                out.insert(std::mem::take(&mut current));
            }
            current.clear();
        }
        previous = c;
    }
    if !current.is_empty() {
        out.insert(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_numbers_skip_words_and_keep_dates_whole() {
        let found = numbers("IDR 99.000/mo from 2026-01-05, h1 v23 S3 1,200 users; 15% off 3x");
        let found: Vec<&str> = found.iter().map(String::as_str).collect();
        assert_eq!(found, ["1,200", "15", "2026-01-05", "99.000"]);
    }

    #[test]
    fn stale_heading_key_drops_the_title_and_skips_sheets() {
        assert_eq!(
            heading_key("PRD > Pricing  > IDR").as_deref(),
            Some("pricing > idr")
        );
        assert_eq!(heading_key("Pricing").as_deref(), Some("pricing"));
        assert_eq!(heading_key("rows 1-20"), None);
        assert_eq!(heading_key(""), None);
    }
}

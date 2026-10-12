//! The one shared secret filter (AGENTS.md): text in, redacted text out.
//!
//! Everything that leaves a trust boundary as text — agent output, logs,
//! error messages, events, hand-off packs — goes through [`redact`] first.
//! It removes, in order:
//! 1. every known secret value (from the secret store, runtime env, ...);
//! 2. well-known token shapes (`sk-…`, `ghp_…`, `AKIA…`, `Bearer …`, ...);
//! 3. values assigned to sensitive names (`API_KEY=…`, `"password": "…"`).
//!
//! It errs toward over-redaction: a false positive costs a log line, a false
//! negative leaks a key. [`redact_known`] is the same filter with the
//! process-wide set of stored secret values, for sinks without a store.

/// Replacement text for a redacted value.
pub const REDACTED: &str = "[REDACTED]";

/// Known secrets shorter than this are ignored (they would shred normal text).
pub const MIN_SECRET_LEN: usize = 6;

/// Token prefixes; the prefix is kept, the rest of the token is redacted.
const TOKEN_PREFIXES: &[&str] = &[
    "Bearer ",
    "bearer ",
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "AKIA",
    "AIza",
];

/// A prefixed token must have at least this many chars after the prefix.
const MIN_TOKEN_TAIL: usize = 16;

/// Names whose assigned value is redacted (matched as a lowercase suffix).
const SENSITIVE_NAME_SUFFIXES: &[&str] = &["key", "token", "secret", "password", "passwd"];

/// The process's known secret values: set at boot from the store
/// ([`set_known_secrets`]), grown by every value the store seals or opens
/// ([`remember_secret`]), so a secret set while the kernel runs is redacted
/// at once. Never shrinks until the next boot: over-redaction is safe.
static KNOWN: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());

/// Replaces the known secret values (boot).
pub fn set_known_secrets(secrets: Vec<String>) {
    *KNOWN.write().unwrap_or_else(|p| p.into_inner()) = secrets;
}

/// Adds one value to the known secrets (it was just set or read).
pub fn remember_secret(secret: &str) {
    let mut known = KNOWN.write().unwrap_or_else(|p| p.into_inner());
    if secret.len() >= MIN_SECRET_LEN && !known.iter().any(|s| s == secret) {
        known.push(secret.to_owned());
    }
}

/// [`redact`] with the process's known secret values: for text that leaves
/// the kernel with no store at hand (log lines, mod stderr).
pub fn redact_known(text: &str) -> String {
    let known = KNOWN.read().unwrap_or_else(|p| p.into_inner());
    let known: Vec<&str> = known.iter().map(String::as_str).collect();
    redact(text, &known)
}

/// Returns `text` with secrets replaced by [`REDACTED`].
pub fn redact(text: &str, known_secrets: &[&str]) -> String {
    let mut secrets: Vec<&str> = known_secrets
        .iter()
        .copied()
        .filter(|s| s.len() >= MIN_SECRET_LEN)
        .collect();
    // Longest first, so a secret containing another is removed whole.
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_owned();
    for secret in secrets {
        out = out.replace(secret, REDACTED);
    }
    redact_assignments(&redact_tokens(&out))
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// Length in bytes of the token starting at `s`, without trailing dots.
fn token_len(s: &str) -> usize {
    let end = s.find(|c: char| !is_token_char(c)).unwrap_or(s.len());
    s[..end].trim_end_matches('.').len()
}

fn redact_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut prev_is_token_char = false;
    while let Some(c) = rest.chars().next() {
        if !prev_is_token_char {
            let hit = TOKEN_PREFIXES.iter().find_map(|prefix| {
                let tail = token_len(rest.strip_prefix(prefix)?);
                (tail >= MIN_TOKEN_TAIL).then_some((prefix.len(), tail))
            });
            if let Some((prefix_len, tail)) = hit {
                out.push_str(&rest[..prefix_len]);
                out.push_str(REDACTED);
                rest = &rest[prefix_len + tail..];
                prev_is_token_char = false;
                continue;
            }
        }
        out.push(c);
        prev_is_token_char = is_token_char(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn is_sensitive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SENSITIVE_NAME_SUFFIXES.iter().any(|s| name.ends_with(s))
}

fn is_value_end(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b',' | b';' | b'&' | b'}' | b')')
}

/// Redacts `name=value` / `name: value` / `"name": "value"` for sensitive names.
fn redact_assignments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'=' | b':') {
            let head = text[..i].trim_end_matches(['"', '\'']);
            let name = &head[head.trim_end_matches(is_token_char).len()..];
            if is_sensitive_name(name) {
                let mut start = i + 1;
                while start < bytes.len() && matches!(bytes[start], b' ' | b'"' | b'\'') {
                    start += 1;
                }
                let mut end = start;
                while end < bytes.len() && !is_value_end(bytes[end]) {
                    end += 1;
                }
                if end > start {
                    out.push_str(&text[copied..start]);
                    out.push_str(REDACTED);
                    copied = end;
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_secrets_are_removed_longest_first() {
        assert_eq!(
            redact("pw is hunter2hunter2!", &["hunter2hunter2", "hunter2"]),
            "pw is [REDACTED]!"
        );
        // Too short to redact safely; empty never matches.
        assert_eq!(redact("abc", &["abc", ""]), "abc");
    }

    #[test]
    fn token_shapes_are_redacted_keeping_the_prefix() {
        assert_eq!(
            redact("key sk-ant-abcdefghijklmnop123 used.", &[]),
            "key sk-[REDACTED] used."
        );
        assert_eq!(
            redact("Authorization: Bearer eyJhbGciOi.eyJzdWIiOi.sig123", &[]),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(
            redact("aws AKIAABCDEFGHIJKLMNOP", &[]),
            "aws AKIA[REDACTED]"
        );
        assert_eq!(
            redact("gh=ghp_0123456789abcdefABCDEF", &[]),
            "gh=ghp_[REDACTED]"
        );
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        for text in [
            "task-runner and risk-assessment-of-the-new-thing",
            "sk-short",
            "pwd=/home/me; monkey business",
            "a == b",
            "naïve café ✓",
        ] {
            assert_eq!(redact(text, &[]), text);
        }
    }

    #[test]
    fn sensitive_assignments_are_redacted() {
        assert_eq!(
            redact("OPENAI_API_KEY=abc123 next", &[]),
            "OPENAI_API_KEY=[REDACTED] next"
        );
        assert_eq!(
            redact(r#"{"password": "p4ss", "user": "me"}"#, &[]),
            r#"{"password": "[REDACTED]", "user": "me"}"#
        );
        assert_eq!(
            redact("https://x.io/cb?access_token=zzz&state=1", &[]),
            "https://x.io/cb?access_token=[REDACTED]&state=1"
        );
        assert_eq!(
            redact("client_secret: s3cr3t", &[]),
            "client_secret: [REDACTED]"
        );
    }
}

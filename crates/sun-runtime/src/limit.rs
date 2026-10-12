//! Shared limit classifier: a regex list that turns CLI
//! text into [`LimitKind`] and a parser for the reset time it mentions.
//!
//! Every adapter uses this for text it can't classify from structured fields.
//! Rules are ordered (**Chain of Responsibility**): the first match wins, so
//! specific wording comes before broad wording. Add a CLI's wording here, not
//! in the adapter.

use std::sync::LazyLock;

use chrono::{Duration, NaiveDateTime, NaiveTime, TimeZone, Utc};
use regex::Regex;
use sun_core::{LimitKind, SunEvent, Timestamp};

/// `(pattern, kind)`; matched case-insensitively, first match wins.
const RULES: &[(&str, LimitKind)] = &[
    // Out of paid balance.
    (r"credit balance is too low", LimitKind::Credits),
    (
        r"insufficient[_ ](quota|credits?|balance|funds)",
        LimitKind::Credits,
    ),
    (r"out of (credits|funds)", LimitKind::Credits),
    (r"payment required|\b402\b", LimitKind::Credits),
    // Subscription windows (Claude "session/weekly limit", Codex "usage limit").
    (r"usage limit", LimitKind::Quota),
    (
        r"(session|weekly|daily|monthly|5-hour|five[_ ]hour|opus) limit",
        LimitKind::Quota,
    ),
    (r"hit your .{0,20}limit", LimitKind::Quota),
    (r"quota (exceeded|exhausted|reached)", LimitKind::Quota),
    // Short-term throttling.
    (r"rate[ _-]?limit", LimitKind::RateLimit),
    (
        r"too many requests|\b429\b|resource[_ ]exhausted",
        LimitKind::RateLimit,
    ),
];

static COMPILED: LazyLock<Vec<(Regex, LimitKind)>> = LazyLock::new(|| {
    RULES
        .iter()
        .map(|(pattern, kind)| (re(&format!("(?i){pattern}")), *kind))
        .collect()
});

/// Compiles a built-in pattern. These are literals exercised by the tests
/// below, so a bad one is a programming error caught before release.
fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("built-in regex `{pattern}`: {e}"))
}

/// Classifies `text` as a limit, or `None` if no rule matches.
pub fn classify(text: &str) -> Option<LimitKind> {
    COMPILED
        .iter()
        .find(|(re, _)| re.is_match(text))
        .map(|(_, kind)| *kind)
}

/// [`SunEvent::LimitHit`] for `text` if it reads like a limit. `raw` is the
/// text as-is; the process driver redacts every event it emits.
pub fn detect(text: &str, now: Timestamp) -> Option<SunEvent> {
    classify(text).map(|limit| SunEvent::LimitHit {
        limit,
        reset_at: parse_reset(text, now),
        raw: text.to_owned(),
    })
}

static EPOCH: LazyLock<Regex> = LazyLock::new(|| re(r"\|(\d{10})\b"));
static IN_DURATION: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)(?:try again|retry|resets?)\s+in\s+((?:\d+\s*[a-z]+[\s,]*(?:and\s+)?)+)")
});
static DURATION_PART: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)(\d+)\s*([a-z]+)"));
static RETRY_AFTER: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)retry[- ]after\W{0,3}(\d+)"));
static AT_DATE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?i)try again at ([a-z]{3}) (\d{1,2})(?:st|nd|rd|th)?, (\d{4}) (\d{1,2}:\d{2}) ?([ap]m)")
});
static AT_TIME: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)resets?(?: at)? (\d{1,2})(?::(\d{2}))? ?([ap]m)(?: \(([^)]+)\))?"));

/// When the limit in `text` resets, if it says so. Understands:
/// `try again in 2 hours 5 minutes`, `retry-after: 3600`, `…|<epoch secs>`,
/// `try again at Oct 7th, 2026 11:50 PM` and `resets 9:50pm (UTC)`.
///
/// ponytail: absolute times without a zone are read as UTC, and named zones
/// other than UTC give `None`; add chrono-tz when a CLI prints local time.
pub fn parse_reset(text: &str, now: Timestamp) -> Option<Timestamp> {
    if let Some(c) = EPOCH.captures(text) {
        return Utc.timestamp_opt(c[1].parse().ok()?, 0).single();
    }
    if let Some(c) = IN_DURATION.captures(text) {
        let mut total = Duration::zero();
        for part in DURATION_PART.captures_iter(&c[1]) {
            let n: i64 = part[1].parse().ok()?;
            total += match part[2].to_ascii_lowercase().as_str() {
                "d" | "day" | "days" => Duration::days(n),
                "h" | "hr" | "hrs" | "hour" | "hours" => Duration::hours(n),
                "m" | "min" | "mins" | "minute" | "minutes" => Duration::minutes(n),
                "s" | "sec" | "secs" | "second" | "seconds" => Duration::seconds(n),
                _ => continue,
            };
        }
        return (total > Duration::zero()).then(|| now + total);
    }
    if let Some(c) = RETRY_AFTER.captures(text) {
        return Some(now + Duration::seconds(c[1].parse().ok()?));
    }
    if let Some(c) = AT_DATE.captures(text) {
        let when = format!("{} {} {} {} {}", &c[1], &c[2], &c[3], &c[4], &c[5]);
        let naive = NaiveDateTime::parse_from_str(&when, "%b %d %Y %I:%M %p").ok()?;
        return Some(naive.and_utc());
    }
    if let Some(c) = AT_TIME.captures(text) {
        if c.get(4)
            .is_some_and(|zone| !zone.as_str().eq_ignore_ascii_case("utc"))
        {
            return None;
        }
        let minute = c.get(2).map_or("00", |m| m.as_str());
        let time =
            NaiveTime::parse_from_str(&format!("{}:{} {}", &c[1], minute, &c[3]), "%I:%M %p")
                .ok()?;
        let today = now.date_naive().and_time(time).and_utc();
        return Some(if today > now {
            today
        } else {
            today + Duration::days(1)
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn classifies_cli_wording() {
        let cases = [
            (
                "You've hit your session limit · resets 9:50pm (UTC)",
                Some(LimitKind::Quota),
            ),
            (
                "Claude AI usage limit reached|1791323400",
                Some(LimitKind::Quota),
            ),
            (
                "You've hit your usage limit. Upgrade to Pro, visit … to purchase more credits or try again at Oct 7th, 2026 11:50 PM.",
                Some(LimitKind::Quota),
            ),
            (
                "Rate limit exceeded. Please try again later.",
                Some(LimitKind::RateLimit),
            ),
            ("HTTP 429 Too Many Requests", Some(LimitKind::RateLimit)),
            (
                "Your credit balance is too low to access the Anthropic API",
                Some(LimitKind::Credits),
            ),
            ("error code: insufficient_quota", Some(LimitKind::Credits)),
            ("File does not exist.", None),
            ("Line 4290 has a typo", None),
        ];
        for (text, want) in cases {
            assert_eq!(classify(text), want, "{text}");
        }
    }

    #[test]
    fn parses_reset_times() {
        let now = at("2026-10-07T12:00:00Z");
        let cases = [
            (
                "usage limit reached|1791323400",
                Some(at("2026-10-06T21:50:00Z")),
            ),
            (
                "try again in 2 hours 5 minutes",
                Some(at("2026-10-07T14:05:00Z")),
            ),
            ("Please retry in 30s", Some(at("2026-10-07T12:00:30Z"))),
            ("retry-after: 3600", Some(at("2026-10-07T13:00:00Z"))),
            (
                "or try again at Oct 7th, 2026 11:50 PM.",
                Some(at("2026-10-07T23:50:00Z")),
            ),
            ("resets 9:50pm (UTC)", Some(at("2026-10-07T21:50:00Z"))),
            ("resets 9am", Some(at("2026-10-08T09:00:00Z"))),
            ("resets 9:50pm (Europe/Berlin)", None),
            ("try again later", None),
        ];
        for (text, want) in cases {
            assert_eq!(parse_reset(text, now), want, "{text}");
        }
    }

    #[test]
    fn detect_builds_limit_hit() {
        let now = at("2026-10-07T12:00:00Z");
        assert_eq!(
            detect("Rate limit exceeded, retry after 60", now),
            Some(SunEvent::LimitHit {
                limit: LimitKind::RateLimit,
                reset_at: Some(at("2026-10-07T12:01:00Z")),
                raw: "Rate limit exceeded, retry after 60".into(),
            })
        );
        assert_eq!(detect("all good", now), None);
    }
}

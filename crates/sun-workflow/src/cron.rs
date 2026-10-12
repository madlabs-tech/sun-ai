//! `cron:<expr>` triggers: the classic five fields `minute hour
//! day-of-month month day-of-week`, each `*`, `n`, `a-b`, `*/s` or `a-b/s`,
//! comma-separated. Day of week is 0–7 (0 and 7 are Sunday). When both day
//! fields are restricted, a day matches either (as in Vixie cron).

/// A parsed cron expression: one bit per allowed value of each field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    any_day: bool,
    any_weekday: bool,
}

/// The fields' names and ranges.
const FIELDS: [(&str, u32, u32); 5] = [
    ("minute", 0, 59),
    ("hour", 0, 23),
    ("day of month", 1, 31),
    ("month", 1, 12),
    ("day of week", 0, 7),
];

impl Cron {
    /// Parses `expr`; the error says which field is wrong and why.
    pub fn parse(expr: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron `{expr}` needs 5 fields (minute hour day-of-month month day-of-week), e.g. `0 9 * * 1`"
            ));
        }
        let mut bits = [0u64; 5];
        for (i, (text, (name, low, high))) in fields.iter().zip(FIELDS).enumerate() {
            bits[i] = field(text, low, high).ok_or_else(|| {
                format!("cron `{expr}`: bad {name} `{text}` (allowed {low}-{high})")
            })?;
        }
        // Sunday is 0 and 7.
        let weekdays = (bits[4] | (bits[4] >> 7)) & 0x7f;
        Ok(Self {
            minutes: bits[0],
            hours: bits[1],
            days: bits[2],
            months: bits[3],
            weekdays,
            any_day: fields[2] == "*",
            any_weekday: fields[4] == "*",
        })
    }

    /// Whether the minute at `minute hour day month weekday` (weekday
    /// 0 = Sunday) is due.
    pub fn matches(&self, minute: u32, hour: u32, day: u32, month: u32, weekday: u32) -> bool {
        let has = |bits: u64, v: u32| v < 64 && bits & (1 << v) != 0;
        let by_day = has(self.days, day);
        let by_weekday = has(self.weekdays, weekday % 7);
        let day_ok = match (self.any_day, self.any_weekday) {
            (false, false) => by_day || by_weekday,
            _ => by_day && by_weekday,
        };
        has(self.minutes, minute) && has(self.hours, hour) && has(self.months, month) && day_ok
    }
}

/// One field's allowed values as bits, or `None` when malformed.
fn field(text: &str, low: u32, high: u32) -> Option<u64> {
    let mut bits = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().ok().filter(|s| *s > 0)?),
            None => (part, 1),
        };
        let (from, to) = match range {
            "*" => (low, high),
            _ => match range.split_once('-') {
                Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
                None => {
                    let n = range.parse().ok()?;
                    (n, n)
                }
            },
        };
        if from < low || to > high || from > to {
            return None;
        }
        bits |= (from..=to)
            .step_by(step as usize)
            .fold(0, |b, v| b | 1 << v);
    }
    Some(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_fields_ranges_steps_and_days() {
        let monday_nine = Cron::parse("0 9 * * 1").unwrap();
        assert!(monday_nine.matches(0, 9, 6, 10, 1));
        assert!(!monday_nine.matches(1, 9, 6, 10, 1));
        assert!(!monday_nine.matches(0, 9, 7, 10, 2));

        let every_15 = Cron::parse("*/15 8-18 * * 1-5").unwrap();
        assert!(every_15.matches(45, 18, 1, 1, 5));
        assert!(!every_15.matches(50, 18, 1, 1, 5));
        assert!(!every_15.matches(0, 12, 1, 1, 6));

        // Sunday as 7; either day field when both are set.
        let sunday = Cron::parse("0 0 1 * 7").unwrap();
        assert!(sunday.matches(0, 0, 1, 3, 3), "the 1st");
        assert!(sunday.matches(0, 0, 9, 3, 0), "a Sunday");
        assert!(!sunday.matches(0, 0, 9, 3, 1));

        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* * 0 * *",
            "*/0 * * * *",
            "5-1 * * * *",
            "a * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
        assert!(Cron::parse("1,2,3 * * * *").unwrap().matches(2, 5, 5, 5, 5));
    }
}

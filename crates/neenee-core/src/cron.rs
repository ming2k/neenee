//! Five-field cron expression engine for the `/repeat` scheduler.
//!
//! A cron expression is five whitespace-separated fields:
//!
//! ```text
//! minute  hour  day-of-month  month  day-of-week
//! ```
//!
//! Each field accepts: `*`, a single value, a range `a-b`, a list `a,b,c`, and
//! a step (`*/n`, `a-b/n`, `a/n`). Day-of-week is `0–6` with `0` = Sunday
//! (`7` is accepted as an alias for Sunday). Months are `1–12`.
//!
//! The engine computes the next fire time after a given instant. Standard cron
//! semantics apply, including the day-of-month / day-of-week OR rule: when
//! both fields are restricted (neither is `*`), a day matches if **either**
//! field matches; when only one is restricted, both must match (the `*` field
//! matches everything, so this collapses to the restricted one).

use chrono::{DateTime, Datelike, Timelike, Utc};

/// Bounds for each cron field.
struct Bounds {
    min: u32,
    max: u32,
}

const MINUTE: Bounds = Bounds { min: 0, max: 59 };
const HOUR: Bounds = Bounds { min: 0, max: 23 };
const DOM: Bounds = Bounds { min: 1, max: 31 };
const MONTH: Bounds = Bounds { min: 1, max: 12 };
const DOW: Bounds = Bounds { min: 0, max: 7 }; // 7 aliases to Sunday(0).
const DOW6: Bounds = Bounds { min: 0, max: 6 }; // post-normalisation range.

/// A compiled five-field cron expression. Each field is a bitmask of allowed
/// values (bit `i` set ⇒ value `i` is allowed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronExpr {
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64, // 0–6 after normalisation
}

fn bit(mask: u64, v: u32) -> bool {
    (mask >> v) & 1 == 1
}

fn full(bounds: &Bounds) -> u64 {
    let mut m = 0u64;
    let mut v = bounds.min;
    while v <= bounds.max {
        m |= 1 << v;
        v += 1;
    }
    m
}

fn parse_field(raw: &str, bounds: &Bounds) -> Result<u64, String> {
    let mut mask: u64 = 0;
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (range_part, step_part) = match item.split_once('/') {
            Some((r, s)) => (r, Some(s)),
            None => (item, None),
        };
        let (lo, hi) = if range_part == "*" {
            (bounds.min, bounds.max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            let a = a
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid range start '{a}'"))?;
            let b = b
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid range end '{b}'"))?;
            (a, b)
        } else {
            let v = range_part
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid value '{range_part}'"))?;
            (v, v)
        };
        if lo < bounds.min || hi > bounds.max {
            return Err(format!(
                "value {}-{} out of bounds [{}, {}]",
                lo, hi, bounds.min, bounds.max
            ));
        }
        if lo > hi {
            return Err(format!("descending range {lo}-{hi}"));
        }
        let step: u32 = match step_part {
            Some(s) => s
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid step '{s}'"))?,
            None => 1,
        };
        if step == 0 {
            return Err("step must be greater than zero".to_string());
        }
        let mut v = lo;
        while v <= hi {
            mask |= 1 << v;
            v = v.saturating_add(step);
        }
    }
    if mask == 0 {
        return Err(format!("empty field '{raw}'"));
    }
    Ok(mask)
}

impl CronExpr {
    /// Parse a five-field cron expression.
    pub fn parse(expr: &str) -> Result<CronExpr, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron expression must have 5 fields, got {}: '{expr}'",
                fields.len()
            ));
        }
        let minute = parse_field(fields[0], &MINUTE)?;
        let hour = parse_field(fields[1], &HOUR)?;
        let dom = parse_field(fields[2], &DOM)?;
        let month = parse_field(fields[3], &MONTH)?;
        let mut dow = parse_field(fields[4], &DOW)?;
        // 7 aliases to Sunday (0).
        if dow & (1 << 7) != 0 {
            dow |= 1;
            dow &= !(1 << 7);
        }
        Ok(CronExpr {
            minute,
            hour,
            dom,
            month,
            dow,
        })
    }

    /// The next fire time strictly after `after`. Returns `None` if no match
    /// exists within the next 366 days (e.g. `30 2 30 2 *` — Feb 30 never
    /// occurs).
    pub fn next_fire(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let truncated = after
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .unwrap_or(after);
        let mut t = truncated + chrono::Duration::minutes(1);
        let cap = truncated + chrono::Duration::days(366);

        let dom_restricted = self.dom != full(&DOM);
        // DoW is normalised to 0–6 (7 folded into 0), so compare against the
        // 0–6 full mask — otherwise `*` would always look restricted.
        let dow_restricted = self.dow != full(&DOW6);

        while t <= cap {
            if bit(self.minute, t.minute())
                && bit(self.hour, t.hour())
                && bit(self.month, t.month())
            {
                let weekday = t.weekday().num_days_from_sunday();
                let day_ok = if dom_restricted && dow_restricted {
                    bit(self.dom, t.day()) || bit(self.dow, weekday)
                } else {
                    bit(self.dom, t.day()) && bit(self.dow, weekday)
                };
                if day_ok {
                    return Some(t);
                }
            }
            t += chrono::Duration::minutes(1);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn m(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn parses_every_minute() {
        let e = CronExpr::parse("* * * * *").unwrap();
        assert_eq!(e.next_fire(m(2026, 1, 1, 0, 0)), Some(m(2026, 1, 1, 0, 1)));
    }

    #[test]
    fn parses_step_minutes() {
        let e = CronExpr::parse("*/15 * * * *").unwrap();
        // 00:07 -> next is 00:15
        assert_eq!(e.next_fire(m(2026, 1, 1, 0, 7)), Some(m(2026, 1, 1, 0, 15)));
        // 00:15 -> next is 00:30
        assert_eq!(e.next_fire(m(2026, 1, 1, 0, 15)), Some(m(2026, 1, 1, 0, 30)));
    }

    #[test]
    fn parses_daily_at_nine() {
        let e = CronExpr::parse("0 9 * * *").unwrap();
        // 2026-01-01 08:59 -> 09:00 same day
        assert_eq!(e.next_fire(m(2026, 1, 1, 8, 59)), Some(m(2026, 1, 1, 9, 0)));
        // 2026-01-01 09:30 -> next day 09:00
        assert_eq!(e.next_fire(m(2026, 1, 1, 9, 30)), Some(m(2026, 1, 2, 9, 0)));
    }

    #[test]
    fn parses_monthly_on_first() {
        let e = CronExpr::parse("30 14 1 * *").unwrap();
        // 2026-01-15 -> 2026-02-01 14:30
        assert_eq!(
            e.next_fire(m(2026, 1, 15, 0, 0)),
            Some(m(2026, 2, 1, 14, 30))
        );
    }

    #[test]
    fn weekday_fires_on_monday() {
        // `0 9 * * 1` — Mondays at 09:00. 2026-01-02 is a Friday; next Monday
        // is 2026-01-05.
        let e = CronExpr::parse("0 9 * * 1").unwrap();
        assert_eq!(e.next_fire(m(2026, 1, 2, 0, 0)), Some(m(2026, 1, 5, 9, 0)));
    }

    #[test]
    fn weekday_seven_aliases_sunday() {
        // `0 12 * * 7` should equal `0 12 * * 0` (Sunday). 2026-01-03 is a
        // Saturday; next Sunday is 2026-01-04.
        let e = CronExpr::parse("0 12 * * 7").unwrap();
        assert_eq!(e.next_fire(m(2026, 1, 3, 0, 0)), Some(m(2026, 1, 4, 12, 0)));
    }

    #[test]
    fn dom_dow_or_rule() {
        // `0 0 1 * 1` — fires on the 1st OR any Monday at 00:00. 2026-01-02
        // (Friday): next is Monday 2026-01-05 (the 1st already passed).
        let e = CronExpr::parse("0 0 1 * 1").unwrap();
        assert_eq!(e.next_fire(m(2026, 1, 2, 0, 0)), Some(m(2026, 1, 5, 0, 0)));
        // If the 1st were a Monday it fires once; here it fires on the 1st
        // (Thursday 2026-01-01) before any Monday.
        assert_eq!(e.next_fire(m(2025, 12, 31, 23, 59)), Some(m(2026, 1, 1, 0, 0)));
    }

    #[test]
    fn impossible_date_returns_none() {
        // Feb 30 never exists.
        let e = CronExpr::parse("0 0 30 2 *").unwrap();
        assert!(e.next_fire(m(2026, 1, 1, 0, 0)).is_none());
    }

    #[test]
    fn rejects_wrong_field_count() {
        assert!(CronExpr::parse("* * * *").is_err());
        assert!(CronExpr::parse("* * * * * *").is_err());
    }

    #[test]
    fn rejects_out_of_range_values() {
        assert!(CronExpr::parse("60 * * * *").is_err());
        assert!(CronExpr::parse("* 24 * * *").is_err());
        assert!(CronExpr::parse("* * 32 * *").is_err());
        assert!(CronExpr::parse("* * * 13 *").is_err());
        assert!(CronExpr::parse("* * * * 8").is_err());
    }

    #[test]
    fn rejects_zero_step() {
        assert!(CronExpr::parse("*/0 * * * *").is_err());
    }

    #[test]
    fn accepts_lists_and_ranges_with_steps() {
        let e = CronExpr::parse("0,30 9-17/2 * * 1-5").unwrap();
        // 2026-01-02 (Friday) 09:00 is a valid fire (09:00 is in 9-17/2).
        assert_eq!(e.next_fire(m(2026, 1, 2, 8, 0)), Some(m(2026, 1, 2, 9, 0)));
    }
}

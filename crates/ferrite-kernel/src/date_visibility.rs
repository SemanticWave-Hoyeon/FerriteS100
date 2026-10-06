//! Calendar selectors for S-100 Date declarations, with reduced accuracy and recurrence.
use crate::{IntervalClosure, TemporalBounds, TemporalInterval};
use anyhow::{bail, ensure, Context, Result};
use chrono::{Datelike, NaiveDate};

pub fn parse_viewing_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(s, "%Y%m%d"))
        .with_context(|| format!("invalid viewing date: {s}"))
}
/// Validate a complete or reduced S-100 date without assigning a viewing date.
/// ISO 8211/HDF5 use eight characters; XML lexical equivalents are also accepted.
pub fn validate_s100_date(value: &str) -> Result<()> {
    Pattern::parse(value).map(|_| ())
}

#[derive(Clone, Copy, Debug)]
struct Pattern {
    year: Option<i32>,
    month: Option<u32>,
    day: Option<u32>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cycle {
    Fixed,
    Annual,
    Monthly,
}
impl Pattern {
    fn parse(s: &str) -> Result<Self> {
        // Part 1/Part 3 allow equivalent XML Schema reduced-accuracy types.
        // Older S-101 portrayal output may retain these lexical forms (e.g. --05).
        let canonical: std::borrow::Cow<'_, str> = if s.is_ascii() {
            match s.len() {
                4 if s.starts_with("--") => format!("----{}--", &s[2..]).into(),
                4 => format!("{s}----").into(),
                5 if s.starts_with("---") => format!("------{}", &s[3..]).into(),
                6 if s.starts_with("--") && s.ends_with("--") => {
                    format!("----{}--", &s[2..4]).into()
                }
                7 if s.starts_with("--") && &s[4..5] == "-" => {
                    format!("----{}{}", &s[2..4], &s[5..]).into()
                }
                7 if &s[4..5] == "-" => format!("{}{}--", &s[..4], &s[5..]).into(),
                10 if &s[4..5] == "-" && &s[7..8] == "-" => {
                    format!("{}{}{}", &s[..4], &s[5..7], &s[8..]).into()
                }
                _ => s.into(),
            }
        } else {
            s.into()
        };
        let s = canonical.as_ref();
        ensure!(
            s.len() == 8 && s.is_ascii(),
            "S100_TruncatedDate must have 8 ASCII characters: {s}"
        );
        fn component(s: &str) -> Result<Option<u32>> {
            if s.bytes().all(|b| b == b'-') {
                Ok(None)
            } else {
                ensure!(
                    s.bytes().all(|b| b.is_ascii_digit()),
                    "mixed missing date component: {s}"
                );
                Ok(Some(s.parse()?))
            }
        }
        let year = component(&s[..4])?.map(|v| v as i32);
        let month = component(&s[4..6])?;
        let day = component(&s[6..])?;
        ensure!(
            year.is_some() || month.is_some() || day.is_some(),
            "empty truncated date"
        );
        ensure!(
            month.is_none_or(|m| (1..=12).contains(&m)),
            "invalid month: {s}"
        );
        ensure!(
            day.is_none_or(|d| (1..=31).contains(&d)),
            "invalid day: {s}"
        );
        if let (Some(m), Some(d)) = (month, day) {
            ensure!(
                NaiveDate::from_ymd_opt(year.unwrap_or(2000), m, d).is_some(),
                "invalid calendar date: {s}"
            );
        }
        Ok(Self { year, month, day })
    }
    fn cycle(self) -> Cycle {
        if self.month.is_none() && self.day.is_some() {
            Cycle::Monthly
        } else if self.year.is_none() {
            Cycle::Annual
        } else {
            Cycle::Fixed
        }
    }
    fn resolve(self, anchor: NaiveDate, end: bool) -> Option<NaiveDate> {
        let y = self.year.unwrap_or(anchor.year());
        let m = self.month.unwrap_or(if self.day.is_some() {
            anchor.month()
        } else if end {
            12
        } else {
            1
        });
        let d = self.day.unwrap_or(if end { last_day(y, m)? } else { 1 });
        NaiveDate::from_ymd_opt(y, m, d)
    }
}
fn last_day(y: i32, m: u32) -> Option<u32> {
    (28..=31)
        .rev()
        .find(|d| NaiveDate::from_ymd_opt(y, m, *d).is_some())
}
fn shift(anchor: NaiveDate, cycle: Cycle, delta: i32) -> Option<NaiveDate> {
    let (y, m) = match cycle {
        Cycle::Fixed => (anchor.year(), anchor.month()),
        Cycle::Annual => (anchor.year() + delta, anchor.month()),
        Cycle::Monthly => {
            let k = anchor.year() * 12 + anchor.month0() as i32 + delta;
            (k.div_euclid(12), k.rem_euclid(12) as u32 + 1)
        }
    };
    if !(0..=9999).contains(&y) {
        return None;
    }
    NaiveDate::from_ymd_opt(y, m, 1)
}
pub(crate) fn closure_matches<T: Ord + Copy>(
    closure: IntervalClosure,
    today: T,
    lo: Option<T>,
    hi: Option<T>,
) -> bool {
    use IntervalClosure::*;
    match closure {
        Closed => lo.is_none_or(|v| today >= v) && hi.is_none_or(|v| today <= v),
        Open => lo.is_none_or(|v| today > v) && hi.is_none_or(|v| today < v),
        LeftClosed => lo.is_none_or(|v| today >= v) && hi.is_none_or(|v| today < v),
        RightClosed => lo.is_none_or(|v| today > v) && hi.is_none_or(|v| today <= v),
        Greater => lo.is_some_and(|v| today > v),
        GreaterEqual => lo.is_some_and(|v| today >= v),
        Less => hi.is_some_and(|v| today < v),
        LessEqual => hi.is_some_and(|v| today <= v),
    }
}
fn bounds_visible(
    bounds: &TemporalBounds,
    closure: IntervalClosure,
    today: NaiveDate,
) -> Result<bool> {
    let lo = bounds.begin.as_deref().map(Pattern::parse).transpose()?;
    let hi = bounds.end.as_deref().map(Pattern::parse).transpose()?;
    use IntervalClosure::*;
    if matches!(closure, Greater | GreaterEqual) {
        ensure!(lo.is_some(), "left ray requires begin");
    }
    if matches!(closure, Less | LessEqual) {
        ensure!(hi.is_some(), "right ray requires end");
    }
    let cycle = lo.or(hi).context("missing date bounds")?.cycle();
    if let (Some(a), Some(b)) = (lo, hi) {
        ensure!(
            a.cycle() == b.cycle(),
            "mixed date recurrence cycles require an explicit policy"
        );
    }
    let deltas: &[i32] = if cycle == Cycle::Fixed {
        &[0]
    } else {
        &[-1, 0]
    };
    for &delta in deltas {
        let Some(anchor) = shift(today, cycle, delta) else {
            continue;
        };
        let lower = lo.and_then(|p| p.resolve(anchor, false));
        let mut upper = hi.and_then(|p| p.resolve(anchor, true));
        if lo.is_some() && lower.is_none() || hi.is_some() && upper.is_none() {
            continue;
        }
        if let (Some(a), Some(b)) = (lower, upper) {
            if b < a {
                if cycle == Cycle::Fixed {
                    bail!("date end precedes begin");
                }
                upper = shift(anchor, cycle, 1).and_then(|next| hi.unwrap().resolve(next, true));
                if upper.is_none() {
                    continue;
                }
            }
        }
        // Rays recur within the selector's cycle, rather than claiming all future cycles.
        if (matches!(closure, Greater | GreaterEqual | Less | LessEqual)
            || lo.is_none()
            || hi.is_none())
            && delta != 0
        {
            continue;
        }
        if closure_matches(closure, today, lower, upper) {
            return Ok(true);
        }
    }
    Ok(false)
}
/// OR between intervals. DateTime/Time selectors deliberately return a diagnostic
/// until a clock selector is supplied; callers must preserve the feature on error.
pub fn date_intervals_visible(intervals: &[TemporalInterval], today: NaiveDate) -> Result<bool> {
    if intervals.is_empty() {
        return Ok(true);
    }
    let mut valid = false;
    for i in intervals {
        ensure!(
            i.time.is_none() && i.date_time.is_none(),
            "Time/DateTime requires a clock selector"
        );
        valid |= bounds_visible(
            i.date.as_ref().context("missing Date selector")?,
            i.closure,
            today,
        )?;
    }
    Ok(valid)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn interval(lo: &str, hi: &str, c: IntervalClosure) -> TemporalInterval {
        TemporalInterval::new(
            Some(
                TemporalBounds::new(
                    (!lo.is_empty()).then(|| lo.into()),
                    (!hi.is_empty()).then(|| hi.into()),
                )
                .unwrap(),
            ),
            None,
            None,
            c,
        )
        .unwrap()
    }
    fn visible(i: &TemporalInterval, d: &str) -> bool {
        date_intervals_visible(std::slice::from_ref(i), parse_viewing_date(d).unwrap()).unwrap()
    }
    #[test]
    fn annual_wrap_and_reduced_accuracy_follow_part3_examples() {
        let i = interval("----1101", "----0331", IntervalClosure::Closed);
        for d in ["2026-11-01", "2027-01-01", "2027-03-31"] {
            assert!(visible(&i, d));
        }
        for d in ["2026-10-31", "2027-04-01"] {
            assert!(!visible(&i, d));
        }
        let i = interval("----01--", "----02--", IntervalClosure::Closed);
        assert!(visible(&i, "2024-02-29"));
        assert!(visible(&i, "2025-02-28"));
        assert!(!visible(&i, "2025-03-01"));
        let i = interval("2026----", "2026----", IntervalClosure::Closed);
        assert!(visible(&i, "2026-12-31"));
        assert!(!visible(&i, "2027-01-01"));
    }
    #[test]
    fn monthly_partial_dates_and_leap_only_instant() {
        let i = interval("------25", "------05", IntervalClosure::Closed);
        assert!(visible(&i, "2026-02-01"));
        assert!(!visible(&i, "2026-02-10"));
        let i = interval("----0229", "----0229", IntervalClosure::Closed);
        assert!(visible(&i, "2024-02-29"));
        assert!(!visible(&i, "2025-02-28"));
    }
    #[test]
    fn all_closures_and_union_have_independent_boundary_expectations() {
        use IntervalClosure::*;
        for (c, a, b) in [
            (Closed, true, true),
            (Open, false, false),
            (LeftClosed, true, false),
            (RightClosed, false, true),
            (Greater, false, true),
            (GreaterEqual, true, true),
            (Less, true, false),
            (LessEqual, true, true),
        ] {
            let i = interval("20260101", "20260131", c);
            assert_eq!(visible(&i, "2026-01-01"), a);
            assert_eq!(visible(&i, "2026-01-31"), b);
        }
        let intervals = [
            interval("20260101", "20260131", Closed),
            interval("20261101", "20261130", Closed),
        ];
        assert!(
            date_intervals_visible(&intervals, parse_viewing_date("20261115").unwrap()).unwrap()
        );
        assert!(
            !date_intervals_visible(&intervals, parse_viewing_date("20260715").unwrap()).unwrap()
        );
    }
    #[test]
    fn invalid_and_unsupported_declarations_return_diagnostics() {
        for (a, b) in [
            ("20260230", "20260301"),
            ("--------", "20260301"),
            ("20261301", "20270101"),
            ("20260302", "20260301"),
            ("----01--", "------05"),
        ] {
            assert!(date_intervals_visible(
                &[interval(a, b, IntervalClosure::Closed)],
                parse_viewing_date("20260101").unwrap()
            )
            .is_err());
        }
    }
}

#[cfg(test)]
mod xml_lexical_tests {
    use super::*;
    #[test]
    fn reduced_xml_dates_match_canonical_s100_dates() {
        for (xml, canonical) in [
            ("--05", "----05--"),
            ("--05--", "----05--"),
            ("--05-17", "----0517"),
            ("---17", "------17"),
            ("2026", "2026----"),
            ("2026-05", "202605--"),
            ("2026-05-17", "20260517"),
        ] {
            let a = Pattern::parse(xml).unwrap();
            let b = Pattern::parse(canonical).unwrap();
            assert_eq!((a.year, a.month, a.day), (b.year, b.month, b.day));
        }
        let i = TemporalInterval::new(
            Some(TemporalBounds::new(Some("--05".into()), Some("--10".into())).unwrap()),
            None,
            None,
            IntervalClosure::Closed,
        )
        .unwrap();
        for (d, expected) in [
            ("2026-04-30", false),
            ("2026-05-01", true),
            ("2026-10-31", true),
            ("2026-11-01", false),
        ] {
            assert_eq!(
                date_intervals_visible(std::slice::from_ref(&i), parse_viewing_date(d).unwrap())
                    .unwrap(),
                expected
            );
        }
    }
}

#[cfg(test)]
mod lexical_validation_tests {
    use super::*;
    #[test]
    fn validates_dates_without_resolving_recurrence() {
        for date in [
            "00000101",
            "20240229",
            "2024----",
            "2024--31",
            "----0229",
            "----12--",
            "------31",
            "--02-29",
            "2024-02",
            "2024-02-29",
        ] {
            assert!(validate_s100_date(date).is_ok(), "{date}");
        }
        for date in [
            "20230229",
            "----0230",
            "--------",
            "20241301",
            "20240100",
            "2024-101",
            "２０２４０１０１",
            "2024-02-30",
        ] {
            assert!(validate_s100_date(date).is_err(), "{date}");
        }
    }
}

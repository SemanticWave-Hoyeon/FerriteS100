//! S-100 Part 1 ISO basic/XML clock selectors. Fixed offsets are explicit;
//! unzoned source bounds use a separate explicit source-local offset.
use crate::date_visibility::closure_matches;
use crate::{
    date_intervals_visible, parse_viewing_date, IntervalClosure, TemporalBounds, TemporalInterval,
};
use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Datelike, Duration, FixedOffset, TimeZone, Timelike};
pub type ViewingInstant = DateTime<FixedOffset>;
const SECOND: i64 = 1_000_000_000;
const DAY: i64 = 86400 * SECOND;
#[derive(Clone, Copy)]
struct Clock {
    local_ns: i64,
    offset: FixedOffset,
    explicit: bool,
}
fn clock(s: &str, default: FixedOffset) -> Result<Clock> {
    ensure!(s.is_ascii(), "Time must use ASCII ISO representation");
    let (body, offset, explicit) = if let Some(body) = s.strip_suffix('Z') {
        (body, FixedOffset::east_opt(0).unwrap(), true)
    } else if let Some(index) = s.bytes().rposition(|c| c == b'+' || c == b'-') {
        let zone = &s[index..];
        ensure!(
            zone[1..].bytes().all(|b| b.is_ascii_digit() || b == b':'),
            "invalid UTC offset digits"
        );
        let (h, m) = match zone.len() {
            5 => (zone[1..3].parse::<i32>()?, zone[3..5].parse::<i32>()?),
            6 if &zone[3..4] == ":" => (zone[1..3].parse::<i32>()?, zone[4..6].parse::<i32>()?),
            _ => bail!("invalid UTC offset: {zone}"),
        };
        ensure!(
            (0..24).contains(&h) && (0..60).contains(&m),
            "invalid UTC offset: {zone}"
        );
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        (
            &s[..index],
            FixedOffset::east_opt(sign * (h * 3600 + m * 60)).context("UTC offset out of range")?,
            true,
        )
    } else {
        (s, default, false)
    };
    let (main, fraction) = body.split_once('.').unwrap_or((body, ""));
    ensure!(
        !body.ends_with('.') && fraction.len() <= 9 && fraction.bytes().all(|b| b.is_ascii_digit()),
        "invalid fractional seconds"
    );
    let (h, m, sec) = match main.len() {
        6 if main.bytes().all(|b| b.is_ascii_digit()) => (
            main[..2].parse::<u32>()?,
            main[2..4].parse::<u32>()?,
            main[4..].parse::<u32>()?,
        ),
        8 if &main[2..3] == ":"
            && &main[5..6] == ":"
            && main.bytes().enumerate().all(|(i, b)| {
                if i == 2 || i == 5 {
                    b == b':'
                } else {
                    b.is_ascii_digit()
                }
            }) =>
        {
            (
                main[..2].parse::<u32>()?,
                main[3..5].parse::<u32>()?,
                main[6..].parse::<u32>()?,
            )
        }
        _ => bail!("Time requires complete HHMMSS or HH:MM:SS: {s}"),
    };
    let ns = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<i64>()? * 10_i64.pow(9 - fraction.len() as u32)
    };
    ensure!(
        h <= 24 && m < 60 && sec < 60,
        "invalid clock or unsupported leap second: {s}"
    );
    ensure!(
        h != 24 || (m == 0 && sec == 0 && ns == 0),
        "24-hour boundary must be midnight"
    );
    Ok(Clock {
        local_ns: ((h * 3600 + m * 60 + sec) as i64) * SECOND + ns,
        offset,
        explicit,
    })
}
fn datetime(s: &str, default: FixedOffset) -> Result<(ViewingInstant, bool)> {
    let (date, time) = s.split_once('T').context("DateTime requires T separator")?;
    let date = parse_viewing_date(date)?;
    ensure!(
        (0..=9999).contains(&date.year()),
        "DateTime year outside S-100 range"
    );
    let c = clock(time, default)?;
    let naive = date
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .checked_add_signed(Duration::nanoseconds(c.local_ns))
        .context("DateTime overflow")?;
    let value = c
        .offset
        .from_local_datetime(&naive)
        .single()
        .context("DateTime offset overflow")?;
    Ok((value, c.explicit))
}
/// Reproducible viewing instants must name Z or an explicit UTC offset.
pub fn parse_viewing_instant(s: &str) -> Result<ViewingInstant> {
    let (instant, explicit) = datetime(s, FixedOffset::east_opt(0).unwrap())?;
    ensure!(
        explicit,
        "viewing instant requires Z or an explicit UTC offset"
    );
    Ok(instant)
}
fn ray_bounds<T>(closure: IntervalClosure, lo: &Option<T>, hi: &Option<T>) -> Result<()> {
    use IntervalClosure::*;
    if matches!(closure, Greater | GreaterEqual) {
        ensure!(lo.is_some(), "left ray requires begin");
    }
    if matches!(closure, Less | LessEqual) {
        ensure!(hi.is_some(), "right ray requires end");
    }
    Ok(())
}
fn datetime_visible(
    bounds: &TemporalBounds,
    closure: IntervalClosure,
    view: &ViewingInstant,
    local_offset: FixedOffset,
) -> Result<bool> {
    let lo = bounds
        .begin
        .as_deref()
        .map(|s| datetime(s, local_offset).map(|v| v.0))
        .transpose()?;
    let hi = bounds
        .end
        .as_deref()
        .map(|s| datetime(s, local_offset).map(|v| v.0))
        .transpose()?;
    ray_bounds(closure, &lo, &hi)?;
    if let (Some(a), Some(b)) = (lo, hi) {
        ensure!(a <= b, "DateTime end precedes begin");
    }
    Ok(closure_matches(closure, *view, lo, hi))
}
fn time_visible(
    bounds: &TemporalBounds,
    closure: IntervalClosure,
    view: &ViewingInstant,
    local_offset: FixedOffset,
) -> Result<bool> {
    let lo = bounds
        .begin
        .as_deref()
        .map(|s| clock(s, local_offset))
        .transpose()?;
    let hi = bounds
        .end
        .as_deref()
        .map(|s| clock(s, local_offset))
        .transpose()?;
    ray_bounds(closure, &lo, &hi)?;
    use IntervalClosure::*;
    let frame = if matches!(closure, Less | LessEqual) {
        hi.or(lo)
    } else {
        lo.or(hi)
    }
    .context("missing Time bounds")?
    .offset;
    let local = view.with_timezone(&frame);
    let t = local.num_seconds_from_midnight() as i64 * SECOND + local.nanosecond() as i64;
    let adjusted = |c: Clock| {
        c.local_ns + (frame.local_minus_utc() - c.offset.local_minus_utc()) as i64 * SECOND
    };
    let lower = lo.map(|c| adjusted(c).rem_euclid(DAY));
    let upper = hi.map(|c| {
        let value = adjusted(c);
        if value == DAY {
            DAY
        } else {
            value.rem_euclid(DAY)
        }
    });
    if matches!(closure, Greater | GreaterEqual | Less | LessEqual)
        || lower.is_none()
        || upper.is_none()
    {
        return Ok(closure_matches(closure, t, lower, upper));
    }
    let start = lower.unwrap();
    let delta = adjusted(hi.unwrap()) - adjusted(lo.unwrap());
    let duration = if delta >= DAY {
        DAY
    } else {
        delta.rem_euclid(DAY)
    };
    let end = start + duration;
    Ok(closure_matches(closure, t, Some(start), Some(end))
        || closure_matches(closure, t + DAY, Some(start), Some(end)))
}
/// OR across intervals. A single interval selects Date, Time or DateTime.
/// Mixed state is preserved with a diagnostic until a combined-selector policy is implemented.
pub fn temporal_intervals_visible(
    intervals: &[TemporalInterval],
    view: &ViewingInstant,
) -> Result<bool> {
    temporal_intervals_visible_with_offset(intervals, view, FixedOffset::east_opt(0).unwrap())
}
/// Parse the explicit local-time policy for unzoned source bounds.
pub fn parse_local_time_offset(value: &str) -> Result<FixedOffset> {
    ensure!(
        value == "Z" || value.starts_with('+') || value.starts_with('-'),
        "local time offset requires Z or +/-HHMM"
    );
    let parsed = clock(&format!("000000{value}"), FixedOffset::east_opt(0).unwrap())?;
    ensure!(parsed.explicit, "local time offset is required");
    Ok(parsed.offset)
}
/// Source-local offset is independent of how the viewing instant is serialized.
/// Calendar Date conditions also use this explicitly supplied local calendar.
pub fn temporal_intervals_visible_with_offset(
    intervals: &[TemporalInterval],
    view: &ViewingInstant,
    local_offset: FixedOffset,
) -> Result<bool> {
    if intervals.is_empty() {
        return Ok(true);
    }
    ensure!(
        local_offset.local_minus_utc() % 60 == 0,
        "source-local offset must use whole minutes"
    );
    let mut valid = false;
    for i in intervals {
        ensure!(
            usize::from(i.date.is_some())
                + usize::from(i.time.is_some())
                + usize::from(i.date_time.is_some())
                == 1,
            "mixed Date/Time/DateTime selector state requires a combined policy"
        );
        valid |= if let Some(bounds) = &i.date_time {
            datetime_visible(bounds, i.closure, view, local_offset)?
        } else if let Some(bounds) = &i.time {
            time_visible(bounds, i.closure, view, local_offset)?
        } else {
            date_intervals_visible(
                std::slice::from_ref(i),
                view.with_timezone(&local_offset).date_naive(),
            )?
        };
    }
    Ok(valid)
}
/// Earliest possible visibility boundary; union intervals may cause harmless
/// extra wake-ups, while the caller compares actual visibility before rebuilding.
pub fn next_temporal_change_after(
    intervals: &[TemporalInterval],
    view: &ViewingInstant,
    local_offset: FixedOffset,
) -> Result<Option<ViewingInstant>> {
    temporal_intervals_visible_with_offset(intervals, view, local_offset)?;
    let mut next = None;
    let mut consider = |value: ViewingInstant| {
        if value > *view && next.is_none_or(|old| value < old) {
            next = Some(value);
        }
    };
    let midnight = |offset: FixedOffset| -> Result<ViewingInstant> {
        let date = view
            .with_timezone(&offset)
            .date_naive()
            .succ_opt()
            .context("calendar overflow")?;
        offset
            .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .context("midnight overflow")
    };
    for i in intervals {
        use IntervalClosure::*;
        let lower_exclusive = matches!(i.closure, Open | RightClosed | Greater);
        let upper_inclusive = matches!(i.closure, Closed | RightClosed | LessEqual);
        if i.date.is_some() {
            consider(midnight(local_offset)?);
        }
        if let Some(bounds) = &i.date_time {
            for (bound, exclusive) in [
                (bounds.begin.as_deref(), lower_exclusive),
                (bounds.end.as_deref(), upper_inclusive),
            ] {
                if let Some(bound) = bound {
                    let value = datetime(bound, local_offset)?
                        .0
                        .checked_add_signed(Duration::nanoseconds(i64::from(exclusive)))
                        .context("boundary overflow")?;
                    consider(value);
                }
            }
        }
        if let Some(bounds) = &i.time {
            for (bound, exclusive) in [
                (bounds.begin.as_deref(), lower_exclusive),
                (bounds.end.as_deref(), upper_inclusive),
            ] {
                if let Some(bound) = bound {
                    let c = clock(bound, local_offset)?;
                    consider(midnight(c.offset)?);
                    let day = view.with_timezone(&c.offset).date_naive();
                    let naive = day
                        .and_hms_opt(0, 0, 0)
                        .unwrap()
                        .checked_add_signed(Duration::nanoseconds(
                            c.local_ns + i64::from(exclusive),
                        ))
                        .context("time boundary overflow")?;
                    let mut value = c
                        .offset
                        .from_local_datetime(&naive)
                        .single()
                        .context("time offset overflow")?;
                    if value <= *view {
                        value = value
                            .checked_add_signed(Duration::days(1))
                            .context("time recurrence overflow")?;
                    }
                    consider(value);
                }
            }
        }
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn interval(lo: &str, hi: &str, dt: bool, c: IntervalClosure) -> TemporalInterval {
        let b = TemporalBounds::new(
            (!lo.is_empty()).then(|| lo.into()),
            (!hi.is_empty()).then(|| hi.into()),
        )
        .unwrap();
        TemporalInterval::new(None, (!dt).then(|| b.clone()), dt.then_some(b), c).unwrap()
    }
    fn visible(i: &TemporalInterval, v: &str) -> bool {
        temporal_intervals_visible(std::slice::from_ref(i), &parse_viewing_instant(v).unwrap())
            .unwrap()
    }
    #[test]
    fn iso_basic_xml_offsets_and_unzoned_source_share_instants() {
        for s in [
            "20261004T090000+0900",
            "2026-10-04T09:00:00+09:00",
            "20261004T000000Z",
        ] {
            assert_eq!(
                parse_viewing_instant(s).unwrap(),
                parse_viewing_instant("2026-10-04T00:00:00Z").unwrap()
            );
        }
        let i = interval(
            "20261004T090000",
            "20261004T100000",
            true,
            IntervalClosure::Closed,
        );
        let a = parse_viewing_instant("2026-10-04T09:30:00+09:00").unwrap();
        let b = parse_viewing_instant("2026-10-04T00:30:00Z").unwrap();
        let local = parse_local_time_offset("+09:00").unwrap();
        assert!(
            temporal_intervals_visible_with_offset(std::slice::from_ref(&i), &a, local).unwrap()
        );
        assert!(
            temporal_intervals_visible_with_offset(std::slice::from_ref(&i), &b, local).unwrap()
        );
        assert!(!visible(&i, "2026-10-04T09:30:00+09:00"));
        assert!(visible(&i, "2026-10-04T09:30:00Z"));
        let daily = interval("120000", "130000", false, IntervalClosure::Closed);
        for representation in ["20261004T123000+0900", "20261004T033000Z"] {
            assert!(temporal_intervals_visible_with_offset(
                std::slice::from_ref(&daily),
                &parse_viewing_instant(representation).unwrap(),
                local
            )
            .unwrap());
        }
        let explicit = interval(
            "20261004T090000+0900",
            "20261004T100000+0900",
            true,
            IntervalClosure::Closed,
        );
        assert!(!visible(&explicit, "2026-10-04T09:30:00Z"));
    }
    #[test]
    fn daily_midnight_offset_rays_and_end_of_day() {
        let i = interval("230000+0900", "010000+0900", false, IntervalClosure::Closed);
        for v in [
            "2026-10-04T14:00:00Z",
            "2026-10-04T15:00:00Z",
            "2026-10-04T16:00:00Z",
        ] {
            assert!(visible(&i, v));
        }
        assert!(!visible(&i, "2026-10-04T17:00:00Z"));
        let i = interval("230000+0900", "", false, IntervalClosure::GreaterEqual);
        assert!(!visible(&i, "2026-10-04T15:00:00Z"));
        assert!(visible(&i, "2026-10-04T14:30:00Z"));
        let until_midnight = interval("", "240000", false, IntervalClosure::Less);
        assert!(visible(&until_midnight, "2026-10-04T12:00:00Z"));
        assert!(visible(&until_midnight, "2026-10-04T00:00:00Z"));
        let i = interval("000000", "240000", false, IntervalClosure::Closed);
        assert!(visible(&i, "2026-10-04T12:00:00Z"));
        let i = interval("230000", "240000", false, IntervalClosure::Closed);
        assert!(visible(&i, "2026-10-04T00:00:00Z"));
        assert!(
            parse_viewing_instant("20261004T240000Z").unwrap()
                == parse_viewing_instant("20261005T000000Z").unwrap()
        );
    }
    #[test]
    fn nanosecond_closures_use_exact_integer_comparison() {
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
            for dt in [false, true] {
                let (lo, hi) = if dt {
                    ("20261004T120000.000000001Z", "20261004T120000.000000002Z")
                } else {
                    ("120000.000000001Z", "120000.000000002Z")
                };
                let i = interval(lo, hi, dt, c);
                assert_eq!(visible(&i, "20261004T120000.000000001Z"), a);
                assert_eq!(visible(&i, "20261004T120000.000000002Z"), b);
            }
        }
    }
    #[test]
    fn unions_invalid_offsets_and_mixed_conditions() {
        let i = [
            interval("100000Z", "110000Z", false, IntervalClosure::Closed),
            interval("180000Z", "190000Z", false, IntervalClosure::Closed),
        ];
        assert!(temporal_intervals_visible(
            &i,
            &parse_viewing_instant("20261004T183000Z").unwrap()
        )
        .unwrap());
        assert!(!temporal_intervals_visible(
            &i,
            &parse_viewing_instant("20261004T123000Z").unwrap()
        )
        .unwrap());
        for s in [
            "20261004T120000",
            "20261004T120000+2460",
            "20261004T250000Z",
            "20261004T12Z",
            "20260230T120000Z",
            "20261004T120000.Z",
            "20261004T120000.1234567890Z",
        ] {
            assert!(parse_viewing_instant(s).is_err(), "{s}");
        }
        assert!(temporal_intervals_visible(
            &[interval(
                "20261005T120000Z",
                "20261004T120000Z",
                true,
                IntervalClosure::Closed
            )],
            &parse_viewing_instant("20261004T120000Z").unwrap()
        )
        .is_err());
        let mut mixed = interval("120000", "130000", false, IntervalClosure::Closed);
        mixed.date =
            Some(TemporalBounds::new(Some("20261004".into()), Some("20261004".into())).unwrap());
        assert!(temporal_intervals_visible(
            &[mixed],
            &parse_viewing_instant("20261004T123000Z").unwrap()
        )
        .is_err());
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    #[test]
    fn closed_end_and_open_start_schedule_exact_nanosecond_boundary() {
        let bounds = TemporalBounds::new(
            Some("20261004T120000Z".into()),
            Some("20261004T130000Z".into()),
        )
        .unwrap();
        let mut i =
            TemporalInterval::new(None, None, Some(bounds), IntervalClosure::Closed).unwrap();
        let offset = parse_local_time_offset("Z").unwrap();
        let now = parse_viewing_instant("20261004T120000Z").unwrap();
        assert_eq!(
            next_temporal_change_after(std::slice::from_ref(&i), &now, offset).unwrap(),
            Some(parse_viewing_instant("20261004T130000.000000001Z").unwrap())
        );
        i.closure = IntervalClosure::Open;
        assert_eq!(
            next_temporal_change_after(&[i], &now, offset).unwrap(),
            Some(parse_viewing_instant("20261004T120000.000000001Z").unwrap())
        );
    }
    #[test]
    fn daily_rays_reset_at_midnight_and_calendar_uses_source_offset() {
        let offset = parse_local_time_offset("+0900").unwrap();
        let now = parse_viewing_instant("20261004T143000Z").unwrap();
        let i = TemporalInterval::new(
            None,
            Some(TemporalBounds::new(Some("230000".into()), None).unwrap()),
            None,
            IntervalClosure::GreaterEqual,
        )
        .unwrap();
        assert_eq!(
            next_temporal_change_after(&[i], &now, offset).unwrap(),
            Some(parse_viewing_instant("20261004T150000Z").unwrap())
        );
        let i = TemporalInterval::new(
            Some(TemporalBounds::new(Some("----1101".into()), Some("----0331".into())).unwrap()),
            None,
            None,
            IntervalClosure::Closed,
        )
        .unwrap();
        assert_eq!(
            next_temporal_change_after(&[i], &now, offset).unwrap(),
            Some(parse_viewing_instant("20261004T150000Z").unwrap())
        );
    }
}

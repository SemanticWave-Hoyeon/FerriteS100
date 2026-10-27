//! S-124 Navigational Warning lifecycle (S-124 2.0.0 clauses 8.1, 9.2, 9.3,
//! 11.1 and 11.2): message identity, validity, cancellation, in-force
//! bulletins, user classification, portrayal selection and listing order.
//!
//! This module is pure logic. The caller owns the loaded datasets and the
//! time each was received; [`evaluate`] produces a [`NavwarnRegistry`]
//! snapshot for one instant `now`. Re-evaluate when datasets arrive or time
//! advances (cancellation dates and fixed date ranges are time dependent).
//!
//! Decisions on points the standard leaves open (also noted at each item):
//! * Message identity is agency + name of series + warning number + year.
//!   Warning type and nationality are not part of the identity because an
//!   in-force bulletin lists warnings of other warning types in its series,
//!   and nationality is optional (2.0.0 uses `nationality`, some producers
//!   still encode `countryName`). Two-digit years are read as 20yy.
//! * A series is agency + name of series (normalised: trimmed, upper case,
//!   collapsed white space).
//! * A dataset is an in-force bulletin when its warning type is 9-12, when it
//!   has a References with referenceCategory 3, or when it is a "no warning"
//!   message (warning type 5-8) carrying noMessageOnHand = true (8.1.4).
//!   noMessageOnHand = true lists nothing, so it cancels every earlier
//!   dataset of the series.
//! * "Prior to the bulletin" (9.2.1) compares publicationTime; the received
//!   time is used when a dataset has no readable publicationTime.
//! * The latest bulletin of a series keeps its fail-safe effect after its own
//!   cancellationDate: a lapsed bulletin does not revive cancelled datasets.
//! * Cancellation by References category 1 applies regardless of the order
//!   of receipt and regardless of the cancelling dataset's own status;
//!   references to the dataset's own identity are ignored.
//! * Imported (non-S-100) datasets never cancel anything and are never
//!   bulletins, but they can be cancelled by authoritative datasets that
//!   match their synthesised identity, and an authoritative dataset with the
//!   same identity replaces them.
//! * `NotYetActive`: publicationTime in the future, or every NavwarnPart has a
//!   fixedDateRange that has not started yet.
//! * fixedDateRange (11.1.2 EXAMPLE 1): dateStart + timeOfDayStart is the
//!   start instant, dateEnd + timeOfDayEnd the end instant (the end of
//!   dateEnd when no time is given), as the DCEG binds each time of day to
//!   its date. Inside any range the part must be shown; before the first
//!   range starts it may be shown ([`PortrayalPolicy::show_before_fixed_date_range`],
//!   default true as the conservative choice); after every range has ended
//!   it is not shown (the warning itself stays listed until cancelled).
//!   Month-day truncated dates (`--MM-DD`) recur yearly. A range that cannot
//!   be read is treated as covering `now` so information is not lost.
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use ferrite_s100_gml::{Dataset, Object, ObjectKind, Value};
use std::collections::{HashMap, HashSet};
use std::fmt;

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Normalised text for identity comparison.
fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase()
}

/// Producer and series, the scope of an in-force bulletin.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SeriesKey {
    pub agency: String,
    pub series: String,
}

impl fmt::Display for SeriesKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.agency, self.series)
    }
}

/// A decoded `messageSeriesIdentifier`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageId {
    pub agency: String,
    pub nationality: Option<String>,
    pub series: String,
    pub number: Option<u32>,
    pub year: Option<i32>,
    /// warningType code 1-12.
    pub warning_type: Option<u8>,
}

impl MessageId {
    pub fn from_attributes(attrs: &[ferrite_s100_gml::Attribute]) -> Option<Self> {
        let get = |code: &str| {
            attrs
                .iter()
                .find(|a| a.code == code)
                .and_then(|a| a.simple_value())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let series = get("nameOfSeries")?;
        let year = get("year").and_then(|y| y.parse::<i32>().ok()).map(|y| {
            if (0..100).contains(&y) {
                2000 + y
            } else {
                y
            }
        });
        Some(Self {
            agency: get("agencyResponsibleForProduction")
                .unwrap_or_default()
                .to_string(),
            nationality: get("nationality")
                .or_else(|| get("countryName"))
                .map(str::to_owned),
            series: series.to_string(),
            number: get("warningNumber").and_then(|n| n.parse().ok()),
            year,
            warning_type: get("warningType").and_then(|t| t.parse().ok()),
        })
    }

    pub fn series_key(&self) -> SeriesKey {
        SeriesKey {
            agency: norm(&self.agency),
            series: norm(&self.series),
        }
    }

    /// True when both identify the same message (series, number and year).
    pub fn same_message(&self, other: &MessageId) -> bool {
        self.number.is_some()
            && self.year.is_some()
            && self.number == other.number
            && self.year == other.year
            && norm(&self.series) == norm(&other.series)
            && norm(&self.agency) == norm(&other.agency)
    }

    /// Stable key for user state (classification), or None when the
    /// identity is incomplete.
    pub fn key(&self) -> Option<String> {
        Some(format!(
            "{}|{}|{}|{}",
            norm(&self.agency),
            norm(&self.series),
            self.number?,
            self.year?
        ))
    }
}

impl fmt::Display for MessageId {
    /// For example `WZ 43/26` or `NAVAREA II 85/22`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.series)?;
        if let Some(n) = self.number {
            write!(f, " {n}")?;
        }
        if let Some(y) = self.year {
            write!(f, "/{:02}", y.rem_euclid(100))?;
        }
        Ok(())
    }
}

/// warningType listed value labels (S-124 FC 2.0.0).
pub fn warning_type_label(code: u8) -> Option<&'static str> {
    Some(match code {
        1 => "Local Navigational Warning",
        2 => "Coastal Navigational Warning",
        3 => "Sub-Area Navigational Warning",
        4 => "NAVAREA Navigational Warning",
        5 => "NAVAREA No Warning",
        6 => "Sub-Area No Warning",
        7 => "Coastal No Warning",
        8 => "Local No Warning",
        9 => "NAVAREA In-Force Bulletin",
        10 => "Sub-Area In-Force Bulletin",
        11 => "Coastal In-Force Bulletin",
        12 => "Local In-Force Bulletin",
        _ => return None,
    })
}

/// navwarnTypeGeneral codelist labels (S-124 FC 2.0.0).
pub const NAVWARN_TYPE_GENERAL: [&str; 20] = [
    "Aids to Navigation Changes",
    "Aquaculture and Fishing Installations",
    "Drifting Hazards",
    "ECDIS Operating Anomalies including Official Data Issues",
    "Other Hazards",
    "Health Advisories",
    "Ice Information",
    "In-Force Bulletin",
    "Dangerous Natural Phenomena",
    "Newly Discovered Dangers",
    "Offshore Infrastructure",
    "Piracy or Robbery",
    "Communication or Broadcast Service Change",
    "Routeing Change",
    "Scientific Instruments Change",
    "Security Requirement Change",
    "Special Operations",
    "Towing Operations",
    "Works",
    "Rig List",
];

/// Label of a navwarnTypeGeneral code (1-based).
pub fn navwarn_type_general_label(code: u32) -> Option<&'static str> {
    NAVWARN_TYPE_GENERAL
        .get((code as usize).checked_sub(1)?)
        .copied()
}

// ---------------------------------------------------------------------------
// Time parsing
// ---------------------------------------------------------------------------

/// Parse an S-100 time of day (`hh:mm:ss`, `hhmmss`, `hhmm`, optional
/// fraction, `Z` or `±hh[:]mm` offset) to UTC.
pub fn parse_time_of_day(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    let (body, offset_min) = split_offset(s)?;
    let digits: String = body.chars().filter(|c| *c != ':').collect();
    let (main, _frac) = digits.split_once('.').unwrap_or((&digits, ""));
    if !main.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (h, m, sec) = match main.len() {
        6 => (&main[0..2], &main[2..4], &main[4..6]),
        4 => (&main[0..2], &main[2..4], "00"),
        2 => (&main[0..2], "00", "00"),
        _ => return None,
    };
    let (h, m, sec): (u32, u32, u32) = (h.parse().ok()?, m.parse().ok()?, sec.parse().ok()?);
    let t = if h == 24 && m == 0 && sec == 0 {
        NaiveTime::from_hms_opt(0, 0, 0)?
    } else {
        NaiveTime::from_hms_opt(h, m, sec)?
    };
    Some(t - Duration::minutes(offset_min as i64))
}

/// Split a trailing `Z` / `±hh[:]mm` zone designator; minutes east of UTC.
fn split_offset(s: &str) -> Option<(&str, i32)> {
    if let Some(b) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        return Some((b, 0));
    }
    // Look for a sign after the first two characters (time part only).
    if let Some(pos) = s.rfind(['+', '-']).filter(|p| *p >= 2) {
        let zone: String = s[pos + 1..].chars().filter(|c| *c != ':').collect();
        if zone.len() == 4 && zone.chars().all(|c| c.is_ascii_digit()) {
            let mins = zone[0..2].parse::<i32>().ok()? * 60 + zone[2..4].parse::<i32>().ok()?;
            let sign = if &s[pos..pos + 1] == "-" { -1 } else { 1 };
            return Some((&s[..pos], sign * mins));
        }
        if zone.len() == 2 && zone.chars().all(|c| c.is_ascii_digit()) {
            let sign = if &s[pos..pos + 1] == "-" { -1 } else { 1 };
            return Some((&s[..pos], sign * zone.parse::<i32>().ok()? * 60));
        }
    }
    Some((s, 0))
}

fn parse_full_date(s: &str) -> Option<NaiveDate> {
    let d: String = s.trim().chars().filter(|c| *c != '-').collect();
    if d.len() != 8 || !d.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    NaiveDate::from_ymd_opt(
        d[0..4].parse().ok()?,
        d[4..6].parse().ok()?,
        d[6..8].parse().ok()?,
    )
}

/// Parse an S-100 dateTime in extended (`2023-07-04T01:00:00Z`) or basic
/// (`20230704T010000Z`) form; no zone means UTC. A plain date is midnight.
pub fn parse_date_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    match s.split_once(['T', 't']) {
        Some((date, time)) => {
            let date = parse_full_date(date)?;
            let t = parse_time_with_carry(time)?;
            Some(Utc.from_utc_datetime(&date.and_time(t.0)) + Duration::days(t.1))
        }
        None => parse_full_date(s).map(|d| Utc.from_utc_datetime(&d.and_time(NaiveTime::MIN))),
    }
}

/// Time of day converted to UTC with the day carry caused by the offset.
fn parse_time_with_carry(s: &str) -> Option<(NaiveTime, i64)> {
    let (_, off) = split_offset(s.trim())?;
    let local = parse_time_of_day(&{
        let (b, _) = split_offset(s.trim())?;
        b.to_string()
    })?;
    let secs = local.num_seconds_from_midnight_i64() - off as i64 * 60;
    let carry = secs.div_euclid(86_400);
    let t = NaiveTime::from_num_seconds_from_midnight_opt(secs.rem_euclid(86_400) as u32, 0)?;
    Some((t, carry))
}

trait SecondsFromMidnight {
    fn num_seconds_from_midnight_i64(&self) -> i64;
}
impl SecondsFromMidnight for NaiveTime {
    fn num_seconds_from_midnight_i64(&self) -> i64 {
        use chrono::Timelike;
        self.num_seconds_from_midnight() as i64
    }
}

// ---------------------------------------------------------------------------
// fixedDateRange
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum DateSpec {
    Full(NaiveDate),
    /// `--MM-DD`: recurs every year.
    MonthDay(u32, u32),
}

fn parse_truncated_date(s: &str) -> Option<DateSpec> {
    let s = s.trim();
    if let Some(md) = s.strip_prefix("--") {
        let d: String = md.chars().filter(|c| *c != '-').collect();
        if d.len() == 4 && d.chars().all(|c| c.is_ascii_digit()) {
            let (m, day) = (d[0..2].parse().ok()?, d[2..4].parse().ok()?);
            NaiveDate::from_ymd_opt(2000, m, day)?; // validate (leap year)
            return Some(DateSpec::MonthDay(m, day));
        }
        return None;
    }
    parse_full_date(s).map(DateSpec::Full)
}

/// Timing of a NavwarnPart relative to its fixedDateRange values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PartTiming {
    /// No fixedDateRange: follows the dataset's validity.
    NoRange,
    /// Inside a range (or a range could not be read): must be shown.
    InRange,
    /// Before the first range: optionally shown (11.1.2 EXAMPLE 1).
    BeforeRange { starts: DateTime<Utc> },
    /// Every range has ended.
    AfterRange { ended: DateTime<Utc> },
    /// Only recurring (month-day) ranges and `now` is outside all of them.
    OutsideRecurring,
}

#[derive(Debug, Clone, Copy)]
struct Range {
    start: Option<DateTime<Utc>>,
    /// Inclusive end instant.
    end: Option<DateTime<Utc>>,
}

fn range_bound(
    date: DateSpec,
    time: Option<NaiveTime>,
    end: bool,
    year: i32,
) -> Option<DateTime<Utc>> {
    let d = match date {
        DateSpec::Full(d) => d,
        DateSpec::MonthDay(m, day) => NaiveDate::from_ymd_opt(year, m, day)
            .or_else(|| NaiveDate::from_ymd_opt(year, m, day - 1))?,
    };
    Some(match (time, end) {
        (Some(t), _) => Utc.from_utc_datetime(&d.and_time(t)),
        (None, false) => Utc.from_utc_datetime(&d.and_time(NaiveTime::MIN)),
        (None, true) => {
            Utc.from_utc_datetime(&d.and_time(NaiveTime::MIN)) + Duration::days(1)
                - Duration::milliseconds(1)
        }
    })
}

/// Timing of one NavwarnPart at `now`.
pub fn part_timing(part: &Object, now: DateTime<Utc>) -> PartTiming {
    let mut any = false;
    let mut first_future: Option<DateTime<Utc>> = None;
    let mut last_end: Option<DateTime<Utc>> = None;
    let mut recurring_outside = false;
    for r in part.attributes_named("fixedDateRange") {
        any = true;
        let get = |c: &str| {
            r.children()
                .iter()
                .find(|a| a.code == c)
                .and_then(|a| a.simple_value())
                .filter(|s| !s.trim().is_empty())
        };
        let ds = get("dateStart").map(parse_truncated_date);
        let de = get("dateEnd").map(parse_truncated_date);
        let ts = get("timeOfDayStart").map(parse_time_of_day);
        let te = get("timeOfDayEnd").map(parse_time_of_day);
        // Unreadable values: conservative, treat as covering now.
        if matches!(ds, Some(None))
            || matches!(de, Some(None))
            || matches!(ts, Some(None))
            || matches!(te, Some(None))
        {
            return PartTiming::InRange;
        }
        let (ds, de, ts, te) = (ds.flatten(), de.flatten(), ts.flatten(), te.flatten());
        let recurring = matches!(ds, Some(DateSpec::MonthDay(..)))
            || matches!(de, Some(DateSpec::MonthDay(..)));
        if recurring {
            // Try the occurrence starting last year and this year.
            let mut inside = false;
            for y in [now.year_ce() - 1, now.year_ce()] {
                let start = ds.and_then(|d| range_bound(d, ts, false, y));
                let mut end = de.and_then(|d| range_bound(d, te, true, y));
                if let (Some(s), Some(e)) = (start, end) {
                    if e < s {
                        end = de.and_then(|d| range_bound(d, te, true, y + 1));
                    }
                }
                let after_start = start.is_none_or(|s| now >= s);
                let before_end = end.is_none_or(|e| now <= e);
                if after_start && before_end {
                    inside = true;
                }
            }
            if inside {
                return PartTiming::InRange;
            }
            recurring_outside = true;
            continue;
        }
        let range = Range {
            start: ds.and_then(|d| range_bound(d, ts, false, 0)),
            end: de.and_then(|d| range_bound(d, te, true, 0)),
        };
        let after_start = range.start.is_none_or(|s| now >= s);
        let before_end = range.end.is_none_or(|e| now <= e);
        if after_start && before_end {
            return PartTiming::InRange;
        }
        if !after_start {
            let s = range.start.expect("start set");
            first_future = Some(first_future.map_or(s, |f| f.min(s)));
        } else if let Some(e) = range.end {
            last_end = Some(last_end.map_or(e, |l| l.max(e)));
        }
    }
    if !any {
        return PartTiming::NoRange;
    }
    if let Some(starts) = first_future {
        return PartTiming::BeforeRange { starts };
    }
    if let Some(ended) = last_end {
        return PartTiming::AfterRange { ended };
    }
    if recurring_outside {
        return PartTiming::OutsideRecurring;
    }
    PartTiming::InRange
}

trait YearCe {
    fn year_ce(&self) -> i32;
}
impl YearCe for DateTime<Utc> {
    fn year_ce(&self) -> i32 {
        use chrono::Datelike;
        self.year()
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Why a dataset is cancelled (9.3). Indices are into the evaluated inputs.
#[derive(Debug, Clone, PartialEq)]
pub enum CancelReason {
    /// cancellationDate has passed.
    CancellationDate(DateTime<Utc>),
    /// A References with referenceCategory 1 in another dataset.
    CancelledBy { dataset: usize },
    /// Prior to and not listed in the latest in-force bulletin of its series.
    NotInForceBulletin { bulletin: usize },
    /// An in-force bulletin superseded by a newer bulletin of its series.
    SupersededInForceBulletin { by: usize },
    /// An imported copy replaced by an authoritative dataset of the same
    /// message identity.
    ReplacedByAuthoritative { by: usize },
}

impl CancelReason {
    fn rank(&self) -> u8 {
        match self {
            Self::CancelledBy { .. } => 0,
            Self::CancellationDate(_) => 1,
            Self::SupersededInForceBulletin { .. } => 2,
            Self::ReplacedByAuthoritative { .. } => 3,
            Self::NotInForceBulletin { .. } => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Active,
    /// Valid but not yet in effect (publication in the future, or every
    /// NavwarnPart's fixedDateRange still to come).
    NotYetActive,
    /// Not displayed on the navigation screen; listed as cancelled (11.1.3).
    Cancelled(CancelReason),
    /// The latest in-force bulletin of its series.
    InForceBulletin,
    /// A cancellation dataset without NavwarnPart: it only cancels.
    CancellationOnly,
}

impl Status {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
    }
    /// Short label for listings.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::NotYetActive => "Not yet active",
            Self::Cancelled(_) => "Cancelled",
            Self::InForceBulletin => "In-force bulletin",
            Self::CancellationOnly => "Cancellation",
        }
    }
}

/// User relevance classification (11.1.2). Default is on chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub enum UserClassification {
    #[default]
    OnChart,
    OffChart,
    Information,
}

/// A loaded S-124 dataset and when it was received into the system.
#[derive(Debug, Clone, Copy)]
pub struct NavwarnInput<'a> {
    pub dataset: &'a Dataset,
    pub received: DateTime<Utc>,
}

/// A NavwarnPart and its associated features.
#[derive(Debug, Clone)]
pub struct PartInfo {
    pub id: String,
    pub timing: PartTiming,
    /// TextPlacement feature ids associated with the part.
    pub text_placements: Vec<String>,
    /// NavwarnAreaAffected feature ids associated with the part.
    pub areas_affected: Vec<String>,
    /// warningInformation/information/text values.
    pub information: Vec<String>,
    /// warningInformation/navwarnTypeDetails labels (or codes).
    pub type_details: Vec<String>,
    pub has_geometry: bool,
}

impl PartInfo {
    /// Abbreviated view of the warningInformation (11.1.1 g).
    pub fn abbreviated(&self, max_chars: usize) -> String {
        let mut s = self.type_details.join(", ");
        for i in &self.information {
            if !s.is_empty() {
                s.push_str(": ");
            }
            s.push_str(i.trim());
        }
        let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if s.chars().count() > max_chars {
            let mut t: String = s.chars().take(max_chars.saturating_sub(1)).collect();
            t.push('…');
            t
        } else {
            s
        }
    }
}

/// Evaluated state of one input dataset.
#[derive(Debug, Clone)]
pub struct MessageInfo {
    /// Index into the evaluated inputs.
    pub index: usize,
    pub identity: Option<MessageId>,
    /// Key for user state: identity key, else `sha256:<hex>` of the dataset.
    pub key: String,
    /// `sha256:<hex>` of the dataset, the key for the new-message indication.
    pub dataset_key: String,
    pub warning_type: Option<u8>,
    pub navwarn_type_general: Option<String>,
    pub title: Option<String>,
    pub general_area: Vec<String>,
    pub locality: Vec<String>,
    pub publication_time: Option<DateTime<Utc>>,
    pub cancellation_date: Option<DateTime<Utc>>,
    pub received: DateTime<Utc>,
    pub imported: bool,
    pub is_bulletin: bool,
    pub no_message_on_hand: bool,
    /// Messages this dataset cancels (References category 1).
    pub cancels: Vec<MessageId>,
    /// Messages listed in force (References category 3).
    pub in_force: Vec<MessageId>,
    /// Other referenced messages (References category 2).
    pub references: Vec<MessageId>,
    pub status: Status,
    /// Every applicable reason; the status carries the first by priority
    /// (explicit cancellation, cancellation date, superseded, replaced,
    /// bulletin fail-safe).
    pub cancel_reasons: Vec<CancelReason>,
    pub parts: Vec<PartInfo>,
    /// Ids of every feature (geometry carrying) object of the dataset.
    pub feature_ids: Vec<String>,
    /// TextPlacement not associated with any NavwarnPart.
    pub unassociated_text: Vec<String>,
    /// NavwarnAreaAffected not associated with any NavwarnPart.
    pub unassociated_areas: Vec<String>,
}

impl MessageInfo {
    /// Time used for "prior to the bulletin" and issue-time ordering.
    pub fn effective_time(&self) -> DateTime<Utc> {
        self.publication_time.unwrap_or(self.received)
    }
    pub fn series(&self) -> Option<SeriesKey> {
        self.identity.as_ref().map(MessageId::series_key)
    }
    /// Listing caption, e.g. `WZ 43/26 OUTER SPIT BUOY OFF STATION.`
    pub fn caption(&self) -> String {
        let id = self.identity.as_ref().map(|i| i.to_string());
        match (id, &self.title) {
            (Some(i), Some(t)) => format!("{i} {t}"),
            (Some(i), None) => i,
            (None, Some(t)) => t.clone(),
            (None, None) => self.dataset_key.clone(),
        }
    }
}

/// A listed warning of the latest in-force bulletin that is not loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct MissingMessage {
    pub id: MessageId,
    /// An imported (non-authoritative) copy, if one is loaded.
    pub imported_copy: Option<usize>,
}

/// The latest in-force bulletin of a series (9.2.2).
#[derive(Debug, Clone)]
pub struct BulletinSummary {
    pub series: SeriesKey,
    pub bulletin: usize,
    pub no_message_on_hand: bool,
    /// Listed warnings not present in the system: an indication should be
    /// given (9.2.2).
    pub missing: Vec<MissingMessage>,
}

/// Portrayal options for [`NavwarnRegistry::should_portray`].
#[derive(Debug, Clone, Copy)]
pub struct PortrayalPolicy {
    /// Show parts before their fixedDateRange starts (optional per 11.1.2
    /// EXAMPLE 1). Default true.
    pub show_before_fixed_date_range: bool,
    /// Show messages the user classified as Information. Default false
    /// ("need not be visualized").
    pub show_information: bool,
    /// Show messages whose publication time is in the future. Default true.
    pub show_not_yet_published: bool,
    /// Show imported (non-authoritative) datasets. Default true.
    pub show_imported: bool,
}

impl Default for PortrayalPolicy {
    fn default() -> Self {
        Self {
            show_before_fixed_date_range: true,
            show_information: false,
            show_not_yet_published: true,
            show_imported: true,
        }
    }
}

/// `sha256:<hex>` of a dataset: [`MessageInfo::dataset_key`], the key of the
/// new-message indication and of the received time.
pub fn dataset_key(ds: &Dataset) -> String {
    format!("sha256:{}", hex(&ds.sha256))
}

/// Per-user state (11.1.1 a, c): classifications by message key, the
/// datasets already displayed and when each dataset was first received (both
/// by dataset key). The caller persists it per user so that it survives a
/// restart or watch handover (11.1.1).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NavwarnUserState {
    pub classification: HashMap<String, UserClassification>,
    pub displayed: HashSet<String>,
    /// First receipt of each dataset (by dataset key).
    pub received: HashMap<String, DateTime<Utc>>,
}

/// How long first-receipt times are kept after receipt ([`NavwarnUserState::prune`]).
pub const RECEIVED_RETENTION_DAYS: i64 = 400;

impl NavwarnUserState {
    /// When dataset `key` was first received: the recorded time, else `now`
    /// (recorded). A recorded time in the future of `now` (clock moved back)
    /// is replaced by `now`. Returns the time and whether the state changed.
    pub fn receive(&mut self, dataset_key: &str, now: DateTime<Utc>) -> (DateTime<Utc>, bool) {
        match self.received.get(dataset_key) {
            Some(t) if *t <= now => (*t, false),
            _ => {
                self.received.insert(dataset_key.to_owned(), now);
                (now, true)
            }
        }
    }
    /// Drop receipt times older than [`RECEIVED_RETENTION_DAYS`] and
    /// "displayed" marks that can no longer affect the new-message indication
    /// (receipt unknown or older than [`NEW_MESSAGE_PERIOD_HOURS`]).
    /// Classifications are kept. Returns true when anything was removed.
    pub fn prune(&mut self, now: DateTime<Utc>) -> bool {
        let before = (self.received.len(), self.displayed.len());
        self.received
            .retain(|_, t| now - *t < Duration::days(RECEIVED_RETENTION_DAYS));
        let received = &self.received;
        self.displayed.retain(|k| {
            received
                .get(k)
                .is_some_and(|t| now - *t < Duration::hours(NEW_MESSAGE_PERIOD_HOURS))
        });
        before != (self.received.len(), self.displayed.len())
    }
    pub fn classification_of(&self, m: &MessageInfo) -> UserClassification {
        self.classification.get(&m.key).copied().unwrap_or_default()
    }
    pub fn classify(&mut self, m: &MessageInfo, c: UserClassification) {
        if c == UserClassification::OnChart {
            self.classification.remove(&m.key);
        } else {
            self.classification.insert(m.key.clone(), c);
        }
    }
    pub fn mark_displayed(&mut self, m: &MessageInfo) {
        self.displayed.insert(m.dataset_key.clone());
    }
}

/// How long the new-message indication lasts without display (11.1.1 c).
pub const NEW_MESSAGE_PERIOD_HOURS: i64 = 24;

/// Listing order (11.1.1 b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// Newest received first.
    Received,
    /// Newest publicationTime first.
    IssueTime,
    /// warningType code, then series and number.
    WarningType,
    /// Producer, series, year, number.
    ProducerSeries,
    /// On chart, off chart, information; then newest issue first.
    UserClassification,
}

/// Feature ids of one dataset to hide or highlight on the chart.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeatureFilter {
    /// Features not to portray.
    pub suppress: HashSet<String>,
    /// Features to highlight (selected NavwarnPart and its
    /// NavwarnAreaAffected, 11.2).
    pub highlight: HashSet<String>,
}

/// Evaluated lifecycle of all loaded S-124 datasets at one instant.
#[derive(Debug, Clone)]
pub struct NavwarnRegistry {
    pub now: DateTime<Utc>,
    /// One entry per input, in input order.
    pub messages: Vec<MessageInfo>,
    /// The latest bulletin of each series with one.
    pub bulletins: Vec<BulletinSummary>,
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

fn texts_of(obj: &Object, code: &str) -> Vec<String> {
    obj.attributes_named(code)
        .filter_map(|a| {
            a.children()
                .iter()
                .find(|c| c.code == "text")
                .and_then(|c| c.text())
                .or_else(|| {
                    a.children()
                        .iter()
                        .find(|c| c.code == "locationName")
                        .and_then(|l| {
                            l.children()
                                .iter()
                                .find(|c| c.code == "text")
                                .and_then(|c| c.text())
                        })
                })
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn area_names(obj: &Object, code: &str) -> Vec<String> {
    let mut out = Vec::new();
    for a in obj.attributes_named(code) {
        for l in a.children().iter().filter(|c| c.code == "locationName") {
            if let Some(t) = l
                .children()
                .iter()
                .find(|c| c.code == "text")
                .and_then(|c| c.text())
            {
                if !t.trim().is_empty() {
                    out.push(t.trim().to_string());
                }
            }
        }
    }
    out
}

fn bool_value(obj: &Object, code: &str) -> Option<bool> {
    obj.value(&[code])
        .map(|v| matches!(v.trim(), "true" | "1" | "True" | "TRUE"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct RefSet {
    category: Option<u8>,
    no_message_on_hand: bool,
    ids: Vec<MessageId>,
}

fn part_info(ds: &Dataset, idx: usize, now: DateTime<Utc>) -> PartInfo {
    let part = &ds.objects[idx];
    let mut text_placements = Vec::new();
    let mut areas = Vec::new();
    // Associations either direction, by target class (role names vary
    // between producers and editions).
    for t in part.references.iter().filter_map(|r| r.target) {
        match ds.objects[t].code.as_str() {
            "TextPlacement" => text_placements.push(ds.objects[t].id.clone()),
            "NavwarnAreaAffected" => areas.push(ds.objects[t].id.clone()),
            _ => {}
        }
    }
    for o in &ds.objects {
        if o.references.iter().any(|r| r.target == Some(idx)) {
            let list = match o.code.as_str() {
                "TextPlacement" => &mut text_placements,
                "NavwarnAreaAffected" => &mut areas,
                _ => continue,
            };
            if !list.contains(&o.id) {
                list.push(o.id.clone());
            }
        }
    }
    let mut information = Vec::new();
    let mut type_details = Vec::new();
    for wi in part.attributes_named("warningInformation") {
        for c in wi.children() {
            match c.code.as_str() {
                "information" => {
                    if let Some(t) = c
                        .children()
                        .iter()
                        .find(|a| a.code == "text")
                        .and_then(|a| a.text())
                    {
                        information.push(t.trim().to_string());
                    }
                }
                "navwarnTypeDetails" => {
                    if let Value::Simple { text, code } = &c.value {
                        let label = if text.trim().is_empty() {
                            code.clone().unwrap_or_default()
                        } else {
                            text.trim().to_string()
                        };
                        if !label.is_empty() {
                            type_details.push(label);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    PartInfo {
        id: part.id.clone(),
        timing: part_timing(part, now),
        text_placements,
        areas_affected: areas,
        information,
        type_details,
        has_geometry: !part.geometry.is_empty(),
    }
}

fn extract(input: &NavwarnInput, index: usize, now: DateTime<Utc>) -> (MessageInfo, Vec<RefSet>) {
    let ds = input.dataset;
    let preamble = ds.objects_of("NavwarnPreamble").map(|(_, o)| o).next();
    let identity = preamble
        .and_then(|p| p.attribute("messageSeriesIdentifier"))
        .and_then(|a| MessageId::from_attributes(a.children()));
    let dataset_key = dataset_key(ds);
    let key = identity
        .as_ref()
        .and_then(MessageId::key)
        .unwrap_or_else(|| dataset_key.clone());
    let refs: Vec<RefSet> = ds
        .objects_of("References")
        .map(|(_, r)| RefSet {
            category: r
                .value(&["referenceCategory"])
                .and_then(|v| v.trim().parse().ok()),
            no_message_on_hand: bool_value(r, "noMessageOnHand").unwrap_or(false),
            ids: r
                .attributes_named("messageSeriesIdentifier")
                .filter_map(|a| MessageId::from_attributes(a.children()))
                .collect(),
        })
        .collect();
    let warning_type = identity.as_ref().and_then(|i| i.warning_type);
    let is_bulletin = !ds.imported
        && (matches!(warning_type, Some(9..=12))
            || refs.iter().any(|r| r.category == Some(3))
            || (matches!(warning_type, Some(5..=8)) && refs.iter().any(|r| r.no_message_on_hand)));
    let no_message_on_hand = is_bulletin && refs.iter().any(|r| r.no_message_on_hand);
    let collect = |cat: u8| -> Vec<MessageId> {
        refs.iter()
            .filter(|r| r.category == Some(cat) && !r.no_message_on_hand)
            .flat_map(|r| r.ids.iter().cloned())
            .collect()
    };
    let cancels = if ds.imported { Vec::new() } else { collect(1) };
    let in_force = if is_bulletin && !no_message_on_hand {
        collect(3)
    } else {
        Vec::new()
    };
    let references = collect(2);
    let parts: Vec<PartInfo> = ds
        .objects_of("NavwarnPart")
        .map(|(i, _)| part_info(ds, i, now))
        .collect();
    let associated_text: HashSet<&str> = parts
        .iter()
        .flat_map(|p| p.text_placements.iter().map(String::as_str))
        .collect();
    let associated_area: HashSet<&str> = parts
        .iter()
        .flat_map(|p| p.areas_affected.iter().map(String::as_str))
        .collect();
    let unassociated_text = ds
        .objects_of("TextPlacement")
        .filter(|(_, o)| !associated_text.contains(o.id.as_str()))
        .map(|(_, o)| o.id.clone())
        .collect();
    let unassociated_areas = ds
        .objects_of("NavwarnAreaAffected")
        .filter(|(_, o)| !associated_area.contains(o.id.as_str()))
        .map(|(_, o)| o.id.clone())
        .collect();
    let feature_ids = ds
        .objects
        .iter()
        .filter(|o| o.kind == ObjectKind::Feature || !o.geometry.is_empty())
        .map(|o| o.id.clone())
        .collect();
    let info = MessageInfo {
        index,
        key,
        dataset_key,
        warning_type,
        navwarn_type_general: preamble
            .and_then(|p| p.attribute("navwarnTypeGeneral"))
            .and_then(|a| match &a.value {
                Value::Simple { text, code } => {
                    let t = text.trim();
                    if !t.is_empty() {
                        Some(t.to_string())
                    } else {
                        code.as_deref()
                            .and_then(|c| c.trim().parse().ok())
                            .and_then(navwarn_type_general_label)
                            .map(str::to_owned)
                            .or_else(|| code.clone())
                    }
                }
                _ => None,
            }),
        title: preamble
            .map(|p| texts_of(p, "navwarnTitle"))
            .and_then(|t| (!t.is_empty()).then(|| t.join(" / ")))
            .or_else(|| ds.identification.title().map(str::to_owned)),
        general_area: preamble
            .map(|p| area_names(p, "generalArea"))
            .unwrap_or_default(),
        locality: preamble
            .map(|p| area_names(p, "locality"))
            .unwrap_or_default(),
        publication_time: preamble
            .and_then(|p| p.value(&["publicationTime"]))
            .and_then(parse_date_time),
        cancellation_date: preamble
            .and_then(|p| p.value(&["cancellationDate"]))
            .and_then(parse_date_time),
        received: input.received,
        imported: ds.imported,
        is_bulletin,
        no_message_on_hand,
        cancels,
        in_force,
        references,
        status: Status::Active,
        cancel_reasons: Vec::new(),
        parts,
        feature_ids,
        unassociated_text,
        unassociated_areas,
        identity,
    };
    (info, refs)
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Evaluate the lifecycle of every loaded S-124 dataset at `now`.
pub fn evaluate(inputs: &[NavwarnInput], now: DateTime<Utc>) -> NavwarnRegistry {
    let mut messages: Vec<MessageInfo> = inputs
        .iter()
        .enumerate()
        .map(|(i, input)| extract(input, i, now).0)
        .collect();
    let n = messages.len();
    let mut reasons: Vec<Vec<CancelReason>> = vec![Vec::new(); n];

    // 9.3 bullet 1: cancellationDate passed.
    for (i, m) in messages.iter().enumerate() {
        if let Some(c) = m.cancellation_date {
            if now >= c {
                reasons[i].push(CancelReason::CancellationDate(c));
            }
        }
    }
    // 9.3 bullets 2-3: References category 1 in another dataset.
    for (j, canceller) in messages.iter().enumerate() {
        for target in &canceller.cancels {
            for (i, m) in messages.iter().enumerate() {
                if i == j {
                    continue;
                }
                if m.identity
                    .as_ref()
                    .is_some_and(|id| id.same_message(target))
                    && !reasons[i].contains(&CancelReason::CancelledBy { dataset: j })
                {
                    reasons[i].push(CancelReason::CancelledBy { dataset: j });
                }
            }
        }
    }
    // Imported copies replaced by an authoritative dataset of the same message.
    for (i, m) in messages.iter().enumerate().filter(|(_, m)| m.imported) {
        let Some(id) = &m.identity else { continue };
        if let Some(j) = messages.iter().position(|o| {
            !o.imported && o.identity.as_ref().is_some_and(|oid| oid.same_message(id))
        }) {
            reasons[i].push(CancelReason::ReplacedByAuthoritative { by: j });
        }
    }
    // 9.2.1 / 9.3 bullet 4: latest in-force bulletin per series.
    let mut by_series: HashMap<SeriesKey, Vec<usize>> = HashMap::new();
    for (i, m) in messages.iter().enumerate().filter(|(_, m)| m.is_bulletin) {
        if let Some(s) = m.series() {
            by_series.entry(s).or_default().push(i);
        }
    }
    let mut series_list: Vec<_> = by_series.into_iter().collect();
    series_list.sort_by(|a, b| a.0.cmp(&b.0));
    let mut bulletins = Vec::new();
    for (series, list) in series_list {
        let latest = *list
            .iter()
            .max_by_key(|&&i| (messages[i].effective_time(), messages[i].received, i))
            .expect("non-empty");
        // A bulletin always cancels the previous bulletin.
        for &i in list.iter().filter(|&&i| i != latest) {
            reasons[i].push(CancelReason::SupersededInForceBulletin { by: latest });
        }
        let b = &messages[latest];
        let b_time = b.effective_time();
        let listed = |id: &MessageId| b.in_force.iter().any(|l| l.same_message(id));
        for (i, m) in messages.iter().enumerate() {
            if m.is_bulletin || m.series().as_ref() != Some(&series) {
                continue;
            }
            if m.effective_time() >= b_time {
                continue; // only datasets prior to the bulletin
            }
            let in_list = m.identity.as_ref().is_some_and(listed);
            if !in_list {
                reasons[i].push(CancelReason::NotInForceBulletin { bulletin: latest });
            }
        }
        // 9.2.2: listed warnings not present in the system.
        let mut missing = Vec::new();
        for id in &b.in_force {
            // The bulletin lists itself (8.1.3) and may list other bulletins.
            if messages
                .iter()
                .any(|m| m.is_bulletin && m.identity.as_ref().is_some_and(|x| x.same_message(id)))
            {
                continue;
            }
            if matches!(id.warning_type, Some(9..=12)) {
                continue;
            }
            let present = messages
                .iter()
                .any(|m| !m.imported && m.identity.as_ref().is_some_and(|x| x.same_message(id)));
            if !present
                && !missing
                    .iter()
                    .any(|x: &MissingMessage| x.id.same_message(id))
            {
                missing.push(MissingMessage {
                    id: id.clone(),
                    imported_copy: messages.iter().position(|m| {
                        m.imported && m.identity.as_ref().is_some_and(|x| x.same_message(id))
                    }),
                });
            }
        }
        bulletins.push(BulletinSummary {
            series,
            bulletin: latest,
            no_message_on_hand: b.no_message_on_hand,
            missing,
        });
    }

    for (m, mut r) in messages.iter_mut().zip(reasons) {
        r.sort_by_key(CancelReason::rank);
        let superseded = r
            .iter()
            .find(|x| matches!(x, CancelReason::SupersededInForceBulletin { .. }))
            .cloned();
        m.status = if m.is_bulletin {
            match superseded {
                Some(s) => Status::Cancelled(s),
                // The latest bulletin stays the series' fail-safe reference
                // even after its own cancellation date.
                None => Status::InForceBulletin,
            }
        } else if m.parts.is_empty() && !m.cancels.is_empty() {
            Status::CancellationOnly
        } else if let Some(first) = r.first() {
            Status::Cancelled(first.clone())
        } else if m.publication_time.is_some_and(|p| p > now)
            || (!m.parts.is_empty()
                && m.parts
                    .iter()
                    .all(|p| matches!(p.timing, PartTiming::BeforeRange { .. })))
        {
            Status::NotYetActive
        } else {
            Status::Active
        };
        m.cancel_reasons = r;
    }
    NavwarnRegistry {
        now,
        messages,
        bulletins,
    }
}

impl NavwarnRegistry {
    pub fn message(&self, index: usize) -> Option<&MessageInfo> {
        self.messages.get(index)
    }

    /// Bulletin summaries with at least one listed warning missing (9.2.2).
    pub fn missing_indications(&self) -> impl Iterator<Item = &BulletinSummary> {
        self.bulletins.iter().filter(|b| !b.missing.is_empty())
    }

    /// New-message indication (11.1.1 c): until displayed or 24 h after
    /// receipt. `meets_filter` lets the caller suppress the indication for
    /// messages outside the user's filter criteria.
    pub fn is_new(&self, index: usize, state: &NavwarnUserState, meets_filter: bool) -> bool {
        self.is_new_at(index, state, meets_filter, self.now)
    }

    /// [`Self::is_new`] at `clock`: the 24 h period runs on the real clock
    /// even when the lifecycle is evaluated at a user-selected viewing time.
    pub fn is_new_at(
        &self,
        index: usize,
        state: &NavwarnUserState,
        meets_filter: bool,
        clock: DateTime<Utc>,
    ) -> bool {
        let Some(m) = self.messages.get(index) else {
            return false;
        };
        meets_filter
            && !state.displayed.contains(&m.dataset_key)
            && clock >= m.received
            && clock - m.received < Duration::hours(NEW_MESSAGE_PERIOD_HOURS)
    }

    /// Whether the dataset as a whole may be portrayed on the chart.
    pub fn dataset_portrayed(
        &self,
        index: usize,
        state: &NavwarnUserState,
        policy: &PortrayalPolicy,
    ) -> bool {
        let Some(m) = self.messages.get(index) else {
            return false;
        };
        let status_ok = match &m.status {
            Status::Active => true,
            Status::NotYetActive => {
                // Future publication: policy; parts before their range are
                // decided per part.
                !m.publication_time.is_some_and(|p| p > self.now) || policy.show_not_yet_published
            }
            // 11.1.3: cancelled datasets are not displayed. Bulletins carry
            // no geometry (8.1.2) and cancellation-only datasets nothing to show.
            Status::Cancelled(_) | Status::InForceBulletin | Status::CancellationOnly => false,
        };
        let class_ok = match state.classification_of(m) {
            UserClassification::OnChart => true,
            UserClassification::OffChart => false,
            UserClassification::Information => policy.show_information,
        };
        status_ok && class_ok && (!m.imported || policy.show_imported)
    }

    /// Whether NavwarnPart `part_id` of dataset `index` is portrayed.
    pub fn should_portray(
        &self,
        index: usize,
        part_id: &str,
        state: &NavwarnUserState,
        policy: &PortrayalPolicy,
    ) -> bool {
        if !self.dataset_portrayed(index, state, policy) {
            return false;
        }
        let Some(part) = self.messages[index].parts.iter().find(|p| p.id == part_id) else {
            return false;
        };
        timing_visible(part.timing, policy)
    }

    /// Features of dataset `index` to suppress and to highlight given the
    /// user's selection (`selected`: ids of a NavwarnPart, or of its
    /// TextPlacement / NavwarnAreaAffected picked on the chart).
    ///
    /// * A NavwarnPart not portrayed is suppressed together with its
    ///   TextPlacement features (11.2).
    /// * NavwarnAreaAffected has no portrayal: it is suppressed unless its
    ///   NavwarnPart is selected, then highlighted (11.2) — also when the part
    ///   itself is filtered from the chart, so a selection from the listing
    ///   can be interrogated. Cancelled datasets are never highlighted.
    /// * A selected part that is portrayed is highlighted as well.
    pub fn feature_filter(
        &self,
        index: usize,
        state: &NavwarnUserState,
        policy: &PortrayalPolicy,
        selected: &[&str],
    ) -> FeatureFilter {
        let mut f = FeatureFilter::default();
        let Some(m) = self.messages.get(index) else {
            return f;
        };
        let portrayed = self.dataset_portrayed(index, state, policy);
        let selectable = !matches!(m.status, Status::Cancelled(_));
        let selected_parts: HashSet<&str> = m
            .parts
            .iter()
            .filter(|p| {
                selected.iter().any(|s| {
                    *s == p.id
                        || p.text_placements.iter().any(|t| t == s)
                        || p.areas_affected.iter().any(|a| a == s)
                })
            })
            .map(|p| p.id.as_str())
            .collect();
        for p in &m.parts {
            let show = portrayed && timing_visible(p.timing, policy);
            let is_selected = selectable && selected_parts.contains(p.id.as_str());
            if show {
                if is_selected {
                    f.highlight.insert(p.id.clone());
                }
            } else {
                f.suppress.insert(p.id.clone());
                f.suppress.extend(p.text_placements.iter().cloned());
            }
            for a in &p.areas_affected {
                if is_selected {
                    f.highlight.insert(a.clone());
                } else {
                    f.suppress.insert(a.clone());
                }
            }
        }
        f.suppress.extend(m.unassociated_areas.iter().cloned());
        if !portrayed {
            f.suppress.extend(m.unassociated_text.iter().cloned());
            for id in &m.feature_ids {
                if !f.highlight.contains(id) {
                    f.suppress.insert(id.clone());
                }
            }
        }
        // Never both.
        let hl = f.highlight.clone();
        f.suppress.retain(|id| !hl.contains(id));
        f
    }

    /// Indices of all messages in listing order (11.1.1 b).
    pub fn sorted(&self, order: SortOrder, state: &NavwarnUserState) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.messages.len()).collect();
        let m = &self.messages;
        let ident = |i: usize| {
            let id = m[i].identity.as_ref();
            (
                id.map(|x| norm(&x.agency)).unwrap_or_default(),
                id.map(|x| norm(&x.series)).unwrap_or_default(),
                id.and_then(|x| x.year).unwrap_or(i32::MIN),
                id.and_then(|x| x.number).unwrap_or(0),
            )
        };
        match order {
            SortOrder::Received => {
                idx.sort_by(|&a, &b| m[b].received.cmp(&m[a].received).then(a.cmp(&b)))
            }
            SortOrder::IssueTime => idx.sort_by(|&a, &b| {
                m[b].effective_time()
                    .cmp(&m[a].effective_time())
                    .then(a.cmp(&b))
            }),
            SortOrder::WarningType => idx.sort_by(|&a, &b| {
                m[a].warning_type
                    .unwrap_or(u8::MAX)
                    .cmp(&m[b].warning_type.unwrap_or(u8::MAX))
                    .then_with(|| ident(a).cmp(&ident(b)))
                    .then(a.cmp(&b))
            }),
            SortOrder::ProducerSeries => {
                idx.sort_by(|&a, &b| ident(a).cmp(&ident(b)).then(a.cmp(&b)))
            }
            SortOrder::UserClassification => idx.sort_by(|&a, &b| {
                state
                    .classification_of(&m[a])
                    .cmp(&state.classification_of(&m[b]))
                    .then_with(|| m[b].effective_time().cmp(&m[a].effective_time()))
                    .then(a.cmp(&b))
            }),
        }
        idx
    }
}

fn timing_visible(t: PartTiming, policy: &PortrayalPolicy) -> bool {
    match t {
        PartTiming::NoRange | PartTiming::InRange => true,
        PartTiming::BeforeRange { .. } => policy.show_before_fixed_date_range,
        PartTiming::AfterRange { .. } | PartTiming::OutsideRecurring => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_time_forms() {
        let a = parse_date_time("20230704T010000Z").unwrap();
        assert_eq!(a, Utc.with_ymd_and_hms(2023, 7, 4, 1, 0, 0).unwrap());
        assert_eq!(parse_date_time("2023-07-04T01:00:00Z"), Some(a));
        assert_eq!(parse_date_time("2023-07-04T01:00:00"), Some(a));
        assert_eq!(parse_date_time("2023-07-04T03:00:00+02:00"), Some(a));
        assert_eq!(parse_date_time("20230704T0100Z"), Some(a));
        assert_eq!(
            parse_date_time("2023-07-04T00:30:00-01:00"),
            Some(Utc.with_ymd_and_hms(2023, 7, 4, 1, 30, 0).unwrap())
        );
        assert_eq!(
            parse_time_of_day("160000Z"),
            NaiveTime::from_hms_opt(16, 0, 0)
        );
        assert_eq!(
            parse_time_of_day("2359Z"),
            NaiveTime::from_hms_opt(23, 59, 0)
        );
        assert!(parse_date_time("garbage").is_none());
    }

    #[test]
    fn receipt_times_are_kept_and_pruned() {
        let t0 = Utc.with_ymd_and_hms(2026, 2, 10, 0, 0, 0).unwrap();
        let mut s = NavwarnUserState::default();
        assert_eq!(s.receive("sha256:a", t0), (t0, true));
        // A later sighting keeps the first receipt.
        assert_eq!(s.receive("sha256:a", t0 + Duration::hours(30)), (t0, false));
        // Clock moved back before the receipt: re-recorded.
        let back = t0 - Duration::hours(1);
        assert_eq!(s.receive("sha256:a", back), (back, true));
        s.displayed.insert("sha256:a".into());
        s.displayed.insert("sha256:unknown".into());
        s.classification
            .insert("UKHO|WZ|1|2026".into(), UserClassification::OffChart);
        assert!(s.prune(back + Duration::hours(1)));
        assert!(s.displayed.contains("sha256:a"));
        assert!(!s.displayed.contains("sha256:unknown"));
        assert!(s.prune(back + Duration::hours(25)));
        assert!(s.displayed.is_empty() && s.received.contains_key("sha256:a"));
        assert!(s.prune(back + Duration::days(RECEIVED_RETENTION_DAYS)));
        assert!(s.received.is_empty());
        assert_eq!(s.classification.len(), 1);
        assert!(!s.prune(back + Duration::days(RECEIVED_RETENTION_DAYS)));
    }
}

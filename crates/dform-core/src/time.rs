//! `time` (R-62): a zoned instant, the Temporal model through jiff. A time
//! carries its zone, prints canonically
//! (`2026-10-02T09:00:00+02:00[Europe/Paris]`), and orders by its instant.
//! Zone data is the tzdb bundled into dform (jiff-tzdb), never the host's,
//! so a plan is the same on every machine.

use crate::quantity::Span;
use jiff::Zoned;
use jiff::fmt::temporal::{DateTimeParser, Pieces, PiecesOffset};
use jiff::tz::{Offset, TimeZone, TimeZoneDatabase};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// An instant and the zone it is read in. Ordered by the instant first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Time {
    /// Seconds since the Unix epoch.
    pub secs: i64,
    /// The nanoseconds past `secs`.
    pub nanos: i32,
    /// An IANA name (`Europe/Paris`, `UTC`) or a fixed offset (`+02:00`).
    pub zone: String,
}

/// The bundled zone database: never the host's (jiff's global `tz::db()`
/// may read the system's zoneinfo).
fn db() -> &'static TimeZoneDatabase {
    static DB: LazyLock<TimeZoneDatabase> = LazyLock::new(TimeZoneDatabase::bundled);
    &DB
}

/// The bundled tzdb's release (`2025b`), as `dform version` prints it.
pub fn tzdb_version() -> &'static str {
    jiff_tzdb::VERSION.unwrap_or("unknown")
}

const SHAPES: &str = "a time is RFC 3339 with an offset (`2026-10-02T09:00:00+02:00`) or with a \
                      zone (`2026-10-02T09:00[Europe/Paris]`)";

fn zone(name: &str) -> Result<TimeZone, String> {
    if let Some(off) = fixed(name) {
        return Ok(TimeZone::fixed(off));
    }
    db().get(name).map_err(|_| {
        format!(
            "`{name}` is not a time zone of the tzdb ({})",
            tzdb_version()
        )
    })
}

/// `+02:00`, `-05:30`: a fixed offset.
fn fixed(s: &str) -> Option<Offset> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':').unwrap_or((rest, "0"));
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    Offset::from_seconds(sign * (h * 3600 + m * 60)).ok()
}

fn name_of(tz: &TimeZone, z: &Zoned) -> String {
    match tz.iana_name() {
        Some(n) => n.to_string(),
        None => z.offset().to_string(),
    }
}

impl Time {
    fn of(z: &Zoned) -> Time {
        let ts = z.timestamp();
        Time {
            secs: ts.as_second(),
            nanos: ts.subsec_nanosecond(),
            zone: name_of(z.time_zone(), z),
        }
    }

    /// The time as jiff's zoned datetime.
    pub fn zoned(&self) -> Option<Zoned> {
        let ts = jiff::Timestamp::new(self.secs, self.nanos).ok()?;
        Some(ts.to_zoned(zone(&self.zone).ok()?))
    }

    /// `text`: RFC 3339 with an offset (`Z` is UTC), or a date and time
    /// with a zone annotation, with or without an offset. A zone and an
    /// offset that disagree are an error.
    pub fn parse(text: &str) -> Result<Time, String> {
        let text = text.trim();
        let pieces = Pieces::parse(text).map_err(|e| format!("`{text}`: {e}; {SHAPES}"))?;
        let z = if pieces.time_zone_annotation().is_some() {
            DateTimeParser::new()
                .parse_zoned_with(db(), text)
                .map_err(|e| format!("`{text}`: {e}"))?
        } else {
            let tz = match pieces.offset() {
                Some(PiecesOffset::Zulu) => TimeZone::UTC,
                Some(PiecesOffset::Numeric(n)) => TimeZone::fixed(n.offset()),
                _ => {
                    return Err(format!(
                        "`{text}` has neither an offset nor a zone; {SHAPES}"
                    ));
                }
            };
            let ts: jiff::Timestamp = text.parse().map_err(|e| format!("`{text}`: {e}"))?;
            ts.to_zoned(tz)
        };
        Ok(Time::of(&z))
    }

    /// The same instant read in `name`.
    pub fn in_zone(&self, name: &str) -> Option<Time> {
        let z = self.zoned()?.with_time_zone(zone(name).ok()?);
        Some(Time::of(&z))
    }

    /// `self + d`, in the time's zone: a month is a month (the 31st plus
    /// a month is the last day of the next), a day is a calendar day across
    /// a DST change.
    pub fn add(&self, d: Span) -> Option<Time> {
        let z = self.zoned()?.checked_add(d.to_jiff()?).ok()?;
        Some(Time::of(&z))
    }

    /// The exact span from `self` to `later` (negative when it is
    /// earlier).
    pub fn until(&self, later: &Time) -> Option<Span> {
        let ns = later.instant() - self.instant();
        Some(Span {
            nanos: i64::try_from(ns).ok()?,
            ..Span::default()
        })
    }

    /// Nanoseconds since the Unix epoch: what times compare by.
    pub fn instant(&self) -> i128 {
        self.secs as i128 * 1_000_000_000 + self.nanos as i128
    }

    /// The time in strftime's `layout` (`%Y-%m-%d %H:%M %Z`).
    pub fn format(&self, layout: &str) -> Option<String> {
        jiff::fmt::strtime::format(layout, &self.zoned()?).ok()
    }
}

impl std::fmt::Display for Time {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.zoned() {
            Some(z) => write!(f, "{z}"),
            None => write!(f, "{}.{:09}[{}]", self.secs, self.nanos, self.zone),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Time {
        Time::parse(s).unwrap()
    }

    #[test]
    fn a_time_carries_its_zone_and_prints_canonically() {
        assert_eq!(
            t("2026-10-02T09:00[Europe/Paris]").to_string(),
            "2026-10-02T09:00:00+02:00[Europe/Paris]"
        );
        assert_eq!(
            t("2026-10-02T09:00:00Z").to_string(),
            "2026-10-02T09:00:00+00:00[UTC]"
        );
        assert_eq!(
            t("2026-10-02T09:00:00+02:00").to_string(),
            "2026-10-02T09:00:00+02:00[+02:00]"
        );
        assert!(Time::parse("2026-13-01").is_err());
        assert!(
            Time::parse("2026-10-02T09:00")
                .unwrap_err()
                .contains("neither")
        );
        assert!(Time::parse("2026-10-02T09:00[Mars/Olympus]").is_err());
    }

    #[test]
    fn times_order_by_instant() {
        let paris = t("2026-10-02T09:00[Europe/Paris]");
        let utc = t("2026-10-02T07:00:00Z");
        assert_eq!(paris.instant(), utc.instant());
        assert!(t("2026-10-02T08:30:00Z") > paris);
    }

    #[test]
    fn a_day_across_dst_is_a_calendar_day_and_a_month_clamps() {
        // Europe/Paris leaves summer time on 2026-10-25.
        let before = t("2026-10-24T12:00[Europe/Paris]");
        let day = Span {
            days: 1,
            ..Span::default()
        };
        let after = before.add(day).unwrap();
        assert_eq!(after.to_string(), "2026-10-25T12:00:00+01:00[Europe/Paris]");
        assert_eq!(before.until(&after).unwrap().to_string(), "25h");
        let month = Span {
            months: 1,
            ..Span::default()
        };
        assert_eq!(
            t("2026-01-31T00:00[UTC]").add(month).unwrap().to_string(),
            "2026-02-28T00:00:00+00:00[UTC]"
        );
    }
}

//! `time` and `duration` (R-62): a zoned instant and a span, the Temporal
//! model through jiff, zones from the tzdb bundled into dform. The
//! compile-time errors are tests/syntax/err/durations.df's.

mod common;
use common::{Scratch, query};

fn one(s: &Scratch, goal: &str) -> String {
    let r = query(s, goal).success();
    r.stdout
        .lines()
        .nth(1)
        .unwrap_or_else(|| panic!("{goal}: {}", r.stdout))
        .to_string()
}

fn program(body: &str) -> String {
    format!("\n\n{body}\nuse fake\n")
}

/// A day added across Paris's change from summer time is a calendar day,
/// 25 hours; a month added to the 31st is the next month's last day; the
/// zone is the time's own.
#[test]
fn a_day_across_dst_and_a_month_at_the_31st() {
    let s = Scratch::new("time-calendar");
    s.write(
        "p.df",
        &program(
            "let eve = time(\"2026-10-24T12:00[Europe/Paris]\")\n\
             next(t) where t = time.add(eve, 1d)\n\
             gap(d) where d = time.until(eve, time.add(eve, 1d))\n\
             op(t) where t = eve + 1d\n\
             back(t) where t = eve - 1mo\n\
             feb(t) where t = time.add(time(\"2026-01-31T00:00[UTC]\"), 1mo)\n\
             leap(t) where t = time.add(time(\"2028-01-31T00:00[UTC]\"), 1mo)\n",
        ),
    );
    assert_eq!(
        one(&s, "next(t)"),
        "2026-10-25T12:00:00+01:00[Europe/Paris]"
    );
    assert_eq!(one(&s, "gap(d)"), "25h");
    assert_eq!(one(&s, "op(t)"), "2026-10-25T12:00:00+01:00[Europe/Paris]");
    assert_eq!(
        one(&s, "back(t)"),
        "2026-09-24T12:00:00+02:00[Europe/Paris]"
    );
    assert_eq!(one(&s, "feb(t)"), "2026-02-28T00:00:00+00:00[UTC]");
    assert_eq!(one(&s, "leap(t)"), "2028-02-29T00:00:00+00:00[UTC]");
}

/// Times compare by their instant whatever their zones; each keeps its
/// own, and `in_zone` reads the same instant in another.
#[test]
fn times_in_two_zones_compare_by_instant() {
    let s = Scratch::new("time-zones");
    s.write(
        "p.df",
        &program(
            "let paris = time(\"2026-10-02T09:00[Europe/Paris]\")\n\
             let utc = time(\"2026-10-02T08:30:00Z\")\n\
             first() where time.before(paris, utc)\n\
             later() where utc > paris\n\
             same() where time.in_zone(paris, \"UTC\") == time(\"2026-10-02T07:00:00Z\")\n\
             ny(t) where t = time.in_zone(paris, \"America/New_York\")\n\
             stamp(x) where x = time.format(paris, \"%Y-%m-%d %H:%M %Z\")\n\
             earliest(t) where t = min(x), x in [utc, paris]\n\
             offset(t) where t = time(\"2026-10-02T09:00:00+05:30\")\n",
        ),
    );
    assert!(query(&s, "first()").success().stdout.contains("yes"));
    assert!(query(&s, "later()").success().stdout.contains("yes"));
    assert!(query(&s, "same()").success().stdout.contains("yes"));
    assert_eq!(
        one(&s, "ny(t)"),
        "2026-10-02T03:00:00-04:00[America/New_York]"
    );
    assert_eq!(one(&s, "stamp(x)"), "\"2026-10-02 09:00 CEST\"");
    assert_eq!(
        one(&s, "earliest(t)"),
        "2026-10-02T09:00:00+02:00[Europe/Paris]"
    );
    assert_eq!(one(&s, "offset(t)"), "2026-10-02T09:00:00+05:30[+05:30]");
}

/// A duration is read from its friendly or ISO 8601 form and prints the
/// friendly form, integers with one unit each; it compares within exact
/// units (a day as 24 hours) and totals to a unit.
#[test]
fn durations_parse_print_and_total() {
    let s = Scratch::new("time-durations");
    s.write(
        "p.df",
        "\n\ninput ttl: duration = \"PT36H\"\n\n\
         iso(d) where d = duration.parse(\"P1Y2M3DT4H5M\")\n\
         ttl_hours(n) where n = duration.total(ttl, \"hours\")\n\
         long() where ttl > 1d\n\
         minutes(d) where d = 90m + 0s\n\
         twice(d) where d = 2 * duration(45m)\n\
         month(d) where d = duration(\"P1M\")\n\
         use fake\n",
    );
    assert_eq!(one(&s, "iso(d)"), "1y2mo3d4h5m");
    assert_eq!(one(&s, "ttl_hours(n)"), "36");
    assert!(query(&s, "long()").success().stdout.contains("yes"));
    assert_eq!(one(&s, "minutes(d)"), "1h30m");
    assert_eq!(one(&s, "twice(d)"), "1h30m");
    assert_eq!(one(&s, "month(d)"), "1mo");
}

/// A time literal is checked where it is written (R-31): a month 13 is an
/// error at the literal, in a constructor and in an input's default.
#[test]
fn a_bad_time_literal_is_a_compile_error() {
    let s = Scratch::new("time-bad");
    s.write(
        "p.df",
        &program("t(x) where x = time(\"2026-13-01T00:00:00Z\")\n"),
    );
    let r = query(&s, "t(x)").failure();
    assert!(
        r.stderr
            .contains("p.df:3:16: not a time: `2026-13-01T00:00:00Z`"),
        "{}",
        r.stderr
    );
    s.write(
        "p.df",
        "\n\ninput expires: time = \"2026-10-02\"\n\nuse fake\n",
    );
    let r = query(&s, "x(y)").failure();
    assert!(
        r.stderr.contains("input expires is time: `2026-10-02`"),
        "{}",
        r.stderr
    );
}

/// The zone database is dform's own, and `dform version` names its
/// release.
#[test]
fn dform_version_prints_the_bundled_tzdb() {
    let s = Scratch::new("time-version");
    let r = s.run(&["version"]).success();
    let tz = r
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("tzdb "))
        .unwrap_or_else(|| panic!("{}", r.stdout));
    assert!(
        tz.len() == 5 && tz[..4].chars().all(|c| c.is_ascii_digit()),
        "{tz}"
    );
}

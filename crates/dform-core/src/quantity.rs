//! Quantities (R-66): a number with a unit, held in its base unit, beside
//! `inet` (value.rs) as a value the program compares and does arithmetic
//! on. Three dimensions:
//!
//! - `bytes`, in bytes: binary units only, `Ki Mi Gi Ti Pi`, or a bare
//!   integer for bytes (`kB`, `GB`, `MiB` are errors naming the unit to
//!   write); a fraction is fine when it is whole bytes (`1.5Gi`).
//! - `cpu`, in millicores: whole cores (`2`, `0.5` in a cpu position) or
//!   millicores (`500m`).
//! - `duration` (R-62), a [`Span`]: months, days (both calendar units:
//!   their length depends on the time they are added to) and an exact
//!   part; written `1y 1mo 1w 1d 1h 1m 1s 1ms 1us 1ns`, integers with one
//!   unit each, largest first (`1h30m`), or ISO 8601 in a string (`"P1M"`).
//!
//! Every dimension prints canonically (the largest unit that divides
//! exactly: `1536Mi`, `2`, `500m`, `1h30m`), so equal values print alike.
//! `m` is millicores in a `cpu` position and minutes in a `duration` one:
//! the literal alone (`500m`) is read by its position, and is an error
//! where the position has no type.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// A quantity in its base unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Quantity {
    /// Bytes.
    Bytes(i64),
    /// Millicores.
    Cpu(i64),
    Duration(Span),
}

/// A duration: calendar months and days, and an exact part in
/// nanoseconds. The parts never have opposite signs.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Span {
    pub months: i64,
    pub days: i64,
    pub nanos: i64,
}

/// A quantity's dimension: the type it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dim {
    Bytes,
    Cpu,
    Duration,
}

impl Dim {
    pub fn name(self) -> &'static str {
        match self {
            Dim::Bytes => "bytes",
            Dim::Cpu => "cpu",
            Dim::Duration => "duration",
        }
    }

    pub fn parse(s: &str) -> Option<Dim> {
        Some(match s {
            "bytes" => Dim::Bytes,
            "cpu" => Dim::Cpu,
            "duration" => Dim::Duration,
            _ => return None,
        })
    }
}

impl Quantity {
    /// `self + b` of one dimension; none across dimensions.
    pub fn checked_add(&self, b: &Quantity) -> Option<Quantity> {
        self.sum(b, false)
    }

    /// `self - b` of one dimension; none across dimensions.
    pub fn checked_sub(&self, b: &Quantity) -> Option<Quantity> {
        self.sum(b, true)
    }

    fn sum(&self, b: &Quantity, sub: bool) -> Option<Quantity> {
        let a = self;
        let op = |x: i64, y: i64| {
            if sub {
                x.checked_sub(y)
            } else {
                x.checked_add(y)
            }
        };
        Some(match (a, b) {
            (Quantity::Bytes(x), Quantity::Bytes(y)) => Quantity::Bytes(op(*x, *y)?),
            (Quantity::Cpu(x), Quantity::Cpu(y)) => Quantity::Cpu(op(*x, *y)?),
            (Quantity::Duration(x), Quantity::Duration(y)) => Quantity::Duration(
                Span {
                    months: op(x.months, y.months)?,
                    days: op(x.days, y.days)?,
                    nanos: op(x.nanos, y.nanos)?,
                }
                .signed()?,
            ),
            _ => return None,
        })
    }

    /// `self * n`: a quantity scaled.
    pub fn checked_mul(&self, n: i64) -> Option<Quantity> {
        Some(match self {
            Quantity::Bytes(x) => Quantity::Bytes(x.checked_mul(n)?),
            Quantity::Cpu(x) => Quantity::Cpu(x.checked_mul(n)?),
            Quantity::Duration(s) => Quantity::Duration(Span {
                months: s.months.checked_mul(n)?,
                days: s.days.checked_mul(n)?,
                nanos: s.nanos.checked_mul(n)?,
            }),
        })
    }

    /// `self / n`: whole base units, as integer division; a duration's calendar
    /// parts must divide exactly.
    pub fn checked_div(&self, n: i64) -> Option<Quantity> {
        if n == 0 {
            return None;
        }
        Some(match self {
            Quantity::Bytes(x) => Quantity::Bytes(x.checked_div(n)?),
            Quantity::Cpu(x) => Quantity::Cpu(x.checked_div(n)?),
            Quantity::Duration(s) => {
                if s.months % n != 0 || s.days % n != 0 {
                    return None;
                }
                Quantity::Duration(Span {
                    months: s.months / n,
                    days: s.days / n,
                    nanos: s.nanos.checked_div(n)?,
                })
            }
        })
    }

    /// `self / b` of one dimension: a plain number, as integer division.
    pub fn ratio(&self, b: &Quantity) -> Option<i64> {
        let (x, y) = self.magnitudes(b)?;
        if y == 0 {
            return None;
        }
        i64::try_from(x / y).ok()
    }

    /// How `self` and `b`, of one dimension, order; none across dimensions,
    /// or for durations whose months make them incomparable without a date.
    pub fn compare(&self, b: &Quantity) -> Option<Ordering> {
        let (x, y) = self.magnitudes(b)?;
        Some(x.cmp(&y))
    }

    /// `self` and `b`, of one dimension, in one unit: base units; for
    /// durations nanoseconds (a day as 24 hours), or months when both are
    /// whole months.
    fn magnitudes(&self, b: &Quantity) -> Option<(i128, i128)> {
        match (self, b) {
            (Quantity::Bytes(x), Quantity::Bytes(y)) | (Quantity::Cpu(x), Quantity::Cpu(y)) => {
                Some((*x as i128, *y as i128))
            }
            (Quantity::Duration(x), Quantity::Duration(y)) => match (x.exact(), y.exact()) {
                (Some(x), Some(y)) => Some((x, y)),
                _ => {
                    let months =
                        |s: &Span| (s.days == 0 && s.nanos == 0).then_some(s.months as i128);
                    Some((months(x)?, months(y)?))
                }
            },
            _ => None,
        }
    }

    /// The quantity in `unit`, spelled as its literals spell it (`"Mi"`,
    /// `"m"`, `"h"`; `""` for bytes and cores), when it is a whole number of
    /// them (R-134: one spelling per unit, `to(q, unit)`).
    pub fn in_unit(&self, unit: &str) -> Option<i64> {
        let (n, size): (i128, i128) = match self {
            Quantity::Bytes(n) => {
                let shift = match unit {
                    "" => 0,
                    u => BINARY.iter().find(|(x, _)| *x == u)?.1,
                };
                (*n as i128, 1i128 << shift)
            }
            Quantity::Cpu(n) => (
                *n as i128,
                match unit {
                    "m" => 1,
                    "" => 1000,
                    _ => return None,
                },
            ),
            Quantity::Duration(s) => {
                if let Some(months) = match unit {
                    "mo" => Some(1),
                    "y" => Some(12),
                    _ => None,
                } {
                    if s.days != 0 || s.nanos != 0 {
                        return None;
                    }
                    (s.months as i128, months)
                } else {
                    let size = match unit {
                        "w" => 7 * DAY,
                        "d" => DAY,
                        "h" => HOUR,
                        "m" => MIN,
                        "s" => SEC,
                        "ms" => MS,
                        "us" => US,
                        "ns" => NS,
                        _ => return None,
                    };
                    (s.exact()?, size as i128)
                }
            }
        };
        (n % size == 0)
            .then(|| i64::try_from(n / size).ok())
            .flatten()
    }

    pub fn dim(&self) -> Dim {
        match self {
            Quantity::Bytes(_) => Dim::Bytes,
            Quantity::Cpu(_) => Dim::Cpu,
            Quantity::Duration(_) => Dim::Duration,
        }
    }

    /// The value in its base unit, as hover shows it: `1610612736 bytes`,
    /// `500 millicores`, `5400000000000 ns` (with the calendar parts).
    pub fn base(&self) -> String {
        match self {
            Quantity::Bytes(n) => format!("{n} bytes"),
            Quantity::Cpu(n) => format!("{n} millicores"),
            Quantity::Duration(s) => {
                let mut parts = Vec::new();
                if s.months != 0 {
                    parts.push(format!("{} months", s.months));
                }
                if s.days != 0 {
                    parts.push(format!("{} days", s.days));
                }
                if s.nanos != 0 || parts.is_empty() {
                    parts.push(format!("{} ns", s.nanos));
                }
                parts.join(" + ")
            }
        }
    }
}

const BINARY: [(&str, u32); 5] = [("Pi", 50), ("Ti", 40), ("Gi", 30), ("Mi", 20), ("Ki", 10)];

const NS: i64 = 1;
const US: i64 = 1_000;
const MS: i64 = 1_000_000;
const SEC: i64 = 1_000_000_000;
const MIN: i64 = 60 * SEC;
const HOUR: i64 = 60 * MIN;
/// A day as 24 hours, where two durations are compared (`Span::exact`).
const DAY: i64 = 24 * HOUR;

/// The exact units, largest first, as a duration prints them.
const EXACT: [(&str, i64); 6] = [
    ("h", HOUR),
    ("m", MIN),
    ("s", SEC),
    ("ms", MS),
    ("us", US),
    ("ns", NS),
];

impl std::fmt::Display for Quantity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Quantity::Bytes(n) => {
                for (unit, shift) in BINARY {
                    let d = 1i64 << shift;
                    if *n != 0 && n % d == 0 {
                        return write!(f, "{}{unit}", n / d);
                    }
                }
                write!(f, "{n}")
            }
            Quantity::Cpu(m) if m % 1000 == 0 => write!(f, "{}", m / 1000),
            Quantity::Cpu(m) => write!(f, "{m}m"),
            Quantity::Duration(s) => write!(f, "{s}"),
        }
    }
}

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let neg = self.months < 0 || self.days < 0 || self.nanos < 0;
        if neg {
            f.write_str("-")?;
        }
        let (months, days, mut nanos) = (
            self.months.unsigned_abs(),
            self.days.unsigned_abs(),
            self.nanos.unsigned_abs(),
        );
        let mut any = false;
        let mut part = |f: &mut std::fmt::Formatter<'_>, n: u64, unit: &str| {
            if n != 0 {
                any = true;
                write!(f, "{n}{unit}")
            } else {
                Ok(())
            }
        };
        part(f, months / 12, "y")?;
        part(f, months % 12, "mo")?;
        part(f, days, "d")?;
        for (unit, size) in EXACT {
            let size = size as u64;
            part(f, nanos / size, unit)?;
            nanos %= size;
        }
        if !any {
            f.write_str("0s")?;
        }
        Ok(())
    }
}

/// A number as written: its digits without the point, and how many of
/// them follow it (`1.5` is `(15, 1)`).
#[derive(Debug, Clone, Copy)]
struct Number {
    digits: i128,
    scale: u32,
}

impl Number {
    /// `self * mul`, when it is whole.
    fn times(self, mul: i128) -> Option<i128> {
        let n = self.digits.checked_mul(mul)?;
        let d = 10i128.checked_pow(self.scale)?;
        (n % d == 0).then_some(n / d)
    }
}

/// The terms of a quantity's text: `1h30m` is `[(1, "h"), (30, "m")]`.
fn terms(text: &str) -> Result<Vec<(Number, &str)>, String> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let (num, tail) = rest.split_at(end);
        if num.is_empty() {
            return Err(format!(
                "`{text}` is not a quantity: a unit follows a number"
            ));
        }
        let uend = tail
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(tail.len());
        let (unit, tail) = tail.split_at(uend);
        let (int, frac) = num.split_once('.').unwrap_or((num, ""));
        if int.is_empty() || num.matches('.').count() > 1 || (num.contains('.') && frac.is_empty())
        {
            return Err(format!(
                "`{text}` is not a quantity: `{num}` is not a number"
            ));
        }
        let digits = format!("{int}{frac}")
            .parse::<i128>()
            .map_err(|_| format!("`{text}` is out of range"))?;
        out.push((
            Number {
                digits,
                scale: frac.len() as u32,
            },
            unit,
        ));
        rest = tail;
    }
    if out.is_empty() {
        return Err("an empty quantity".to_string());
    }
    Ok(out)
}

/// What a quantity literal is before its position is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    /// One reading: `1Gi`, `30d`, `1h30m`.
    Known(Quantity),
    /// `500m` (millicores or minutes) or `0.5` (cores): read by the type
    /// the position expects.
    Ambiguous,
}

/// The duration units (the exact ones by their size; `y`, `mo`, `w`, `d`
/// calendar).
fn duration_unit(u: &str) -> bool {
    matches!(
        u,
        "y" | "mo" | "w" | "d" | "h" | "m" | "s" | "ms" | "us" | "ns"
    )
}

/// The literal token `text` (`1Gi`, `500m`, `1h30m`, `0.5`), its reading
/// where the position has no type. A unit nothing reads is an error.
pub fn literal(text: &str) -> Result<Literal, String> {
    let ts = terms(text)?;
    if let [(_, unit)] = ts.as_slice()
        && (unit.is_empty() || *unit == "m")
    {
        return Ok(Literal::Ambiguous);
    }
    let first = ts[0].1;
    let dim = if duration_unit(first) {
        Dim::Duration
    } else if BINARY.iter().any(|(u, _)| *u == first) || bytes_misspelt(first).is_some() {
        Dim::Bytes
    } else {
        return Err(unknown_unit(text, first));
    };
    read(dim, text).map(Literal::Known)
}

fn unknown_unit(text: &str, unit: &str) -> String {
    format!(
        "`{text}`: no quantity has the unit `{unit}`; bytes are `Ki Mi Gi Ti Pi`, cpu is \
         cores or `m`, a duration `y mo w d h m s ms us ns`"
    )
}

/// A decimal or misspelt binary unit, and the binary unit to write.
fn bytes_misspelt(u: &str) -> Option<&'static str> {
    Some(match u {
        "k" | "K" | "kB" | "KB" | "KiB" => "Ki",
        "M" | "MB" | "MiB" => "Mi",
        "G" | "GB" | "GiB" => "Gi",
        "T" | "TB" | "TiB" => "Ti",
        "P" | "PB" | "PiB" => "Pi",
        "B" => "",
        _ => return None,
    })
}

/// Why `500m` has no reading where nothing gives its type (R-31).
pub fn ambiguous(text: &str) -> String {
    if text.ends_with('m') {
        format!(
            "`{text}` is millicores in a cpu position and minutes in a duration position, and \
             this position has no type: give it one, `let c: cpu = {text}` or `let d: duration = \
             {text}`"
        )
    } else {
        format!(
            "`{text}` is a number with a fraction, which only a cpu position reads (cores): \
             give it one, `let c: cpu = {text}`, or write the millicores `{}`",
            Number::of(text)
                .and_then(|n| n.times(1000))
                .map_or("Nm".to_string(), |m| format!("{m}m"))
        )
    }
}

impl Number {
    fn of(text: &str) -> Option<Number> {
        match terms(text).ok()?.as_slice() {
            [(n, "")] => Some(*n),
            _ => None,
        }
    }
}

/// `text` read as a quantity of `dim`: a literal token's text, or a
/// string where the type is known (`"512Mi"`, `"P1M"`).
pub fn read(dim: Dim, text: &str) -> Result<Quantity, String> {
    let text = text.trim();
    match dim {
        Dim::Bytes => read_bytes(text),
        Dim::Cpu => read_cpu(text),
        Dim::Duration => read_duration(text).map(Quantity::Duration),
    }
}

fn read_bytes(text: &str) -> Result<Quantity, String> {
    let ts = terms(text).map_err(|e| format!("{e}; bytes are written `512Mi`"))?;
    let [(n, unit)] = ts.as_slice() else {
        return Err(format!(
            "`{text}` is not bytes: one number and one unit, `1536Mi`"
        ));
    };
    let shift = match BINARY.iter().find(|(u, _)| u == unit) {
        Some((_, s)) => *s,
        None if unit.is_empty() => 0,
        None => {
            return Err(match bytes_misspelt(unit) {
                Some("") => format!(
                    "`{text}`: bytes have no `B`; write the byte count `{}`",
                    n.times(1).map_or("N".to_string(), |b| b.to_string())
                ),
                Some(bin) if !unit.ends_with("iB") => {
                    let decimal = match bin {
                        "Ki" => 1_000i128,
                        "Mi" => 1_000_000,
                        "Gi" => 1_000_000_000,
                        "Ti" => 1_000_000_000_000,
                        _ => 1_000_000_000_000_000,
                    };
                    format!(
                        "`{text}` is a decimal unit; bytes take binary units: write `{}{bin}`, \
                         or the byte count `{}`",
                        num_text(*n),
                        n.times(decimal).map_or("N".to_string(), |b| b.to_string())
                    )
                }
                Some(bin) => format!("`{text}`: bytes have no `B`; write `{}{bin}`", num_text(*n)),
                None => format!(
                    "`{text}` is not bytes: the units are `Ki Mi Gi Ti Pi`, or none for bytes"
                ),
            });
        }
    };
    let b = n
        .times(1i128 << shift)
        .ok_or_else(|| format!("`{text}` is not a whole number of bytes"))?;
    i64::try_from(b)
        .map(Quantity::Bytes)
        .map_err(|_| format!("`{text}` is out of range"))
}

fn num_text(n: Number) -> String {
    if n.scale == 0 {
        return n.digits.to_string();
    }
    let s = format!("{:0>width$}", n.digits, width = n.scale as usize + 1);
    let (i, f) = s.split_at(s.len() - n.scale as usize);
    format!("{i}.{f}")
}

fn read_cpu(text: &str) -> Result<Quantity, String> {
    let ts = terms(text).map_err(|e| format!("{e}; a cpu is `2` or `500m`"))?;
    let [(n, unit)] = ts.as_slice() else {
        return Err(format!(
            "`{text}` is not a cpu: cores (`2`) or millicores (`500m`)"
        ));
    };
    let m = match *unit {
        "" => n.times(1000),
        "m" => n.times(1),
        _ => {
            return Err(format!(
                "`{text}` is not a cpu: cores (`2`) or millicores (`500m`)"
            ));
        }
    }
    .ok_or_else(|| format!("`{text}` is not a whole number of millicores"))?;
    i64::try_from(m)
        .map(Quantity::Cpu)
        .map_err(|_| format!("`{text}` is out of range"))
}

/// A duration: the friendly form (`1h30m`, `30d`), or ISO 8601 (`P1M`,
/// `PT6H`), which jiff reads.
fn read_duration(text: &str) -> Result<Span, String> {
    let (neg, body) = match text.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, text),
    };
    if body.starts_with(['P', 'p']) {
        let s: jiff::Span = text
            .parse()
            .map_err(|e| format!("`{text}` is not an ISO 8601 duration: {e}"))?;
        return Span::of_jiff(&s).ok_or_else(|| format!("`{text}` is out of range"));
    }
    let ts = terms(body).map_err(|e| format!("{e}; a duration is written `1h30m`"))?;
    let order = ["y", "mo", "w", "d", "h", "m", "s", "ms", "us", "ns"];
    let mut span = Span::default();
    let mut last = None;
    for (n, unit) in &ts {
        let Some(i) = order.iter().position(|u| u == unit) else {
            return Err(if unit.is_empty() {
                format!("`{text}`: a duration has a unit on every number (`30s`)")
            } else {
                format!(
                    "`{text}` is not a duration: the units are `y mo w d h m s ms us ns`, \
                     not `{unit}`"
                )
            });
        };
        if last.is_some_and(|l| l >= i) {
            return Err(format!(
                "`{text}`: a duration's units are written once each, largest first (`1h30m`)"
            ));
        }
        last = Some(i);
        if n.scale != 0 {
            let exact = EXACT
                .iter()
                .find(|(u, _)| u == unit)
                .and_then(|(_, size)| n.times(*size as i128))
                .and_then(|ns| i64::try_from(ns).ok());
            return Err(match exact {
                Some(ns) => format!(
                    "`{text}`: a duration's numbers are whole: write `{}`",
                    Span {
                        nanos: if neg { -ns } else { ns },
                        ..Span::default()
                    }
                ),
                None => format!("`{text}`: a duration's numbers are whole"),
            });
        }
        let v = i64::try_from(n.digits).map_err(|_| format!("`{text}` is out of range"))?;
        let add = |acc: i64, by: i64| {
            v.checked_mul(by)
                .and_then(|x| acc.checked_add(x))
                .ok_or_else(|| format!("`{text}` is out of range"))
        };
        match *unit {
            "y" => span.months = add(span.months, 12)?,
            "mo" => span.months = add(span.months, 1)?,
            "w" => span.days = add(span.days, 7)?,
            "d" => span.days = add(span.days, 1)?,
            u => {
                let size = EXACT
                    .iter()
                    .find(|(x, _)| *x == u)
                    .map(|(_, s)| *s)
                    .unwrap();
                span.nanos = add(span.nanos, size)?;
            }
        }
    }
    Ok(if neg { span.negate() } else { span })
}

impl Span {
    pub fn negate(self) -> Span {
        Span {
            months: -self.months,
            days: -self.days,
            nanos: -self.nanos,
        }
    }

    /// One sign for every part, or none: a span that is not one is no
    /// duration (`1mo - 1d` has no value).
    fn signed(self) -> Option<Span> {
        let pos = self.months > 0 || self.days > 0 || self.nanos > 0;
        let neg = self.months < 0 || self.days < 0 || self.nanos < 0;
        (!(pos && neg)).then_some(self)
    }

    /// The span in nanoseconds, a day taken as 24 hours; none with months,
    /// whose length depends on the date.
    pub fn exact(self) -> Option<i128> {
        (self.months == 0).then(|| self.days as i128 * DAY as i128 + self.nanos as i128)
    }

    fn of_jiff(s: &jiff::Span) -> Option<Span> {
        let months = i64::from(s.get_years()) * 12 + i64::from(s.get_months());
        let days = i64::from(s.get_weeks()) * 7 + i64::from(s.get_days());
        let nanos = i128::from(s.get_hours()) * HOUR as i128
            + i128::from(s.get_minutes()) * MIN as i128
            + i128::from(s.get_seconds()) * SEC as i128
            + i128::from(s.get_milliseconds()) * MS as i128
            + i128::from(s.get_microseconds()) * US as i128
            + i128::from(s.get_nanoseconds());
        Some(Span {
            months,
            days,
            nanos: i64::try_from(nanos).ok()?,
        })
    }

    /// The span as jiff's, its exact part in hours down to nanoseconds
    /// (`PT1H30M` in ISO 8601).
    pub fn to_jiff(self) -> Option<jiff::Span> {
        let n = self.nanos;
        jiff::Span::new()
            .try_months(self.months)
            .and_then(|s| s.try_days(self.days))
            .and_then(|s| s.try_hours(n / HOUR))
            .and_then(|s| s.try_minutes(n % HOUR / MIN))
            .and_then(|s| s.try_seconds(n % MIN / SEC))
            .and_then(|s| s.try_milliseconds(n % SEC / MS))
            .and_then(|s| s.try_microseconds(n % MS / US))
            .and_then(|s| s.try_nanoseconds(n % US))
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> String {
        read(Dim::Bytes, s)
            .map(|q| q.to_string())
            .unwrap_or_else(|e| e)
    }

    #[test]
    fn bytes_print_in_the_largest_unit_that_divides() {
        assert_eq!(b("1.5Gi"), "1536Mi");
        assert_eq!(b("2048Mi"), "2Gi");
        assert_eq!(b("512"), "512");
        assert_eq!(b("1Ki"), "1Ki");
        assert!(b("20GB").contains("write `20Gi`"), "{}", b("20GB"));
        assert!(b("20GB").contains("20000000000"), "{}", b("20GB"));
        assert!(b("512MiB").contains("write `512Mi`"), "{}", b("512MiB"));
        assert!(b("1.3Gi").contains("whole number of bytes"));
    }

    #[test]
    fn cpu_is_cores_or_millicores() {
        let c = |s: &str| {
            read(Dim::Cpu, s)
                .map(|q| q.to_string())
                .unwrap_or_else(|e| e)
        };
        assert_eq!(c("2000m"), "2");
        assert_eq!(c("0.5"), "500m");
        assert_eq!(c("2"), "2");
        assert_eq!(c("250m"), "250m");
        assert!(c("1Gi").contains("not a cpu"));
    }

    #[test]
    fn durations_are_friendly_or_iso() {
        let d = |s: &str| {
            read(Dim::Duration, s)
                .map(|q| q.to_string())
                .unwrap_or_else(|e| e)
        };
        assert_eq!(d("90m"), "1h30m");
        assert_eq!(d("1h30m"), "1h30m");
        assert_eq!(d("PT6H"), "6h");
        assert_eq!(d("P1M"), "1mo");
        assert_eq!(d("P1Y2M3DT4H"), "1y2mo3d4h");
        assert_eq!(d("2w"), "14d");
        assert_eq!(d("0s"), "0s");
        assert!(d("1.5h").contains("write `1h30m`"), "{}", d("1.5h"));
        assert!(d("30m1h").contains("largest first"));
    }

    #[test]
    fn a_literal_alone_m_or_a_fraction_waits_for_its_position() {
        assert_eq!(literal("500m"), Ok(Literal::Ambiguous));
        assert_eq!(literal("0.5"), Ok(Literal::Ambiguous));
        assert_eq!(literal("1Gi"), Ok(Literal::Known(Quantity::Bytes(1 << 30))));
        assert!(matches!(
            literal("1h30m"),
            Ok(Literal::Known(Quantity::Duration(_)))
        ));
        assert!(
            literal("3x")
                .unwrap_err()
                .contains("no quantity has the unit `x`")
        );
    }

    #[test]
    fn arithmetic_keeps_the_dimension() {
        let gi = Quantity::Bytes(1 << 30);
        let m = Quantity::Cpu(500);
        assert_eq!(gi.checked_add(&m), None);
        assert_eq!(gi.checked_mul(2), Some(Quantity::Bytes(2 << 30)));
        assert_eq!(Quantity::Cpu(1000).ratio(&Quantity::Cpu(250)), Some(4));
        assert_eq!(
            Quantity::Cpu(2000).compare(&Quantity::Cpu(2000)),
            Some(Ordering::Equal)
        );
        let mo = Quantity::Duration(Span {
            months: 1,
            ..Span::default()
        });
        let d30 = Quantity::Duration(Span {
            days: 30,
            ..Span::default()
        });
        assert_eq!(mo.compare(&d30), None);
        assert_eq!(mo.checked_sub(&d30), None);
        assert_eq!(Quantity::Bytes(1536 << 20).in_unit("Mi"), Some(1536));
        assert_eq!(Quantity::Bytes(1536 << 20).in_unit("Gi"), None);
    }
}

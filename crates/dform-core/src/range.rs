//! Ranges (R-180): `range(T)` for every ordered `T` (an int, a float, a
//! quantity, a time, an ip, a semver). A range is written `a..b` (its end
//! left out) or `a..=b` (its end in it), its ends terms of `T`; an end of
//! a type written as a string is quoted (`"10.42.0.2"..="10.42.0.254"`),
//! or the whole range is one string where a `range(T)` is wanted
//! (`"10.42.0.2..=10.42.0.254"`), parsed there. Its canonical print is
//! `a..=b` (`a..b` for a dense type's range without its end).
//!
//! Two uses (docs/grammar.md "Functions"): membership, `x in r` with `x`
//! bound, is `start <= x <= end` and needs only an order, so every range
//! has it; generation, `n in r` with `n` unbound, needs each member's
//! successor and is a discrete type's only (an int's, an ip's). A
//! discrete range is held with its end in it, so `0..3` is `0..=2`.

use crate::spell;
use crate::value::{Value, article, type_name, u32_to_ipv4};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// The types a range is of: those with an order.
pub const ORDERED: &[&str] = &[
    "int", "float", "bytes", "cpu", "duration", "time", "ip", "semver",
];

/// The internal function a range literal lowers to when its ends are
/// computed: `__range(start, end, inclusive)`.
pub const LOWERED: &str = "__range";

/// `T` of the type `range(T)`, `T` ordered.
pub fn element(ty: &str) -> Option<&str> {
    let inner = ty.strip_prefix("range(")?.strip_suffix(')')?.trim();
    ORDERED.contains(&inner).then_some(inner)
}

/// Whether a range of `ty` enumerates its members: an int's and an ip's.
pub fn discrete(ty: &str) -> bool {
    matches!(ty, "int" | "ip")
}

/// A range's ends. A discrete range's end is in it (`inclusive`); an end
/// still a string (`"a".."b"` where no type is known yet) is read as
/// the type of what it is compared with, or of its position.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Range {
    pub start: Value,
    pub end: Value,
    pub inclusive: bool,
}

impl Range {
    /// The range from `start` to `end`, or why it is none: its ends of
    /// one ordered type (an int widened to the other end's float or
    /// quantity, a string read as the other end's type). An end before
    /// its start is an empty range, as `0..0` is.
    pub fn new(start: Value, end: Value, inclusive: bool) -> Result<Range, String> {
        let (x, y) = (type_name(&start), type_name(&end));
        let one = format!(
            "its ends are {} and {}: a range's ends are of one type",
            article(x),
            article(y)
        );
        let read = |v: &Value, ty: &str| crate::value::read_typed(ty, v).map_err(|_| one.clone());
        // The end that is a string, else an int, is read as the other's.
        let (start, end) = match (&start, &end) {
            _ if x == y => (start, end),
            (Value::Str(_), _) | (Value::Int(_), Value::Float(_) | Value::Quantity(_)) => {
                (read(&start, y)?, end)
            }
            (_, Value::Str(_)) | (Value::Float(_) | Value::Quantity(_), Value::Int(_)) => {
                let end = read(&end, x)?;
                (start, end)
            }
            _ => return Err(one),
        };
        let ty = type_name(&start);
        if !matches!(start, Value::Str(_)) && !ORDERED.contains(&ty) {
            return Err(format!(
                "its ends are {}s, which have no order: a range is of {}",
                type_name(&start),
                ORDERED.join(", ")
            ));
        }
        if let (Value::Quantity(a), Value::Quantity(b)) = (&start, &end) {
            crate::quantity::compare(a, b)
                .ok_or_else(|| format!("{a} and {b} do not compare: a range's ends do"))?;
        }
        // A discrete range holds its end: `0..3` is `0..=2`.
        let (end, inclusive) = match (end, inclusive) {
            (Value::Int(n), false) if n > i64::MIN => (Value::Int(n - 1), true),
            (Value::Ip(n), false) if n > 0 => (Value::Ip(n - 1), true),
            (end, inclusive) => (end, inclusive),
        };
        Ok(Range {
            start,
            end,
            inclusive,
        })
    }

    /// `text`, `a..=b` or `a..b`, read as a range of `elem`, or why it
    /// is not one.
    pub fn parse(elem: &str, text: &str) -> Result<Range, String> {
        let (a, b, inclusive) = match (text.split_once("..="), text.split_once("..")) {
            (Some((a, b)), _) => (a, b, true),
            (None, Some((a, b))) => (a, b, false),
            _ => return Err(not_one(elem, text)),
        };
        let end = |s: &str| {
            let s = s.trim().trim_matches('"');
            crate::value::read_typed(elem, &Value::Str(s.to_string()))
        };
        let (a, b) = (
            end(a).map_err(|e| format!("{text:?}: its start {e}"))?,
            end(b).map_err(|e| format!("{text:?}: its end {e}"))?,
        );
        Range::new(a, b, inclusive).map_err(|e| format!("{text:?}: {e}"))
    }

    /// The range with its ends read as `elem` (a string end parsed), or
    /// why it is no range of `elem`.
    pub fn read(&self, elem: &str) -> Result<Range, String> {
        let end = |v: &Value| crate::value::read_typed(elem, v);
        let (a, b) = match (end(&self.start), end(&self.end)) {
            (Ok(a), Ok(b)) => (a, b),
            _ => {
                return Err(format!(
                    "{self} is a range of {}s, not of {elem}s",
                    self.element().unwrap_or("string")
                ));
            }
        };
        Range::new(a, b, self.inclusive)
    }

    /// The type of its ends; none while they are strings.
    pub fn element(&self) -> Option<&'static str> {
        match &self.start {
            Value::Str(_) => None,
            v => Some(type_name(v)),
        }
    }

    /// Whether `x` is in it (`start <= x <= end`, `< end` without its
    /// end), a string end read as `x`'s type; why not when `x` has no
    /// order with its ends.
    pub fn holds(&self, x: &Value) -> Result<bool, String> {
        let r = match (&self.start, x) {
            (Value::Str(_), Value::Str(_)) => {
                return Err(format!(
                    "{} is a string, and a string has no order: read it as the range's type \
                     first (`let x: ip = ..`)",
                    spell::value(x)
                ));
            }
            (Value::Str(_), x) => self.read(type_name(x))?,
            _ => self.clone(),
        };
        let lo = crate::engine::order(&r.start, x)?;
        let hi = crate::engine::order(x, &r.end)?;
        Ok(lo != Ordering::Greater
            && match r.inclusive {
                true => hi != Ordering::Greater,
                false => hi == Ordering::Less,
            })
    }

    /// Its members in order, a discrete range's; why not for a dense
    /// one, which has no next member (`n in 1..=500, size = n * 1Gi`).
    /// A range whose ends are still strings is one of addresses.
    pub fn members(&self) -> Result<Vec<Value>, String> {
        let r = match &self.start {
            Value::Str(_) => self.read("ip").map_err(|_| self.dense())?,
            _ => self.clone(),
        };
        match (&r.start, &r.end) {
            (Value::Int(a), Value::Int(b)) => Ok((*a..=*b).map(Value::Int).collect()),
            (Value::Ip(a), Value::Ip(b)) => Ok((*a..=*b).map(Value::Ip).collect()),
            _ => Err(self.dense()),
        }
    }

    /// Why a dense range enumerates no members.
    fn dense(&self) -> String {
        format!(
            "{self} is a range of {}s, which has no next member: a range enumerates ints \
             and ips only; {}",
            self.element().unwrap_or("string"),
            stepped(self)
        )
    }

    /// Its count of members, a discrete range's.
    pub fn count(&self) -> Option<i64> {
        let n = match (&self.start, &self.end) {
            (Value::Int(a), Value::Int(b)) => i128::from(*b) - i128::from(*a) + 1,
            (Value::Ip(a), Value::Ip(b)) => i128::from(*b) - i128::from(*a) + 1,
            _ => return None,
        };
        i64::try_from(n.max(0)).ok()
    }

    /// `text` read as a range of addresses, its first and last: what a
    /// provider given a `range(ip)` as its text reads.
    pub fn ips(text: &str) -> Option<(u32, u32)> {
        match Range::parse("ip", text).ok()? {
            Range {
                start: Value::Ip(a),
                end: Value::Ip(b),
                inclusive: true,
            } => Some((a, b)),
            _ => None,
        }
    }

    /// Its parts as an object: what `r.start` and `r.end` read.
    pub fn parts(&self) -> Value {
        Value::Obj(
            [
                ("start".to_string(), self.start.clone()),
                ("end".to_string(), self.end.clone()),
            ]
            .into(),
        )
    }
}

impl From<Range> for Value {
    fn from(r: Range) -> Value {
        Value::Range(Box::new(r))
    }
}

/// How the fix for enumerating a dense range reads: ints scaled by the
/// unit its ends share (`n in 1..=500, x = n * 1Gi`), else a bound value
/// tested (`x in r` after what binds `x`).
fn stepped(r: &Range) -> String {
    let unit = |v: &Value| -> Option<(i64, String)> {
        let text = v.typed_text()?;
        let digits = text.find(|c: char| !c.is_ascii_digit())?;
        let n = text[..digits].parse().ok()?;
        Some((n, text[digits..].to_string()))
    };
    let op = if r.inclusive { "..=" } else { ".." };
    match (unit(&r.start), unit(&r.end)) {
        (Some((a, u)), Some((b, w))) if u == w && matches!(r.start, Value::Quantity(_)) => {
            format!("enumerate ints and scale them, `n in {a}{op}{b}, x = n * 1{u}`")
        }
        _ => "bind the value first and test it, `x in r`".to_string(),
    }
}

fn not_one(elem: &str, text: &str) -> String {
    // The form an `iprange` was written in before (`a-b`), named.
    if elem == "ip"
        && let Some((a, b)) = text.split_once('-')
        && crate::value::ipv4_to_u32(a.trim()).is_some()
        && crate::value::ipv4_to_u32(b.trim()).is_some()
    {
        return format!(
            "{text:?} is not a range: a range of addresses is written `{}..={}`",
            a.trim(),
            b.trim()
        );
    }
    format!("{text:?} is not a range (`a..=b`, `a..b`)")
}

/// The canonical print: `0..=2`, `10.42.0.2..=10.42.0.254`, `1Gi..=500Gi`,
/// `1.0..2.0`; a string end quoted.
impl std::fmt::Display for Range {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let end = |v: &Value| match v {
            Value::Str(s) => format!("{s:?}"),
            Value::Ip(n) => u32_to_ipv4(*n),
            v => v.typed_text().unwrap_or_else(|| spell::value(v)),
        };
        let op = if self.inclusive { "..=" } else { ".." };
        write!(f, "{}{op}{}", end(&self.start), end(&self.end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Value {
        Value::Ip(crate::value::ipv4_to_u32(s).unwrap())
    }

    #[test]
    fn a_discrete_range_holds_its_end() {
        let r = Range::new(Value::Int(0), Value::Int(3), false).unwrap();
        assert_eq!(r, Range::new(Value::Int(0), Value::Int(2), true).unwrap());
        assert_eq!(r.to_string(), "0..=2");
        assert_eq!(r.count(), Some(3));
        let e = Range::new(Value::Int(2), Value::Int(2), false).unwrap();
        assert_eq!(e.members().unwrap(), Vec::<Value>::new());
        assert_eq!(e.count(), Some(0));
    }

    #[test]
    fn a_range_parses_and_prints_canonically() {
        let r = Range::parse("ip", "10.42.0.2..=10.42.0.254").unwrap();
        assert_eq!(r.to_string(), "10.42.0.2..=10.42.0.254");
        assert_eq!(r.count(), Some(253));
        assert!(r.holds(&ip("10.42.0.2")).unwrap());
        assert!(!r.holds(&ip("10.42.0.255")).unwrap());
        let b = Range::parse("bytes", "1Gi..=500Gi").unwrap();
        assert_eq!(b.to_string(), "1Gi..=500Gi");
        assert_eq!(b.count(), None);
        let v = Range::parse("semver", "1.2.0..2.0.0").unwrap();
        assert_eq!(v.to_string(), "1.2.0..2.0.0");
        let e = Range::parse("ip", "10.0.0.1-10.0.0.9").unwrap_err();
        assert!(e.contains("written `10.0.0.1..=10.0.0.9`"), "{e}");
    }

    #[test]
    fn a_dense_range_has_no_members() {
        let b = Range::parse("bytes", "1Gi..=500Gi").unwrap();
        let e = b.members().unwrap_err();
        assert!(e.contains("`n in 1..=500, x = n * 1Gi`"), "{e}");
    }
}

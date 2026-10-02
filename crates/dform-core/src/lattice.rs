//! Prototype of the synthesis lattice from proposals/E-synthesis.org.
//!
//! What is here, and only this:
//!   * three-valued equality `eq3` over values that may contain labeled nulls,
//!     driven by the null class (fresh / open / secret);
//!   * the four lattices of direction A (Flat, Map, Set, Keyed) with one new
//!     element, `Stuck`, for a same-rank disagreement that cannot be decided
//!     until a null resolves;
//!   * ranked cells with exactly three ranks (@default, normal, @override) on
//!     Flat only, with rank-blind schema constraints;
//!   * `collapse`, the one non-monotone read, and `resolve`, the substitution
//!     a phase boundary performs.
//!
//! Nothing here touches the evaluator. The tests are the point: they run the
//! four seam-1 cases and check order independence under every permutation.

use crate::value::{NullClass, Value};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Three-valued equality
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    fn and(self, other: Truth) -> Truth {
        use Truth::*;
        match (self, other) {
            (False, _) | (_, False) => False,
            (Unknown, _) | (_, Unknown) => Unknown,
            _ => True,
        }
    }
}

/// Equality with the null classes applied. This is the whole of the
/// "class drives equality" rule:
///   fresh  vs anything with another label: False (Unique Name Assumption)
///   open   vs anything with another label: Unknown
///   secret vs anything with another label: Unknown (and it stays unknown forever
///          to the engine, which is why the ranked cell treats it as a conflict)
///   same label: True
pub fn eq3(a: &Value, b: &Value) -> Truth {
    use Value::*;
    match (a, b) {
        (Null { label: la, .. }, Null { label: lb, .. }) if la == lb => Truth::True,
        (Null { class: ca, .. }, Null { class: cb, .. }) => {
            if *ca == NullClass::Fresh && *cb == NullClass::Fresh {
                Truth::False
            } else {
                Truth::Unknown
            }
        }
        (Null { class, .. }, _) | (_, Null { class, .. }) => {
            if *class == NullClass::Fresh {
                Truth::False
            } else {
                Truth::Unknown
            }
        }
        (List(xs), List(ys)) => {
            if xs.len() != ys.len() {
                return Truth::False;
            }
            xs.iter()
                .zip(ys)
                .fold(Truth::True, |t, (x, y)| t.and(eq3(x, y)))
        }
        (Obj(xm), Obj(ym)) => {
            if xm.keys().ne(ym.keys()) {
                return Truth::False;
            }
            xm.iter()
                .fold(Truth::True, |t, (k, x)| t.and(eq3(x, &ym[k])))
        }
        _ => {
            if a == b {
                Truth::True
            } else {
                Truth::False
            }
        }
    }
}

/// Labels of every null inside a value.
pub fn nulls_in(v: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn go(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::Null { label, .. } => {
                out.insert(label.clone());
            }
            Value::List(xs) => xs.iter().for_each(|x| go(x, out)),
            Value::Obj(m) => m.values().for_each(|x| go(x, out)),
            _ => {}
        }
    }
    go(v, &mut out);
    out
}

pub fn has_secret(v: &Value) -> bool {
    match v {
        Value::Null { class, .. } => *class == NullClass::Secret,
        Value::List(xs) => xs.iter().any(has_secret),
        Value::Obj(m) => m.values().any(has_secret),
        _ => false,
    }
}

/// Substitute one label by a constant everywhere inside a value.
pub fn subst(v: &Value, label: &str, repl: &Value) -> Value {
    match v {
        Value::Null { label: l, .. } if l == label => repl.clone(),
        Value::List(xs) => Value::List(xs.iter().map(|x| subst(x, label, repl)).collect()),
        Value::Obj(m) => Value::Obj(
            m.iter()
                .map(|(k, x)| (k.clone(), subst(x, label, repl)))
                .collect(),
        ),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Lattices
// ---------------------------------------------------------------------------

/// Which lattice assembles a path. Chosen by the schema (`type_lattice/3`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lattice {
    Flat,
    Map(Box<Lattice>),
    Set,
    Keyed {
        keys: Vec<String>,
        elem: Box<Lattice>,
    },
}

/// A witness identifies a contribution; in the engine it is a fact id.
pub type Witness = u32;
pub type Witnesses = BTreeSet<Witness>;

fn union(a: &Witnesses, b: &Witnesses) -> Witnesses {
    a.union(b).copied().collect()
}

/// An element of a lattice at one path. `Stuck` is the addition over
/// direction A: two contributions whose equality is unknown (an open null
/// against a constant, or two open nulls with different labels). It is not a
/// value and it is not a conflict; it becomes one or the other when the null
/// resolves.
///
/// Every non-Bottom element is the *normal form of its full contribution
/// list*, so join is commutative and associative by construction: joining is
/// concatenating the lists and re-normalizing. A `Conflict` therefore carries
/// every contribution (its witness set is the union) and names one canonical
/// pair for the diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Elem {
    Bottom,
    Val(Value, Witnesses),
    Stuck {
        vals: Vec<(Value, Witnesses)>,
        nulls: BTreeSet<String>,
    },
    Conflict {
        path: String,
        vals: Vec<(Value, Witnesses)>,
        a: (Value, Witnesses),
        b: (Value, Witnesses),
        reason: String,
    },
}

impl Elem {
    pub fn witnesses(&self) -> Witnesses {
        match self {
            Elem::Bottom => Witnesses::new(),
            Elem::Val(_, w) => w.clone(),
            Elem::Stuck { vals, .. } | Elem::Conflict { vals, .. } => vals
                .iter()
                .fold(Witnesses::new(), |acc, (_, w)| union(&acc, w)),
        }
    }
    fn contributions(self) -> Vec<(Value, Witnesses)> {
        match self {
            Elem::Bottom => vec![],
            Elem::Val(v, w) => vec![(v, w)],
            Elem::Stuck { vals, .. } | Elem::Conflict { vals, .. } => vals,
        }
    }
}

/// A contribution list: each value with the witnesses that contributed it.
type Contributions = Vec<(Value, Witnesses)>;

/// Merge contributions that are definitely equal; sort for canonical order.
fn dedup_equal(mut items: Vec<(Value, Witnesses)>) -> Vec<(Value, Witnesses)> {
    let mut merged: Vec<(Value, Witnesses)> = Vec::new();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    'outer: for (v, w) in items {
        for m in merged.iter_mut() {
            if eq3(&m.0, &v) == Truth::True {
                m.1 = union(&m.1, &w);
                continue 'outer;
            }
        }
        merged.push((v, w));
    }
    merged
}

/// Flat normal form of a contribution list:
///   any pair definitely unequal            -> Conflict, naming the least such pair
///   else any pair unknown                  -> Stuck over the distinct values
///   else                                   -> the one value, witnesses unioned
fn flat_normalize(path: &str, items: Vec<(Value, Witnesses)>) -> Elem {
    let mut merged = dedup_equal(items);
    if merged.is_empty() {
        return Elem::Bottom;
    }
    if merged.len() == 1 {
        let (v, w) = merged.pop().unwrap();
        return Elem::Val(v, w);
    }
    let mut unknown = false;
    for i in 0..merged.len() {
        for j in i + 1..merged.len() {
            let (a, b) = (&merged[i], &merged[j]);
            // A secret null against anything else can never be decided by the
            // engine, so it is a conflict now rather than a stuck-forever.
            let secret = has_secret(&a.0) || has_secret(&b.0);
            match eq3(&a.0, &b.0) {
                Truth::False => {
                    return Elem::Conflict {
                        path: path.to_string(),
                        a: a.clone(),
                        b: b.clone(),
                        vals: merged.clone(),
                        reason: "two contributions disagree".into(),
                    };
                }
                Truth::Unknown if secret => {
                    return Elem::Conflict {
                        path: path.to_string(),
                        a: a.clone(),
                        b: b.clone(),
                        vals: merged.clone(),
                        reason: "a secret value cannot be compared with another contribution"
                            .into(),
                    };
                }
                Truth::Unknown => unknown = true,
                Truth::True => unreachable!("merged above"),
            }
        }
    }
    debug_assert!(unknown);
    let nulls = merged.iter().flat_map(|(v, _)| nulls_in(v)).collect();
    Elem::Stuck {
        vals: merged,
        nulls,
    }
}

/// Map normal form: pointwise `elem` normal form per key over every object.
fn map_normalize(elem: &Lattice, path: &str, items: Vec<(Value, Witnesses)>) -> Elem {
    let items = dedup_equal(items);
    let mut per_key: BTreeMap<String, Vec<(Value, Witnesses)>> = BTreeMap::new();
    let mut ws = Witnesses::new();
    for (v, w) in &items {
        ws = union(&ws, w);
        let Value::Obj(m) = v else {
            return Elem::Conflict {
                path: path.into(),
                a: (v.clone(), w.clone()),
                b: (Value::Str("<not an object>".into()), Witnesses::new()),
                vals: items.clone(),
                reason: "map contribution is not an object".into(),
            };
        };
        for (k, x) in m {
            per_key
                .entry(k.clone())
                .or_default()
                .push((x.clone(), w.clone()));
        }
    }
    let mut out = BTreeMap::new();
    let mut stuck: BTreeSet<String> = BTreeSet::new();
    for (k, contribs) in per_key {
        let child = format!("{path}.{k}");
        match normalize(elem, &child, contribs) {
            Elem::Bottom => {}
            Elem::Val(v, _) => {
                out.insert(k, v);
            }
            Elem::Stuck { nulls, .. } => stuck.extend(nulls),
            Elem::Conflict {
                a, b, reason, path, ..
            } => {
                return Elem::Conflict {
                    path,
                    a,
                    b,
                    reason,
                    vals: items,
                };
            }
        }
    }
    if !stuck.is_empty() {
        return Elem::Stuck {
            vals: items,
            nulls: stuck,
        };
    }
    Elem::Val(Value::Obj(out), ws)
}

/// A whole-value null (a computed list) at a Set or Keyed path. Alone it is
/// the value, carried like any other; beside other contributions the union
/// is unknown until it resolves, so the cell is Stuck on it.
fn null_collection(items: &[(Value, Witnesses)]) -> Option<Elem> {
    if !items.iter().any(|(v, _)| matches!(v, Value::Null { .. })) {
        return None;
    }
    let mut merged = dedup_equal(items.to_vec());
    if merged.len() == 1 {
        let (v, w) = merged.pop().unwrap();
        return Some(Elem::Val(v, w));
    }
    let nulls = merged.iter().flat_map(|(v, _)| nulls_in(v)).collect();
    Some(Elem::Stuck {
        vals: merged,
        nulls,
    })
}

/// Keyed-list normal form: group elements by their merge-key projection
/// (equality of keys is `eq3`; an unknown key comparison keeps the groups
/// apart until resolution), then `elem` normal form per group.
fn keyed_normalize(
    keys: &[String],
    elem: &Lattice,
    path: &str,
    items: Vec<(Value, Witnesses)>,
) -> Elem {
    if let Some(e) = null_collection(&items) {
        return e;
    }
    let items = dedup_equal(items);
    let mut ws = Witnesses::new();
    let mut groups: Vec<(Vec<Value>, Contributions)> = Vec::new();
    for (v, w) in &items {
        ws = union(&ws, w);
        let Value::List(xs) = v else {
            return Elem::Conflict {
                path: path.into(),
                a: (v.clone(), w.clone()),
                b: (Value::Str("<not a list>".into()), Witnesses::new()),
                vals: items.clone(),
                reason: "keyed contribution is not a list".into(),
            };
        };
        for x in xs {
            let Value::Obj(m) = x else {
                return Elem::Conflict {
                    path: path.into(),
                    a: (x.clone(), w.clone()),
                    b: (Value::Str("<not an object>".into()), Witnesses::new()),
                    vals: items.clone(),
                    reason: "keyed list element is not an object".into(),
                };
            };
            let Some(k): Option<Vec<Value>> = keys.iter().map(|k| m.get(k).cloned()).collect()
            else {
                return Elem::Conflict {
                    path: path.into(),
                    a: (x.clone(), w.clone()),
                    b: (
                        Value::Str("<element lacks merge key>".into()),
                        Witnesses::new(),
                    ),
                    vals: items.clone(),
                    reason: "keyed list element lacks its merge key".into(),
                };
            };
            match groups
                .iter()
                .position(|(gk, _)| gk.iter().zip(&k).all(|(p, q)| eq3(p, q) == Truth::True))
            {
                Some(i) => groups[i].1.push((x.clone(), w.clone())),
                None => groups.push((k, vec![(x.clone(), w.clone())])),
            }
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    let mut stuck: BTreeSet<String> = BTreeSet::new();
    for (k, contribs) in groups {
        let child = format!("{path}[{k:?}]");
        match normalize(elem, &child, contribs) {
            Elem::Bottom => {}
            Elem::Val(v, _) => out.push(v),
            Elem::Stuck { nulls, .. } => stuck.extend(nulls),
            Elem::Conflict {
                a, b, reason, path, ..
            } => {
                return Elem::Conflict {
                    path,
                    a,
                    b,
                    reason,
                    vals: items,
                };
            }
        }
    }
    if !stuck.is_empty() {
        return Elem::Stuck {
            vals: items,
            nulls: stuck,
        };
    }
    Elem::Val(Value::List(out), ws)
}

/// Set normal form: union modulo definite equality. Never conflicts, never
/// stuck: two elements that *might* be equal are both kept until a null
/// resolves, and the provider receives both.
fn set_normalize(path: &str, items: Vec<(Value, Witnesses)>) -> Elem {
    if let Some(e) = null_collection(&items) {
        return e;
    }
    let mut elems: Vec<Value> = Vec::new();
    let mut ws = Witnesses::new();
    for (v, w) in &items {
        ws = union(&ws, w);
        let Value::List(xs) = v else {
            return Elem::Conflict {
                path: path.into(),
                a: (v.clone(), w.clone()),
                b: (Value::Str("<not a list>".into()), Witnesses::new()),
                vals: items.clone(),
                reason: "set contribution is not a list".into(),
            };
        };
        for e in xs {
            if !elems.iter().any(|m| eq3(m, e) == Truth::True) {
                elems.push(e.clone());
            }
        }
    }
    if items.is_empty() {
        return Elem::Bottom;
    }
    elems.sort();
    Elem::Val(Value::List(elems), ws)
}

fn normalize(lat: &Lattice, path: &str, items: Vec<(Value, Witnesses)>) -> Elem {
    match lat {
        Lattice::Flat => flat_normalize(path, items),
        Lattice::Map(elem) => map_normalize(elem, path, items),
        Lattice::Set => set_normalize(path, items),
        Lattice::Keyed { keys, elem } => keyed_normalize(keys, elem, path, items),
    }
}

/// Join two elements under `lat` at `path`: concatenate and re-normalize.
/// There is no fast path around the normal form: `⊥ ⊔ e = normalize(e)`
/// (F8), so a lone contribution is normalized exactly like two.
pub fn join(lat: &Lattice, path: &str, x: Elem, y: Elem) -> Elem {
    match (x, y) {
        (Elem::Bottom, Elem::Bottom) => Elem::Bottom,
        (Elem::Bottom, e) | (e, Elem::Bottom) => normalize(lat, path, e.contributions()),
        (x, y) => {
            let mut items = x.contributions();
            items.extend(y.contributions());
            normalize(lat, path, items)
        }
    }
}

/// Least upper bound of a set of contributions.
pub fn lub(
    lat: &Lattice,
    path: &str,
    contribs: impl IntoIterator<Item = (Witness, Value)>,
) -> Elem {
    contribs.into_iter().fold(Elem::Bottom, |acc, (w, v)| {
        join(lat, path, acc, Elem::Val(v, Witnesses::from([w])))
    })
}

// ---------------------------------------------------------------------------
// Ranked cells
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    Default = 0,
    Normal = 1,
    Override = 2,
}

/// The refinements the lattice can check at join time: the checkable
/// table of E DR-13, spelled as the `type_refine(T, Path, C)` term language
/// (`crate::refine`). Anything not in this enum lowers to a `deny` rule
/// instead. One home per refinement.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Constraint {
    /// Type tags, from a `type` block's declared attribute type.
    IsInt,
    IsStr,
    IsBool,
    IsInet,
    /// `range(Lo, Hi)`: an int in `[Lo, Hi]`.
    Range(i64, i64),
    /// `prefix_len_le(N)` / `prefix_len_ge(N)`: a CIDR's prefix length.
    PrefixLenLe(u8),
    PrefixLenGe(u8),
    /// `len_le(N)` / `len_ge(N)`: a string's characters, a list's or an
    /// object's entries.
    LenLe(i64),
    LenGe(i64),
    /// `regex(S)`: a string the pattern matches whole.
    Regex(String),
    /// `enum([..])`: one of the values.
    OneOf(BTreeSet<Value>),
}

impl Constraint {
    /// Three-valued: `Unknown` when the value carries a null (or a ref,
    /// which only the provider layer resolves), else whether it holds.
    pub fn check(&self, v: &Value) -> Truth {
        if !nulls_in(v).is_empty() || has_ref(v) {
            return Truth::Unknown;
        }
        let prefix = |v: &Value| match v {
            Value::IpNet { prefix, .. } => Some(*prefix),
            Value::Str(s) => crate::value::parse_ipnet(s).map(|(_, p)| p),
            _ => None,
        };
        let len = |v: &Value| match v {
            Value::Str(s) => Some(s.chars().count() as i64),
            Value::List(xs) => Some(xs.len() as i64),
            Value::Obj(m) => Some(m.len() as i64),
            _ => None,
        };
        let ok = match (self, v) {
            (Constraint::IsInt, Value::Int(_)) => true,
            (Constraint::IsStr, Value::Str(_)) => true,
            (Constraint::IsBool, Value::Bool(_)) => true,
            (Constraint::IsInet, v) => prefix(v).is_some(),
            (Constraint::Range(lo, hi), Value::Int(i)) => lo <= i && i <= hi,
            (Constraint::PrefixLenLe(n), v) => prefix(v).is_some_and(|p| p <= *n),
            (Constraint::PrefixLenGe(n), v) => prefix(v).is_some_and(|p| p >= *n),
            (Constraint::LenLe(n), v) => len(v).is_some_and(|l| l <= *n),
            (Constraint::LenGe(n), v) => len(v).is_some_and(|l| l >= *n),
            (Constraint::Regex(re), Value::Str(s)) => crate::refine::regex_matches(re, s),
            (Constraint::OneOf(s), v) => s.contains(v),
            _ => false,
        };
        if ok { Truth::True } else { Truth::False }
    }
}

/// Whether a value holds a ref the engine cannot see through.
fn has_ref(v: &Value) -> bool {
    match v {
        Value::Ref { .. } | Value::CloudRef { .. } => true,
        Value::List(xs) => xs.iter().any(has_ref),
        Value::Obj(m) => m.values().any(has_ref),
        _ => false,
    }
}

/// A ranked cell: one element per rank (a "shelf") plus rank-blind
/// constraints. Join is pointwise on the shelves under the path's lattice and
/// union on constraints, so it is commutative, associative and idempotent
/// whenever that lattice's join is. For Flat, `join`; for Set and Keyed,
/// `join_in`: collapse then takes the highest non-empty shelf whole, so an
/// `@override` set replaces the set and a `@default` set is discarded, not
/// unioned, when a higher shelf is non-empty (DESIGN.org, reopening E DR-9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    pub ranks: [Elem; 3],
    pub constraints: BTreeMap<Constraint, Witnesses>,
}

impl Default for Ranked {
    fn default() -> Self {
        Ranked {
            ranks: [Elem::Bottom, Elem::Bottom, Elem::Bottom],
            constraints: BTreeMap::new(),
        }
    }
}

impl Ranked {
    pub fn at(rank: Rank, w: Witness, v: Value) -> Ranked {
        let mut r = Ranked::default();
        r.ranks[rank as usize] = Elem::Val(v, Witnesses::from([w]));
        r
    }
    pub fn constraint(c: Constraint, w: Witness) -> Ranked {
        let mut r = Ranked::default();
        r.constraints.insert(c, Witnesses::from([w]));
        r
    }
    pub fn join(&self, other: &Ranked, path: &str) -> Ranked {
        self.join_in(&Lattice::Flat, other, path)
    }
    pub fn join_in(&self, lat: &Lattice, other: &Ranked, path: &str) -> Ranked {
        let mut out = self.clone();
        for i in 0..3 {
            out.ranks[i] = join(lat, path, self.ranks[i].clone(), other.ranks[i].clone());
        }
        for (c, w) in &other.constraints {
            let e = out.constraints.entry(c.clone()).or_default();
            *e = union(e, w);
        }
        out
    }
    /// Every witness in the cell, for `why`.
    pub fn all_witnesses(&self) -> Witnesses {
        let mut w = self
            .ranks
            .iter()
            .fold(Witnesses::new(), |acc, e| union(&acc, &e.witnesses()));
        for cw in self.constraints.values() {
            w = union(&w, cw);
        }
        w
    }

    /// A phase boundary: substitute a resolved null and re-normalize every
    /// rank. This is the only operation that can turn Stuck into Val or
    /// Conflict, and a deferred constraint into a Conflict.
    pub fn resolve(&self, label: &str, c: &Value) -> Ranked {
        let mut out = self.clone();
        for i in 0..3 {
            out.ranks[i] = match &self.ranks[i] {
                Elem::Bottom => Elem::Bottom,
                Elem::Val(v, w) => Elem::Val(subst(v, label, c), w.clone()),
                Elem::Stuck { vals, .. } => flat_normalize(
                    ".",
                    vals.iter()
                        .map(|(v, w)| (subst(v, label, c), w.clone()))
                        .collect(),
                ),
                e @ Elem::Conflict { .. } => e.clone(),
            };
        }
        out
    }
}

impl Rank {
    /// The rank's name in the core form `arg(T, A, P, V, Rank)`.
    pub fn name(self) -> &'static str {
        match self {
            Rank::Default => "default",
            Rank::Normal => "normal",
            Rank::Override => "override",
        }
    }
    pub fn parse(s: &str) -> Option<Rank> {
        [Rank::Default, Rank::Normal, Rank::Override]
            .into_iter()
            .find(|r| r.name() == s)
    }
}

fn rank_of(i: usize) -> Rank {
    match i {
        0 => Rank::Default,
        1 => Rank::Normal,
        _ => Rank::Override,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::Str(x.into())
    }
    fn i(n: i64) -> Value {
        Value::Int(n)
    }
    fn net(a: &str, p: u8) -> Value {
        Value::IpNet {
            addr: crate::value::ipv4_to_u32(a).unwrap(),
            prefix: p,
        }
    }
    fn obj(kv: &[(&str, Value)]) -> Value {
        Value::Obj(kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }
    fn fresh(l: &str) -> Value {
        Value::Null {
            label: l.into(),
            class: NullClass::Fresh,
            ty: "string".into(),
        }
    }
    fn open(l: &str) -> Value {
        Value::Null {
            label: l.into(),
            class: NullClass::Open,
            ty: "string".into(),
        }
    }
    fn secret(l: &str) -> Value {
        Value::Null {
            label: l.into(),
            class: NullClass::Secret,
            ty: "string".into(),
        }
    }

    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        fn go<T: Clone>(k: usize, a: &mut Vec<T>, out: &mut Vec<Vec<T>>) {
            if k <= 1 {
                out.push(a.clone());
                return;
            }
            go(k - 1, a, out);
            for i in 0..k - 1 {
                if k.is_multiple_of(2) {
                    a.swap(i, k - 1)
                } else {
                    a.swap(0, k - 1)
                }
                go(k - 1, a, out);
            }
        }
        let mut a = items.to_vec();
        let mut out = Vec::new();
        go(a.len(), &mut a, &mut out);
        out
    }

    /// `ranked_all_orders` for any lattice, with F's shadow-aware collapse.
    fn ranked_all_orders_in(lat: &Lattice, parts: &[Ranked]) -> (Ranked, Collapsed) {
        let fold = |ps: &[Ranked]| {
            ps.iter()
                .fold(Ranked::default(), |acc, p| acc.join_in(lat, p, ".x"))
        };
        let base = fold(parts);
        let basec = base.collapse();
        for p in permutations(parts) {
            let r = fold(&p);
            assert_eq!(r, base, "ranked join is order dependent");
            assert_eq!(r.collapse(), basec, "collapse is order dependent");
        }
        let doubled: Vec<Ranked> = parts.iter().chain(parts.iter()).cloned().collect();
        assert_eq!(fold(&doubled), base, "ranked join is not idempotent");
        (base, basec)
    }

    fn list(xs: &[Value]) -> Value {
        Value::List(xs.to_vec())
    }

    // ---- ranked Set and Keyed: three shelves ----------------------------------

    #[test]
    fn ranked_set_collapses_to_the_highest_nonempty_shelf() {
        // Same shelf: union. A @default set under a normal one is discarded,
        // not unioned.
        let (_, c) = ranked_all_orders_in(
            &Lattice::Set,
            &[
                Ranked::at(Rank::Default, 1, list(&[s("a"), s("b")])),
                Ranked::at(Rank::Normal, 2, list(&[s("d")])),
                Ranked::at(Rank::Normal, 3, list(&[s("c"), s("d")])),
            ],
        );
        let Collapsed::Val {
            value,
            rank: Rank::Normal,
            witnesses,
            shadowed,
            ..
        } = c
        else {
            panic!("{c:?}")
        };
        assert_eq!(value, list(&[s("c"), s("d")]));
        assert_eq!(witnesses, Witnesses::from([2, 3]));
        assert!(shadowed.is_empty(), "a set shelf never disagrees");

        // @override replaces the whole set.
        let (_, c) = ranked_all_orders_in(
            &Lattice::Set,
            &[
                Ranked::at(Rank::Default, 1, list(&[s("a")])),
                Ranked::at(Rank::Normal, 2, list(&[s("b"), s("c")])),
                Ranked::at(Rank::Override, 3, list(&[s("z")])),
            ],
        );
        assert!(
            matches!(&c, Collapsed::Val { value, rank: Rank::Override, .. } if *value == list(&[s("z")]))
        );

        // Only defaults: they are the value, unioned and normalized.
        let (_, c) = ranked_all_orders_in(
            &Lattice::Set,
            &[
                Ranked::at(Rank::Default, 1, list(&[s("b"), s("a"), s("b")])),
                Ranked::at(Rank::Default, 2, list(&[s("c")])),
            ],
        );
        assert!(
            matches!(&c, Collapsed::Val { value, rank: Rank::Default, .. } if *value == list(&[s("a"), s("b"), s("c")]))
        );

        // An empty set at a higher shelf is non-empty as a contribution: it
        // replaces the defaults with nothing.
        let (_, c) = ranked_all_orders_in(
            &Lattice::Set,
            &[
                Ranked::at(Rank::Default, 1, list(&[s("a")])),
                Ranked::at(Rank::Normal, 2, list(&[])),
            ],
        );
        assert!(
            matches!(&c, Collapsed::Val { value, rank: Rank::Normal, .. } if *value == list(&[]))
        );
    }

    #[test]
    fn ranked_keyed_collapses_to_the_highest_nonempty_shelf() {
        let lat = Lattice::Keyed {
            keys: vec!["port".into()],
            elem: Box::new(Lattice::Map(Box::new(Lattice::Flat))),
        };
        let row = |p: i64, proto: &str| obj(&[("port", i(p)), ("proto", s(proto))]);
        let (_, c) = ranked_all_orders_in(
            &lat,
            &[
                Ranked::at(Rank::Default, 1, list(&[row(22, "tcp")])),
                Ranked::at(Rank::Normal, 2, list(&[row(443, "tcp")])),
                Ranked::at(Rank::Normal, 3, list(&[row(80, "tcp"), row(443, "tcp")])),
            ],
        );
        let Collapsed::Val {
            value,
            rank: Rank::Normal,
            ..
        } = c
        else {
            panic!("{c:?}")
        };
        assert_eq!(value, list(&[row(80, "tcp"), row(443, "tcp")]));

        // A same-key disagreement on the winning shelf is a conflict; on a
        // losing shelf it is shadowed.
        let (_, c) = ranked_all_orders_in(
            &lat,
            &[
                Ranked::at(Rank::Normal, 1, list(&[row(22, "tcp")])),
                Ranked::at(Rank::Normal, 2, list(&[row(22, "udp")])),
            ],
        );
        assert!(
            matches!(
                c,
                Collapsed::Conflict {
                    rank: Some(Rank::Normal),
                    ..
                }
            ),
            "{c:?}"
        );
        let (_, c) = ranked_all_orders_in(
            &lat,
            &[
                Ranked::at(Rank::Default, 1, list(&[row(22, "tcp")])),
                Ranked::at(Rank::Default, 2, list(&[row(22, "udp")])),
                Ranked::at(Rank::Override, 3, list(&[row(80, "tcp")])),
            ],
        );
        let Collapsed::Val {
            value, shadowed, ..
        } = c
        else {
            panic!("{c:?}")
        };
        assert_eq!(value, list(&[row(80, "tcp")]));
        assert!(matches!(
            &shadowed[..],
            [Shadowed::Conflict {
                rank: Rank::Default,
                ..
            }]
        ));
    }

    /// A whole-value null at a Set path (a computed list): carried alone,
    /// stuck beside another contribution on the same shelf, discarded under
    /// a higher one.
    #[test]
    fn ranked_set_carries_a_null_contribution() {
        let (_, c) = ranked_all_orders_in(
            &Lattice::Set,
            &[Ranked::at(Rank::Normal, 1, fresh("sg/a#ids"))],
        );
        assert!(matches!(&c, Collapsed::Val { value, .. } if *value == fresh("sg/a#ids")));
        let (_, c) = ranked_all_orders_in(
            &Lattice::Set,
            &[
                Ranked::at(Rank::Normal, 1, fresh("sg/a#ids")),
                Ranked::at(Rank::Normal, 2, list(&[s("x")])),
            ],
        );
        assert!(
            matches!(&c, Collapsed::Stuck { nulls, .. } if nulls.contains("sg/a#ids")),
            "{c:?}"
        );
        let (_, c) = ranked_all_orders_in(
            &Lattice::Set,
            &[
                Ranked::at(Rank::Default, 1, fresh("sg/a#ids")),
                Ranked::at(Rank::Normal, 2, list(&[s("x")])),
            ],
        );
        assert!(matches!(&c, Collapsed::Val { value, .. } if *value == list(&[s("x")])));
    }

    /// F8 and the laws for Set, now that a lone contribution is normalized.
    #[test]
    fn set_join_is_commutative_associative_idempotent() {
        let sample = [
            Elem::Bottom,
            Elem::Val(list(&[s("b"), s("a"), s("b")]), Witnesses::from([1])),
            Elem::Val(list(&[s("c")]), Witnesses::from([2])),
            Elem::Val(list(&[fresh("f1"), s("a")]), Witnesses::from([3])),
            Elem::Val(list(&[open("o1")]), Witnesses::from([4])),
            Elem::Val(list(&[]), Witnesses::from([5])),
        ];
        let j = |x: &Elem, y: &Elem| join(&Lattice::Set, ".x", x.clone(), y.clone());
        for a in &sample {
            assert_eq!(j(&j(a, a), a), j(a, a), "idempotent {a:?}");
            assert_eq!(
                j(a, &Elem::Bottom),
                j(&j(a, &Elem::Bottom), &Elem::Bottom),
                "lone contribution normalized {a:?}"
            );
            for b in &sample {
                assert_eq!(j(a, b), j(b, a), "commutative {a:?} {b:?}");
                for c in &sample {
                    assert_eq!(
                        j(&j(a, b), c),
                        j(a, &j(b, c)),
                        "associative {a:?} {b:?} {c:?}"
                    );
                }
            }
        }
    }

    /// Fold a list of ranked contributions in every order; assert every order
    /// gives the same cell and the same collapse; assert duplication is a no-op.
    fn ranked_all_orders(parts: &[Ranked]) -> (Ranked, Collapsed) {
        let fold = |ps: &[Ranked]| {
            ps.iter()
                .fold(Ranked::default(), |acc, p| acc.join(p, ".x"))
        };
        let base = fold(parts);
        let basec = base.collapse();
        for p in permutations(parts) {
            let r = fold(&p);
            assert_eq!(r, base, "ranked join is order dependent");
            assert_eq!(r.collapse(), basec, "collapse is order dependent");
        }
        let doubled: Vec<Ranked> = parts.iter().chain(parts.iter()).cloned().collect();
        assert_eq!(fold(&doubled), base, "ranked join is not idempotent");
        (base, basec)
    }

    fn flat_all_orders(contribs: &[(Witness, Value)]) -> Elem {
        let base = lub(&Lattice::Flat, ".x", contribs.iter().cloned());
        for p in permutations(contribs) {
            assert_eq!(
                lub(&Lattice::Flat, ".x", p.iter().cloned()),
                base,
                "flat lub order dependent: {p:?}"
            );
        }
        let doubled: Vec<_> = contribs.iter().chain(contribs.iter()).cloned().collect();
        assert_eq!(
            lub(&Lattice::Flat, ".x", doubled),
            base,
            "flat lub not idempotent"
        );
        base
    }

    // ---- eq3 ---------------------------------------------------------------

    #[test]
    fn eq3_follows_the_null_class() {
        assert_eq!(eq3(&fresh("a"), &s("x")), Truth::False);
        assert_eq!(eq3(&fresh("a"), &fresh("b")), Truth::False);
        assert_eq!(eq3(&fresh("a"), &fresh("a")), Truth::True);
        assert_eq!(eq3(&open("a"), &s("x")), Truth::Unknown);
        assert_eq!(eq3(&open("a"), &open("b")), Truth::Unknown);
        assert_eq!(eq3(&open("a"), &fresh("b")), Truth::Unknown);
        assert_eq!(eq3(&secret("a"), &s("x")), Truth::Unknown);
        assert_eq!(
            eq3(
                &Value::List(vec![fresh("a"), s("x")]),
                &Value::List(vec![fresh("a"), s("x")])
            ),
            Truth::True
        );
        assert_eq!(
            eq3(&Value::List(vec![open("a")]), &Value::List(vec![s("x")])),
            Truth::Unknown
        );
        assert_eq!(
            eq3(
                &Value::List(vec![open("a"), s("y")]),
                &Value::List(vec![s("x"), s("z")])
            ),
            Truth::False
        );
    }

    // ---- seam 1, case 1: fresh null vs concrete at different ranks --------

    #[test]
    fn case1_fresh_null_is_rank_aware_not_rank_blind() {
        // normal: ref(net.vpc, vpc, .id) ; @override: "vpc-existing"  -> override wins
        let (_, c) = ranked_all_orders(&[
            Ranked::at(Rank::Normal, 1, fresh("net.vpc/vpc#id")),
            Ranked::at(Rank::Override, 2, s("vpc-existing")),
        ]);
        assert!(
            matches!(&c, Collapsed::Val { value, rank: Rank::Override, .. } if *value == s("vpc-existing"))
        );

        // @override: the fresh null ; normal: "vpc-old" -> the null wins. A fresh
        // null is a definite identity, so it competes by rank like any value.
        let (_, c) = ranked_all_orders(&[
            Ranked::at(Rank::Override, 1, fresh("net.vpc/vpc#id")),
            Ranked::at(Rank::Normal, 2, s("vpc-old")),
        ]);
        assert!(
            matches!(&c, Collapsed::Val { value, rank: Rank::Override, .. } if *value == fresh("net.vpc/vpc#id"))
        );

        // Same rank, fresh vs concrete: UNA says they differ -> conflict now,
        // not stuck. Same for two fresh nulls with different labels.
        let e = flat_all_orders(&[(1, fresh("a#id")), (2, s("vpc-1"))]);
        assert!(matches!(e, Elem::Conflict { .. }));
        let e = flat_all_orders(&[(1, fresh("a#id")), (2, fresh("b#id"))]);
        assert!(matches!(e, Elem::Conflict { .. }));
        // Same label: agree, witnesses union.
        let e = flat_all_orders(&[(1, fresh("a#id")), (2, fresh("a#id"))]);
        assert_eq!(e, Elem::Val(fresh("a#id"), Witnesses::from([1, 2])));
    }

    // ---- seam 1, case 2: open null vs concrete at the same rank -----------

    #[test]
    fn case2_open_null_vs_concrete_same_rank_is_stuck_until_resolution() {
        let parts = [
            Ranked::at(Rank::Normal, 1, open("gke/pngu#endpoint")),
            Ranked::at(Rank::Normal, 2, s("1.2.3.4")),
        ];
        let (cell, c) = ranked_all_orders(&parts);
        assert!(
            matches!(&c, Collapsed::Stuck { rank: Rank::Normal, nulls, .. } if nulls.contains("gke/pngu#endpoint"))
        );

        // Resolution decides: equal -> the value with both witnesses; unequal -> conflict naming both.
        let same = cell.resolve("gke/pngu#endpoint", &s("1.2.3.4")).collapse();
        assert!(
            matches!(&same, Collapsed::Val { value, witnesses, .. } if *value == s("1.2.3.4") && *witnesses == Witnesses::from([1, 2]))
        );
        let diff = cell.resolve("gke/pngu#endpoint", &s("9.9.9.9")).collapse();
        let Collapsed::Conflict { a, b, .. } = diff else {
            panic!("expected conflict, got {diff:?}")
        };
        assert_eq!(union(&a.1, &b.1), Witnesses::from([1, 2]));

        // Two open nulls with different labels at one rank: stuck on both.
        let e = flat_all_orders(&[(1, open("x#ep")), (2, open("y#ep"))]);
        let Elem::Stuck { nulls, .. } = e else {
            panic!()
        };
        assert_eq!(
            nulls,
            BTreeSet::from(["x#ep".to_string(), "y#ep".to_string()])
        );

        // Three-way: open vs concrete vs a *different* concrete is a conflict
        // regardless of the null (the two concretes already disagree).
        let e = flat_all_orders(&[(1, open("x#ep")), (2, s("a")), (3, s("b"))]);
        assert!(matches!(e, Elem::Conflict { .. }));
    }

    #[test]
    fn case2b_open_null_at_a_losing_rank_does_not_block_collapse() {
        // The coordinator's rule ("undefined while any rank holds an open null")
        // is replaced: rank decides first. normal: open null ; @override: concrete.
        let (_, c) = ranked_all_orders(&[
            Ranked::at(Rank::Normal, 1, open("gke/pngu#endpoint")),
            Ranked::at(Rank::Override, 2, s("1.2.3.4")),
        ]);
        assert!(
            matches!(&c, Collapsed::Val { value, rank: Rank::Override, deferred, .. } if *value == s("1.2.3.4") && deferred.is_empty())
        );

        // And the reverse: the open null wins by rank; the collapsed value *is*
        // the null (forwardable); only content readers are stuck on it.
        let (_, c) = ranked_all_orders(&[
            Ranked::at(Rank::Override, 1, open("gke/pngu#endpoint")),
            Ranked::at(Rank::Normal, 2, s("1.2.3.4")),
        ]);
        assert!(
            matches!(&c, Collapsed::Val { value, rank: Rank::Override, .. } if *value == open("gke/pngu#endpoint"))
        );
    }

    // ---- seam 1, case 3: secret null anywhere ------------------------------

    #[test]
    fn case3_secret_null() {
        // Secret at the winning rank: collapses to the secret; constraints are
        // deferred (to the provider), never checked by the engine.
        let (_, c) = ranked_all_orders(&[
            Ranked::at(Rank::Normal, 1, secret("sm/db_pw#secret_data")),
            Ranked::constraint(Constraint::IsStr, 9),
        ]);
        assert!(
            matches!(&c, Collapsed::Val { value, deferred, .. } if has_secret(value) && deferred.iter().map(|d| &d.constraint).eq([&Constraint::IsStr]))
        );

        // Secret vs concrete at the same rank: the engine can never decide
        // equality, so this is a conflict now, not a stuck-forever.
        let e = flat_all_orders(&[(1, secret("sm/db_pw#secret_data")), (2, s("hunter2"))]);
        let Elem::Conflict { reason, .. } = e else {
            panic!()
        };
        assert!(reason.contains("secret"));

        // Secret at a losing rank: the concrete wins and the collapsed witnesses
        // do not include the secret's contributor (no taint from a loser).
        let (cell, c) = ranked_all_orders(&[
            Ranked::at(Rank::Default, 1, secret("sm/db_pw#secret_data")),
            Ranked::at(Rank::Normal, 2, s("literal")),
        ]);
        let Collapsed::Val { witnesses, .. } = &c else {
            panic!()
        };
        assert_eq!(*witnesses, Witnesses::from([2]));
        assert_eq!(cell.all_witnesses(), Witnesses::from([1, 2])); // but `why` still sees it
    }

    // ---- seam 1, case 4: refinement on a null-carrying cell ----------------

    #[test]
    fn case4_refinement_deferred_then_violated_or_satisfied() {
        let parts = [
            Ranked::constraint(Constraint::PrefixLenGe(28), 100), // schema, rank-blind
            Ranked::at(Rank::Normal, 1, open("alloc/cp#cidr")),
        ];
        let (cell, c) = ranked_all_orders(&parts);
        // Plan time: the value is the null; the check is deferred, not violated.
        assert!(
            matches!(&c, Collapsed::Val { value, deferred, .. } if *value == open("alloc/cp#cidr") && deferred.iter().map(|d| &d.constraint).eq([&Constraint::PrefixLenGe(28)]))
        );

        // Phase boundary, violating value: a violation naming the contributor
        // (1) and the schema (100).
        let bad = cell
            .resolve("alloc/cp#cidr", &net("10.0.0.0", 24))
            .collapse();
        let Collapsed::Violated {
            witnesses,
            refinement,
            constraint,
            ..
        } = bad
        else {
            panic!("expected a violation")
        };
        assert_eq!(witnesses, Witnesses::from([1]));
        assert_eq!(refinement, Witnesses::from([100]));
        assert_eq!(constraint, Constraint::PrefixLenGe(28));

        // Phase boundary, satisfying value: a plain value with nothing deferred.
        let ok = cell
            .resolve("alloc/cp#cidr", &net("172.16.3.96", 28))
            .collapse();
        assert!(matches!(&ok, Collapsed::Val { deferred, .. } if deferred.is_empty()));

        // A constraint is never out-ranked: an @override that violates it is a violation.
        let (_, c) = ranked_all_orders(&[
            Ranked::constraint(Constraint::Range(0, 20), 100),
            Ranked::at(Rank::Default, 1, i(3)),
            Ranked::at(Rank::Override, 2, i(30)),
        ]);
        assert!(matches!(c, Collapsed::Violated { .. }));
    }

    // ---- plain ranked behaviour (B's graft, restricted) --------------------

    #[test]
    fn three_ranks_highest_wins_same_rank_disagreement_is_shadowed() {
        let (_, c) = ranked_all_orders(&[
            Ranked::at(Rank::Default, 1, i(3)),
            Ranked::at(Rank::Normal, 2, i(14)),
            Ranked::at(Rank::Override, 3, i(30)),
        ]);
        assert!(
            matches!(&c, Collapsed::Val { value, rank: Rank::Override, witnesses, .. } if *value == i(30) && *witnesses == Witnesses::from([3]))
        );

        // Same rank, different values under a higher rank: the higher rank
        // wins and the disagreement is shadowed, naming both (F DR-9).
        let (_, c) = ranked_all_orders(&[
            Ranked::at(Rank::Normal, 1, i(14)),
            Ranked::at(Rank::Normal, 2, i(20)),
            Ranked::at(Rank::Override, 3, i(30)),
        ]);
        let Collapsed::Val {
            value,
            rank: Rank::Override,
            shadowed,
            ..
        } = c
        else {
            panic!("{c:?}")
        };
        assert_eq!(value, i(30));
        let [
            Shadowed::Conflict {
                rank: Rank::Normal,
                witnesses,
                ..
            },
        ] = shadowed.as_slice()
        else {
            panic!("{shadowed:?}")
        };
        assert_eq!(*witnesses, Witnesses::from([1, 2]));

        // @default replaces arg_default: a default plus one normal value.
        let (_, c) = ranked_all_orders(&[
            Ranked::at(Rank::Default, 1, Value::Bool(false)),
            Ranked::at(Rank::Normal, 2, Value::Bool(true)),
        ]);
        assert!(matches!(&c, Collapsed::Val { value, .. } if *value == Value::Bool(true)));
    }

    // ---- the unranked lattices, with nulls inside ----------------------------

    fn assert_order_independent(lat: &Lattice, contribs: &[(Witness, Value)]) -> Elem {
        let base = lub(lat, ".x", contribs.iter().cloned());
        for p in permutations(contribs) {
            assert_eq!(
                lub(lat, ".x", p.iter().cloned()),
                base,
                "order dependence for {p:?}"
            );
        }
        let doubled: Vec<_> = contribs.iter().chain(contribs.iter()).cloned().collect();
        assert_eq!(lub(lat, ".x", doubled), base, "not idempotent");
        base
    }

    #[test]
    fn map_is_pointwise_and_a_stuck_leaf_makes_the_map_stuck() {
        let lat = Lattice::Map(Box::new(Lattice::Flat));
        let e = assert_order_independent(
            &lat,
            &[
                (1, obj(&[("env", s("prod"))])),
                (2, obj(&[("team", s("platform"))])),
                (3, obj(&[("env", s("prod")), ("id", fresh("vpc#id"))])),
                (4, Value::Obj(BTreeMap::new())),
            ],
        );
        assert_eq!(
            e,
            Elem::Val(
                obj(&[
                    ("env", s("prod")),
                    ("id", fresh("vpc#id")),
                    ("team", s("platform"))
                ]),
                Witnesses::from([1, 2, 3, 4])
            )
        );
        let st = assert_order_independent(
            &lat,
            &[
                (1, obj(&[("ep", open("x#ep"))])),
                (2, obj(&[("ep", s("1.2.3.4"))])),
            ],
        );
        assert!(matches!(st, Elem::Stuck { .. }));
        let c = assert_order_independent(
            &lat,
            &[(1, obj(&[("team", s("a"))])), (2, obj(&[("team", s("b"))]))],
        );
        assert!(matches!(c, Elem::Conflict { .. }));
    }

    #[test]
    fn set_is_union_modulo_definite_equality_and_never_conflicts() {
        let e = assert_order_independent(
            &Lattice::Set,
            &[
                (1, Value::List(vec![s("s3")])),
                (2, Value::List(vec![s("cloudwatch"), fresh("sub-a#id")])),
                (3, Value::List(vec![s("s3"), fresh("sub-a#id")])),
                (4, Value::List(vec![])),
            ],
        );
        let Elem::Val(Value::List(xs), ws) = e else {
            panic!()
        };
        assert_eq!(xs.len(), 3);
        assert_eq!(ws, Witnesses::from([1, 2, 3, 4]));
        // An open null and a constant that might be equal are both kept.
        let e = assert_order_independent(
            &Lattice::Set,
            &[
                (1, Value::List(vec![open("x#ep")])),
                (2, Value::List(vec![s("1.2.3.4")])),
            ],
        );
        let Elem::Val(Value::List(xs), _) = e else {
            panic!()
        };
        assert_eq!(xs.len(), 2);
    }

    #[test]
    fn keyed_list_merges_by_key_and_conflicts_within_a_key() {
        let lat = Lattice::Keyed {
            keys: vec!["action".into(), "resource".into()],
            elem: Box::new(Lattice::Map(Box::new(Lattice::Flat))),
        };
        let st = |a: &str, r: Value, extra: Option<(&str, Value)>| {
            let mut m = vec![("action", s(a)), ("resource", r)];
            if let Some(x) = extra {
                m.push(x)
            }
            obj(&m)
        };
        let e = assert_order_independent(
            &lat,
            &[
                (1, Value::List(vec![st("db.connect", fresh("db#id"), None)])),
                (
                    2,
                    Value::List(vec![
                        st("org.read", s("org"), None),
                        st("db.connect", fresh("db#id"), None),
                    ]),
                ),
                (
                    3,
                    Value::List(vec![st(
                        "db.connect",
                        fresh("db#id"),
                        Some(("effect", s("allow"))),
                    )]),
                ),
            ],
        );
        let Elem::Val(Value::List(items), _) = e else {
            panic!()
        };
        assert_eq!(
            items.len(),
            2,
            "same key (with a fresh null in it) merged into one element"
        );
        let c = lub(
            &lat,
            ".statements",
            [
                (
                    1,
                    Value::List(vec![st(
                        "db.connect",
                        s("db-1"),
                        Some(("effect", s("allow"))),
                    )]),
                ),
                (
                    2,
                    Value::List(vec![st(
                        "db.connect",
                        s("db-1"),
                        Some(("effect", s("deny"))),
                    )]),
                ),
            ],
        );
        assert!(matches!(c, Elem::Conflict { .. }));
    }

    #[test]
    fn bag_would_not_be_idempotent() {
        let a = vec![s("x")];
        let mut bag = a.clone();
        bag.extend(a.clone());
        assert_ne!(bag, a);
    }

    // ---- the laws over a mixed sample, every triple ---------------------------

    #[test]
    fn flat_join_is_commutative_associative_idempotent_over_a_mixed_sample() {
        let sample = [
            Elem::Bottom,
            Elem::Val(s("a"), Witnesses::from([1])),
            Elem::Val(s("b"), Witnesses::from([2])),
            Elem::Val(fresh("f1"), Witnesses::from([3])),
            Elem::Val(fresh("f2"), Witnesses::from([4])),
            Elem::Val(open("o1"), Witnesses::from([5])),
            Elem::Val(open("o2"), Witnesses::from([6])),
            Elem::Val(secret("s1"), Witnesses::from([7])),
            Elem::Val(s("a"), Witnesses::from([8])),
        ];
        let j = |x: &Elem, y: &Elem| join(&Lattice::Flat, ".x", x.clone(), y.clone());
        let same = |x: &Elem, y: &Elem| match (x, y) {
            // Conflicts may pick different witness pairs when three or more
            // contributions disagree; the law that matters is that both are
            // conflicts over the same witness set.
            (Elem::Conflict { .. }, Elem::Conflict { .. }) => x.witnesses() == y.witnesses(),
            _ => x == y,
        };
        for a in &sample {
            assert!(same(&j(a, a), a), "idempotent {a:?}");
            for b in &sample {
                assert!(same(&j(a, b), &j(b, a)), "commutative {a:?} {b:?}");
                for c in &sample {
                    assert!(
                        same(&j(&j(a, b), c), &j(a, &j(b, c))),
                        "associative {a:?} {b:?} {c:?}"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Proposal F additions: shadow-aware collapse, and the adversarial cases
// ---------------------------------------------------------------------------

/// What a losing rank was hiding. Reported as `warn(attr_shadowed, ...)`,
/// never as a stuck or a conflict (NixOS `mkDefault`/`mkOverride` precedent:
/// only the highest priority's values are merged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shadowed {
    Stuck {
        rank: Rank,
        nulls: BTreeSet<String>,
        witnesses: Witnesses,
    },
    Conflict {
        rank: Rank,
        path: String,
        reason: String,
        witnesses: Witnesses,
    },
}

/// A checkable refinement on one path of a cell (a dotted path at or below
/// the group's): rank-blind, checked against the winning value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refinement {
    pub path: String,
    pub constraint: Constraint,
    pub witness: Witness,
}

/// A refinement the winning value could not decide yet: the value at
/// `path` carries a null. Re-checked when the null resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deferred {
    pub path: String,
    pub constraint: Constraint,
    pub value: Value,
    pub witnesses: Witnesses,
}

/// The result of the one non-monotone read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Collapsed {
    Bottom,
    Val {
        value: Value,
        rank: Rank,
        witnesses: Witnesses,
        deferred: Vec<Deferred>,
        shadowed: Vec<Shadowed>,
    },
    Stuck {
        rank: Rank,
        nulls: BTreeSet<String>,
        shadowed: Vec<Shadowed>,
    },
    /// `witnesses` is every contribution the conflict involves (every
    /// contribution at the winning rank, or the value's and the refinement's).
    Conflict {
        rank: Option<Rank>,
        a: (Value, Witnesses),
        b: (Value, Witnesses),
        reason: String,
        witnesses: Witnesses,
        shadowed: Vec<Shadowed>,
    },
    /// The winning value violates a refinement (E §2.4 step 4): the value
    /// at `path` and the contributions that won, and the refinement's own
    /// witnesses.
    Violated {
        path: String,
        constraint: Constraint,
        value: Value,
        witnesses: Witnesses,
        refinement: Witnesses,
        shadowed: Vec<Shadowed>,
    },
}

impl Ranked {
    /// Collapse, F's revision (DR-9 / §4.1 rule 2) of E §2.4: the highest
    /// non-empty rank decides. A Stuck or Conflict at a LOSING rank is shadowed, reported as
    /// a warning, and never blocks. Only the winning rank's element can make
    /// the cell Stuck or Conflict.
    pub fn collapse(&self) -> Collapsed {
        self.collapse_at("")
    }

    /// `collapse` of the cell at `path`, which names the path
    /// of a deferred or violated refinement.
    pub fn collapse_at(&self, path: &str) -> Collapsed {
        let Some(top) = self.ranks.iter().rposition(|e| !matches!(e, Elem::Bottom)) else {
            return Collapsed::Bottom;
        };
        let mut shadowed = Vec::new();
        for (i, e) in self.ranks.iter().enumerate().take(top) {
            match e {
                Elem::Stuck { nulls, .. } => shadowed.push(Shadowed::Stuck {
                    rank: rank_of(i),
                    nulls: nulls.clone(),
                    witnesses: e.witnesses(),
                }),
                Elem::Conflict { path, reason, .. } => shadowed.push(Shadowed::Conflict {
                    rank: rank_of(i),
                    path: path.clone(),
                    reason: reason.clone(),
                    witnesses: e.witnesses(),
                }),
                _ => {}
            }
        }
        let rank = rank_of(top);
        match &self.ranks[top] {
            Elem::Bottom => unreachable!(),
            e @ Elem::Conflict { a, b, reason, .. } => Collapsed::Conflict {
                rank: Some(rank),
                a: a.clone(),
                b: b.clone(),
                reason: reason.clone(),
                witnesses: e.witnesses(),
                shadowed,
            },
            Elem::Stuck { nulls, .. } => Collapsed::Stuck {
                rank,
                nulls: nulls.clone(),
                shadowed,
            },
            Elem::Val(v, w) => {
                let mut deferred = Vec::new();
                for (c, cw) in &self.constraints {
                    match c.check(v) {
                        Truth::True => {}
                        Truth::Unknown => deferred.push(Deferred {
                            path: path.to_string(),
                            constraint: c.clone(),
                            value: v.clone(),
                            witnesses: cw.clone(),
                        }),
                        Truth::False => {
                            return Collapsed::Violated {
                                path: path.to_string(),
                                constraint: c.clone(),
                                value: v.clone(),
                                witnesses: w.clone(),
                                refinement: cw.clone(),
                                shadowed,
                            };
                        }
                    }
                }
                Collapsed::Val {
                    value: v.clone(),
                    rank,
                    witnesses: w.clone(),
                    deferred,
                    shadowed,
                }
            }
        }
    }
}

/// A ranked contribution to one attribute cell: who, at which rank, what.
pub type RankedContribution = (Witness, Rank, Value);

fn max_rank(contribs: &[RankedContribution]) -> Rank {
    contribs
        .iter()
        .map(|(_, r, _)| *r)
        .max()
        .unwrap_or(Rank::Normal)
}

/// The attribute aggregate of E §2.5 for one `(T, A, P)` group: the least
/// upper bound of every contribution under `lat`, collapsed with F's
/// shadow-aware rule. Flat, Set and Keyed cells are ranked shelves (see
/// `Ranked`). A Map is ranked at its leaves: each key is its own cell under
/// the element lattice, so a `@default` tag and a normal tag on another key
/// both survive; a key whose every contribution is an object is itself a
/// Map, so nested objects merge recursively. A Map path with a contribution that is not an object is
/// assembled as one Flat value (which conflicts, or is stuck, on the
/// non-object).
pub fn lub_ranked(lat: &Lattice, path: &str, contribs: &[RankedContribution]) -> Collapsed {
    lub_ranked_refined(lat, path, contribs, &[])
}

/// `lub_ranked` with the cell's checkable refinements (E §2.5, the
/// `constraint(C)` contributions): each is joined into the Flat cell at its
/// path, rank-blind, and checked against the winning value there. A
/// refinement below a path assembled as one value (a Flat object, a set) is
/// checked against the value found at its path in the winning one; a path
/// the winning value does not reach holds vacuously.
pub fn lub_ranked_refined(
    lat: &Lattice,
    path: &str,
    contribs: &[RankedContribution],
    refinements: &[Refinement],
) -> Collapsed {
    let (here, below): (Vec<&Refinement>, Vec<&Refinement>) =
        refinements.iter().partition(|r| r.path == path);
    let collapsed = match lat {
        Lattice::Map(elem) if contribs.iter().all(|(_, _, v)| matches!(v, Value::Obj(_))) => {
            let below: Vec<Refinement> = below.into_iter().cloned().collect();
            let c = lub_ranked_map(elem, path, contribs, &below);
            return check_below(c, path, &here);
        }
        Lattice::Map(_) | Lattice::Flat | Lattice::Set | Lattice::Keyed { .. } => {
            let lat = match lat {
                Lattice::Map(_) => &Lattice::Flat,
                l => l,
            };
            let cell = contribs.iter().fold(Ranked::default(), |acc, (w, r, v)| {
                acc.join_in(lat, &Ranked::at(*r, *w, v.clone()), path)
            });
            here.iter()
                .fold(cell, |acc, r| {
                    acc.join_in(
                        lat,
                        &Ranked::constraint(r.constraint.clone(), r.witness),
                        path,
                    )
                })
                .collapse_at(path)
        }
    };
    check_below(collapsed, path, &below)
}

/// Check refinements at or below `path` against a collapsed value's
/// content there.
fn check_below(c: Collapsed, path: &str, refinements: &[&Refinement]) -> Collapsed {
    let Collapsed::Val {
        value,
        rank,
        witnesses,
        mut deferred,
        shadowed,
    } = c
    else {
        return c;
    };
    let mut refinements = refinements.to_vec();
    refinements.sort_by(|a, b| (&a.path, &a.constraint).cmp(&(&b.path, &b.constraint)));
    for r in refinements {
        let rest = if r.path == path {
            ""
        } else {
            r.path
                .strip_prefix(path)
                .and_then(|p| p.strip_prefix('.'))
                .unwrap_or(&r.path)
        };
        let Some(v) = value_at(&value, rest) else {
            continue;
        };
        match r.constraint.check(v) {
            Truth::True => {}
            Truth::Unknown => deferred.push(Deferred {
                path: r.path.clone(),
                constraint: r.constraint.clone(),
                value: v.clone(),
                witnesses: Witnesses::from([r.witness]),
            }),
            Truth::False => {
                return Collapsed::Violated {
                    path: r.path.clone(),
                    constraint: r.constraint.clone(),
                    value: v.clone(),
                    witnesses,
                    refinement: Witnesses::from([r.witness]),
                    shadowed,
                };
            }
        }
    }
    Collapsed::Val {
        value,
        rank,
        witnesses,
        deferred,
        shadowed,
    }
}

/// The value at a dotted path inside `v` (`""` is `v`).
fn value_at<'v>(v: &'v Value, path: &str) -> Option<&'v Value> {
    if path.is_empty() {
        return Some(v);
    }
    path.split('.').try_fold(v, |v, k| match v {
        Value::Obj(m) => m.get(k),
        _ => None,
    })
}

fn lub_ranked_map(
    elem: &Lattice,
    path: &str,
    contribs: &[RankedContribution],
    refinements: &[Refinement],
) -> Collapsed {
    let mut per_key: BTreeMap<String, Vec<RankedContribution>> = BTreeMap::new();
    for (w, r, v) in contribs {
        let Value::Obj(m) = v else {
            unreachable!("checked by the caller")
        };
        for (k, x) in m {
            per_key
                .entry(k.clone())
                .or_default()
                .push((*w, *r, x.clone()));
        }
    }
    let mut out = BTreeMap::new();
    let mut witnesses = Witnesses::new();
    let mut deferred = Vec::new();
    let mut shadowed = Vec::new();
    let mut stuck: Option<(Rank, BTreeSet<String>)> = None;
    for (k, cs) in per_key {
        // Nested objects merge per key too: a dotted path `a.b.c` is the
        // contribution `{b: {c: V}}` to `a`, so two dotted paths under one
        // attribute meet here and must not conflict on `b`.
        let nested = Lattice::Map(Box::new(elem.clone()));
        let lat = if cs.iter().all(|(_, _, v)| matches!(v, Value::Obj(_))) {
            &nested
        } else {
            elem
        };
        let key_path = format!("{path}.{k}");
        let refs: Vec<Refinement> = refinements
            .iter()
            .filter(|r| {
                r.path == key_path
                    || r.path
                        .strip_prefix(&key_path)
                        .is_some_and(|p| p.starts_with('.'))
            })
            .cloned()
            .collect();
        match lub_ranked_refined(lat, &key_path, &cs, &refs) {
            Collapsed::Bottom => {}
            Collapsed::Val {
                value,
                witnesses: w,
                deferred: d,
                shadowed: sh,
                ..
            } => {
                out.insert(k, value);
                witnesses = union(&witnesses, &w);
                deferred.extend(d);
                shadowed.extend(sh);
            }
            Collapsed::Stuck {
                rank,
                nulls,
                shadowed: sh,
            } => {
                shadowed.extend(sh);
                let st = stuck.get_or_insert((rank, BTreeSet::new()));
                st.0 = st.0.max(rank);
                st.1.extend(nulls);
            }
            Collapsed::Conflict {
                rank,
                a,
                b,
                reason,
                witnesses,
                shadowed: sh,
            } => {
                shadowed.extend(sh);
                return Collapsed::Conflict {
                    rank,
                    a,
                    b,
                    reason,
                    witnesses,
                    shadowed,
                };
            }
            Collapsed::Violated {
                path,
                constraint,
                value,
                witnesses,
                refinement,
                shadowed: sh,
            } => {
                shadowed.extend(sh);
                return Collapsed::Violated {
                    path,
                    constraint,
                    value,
                    witnesses,
                    refinement,
                    shadowed,
                };
            }
        }
    }
    if let Some((rank, nulls)) = stuck {
        return Collapsed::Stuck {
            rank,
            nulls,
            shadowed,
        };
    }
    if out.is_empty() {
        // Only empty objects: the value is `{}`, from every contributor.
        witnesses = contribs.iter().map(|(w, _, _)| *w).collect();
    }
    Collapsed::Val {
        value: Value::Obj(out),
        rank: max_rank(contribs),
        witnesses,
        deferred,
        shadowed,
    }
}

#[cfg(test)]
mod f_tests {
    use super::*;
    use crate::value::NullClass;

    fn s(x: &str) -> Value {
        Value::Str(x.into())
    }
    fn open(l: &str) -> Value {
        Value::Null {
            label: l.into(),
            class: NullClass::Open,
            ty: "string".into(),
        }
    }
    fn secret(l: &str) -> Value {
        Value::Null {
            label: l.into(),
            class: NullClass::Secret,
            ty: "string".into(),
        }
    }
    fn joined(cells: &[Ranked]) -> Ranked {
        cells
            .iter()
            .fold(Ranked::default(), |acc, c| acc.join(c, ".p"))
    }

    /// adv1: same-rank Stuck at a LOSING rank. Two @default contributions
    /// that might disagree (an allocator's open null and a hard-coded
    /// placeholder), overridden by a normal value.
    #[test]
    fn adv1_losing_rank_stuck_does_not_block() {
        let cell = joined(&[
            Ranked::at(Rank::Default, 1, open("alloc/cp#cidr")),
            Ranked::at(Rank::Default, 2, s("10.0.0.0/28")),
            Ranked::at(Rank::Normal, 3, s("172.16.3.96/28")),
        ]);
        // E §2.4 step 2 waited on a null the winning value does not need
        // ("if any rank is Stuck"). F: the normal value wins; the shadowed
        // disagreement is a warning.
        let f = cell.collapse();
        println!("F collapse: {f:?}");
        assert!(
            matches!(&f, Collapsed::Val { value, rank: Rank::Normal, shadowed, .. }
            if *value == s("172.16.3.96/28") && matches!(shadowed[0], Shadowed::Stuck { rank: Rank::Default, .. }))
        );
        // Same for a shadowed CONFLICT at the losing rank.
        let cell = joined(&[
            Ranked::at(Rank::Default, 1, s("a")),
            Ranked::at(Rank::Default, 2, s("b")),
            Ranked::at(Rank::Override, 3, s("c")),
        ]);
        assert!(
            matches!(cell.collapse(), Collapsed::Val { value, shadowed, .. } if value == s("c") && shadowed.len() == 1)
        );
        // And the winning rank still blocks when IT is stuck: nothing lost.
        let cell = joined(&[
            Ranked::at(Rank::Normal, 1, open("alloc/cp#cidr")),
            Ranked::at(Rank::Normal, 2, s("10.0.0.0/28")),
        ]);
        assert!(matches!(
            cell.collapse(),
            Collapsed::Stuck {
                rank: Rank::Normal,
                ..
            }
        ));
    }

    /// adv6: a refinement on a cell whose ONLY contribution is a secret null.
    /// E §2.4 step 4 says every constraint is deferred to the provider; E's
    /// phase assignment says a cell carrying a deferred constraint is never
    /// definite; and a secret is never resolved in the store (E §2.7 Rule 4).
    /// So the deformation is pending forever. `resolve` cannot help: there is
    /// nothing to substitute.
    #[test]
    fn adv6_secret_only_refined_cell_is_pending_forever_under_e() {
        let cell = joined(&[
            Ranked::at(Rank::Normal, 1, secret("sm/db_pw#secret_data")),
            Ranked::constraint(Constraint::IsStr, 9),
        ]);
        let c = cell.collapse();
        println!("secret-only refined cell: {c:?}");
        let Collapsed::Val { deferred, .. } = &c else {
            panic!()
        };
        assert!(
            deferred
                .iter()
                .map(|d| &d.constraint)
                .eq([&Constraint::IsStr])
        );
        // No resolution is ever applied to a secret (E Rule 4), so a second
        // collapse after any number of boundaries is identical.
        let again = cell.resolve("sm/db_pw#secret_data", &s("hunter2"));
        // even if one WERE applied, the engine would now hold the bytes:
        let Collapsed::Val { value, .. } = again.collapse() else {
            panic!()
        };
        assert_eq!(
            value,
            s("hunter2"),
            "resolving a secret in the store materializes it, which E forbids"
        );
        // Therefore: deferred stays non-empty forever without materialization.
        let Collapsed::Val { deferred, .. } = cell.collapse() else {
            panic!()
        };
        assert!(!deferred.is_empty());
    }

    /// adv5: two modules appending to one ORDERED list. Under Flat(List) it
    /// is a conflict (E admits this). Under Keyed with the position as the
    /// merge key it is a lattice, provided each author supplies a distinct
    /// key; a colliding key with a different value is the conflict it
    /// should be.
    #[test]
    fn adv5_two_authors_appending_to_an_ordered_list() {
        let a = Value::List(vec![s("--verbose")]);
        let b = Value::List(vec![s("--port=8080")]);
        let flat = lub(&Lattice::Flat, ".args", [(1, a.clone()), (2, b.clone())]);
        println!("Flat(List): {flat:?}");
        assert!(matches!(flat, Elem::Conflict { .. }));
        // Keyed on an explicit priority: `{prio, arg}` rows, emitted in key order.
        let row = |p: i64, v: &str| {
            let mut m = std::collections::BTreeMap::new();
            m.insert("prio".to_string(), Value::Int(p));
            m.insert("arg".to_string(), s(v));
            Value::Obj(m)
        };
        let keyed = Lattice::Keyed {
            keys: vec!["prio".into()],
            elem: Box::new(Lattice::Flat),
        };
        let e = lub(
            &keyed,
            ".args",
            [
                (1, Value::List(vec![row(10, "--verbose")])),
                (2, Value::List(vec![row(20, "--port=8080")])),
            ],
        );
        println!("Keyed(prio): {e:?}");
        let Elem::Val(Value::List(xs), _) = e else {
            panic!("{e:?}")
        };
        assert_eq!(xs.len(), 2);
        assert_eq!(xs[0], row(10, "--verbose"));
        // Same priority, different value: conflict, naming both.
        let e = lub(
            &keyed,
            ".args",
            [
                (1, Value::List(vec![row(10, "--verbose")])),
                (2, Value::List(vec![row(10, "--quiet")])),
            ],
        );
        assert!(matches!(e, Elem::Conflict { .. }));
        // Set is NOT an answer for an ordered list: it sorts. F8, found on
        // the way: E's prototype never normalized a LONE contribution
        // (`join(Bottom, e)` returned `e` as is), so one Set contribution
        // kept its order and duplicates while the same contribution given
        // twice was sorted and deduplicated. Fixed: `⊥ ⊔ e = normalize(e)`.
        let one = lub(
            &Lattice::Set,
            ".args",
            [(1, Value::List(vec![s("--z"), s("--a"), s("--z")]))],
        );
        let twice = lub(
            &Lattice::Set,
            ".args",
            [
                (1, Value::List(vec![s("--z"), s("--a"), s("--z")])),
                (1, Value::List(vec![s("--z"), s("--a"), s("--z")])),
            ],
        );
        println!("Set, one contribution:   {one:?}");
        println!("Set, same given twice:   {twice:?}");
        let Elem::Val(Value::List(xs1), _) = &one else {
            panic!()
        };
        let Elem::Val(Value::List(xs2), _) = &twice else {
            panic!()
        };
        assert_eq!(
            xs1,
            &vec![s("--a"), s("--z")],
            "a lone contribution is normalized"
        );
        assert_eq!(xs2, &vec![s("--a"), s("--z")]);
        assert_eq!(one, twice, "Set lub is idempotent on one contribution");
    }

    /// Order independence of the shadow-aware collapse under every
    /// permutation, over the three cells above.
    #[test]
    fn shadow_aware_collapse_is_order_independent() {
        let cells = vec![
            Ranked::at(Rank::Default, 1, open("alloc/cp#cidr")),
            Ranked::at(Rank::Default, 2, s("10.0.0.0/28")),
            Ranked::at(Rank::Normal, 3, s("172.16.3.96/28")),
            Ranked::constraint(Constraint::IsStr, 9),
        ];
        let base = joined(&cells).collapse();
        let n = cells.len();
        let mut perm = cells.clone();
        let mut c = vec![0usize; n];
        let mut i = 0;
        while i < n {
            if c[i] < i {
                if i % 2 == 0 {
                    perm.swap(0, i)
                } else {
                    perm.swap(c[i], i)
                }
                assert_eq!(joined(&perm).collapse(), base);
                c[i] += 1;
                i = 0;
            } else {
                c[i] = 0;
                i += 1;
            }
        }
    }
}

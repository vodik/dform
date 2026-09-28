//! The evaluator's fact store: every tuple once, numbered in insertion
//! order, grouped into relations, each with the hash indexes the compiled
//! rules read through ([`super::ops::indexes`]).
//!
//! Tuple ids grow with insertion, so "the tuples of an earlier round" and
//! "the tuples the last round derived" are id windows, and semi-naive
//! evaluation needs no copy of the store.
//!
//! An index answers a lookup exactly as a scan followed by unification
//! would, Rule 2 included: a tuple is left out only when a key column is
//! definitely unequal to the probe before any comparison with it is
//! undecided. Unification compares columns in order and records the
//! instance stuck at the first undecided one (an open or secret null), so a
//! tuple with such a null in a key column is a candidate when the key
//! columns before that one equal the probe's. For a loose read, a tuple
//! with such a null anywhere is a candidate. A probe that holds a null
//! reads every tuple.

use super::fx::{FxHashMap, FxHasher};
use super::ops::{Key, Rel};
use crate::ast::{Atom, Term};
use crate::stuck::has_open_or_secret;
use crate::value::Value;
use std::cell::Cell;
use std::hash::{Hash, Hasher};

pub type TupleId = u32;

/// Tuple ids `lo..hi`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub lo: TupleId,
    pub hi: TupleId,
}

impl Window {
    pub fn below(hi: TupleId) -> Window {
        Window { lo: 0, hi }
    }
}

#[derive(Debug, Default, Clone)]
struct Index {
    /// Tuples by the hash of their key columns (a collision only adds a
    /// candidate; unification decides).
    buckets: FxHashMap<u64, Vec<TupleId>>,
    /// Tuples with an open or secret null in a key column, by the
    /// position in the key of the first such column and the hash of the
    /// key columns before it.
    undecided: FxHashMap<(usize, u64), Vec<TupleId>>,
    /// The positions `undecided` has tuples at.
    undecided_at: Vec<usize>,
}

#[derive(Debug, Default, Clone)]
struct Relation {
    rows: Vec<TupleId>,
    /// Tuples with an open or secret null in some column.
    open: Vec<TupleId>,
    indexes: Vec<(Key, Index)>,
}

#[derive(Debug, Default, Clone)]
pub struct Store {
    atoms: Vec<Atom>,
    ids: FxHashMap<Atom, TupleId>,
    rels: FxHashMap<Rel, Relation>,
    /// Index lookups and tuples read, for the benchmark's operation count.
    pub reads: Cell<u64>,
}

fn val(t: &Term) -> &Value {
    match t {
        Term::Val(v) => v,
        _ => panic!("store: non-ground tuple"),
    }
}

fn key_hash<'a>(vals: impl Iterator<Item = &'a Value>) -> u64 {
    let mut h = FxHasher::default();
    for v in vals {
        v.hash(&mut h);
    }
    h.finish()
}

/// The ids of `ids` (ascending) inside `w`.
fn window(ids: &[TupleId], w: Window) -> &[TupleId] {
    let lo = ids.partition_point(|&i| i < w.lo);
    let hi = ids.partition_point(|&i| i < w.hi);
    &ids[lo..hi]
}

impl Index {
    fn add(&mut self, key: &Key, id: TupleId, a: &Atom) {
        let vals = key.iter().map(|&c| val(&a.args[c]));
        match key
            .iter()
            .position(|&c| has_open_or_secret(val(&a.args[c])))
        {
            Some(j) => {
                if !self.undecided_at.contains(&j) {
                    self.undecided_at.push(j);
                }
                let h = key_hash(vals.take(j));
                self.undecided.entry((j, h)).or_default().push(id);
            }
            None => self.buckets.entry(key_hash(vals)).or_default().push(id),
        }
    }
}

impl Store {
    /// The number of tuples: the id the next insertion gets.
    pub fn len(&self) -> TupleId {
        self.atoms.len() as TupleId
    }

    pub fn is_empty(&self) -> bool {
        self.atoms.is_empty()
    }

    pub fn get(&self, id: TupleId) -> &Atom {
        &self.atoms[id as usize]
    }

    pub fn id(&self, a: &Atom) -> Option<TupleId> {
        self.ids.get(a).copied()
    }

    pub fn contains(&self, a: &Atom) -> bool {
        self.ids.contains_key(a)
    }

    /// Every tuple, in insertion order.
    pub fn atoms(&self) -> &[Atom] {
        &self.atoms
    }

    /// Keep an index on `key` of `rel` from now on (built from the tuples
    /// already there).
    pub fn index(&mut self, rel: &Rel, key: &Key) {
        let r = self.rels.entry(rel.clone()).or_default();
        if r.indexes.iter().any(|(k, _)| k == key) {
            return;
        }
        let mut ix = Index::default();
        for &id in &r.rows {
            ix.add(key, id, &self.atoms[id as usize]);
        }
        r.indexes.push((key.clone(), ix));
    }

    /// Insert a ground tuple: its id, and whether it is new. A tuple equal
    /// to one already stored (spans aside) is that tuple.
    pub fn insert(&mut self, a: Atom) -> (TupleId, bool) {
        if let Some(&id) = self.ids.get(&a) {
            return (id, false);
        }
        let id = self.len();
        let r = self.rels.entry(Rel::of(&a)).or_default();
        r.rows.push(id);
        if a.args.iter().any(|t| has_open_or_secret(val(t))) {
            r.open.push(id);
        }
        for (key, ix) in &mut r.indexes {
            ix.add(key, id, &a);
        }
        self.ids.insert(a.clone(), id);
        self.atoms.push(a);
        (id, true)
    }

    /// The tuples of `rel` inside `w`, in id order.
    pub fn ids(&self, rel: &Rel, w: Window) -> &[TupleId] {
        self.rels.get(rel).map_or(&[], |r| window(&r.rows, w))
    }

    /// Does `rel` have a tuple inside `w`?
    pub fn any_in(&self, rel: &Rel, w: Window) -> bool {
        self.rels
            .get(rel)
            .is_some_and(|r| !window(&r.rows, w).is_empty())
    }

    /// The tuples of `rel` inside `w` that can unify with a literal whose
    /// `key` columns are `probe` (every tuple when `probe` is `None`), in id
    /// order. `loose`: the literal compares columns outside the key.
    pub fn candidates(
        &self,
        rel: &Rel,
        key: &Key,
        probe: Option<&[Value]>,
        loose: bool,
        w: Window,
    ) -> Vec<TupleId> {
        self.reads.set(self.reads.get() + 1);
        let Some(r) = self.rels.get(rel) else {
            return Vec::new();
        };
        let out = match probe {
            Some(p) if !key.is_empty() => {
                let Some((_, ix)) = r.indexes.iter().find(|(k, _)| k == key) else {
                    panic!("store: no index on {}/{} {key:?}", rel.pred, rel.arity)
                };
                let bucket = ix
                    .buckets
                    .get(&key_hash(p.iter()))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let mut out: Vec<TupleId> = window(bucket, w).to_vec();
                for &j in &ix.undecided_at {
                    if let Some(u) = ix.undecided.get(&(j, key_hash(p[..j].iter()))) {
                        out.extend_from_slice(window(u, w));
                    }
                }
                if loose {
                    out.extend_from_slice(window(&r.open, w));
                }
                out.sort_unstable();
                out.dedup();
                out
            }
            _ => window(&r.rows, w).to_vec(),
        };
        self.reads.set(self.reads.get() + out.len() as u64);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::NullClass;

    fn atom(pred: &str, args: Vec<Value>) -> Atom {
        Atom {
            pred: pred.into(),
            args: args.into_iter().map(Term::Val).collect(),
            record: None,
            span: Default::default(),
        }
    }

    fn s(x: &str) -> Value {
        Value::Str(x.into())
    }

    #[test]
    fn a_lookup_reads_its_bucket_and_the_undecided_tuples() {
        let rel = Rel {
            pred: "p".into(),
            arity: 2,
        };
        let open = Value::Null {
            label: "t/a#x".into(),
            class: NullClass::Open,
            ty: "string".into(),
        };
        let mut st = Store::default();
        st.index(&rel, &vec![0]);
        let (a, _) = st.insert(atom("p", vec![s("a"), s("1")]));
        st.insert(atom("p", vec![s("b"), s("2")]));
        let (c, _) = st.insert(atom("p", vec![open.clone(), s("3")]));
        let (d, _) = st.insert(atom("p", vec![s("c"), open]));
        assert!(!st.insert(atom("p", vec![s("a"), s("1")])).1);
        let all = Window::below(st.len());
        let key = vec![0];
        assert_eq!(
            st.candidates(&rel, &key, Some(&[s("a")]), false, all),
            vec![a, c]
        );
        assert_eq!(
            st.candidates(&rel, &key, Some(&[s("a")]), true, all),
            vec![a, c, d]
        );
        assert_eq!(
            st.candidates(&rel, &key, Some(&[s("a")]), false, Window { lo: 1, hi: 3 }),
            vec![c]
        );
        assert_eq!(st.candidates(&rel, &key, None, false, all).len(), 4);
    }

    /// Unification stops at the first unequal column: an undecided tuple
    /// whose decided key columns before its null differ is not a candidate.
    #[test]
    fn an_undecided_tuple_is_read_only_under_its_decided_prefix() {
        let rel = Rel {
            pred: "q".into(),
            arity: 2,
        };
        let open = |l: &str| Value::Null {
            label: l.into(),
            class: NullClass::Open,
            ty: "string".into(),
        };
        let mut st = Store::default();
        st.index(&rel, &vec![0, 1]);
        let (a, _) = st.insert(atom("q", vec![s("a"), open("t/a#x")]));
        st.insert(atom("q", vec![s("b"), open("t/b#x")]));
        let (c, _) = st.insert(atom("q", vec![open("t/c#x"), s("z")]));
        let (d, _) = st.insert(atom("q", vec![s("a"), s("y")]));
        let all = Window::below(st.len());
        let key = vec![0, 1];
        assert_eq!(
            st.candidates(&rel, &key, Some(&[s("a"), s("y")]), false, all),
            vec![a, c, d]
        );
        assert_eq!(
            st.candidates(&rel, &key, Some(&[s("b"), s("n")]), false, all)
                .len(),
            2
        );
    }
}

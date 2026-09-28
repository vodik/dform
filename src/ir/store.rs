//! The evaluator's fact store: every tuple once, numbered in insertion
//! order, grouped into relations.
//!
//! Tuple ids grow with insertion, so "the tuples of an earlier round" and
//! "the tuples the last round derived" are id windows, and semi-naive
//! evaluation needs no copy of the store.

use super::ops::Rel;
use crate::ast::Atom;
use std::cell::Cell;
use std::collections::HashMap;

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
struct Relation {
    rows: Vec<TupleId>,
}

#[derive(Debug, Default, Clone)]
pub struct Store {
    atoms: Vec<Atom>,
    ids: HashMap<Atom, TupleId>,
    rels: HashMap<Rel, Relation>,
    /// Relation reads and tuples read, for the benchmark's operation count.
    pub reads: Cell<u64>,
}

/// The ids of `ids` (ascending) inside `w`.
fn window(ids: &[TupleId], w: Window) -> &[TupleId] {
    let lo = ids.partition_point(|&i| i < w.lo);
    let hi = ids.partition_point(|&i| i < w.hi);
    &ids[lo..hi]
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

    /// Insert a ground tuple: its id, and whether it is new. A tuple equal
    /// to one already stored (spans aside) is that tuple.
    pub fn insert(&mut self, a: Atom) -> (TupleId, bool) {
        if let Some(&id) = self.ids.get(&a) {
            return (id, false);
        }
        let id = self.len();
        let r = self.rels.entry(Rel::of(&a)).or_default();
        r.rows.push(id);
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
        !self.ids(rel, w).is_empty()
    }

    /// The tuples of `rel` inside `w` a literal reading it can unify
    /// with, in id order.
    pub fn candidates(&self, rel: &Rel, w: Window) -> Vec<TupleId> {
        let out = self.ids(rel, w).to_vec();
        self.reads.set(self.reads.get() + 1 + out.len() as u64);
        out
    }
}

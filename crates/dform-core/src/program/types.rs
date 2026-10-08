//! Types of the program's nodes (R-211, R-192): a type variable per node
//! that has a type, unified by `ena` (rustc's union-find), and the type a
//! node settles on. Nothing makes a variable yet; the inference pass
//! (step 11) does, privately, and returns a [`Types`] table.
//!
//! Unification is a join for writers and a check for readers, as infer.rs
//! does it today. Two joins are not equality: a reference's types merge
//! by union (`ref(T1 | T2)`, a column a rule per type fills, R-185), and
//! a quantity's possible dimensions by intersection (`500m` is cpu or
//! duration until a position says which). Any other two shapes keep the
//! first; the solver records each requirement beside the variable and
//! judges a class that disagrees after solving, naming both sites, so the
//! join itself never fails.

use super::node::{ExprId, Name, PatternId};
use crate::quantity::Dim;
use ena::unify::{NoError, UnifyKey, UnifyValue};
use slotmap::SecondaryMap;
use std::collections::{BTreeMap, BTreeSet};

/// A type variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TyVar(u32);

impl UnifyKey for TyVar {
    type Value = TyVal;

    fn index(&self) -> u32 {
        self.0
    }

    fn from_index(i: u32) -> TyVar {
        TyVar(i)
    }

    fn tag() -> &'static str {
        "TyVar"
    }
}

/// What a variable's class is known to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TyVal {
    Unknown,
    Known(Shape),
}

impl UnifyValue for TyVal {
    type Error = NoError;

    fn unify_values(a: &TyVal, b: &TyVal) -> Result<TyVal, NoError> {
        Ok(match (a, b) {
            (TyVal::Unknown, v) | (v, TyVal::Unknown) => v.clone(),
            (TyVal::Known(a), TyVal::Known(b)) => TyVal::Known(a.join(b)),
        })
    }
}

/// A type one level deep, its parts variables of their own: the solver
/// unifies those pairwise after joining the two shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// `string`, `int`, `bool`, `inet`, ...
    Scalar(Name),
    /// A quantity, of one of these dimensions.
    Quantity(DimSet),
    /// `ref(T1 | T2)`.
    Ref(BTreeSet<Name>),
    List(TyVar),
    Map(TyVar),
    /// An object's fields; `open`, it may have more (a spread's part, a
    /// document).
    Object {
        fields: BTreeMap<Name, TyVar>,
        open: bool,
    },
    Secret(TyVar),
    Enum(Vec<Name>),
    Range(TyVar),
    Any,
}

impl Shape {
    /// The shape two writers give one class: references by union, a
    /// quantity's dimensions by intersection, else the first.
    pub fn join(&self, other: &Shape) -> Shape {
        match (self, other) {
            (Shape::Ref(a), Shape::Ref(b)) => Shape::Ref(a | b),
            (Shape::Quantity(a), Shape::Quantity(b)) => Shape::Quantity(a.intersect(*b)),
            (a, _) => a.clone(),
        }
    }
}

/// The dimensions a quantity may still have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DimSet(u8);

impl DimSet {
    fn bit(d: Dim) -> u8 {
        match d {
            Dim::Bytes => 1,
            Dim::Cpu => 2,
            Dim::Duration => 4,
        }
    }

    pub fn of(dims: &[Dim]) -> DimSet {
        DimSet(dims.iter().fold(0, |s, d| s | DimSet::bit(*d)))
    }

    pub fn intersect(self, other: DimSet) -> DimSet {
        DimSet(self.0 & other.0)
    }

    pub fn contains(self, d: Dim) -> bool {
        self.0 & DimSet::bit(d) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The one dimension, when one is left.
    pub fn single(self) -> Option<Dim> {
        [Dim::Bytes, Dim::Cpu, Dim::Duration]
            .into_iter()
            .find(|d| self == DimSet::of(&[*d]))
    }
}

/// The type a node settled on: a [`Shape`] with its parts resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Scalar(Name),
    Quantity(DimSet),
    Ref(BTreeSet<Name>),
    List(Box<Type>),
    Map(Box<Type>),
    Object {
        fields: BTreeMap<Name, Type>,
        open: bool,
    },
    Secret(Box<Type>),
    Enum(Vec<Name>),
    Range(Box<Type>),
    Any,
    /// Nothing typed it.
    Unknown,
}

/// The inference pass's table (step 11): each typed node's type.
#[derive(Debug, Default)]
pub struct Types {
    pub exprs: SecondaryMap<ExprId, Type>,
    pub patterns: SecondaryMap<PatternId, Type>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ena::unify::InPlaceUnificationTable;

    fn known(s: Shape) -> TyVal {
        TyVal::Known(s)
    }

    /// Two writers of one reference column give it both their types; two
    /// positions of `500m` leave it the dimension both allow.
    #[test]
    fn references_join_by_union_and_quantities_by_intersection() {
        let mut t: InPlaceUnificationTable<TyVar> = InPlaceUnificationTable::new();
        let refs = |n: &[&str]| Shape::Ref(n.iter().map(|s| s.to_string()).collect());
        let (a, b) = (
            t.new_key(known(refs(&["net.vpc"]))),
            t.new_key(known(refs(&["net.subnet"]))),
        );
        t.union(a, b);
        assert_eq!(t.probe_value(a), known(refs(&["net.subnet", "net.vpc"])));

        let m = t.new_key(known(Shape::Quantity(DimSet::of(&[
            Dim::Cpu,
            Dim::Duration,
        ]))));
        let cpu = t.new_key(known(Shape::Quantity(DimSet::of(&[Dim::Cpu, Dim::Bytes]))));
        let open = t.new_key(TyVal::Unknown);
        t.union(m, open);
        t.union(open, cpu);
        let TyVal::Known(Shape::Quantity(dims)) = t.probe_value(m) else {
            panic!("{:?}", t.probe_value(m))
        };
        assert_eq!(dims.single(), Some(Dim::Cpu));
    }
}

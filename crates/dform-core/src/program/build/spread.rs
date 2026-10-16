//! An object's and a list's parts (R-199, R-211 step 3): the fields and
//! elements written, the values spread among them (`{ ..base, k: v }`,
//! `[..xs, x]`), and an object's computed keys (`{ "${k}": v }`). The
//! resolver folds the parts it can and writes the rest as `__merge`,
//! `__concat` or `__object` of them; each is built back as its parts in
//! order: a written run's fields or elements, then a spread.

use super::Builder;
use crate::program::node::*;

impl Builder<'_> {
    /// `__merge(parts..)`: a written object a run of fields, any other
    /// part a spread; an empty one a spread of itself, the part it is.
    pub(super) fn merge(&mut self, parts: &[ExprId]) -> ExprId {
        let mut out = Vec::new();
        for &p in parts {
            match self.written_fields(p) {
                Some(fields) if !fields.is_empty() => out.extend(fields),
                _ => out.push(ObjPart::Spread(p)),
            }
        }
        self.expr_node(ExprKind::Object { parts: out })
    }

    /// `__concat(parts..)`: a written list a run of elements, any other
    /// part a spread; an empty one a spread of itself.
    pub(super) fn concat(&mut self, parts: &[ExprId]) -> ExprId {
        let mut out = Vec::new();
        for &p in parts {
            match self.written_elems(p) {
                Some(elems) if !elems.is_empty() => out.extend(elems),
                _ => out.push(ListPart::Spread(p)),
            }
        }
        self.expr_node(ExprKind::List { parts: out })
    }

    /// `__object(k, v, ..)`: an object with a computed key, each key
    /// written or computed, in order.
    pub(super) fn computed(&mut self, pairs: &[ExprId]) -> ExprId {
        let parts = pairs
            .chunks(2)
            .map(|kv| ObjPart::Computed {
                key: kv[0],
                value: kv[1],
            })
            .collect();
        self.expr_node(ExprKind::Object { parts })
    }

    /// The fields of `id` when it is an object of written fields alone.
    fn written_fields(&self, id: ExprId) -> Option<Vec<ObjPart>> {
        let e = &self.program.exprs[id];
        match (&e.kind, e.hoisted) {
            (ExprKind::Object { parts }, None)
                if parts.iter().all(|p| matches!(p, ObjPart::Field { .. })) =>
            {
                Some(parts.clone())
            }
            _ => None,
        }
    }

    /// The elements of `id` when it is a list of written elements alone.
    fn written_elems(&self, id: ExprId) -> Option<Vec<ListPart>> {
        let e = &self.program.exprs[id];
        match (&e.kind, e.hoisted) {
            (ExprKind::List { parts }, None)
                if parts.iter().all(|p| matches!(p, ListPart::Elem(_))) =>
            {
                Some(parts.clone())
            }
            _ => None,
        }
    }
}

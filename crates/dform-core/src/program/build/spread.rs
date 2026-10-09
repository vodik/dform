//! An object's and a list's parts (R-199, R-211 step 3): the fields and
//! elements written, the values spread among them (`{ ..base, k: v }`,
//! `[..xs, x]`), and an object's computed keys (`{ "${k}": v }`). The
//! resolver folds the parts it can and writes the rest as `__merge`,
//! `__concat` or `__object` of them; each is built back as its parts in
//! order: a written run's fields or elements, then a spread.

use super::Builder;
use crate::ast::Term;
use crate::program::node::*;
use std::collections::BTreeMap;

impl Builder<'_> {
    /// `{k: v, ..}`: written fields.
    pub(super) fn object_of(&mut self, m: &BTreeMap<String, Term>) -> ExprId {
        let parts = self.fields_of(m);
        self.expr_node(ExprKind::Object { parts })
    }

    /// `[a, b]`: written elements.
    pub(super) fn list_of(&mut self, xs: &[Term]) -> ExprId {
        let parts = self.elems_of(xs);
        self.expr_node(ExprKind::List { parts })
    }

    /// `__merge(parts..)`: a written object a run of fields, any other
    /// part a spread; an empty one a spread of itself, the part it is.
    pub(super) fn merge(&mut self, parts: &[Term]) -> ExprId {
        let mut out = Vec::new();
        for p in parts {
            match p {
                Term::Obj(m) if !m.is_empty() => out.extend(self.fields_of(m)),
                p => out.push(ObjPart::Spread(self.expr(p))),
            }
        }
        self.expr_node(ExprKind::Object { parts: out })
    }

    /// `__concat(parts..)`: a written list a run of elements, any other
    /// part a spread; an empty one a spread of itself.
    pub(super) fn concat(&mut self, parts: &[Term]) -> ExprId {
        let mut out = Vec::new();
        for p in parts {
            match p {
                Term::List(xs) if !xs.is_empty() => out.extend(self.elems_of(xs)),
                p => out.push(ListPart::Spread(self.expr(p))),
            }
        }
        self.expr_node(ExprKind::List { parts: out })
    }

    /// `__object(k, v, ..)`: an object with a computed key, each key
    /// written or computed, in order.
    pub(super) fn computed(&mut self, pairs: &[Term]) -> ExprId {
        let parts = pairs
            .chunks(2)
            .map(|kv| ObjPart::Computed {
                key: self.expr(&kv[0]),
                value: self.expr(&kv[1]),
            })
            .collect();
        self.expr_node(ExprKind::Object { parts })
    }

    fn fields_of(&mut self, m: &BTreeMap<String, Term>) -> Vec<ObjPart> {
        m.iter()
            .map(|(k, v)| ObjPart::Field {
                key: k.clone(),
                key_span: self.span,
                value: self.expr(v),
                pun: false,
            })
            .collect()
    }

    fn elems_of(&mut self, xs: &[Term]) -> Vec<ListPart> {
        xs.iter().map(|x| ListPart::Elem(self.expr(x))).collect()
    }
}

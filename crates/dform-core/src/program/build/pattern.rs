//! What a binding position matches (R-211): a relation's column, the left
//! of `=`. Step 3 builds it from the lowered term: `_` a hole, a variable
//! a binding (or a comparison, when it is bound: binding order decides,
//! R-10), a list a tuple, an object an object pattern, anything else the
//! term it compares with.

use super::Builder;
use crate::ast::Term;
use crate::program::node::*;

impl Builder<'_> {
    /// The lowered term `t` in a binding position.
    pub(super) fn pattern(&mut self, t: &Term) -> PatternId {
        let kind = match t {
            Term::Wildcard => PatternKind::Hole,
            Term::Var(x) => match self.pending_read(x) {
                Some(read) => PatternKind::Expr(read),
                None => PatternKind::Bind(self.var(x)),
            },
            Term::List(xs) => PatternKind::Tuple {
                elems: xs.iter().map(|x| self.pattern(x)).collect(),
                rest: None,
            },
            Term::Obj(m) => PatternKind::Object {
                fields: self
                    .in_read_order(m)
                    .into_iter()
                    .map(|(k, v)| (k.clone(), self.span, self.pattern(v)))
                    .collect(),
                rest: None,
            },
            t => PatternKind::Expr(self.expr(t)),
        };
        self.pattern_node(kind)
    }

    /// The node `id` in a binding position: `_` a hole, a variable a
    /// binding, a written list a tuple and a written object an object
    /// pattern (each part in its place), anything else the term it
    /// compares with.
    pub fn pattern_of(&mut self, id: ExprId) -> PatternId {
        let e = &self.program.exprs[id];
        let kind = match (&e.kind, e.hoisted) {
            (ExprKind::Hole, None) => PatternKind::Hole,
            (ExprKind::Var(v), None) => PatternKind::Bind(*v),
            (ExprKind::List { parts }, None)
                if parts.iter().all(|p| matches!(p, ListPart::Elem(_))) =>
            {
                let parts = parts.clone();
                let elems = parts
                    .into_iter()
                    .map(|p| match p {
                        ListPart::Elem(x) => self.pattern_of(x),
                        ListPart::Spread(_) => unreachable!("matched"),
                    })
                    .collect();
                PatternKind::Tuple { elems, rest: None }
            }
            (ExprKind::Object { parts }, None)
                if parts.iter().all(|p| matches!(p, ObjPart::Field { .. })) =>
            {
                let parts = parts.clone();
                let fields = parts
                    .into_iter()
                    .map(|p| match p {
                        ObjPart::Field { key, value, .. } => {
                            (key, self.span, self.pattern_of(value))
                        }
                        _ => unreachable!("matched"),
                    })
                    .collect();
                PatternKind::Object { fields, rest: None }
            }
            _ => PatternKind::Expr(id),
        };
        self.pattern_node(kind)
    }

    pub(super) fn pattern_node(&mut self, kind: PatternKind) -> PatternId {
        self.program.patterns.insert(Pattern {
            span: self.span,
            kind,
        })
    }
}

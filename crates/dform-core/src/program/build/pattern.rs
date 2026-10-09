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
            Term::Var(x) => PatternKind::Bind(self.var(x)),
            Term::List(xs) => PatternKind::Tuple {
                elems: xs.iter().map(|x| self.pattern(x)).collect(),
                rest: None,
            },
            Term::Obj(m) => PatternKind::Object {
                fields: m
                    .iter()
                    .map(|(k, v)| (k.clone(), self.span, self.pattern(v)))
                    .collect(),
                rest: None,
            },
            t => PatternKind::Expr(self.expr(t)),
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

//! Aggregates (docs/grammar.md "Aggregates", R-59): `n = count(x)` in a
//! body binds `n` to the fold of `x` over the body's matches, per group of
//! the head's other variables; `let n = count(x) where B` folds over one
//! group. The core applies an aggregate in a rule head (`engine::ops::find_agg`),
//! so a body binding lowers to one:
//!
//! | written                                     | lowers to                                 |
//! |---------------------------------------------|-------------------------------------------|
//! | `p(k, n) where n = count(x), B`             | `p(K, count(X)) :- B`                     |
//! | `let n = count(x) where B`                  | `let("n", count(X), r) :- B`              |
//! | `p(k, n) where n = count(x), B, n > 2`      | `__agg_N(K, count(X)) :- B`; `p(K, N) :- __agg_N(K, N), N > 2` |
//!
//! A literal that reads an aggregate's result is applied after the fold,
//! so what it reads of the body is part of the group too; two aggregates
//! in one body fold over the same body and are joined by the group.

use super::*;

/// One `n = agg(x)` of the statement being lowered.
#[derive(Clone)]
pub(super) struct Agg {
    /// The lowered variable it binds.
    pub var: String,
    /// The call, as written, and where.
    pub text: String,
    pub span: Span,
}

impl Lowerer<'_> {
    /// `n = agg(x)` (or `agg(x) = n`): the binding as `n = agg(x')`, the
    /// aggregate's reads hoisted before it. `None` when the literal is not
    /// one.
    pub(super) fn aggregate_binding(&mut self, rc: &mut Rc, n: &SyntaxNode) -> Option<L<()>> {
        let ts: Vec<SyntaxNode> = terms(n).collect();
        let ops: Vec<SyntaxKind> = tokens(n).map(|t| t.kind()).collect();
        if ts.len() != 2 || ops.as_slice() != [EQ] {
            return None;
        }
        let (name, call) = [(&ts[0], &ts[1]), (&ts[1], &ts[0])]
            .into_iter()
            .find(|(_, c)| c.kind() == CALL && self.aggregate_name(c).is_some())?;
        Some(self.aggregate_binding1(rc, name, call))
    }

    fn aggregate_binding1(&mut self, rc: &mut Rc, name: &SyntaxNode, call: &SyntaxNode) -> L<()> {
        let span = self.span(call);
        if self.nested > 0 {
            return self.error(
                span,
                "an aggregate is named at the top of a clause, not inside `not { }` or a \
                 comprehension",
            );
        }
        if Chain::of(name).is_none_or(|c| !c.is_bare()) {
            return self.error(
                self.span(name),
                format!(
                    "an aggregate binds a name: `n = {}`, and `n` is read after it",
                    call.text()
                ),
            );
        }
        let (_, agg) = self.aggregate_call(rc, call)?;
        let (named, _) = self.bind(true, |l| l.arg_node(rc, name, Pos::Content))?;
        let Term::Var(var) = named else {
            return self.error(
                self.span(name),
                format!(
                    "`{}` is already a name: an aggregate binds a new one",
                    name.text()
                ),
            );
        };
        self.aggs.push(Agg {
            var: var.clone(),
            text: call.text().to_string(),
            span,
        });
        let at = self.span(name.parent().as_ref().unwrap_or(name));
        let g = self.fold_goal(rc, at, &var, agg);
        self.built(g);
        Ok(())
    }

    /// `v = agg(x)` at `span`, `agg` the aggregate's node: its fold.
    pub(super) fn fold_goal(&mut self, rc: &Rc, span: Span, v: &str, agg: ExprId) -> GoalId {
        self.building(rc, span, |b| {
            let var = b.var(v);
            b.fold_goal(span, var, agg)
        })
    }

    /// The aggregate `n` calls, if it is one.
    pub(super) fn aggregate_name(&self, n: &SyntaxNode) -> Option<String> {
        let name = self.callee(n)?;
        crate::partition::AGGREGATES
            .contains(&name.as_str())
            .then_some(name)
    }

    /// `agg(x)` lowered: one positional argument, its reads into `out`.
    pub(super) fn aggregate_call(&mut self, rc: &mut Rc, call: &SyntaxNode) -> L<(Term, ExprId)> {
        let span = self.span(call);
        let name = self.aggregate_name(call).ok_or(Skip)?;
        let list = node(call, ARG_LIST);
        let args: Vec<SyntaxNode> = list.iter().flat_map(|l| l.children()).collect();
        let [arg] = args.as_slice() else {
            return self.error(span, format!("`{name}` aggregates one value: `{name}(x)`"));
        };
        if !is_term(arg.kind()) {
            return self.error(span, format!("`{name}` aggregates one value: `{name}(x)`"));
        }
        let (x, item) = self.bind(false, |l| l.arg_node(rc, arg, Pos::Content))?;
        self.check_aggregated(&name, std::slice::from_ref(&x), span);
        let kind = crate::program::node::aggregate_kind(&name).ok_or(Skip)?;
        let agg = self.node(span, ExprKind::Aggregate { kind, item });
        let t = Term::Func {
            name,
            args: vec![x],
        };
        Ok((t, agg))
    }

    /// The aggregates among `folds` whose item reads a name that nothing
    /// else binds (`bound`, what the statement binds, or around it): an
    /// error at each call; the names, as they lower (`check_bound` leaves
    /// them to it).
    pub(super) fn unbound_aggregates(
        &mut self,
        rc: &Rc,
        folds: &[GoalId],
        bound: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        let around: BTreeSet<String> = rc.bound_around().map(|v| source_name(v)).collect();
        // A variable as the nodes name it, and as it is written and lowered.
        let written = |n: &str| rc.vars.iter().find(|(_, low)| source_name(low) == n);
        let mut out = BTreeSet::new();
        for &f in folds {
            let GoalKind::Fold { var, agg, .. } = self.program.goals[f].kind else {
                continue;
            };
            let ExprKind::Aggregate { item, .. } = self.program.exprs[agg].kind else {
                continue;
            };
            let read = bind::term(&self.program, rc.scope, bind::Term::Expr(item));
            let missing: BTreeSet<String> = read
                .reads
                .into_iter()
                .filter(|n| !bound.contains(n) && !around.contains(n))
                .collect();
            let lowered = &self.program.vars[var].lowered;
            let Some(a) = self.aggs.iter().find(|a| &a.var == lowered).cloned() else {
                continue;
            };
            if missing.is_empty() {
                continue;
            }
            let names: Vec<String> = missing
                .iter()
                .map(|m| format!("`{}`", written(m).map_or(m.as_str(), |(src, _)| src)))
                .collect();
            self.diags.push(
                Diagnostic::error(
                    a.span,
                    format!(
                        "`{}` aggregates {}, which nothing after `where` gives values",
                        a.text,
                        names.join(", ")
                    ),
                )
                .with_help(format!(
                    "give {} values in the same clause, `{}, {} in ..`",
                    names.join(", "),
                    a.text,
                    names[0].trim_matches('`'),
                )),
            );
            out.extend(
                missing
                    .iter()
                    .filter_map(|m| written(m))
                    .map(|(_, low)| low.clone()),
            );
        }
        out
    }

    /// An aggregate bound in `clause`, a statement's that is no rule's: an
    /// error at `span`.
    pub(super) fn aggregate_in(&mut self, clause: Option<ClauseId>, span: Span) -> L<()> {
        let folds = clause.map_or(0, |c| crate::program::build::folds(&self.program, c).len());
        match self.aggs.is_empty() || folds == 0 {
            true => Ok(()),
            false => self.aggregate_outside(span),
        }
    }

    fn aggregate_outside<T>(&mut self, span: Span) -> L<T> {
        self.error(
            span,
            "an aggregate is bound in the body of a rule, a check or a `let`; bind it in a \
             `let` and read that here",
        )
    }
}

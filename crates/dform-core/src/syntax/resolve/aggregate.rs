//! Aggregates (docs/grammar.md "Aggregates", R-59): `n = count(x)` in a
//! body binds `n` to the fold of `x` over the body's matches, per group of
//! the head's other variables; `let n = count(x) where B` folds over one
//! group. The core applies an aggregate in a rule head (`ir::ops::find_agg`),
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
    pub(super) fn aggregate_binding(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        out: &mut Vec<Lit>,
    ) -> Option<L<()>> {
        let ts: Vec<SyntaxNode> = terms(n).collect();
        let ops: Vec<SyntaxKind> = tokens(n).map(|t| t.kind()).collect();
        if ts.len() != 2 || ops.as_slice() != [EQ] {
            return None;
        }
        let (name, call) = [(&ts[0], &ts[1]), (&ts[1], &ts[0])]
            .into_iter()
            .find(|(_, c)| c.kind() == CALL && self.aggregate_name(c).is_some())?;
        Some(self.aggregate_binding1(rc, name, call, out))
    }

    fn aggregate_binding1(
        &mut self,
        rc: &mut Rc,
        name: &SyntaxNode,
        call: &SyntaxNode,
        out: &mut Vec<Lit>,
    ) -> L<()> {
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
        let value = self.aggregate_call(rc, call, out)?;
        let Term::Var(var) = self.bind(true, |l| l.term(rc, name, Pos::Content, out))? else {
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
        out.push(Lit::Eq(Term::Var(var), value));
        Ok(())
    }

    /// The aggregate `n` calls, if it is one.
    pub(super) fn aggregate_name(&self, n: &SyntaxNode) -> Option<String> {
        let name = self.callee(n)?;
        crate::partition::AGGREGATES
            .contains(&name.as_str())
            .then_some(name)
    }

    /// `agg(x)` lowered: one positional argument, its reads into `out`.
    pub(super) fn aggregate_call(
        &mut self,
        rc: &mut Rc,
        call: &SyntaxNode,
        out: &mut Vec<Lit>,
    ) -> L<Term> {
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
        let x = self.bind(false, |l| l.term(rc, arg, Pos::Content, out))?;
        self.check_aggregated(&name, std::slice::from_ref(&x), span);
        Ok(Term::Func {
            name,
            args: vec![x],
        })
    }

    /// The aggregate bindings of `body`: what each binds and its call.
    fn agg_lits(&self, body: &[Lit]) -> Vec<(usize, String, Term)> {
        body.iter()
            .enumerate()
            .filter_map(|(i, l)| match l {
                Lit::Eq(Term::Var(v), t @ Term::Func { name, .. })
                    if crate::partition::AGGREGATES.contains(&name.as_str())
                        && self.aggs.iter().any(|a| &a.var == v) =>
                {
                    Some((i, v.clone(), t.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// An aggregate's value read by nothing the body binds: an error at
    /// the call (`check_bound` leaves these names to it).
    pub(super) fn unbound_aggregates(&mut self, rc: &Rc, body: &[Lit]) -> BTreeSet<String> {
        let found = self.agg_lits(body);
        if found.is_empty() {
            return BTreeSet::new();
        }
        let others: Vec<Lit> = body
            .iter()
            .enumerate()
            .filter(|(i, _)| !found.iter().any(|(j, ..)| j == i))
            .map(|(_, l)| l.clone())
            .collect();
        let mut bound = bound_vars(&others);
        bound.extend(rc.outer.iter().cloned());
        let mut out = BTreeSet::new();
        for (_, v, call) in &found {
            let mut used = BTreeSet::new();
            lit_vars(&Lit::Eq(call.clone(), call.clone()), &mut used);
            let missing: Vec<String> = used.difference(&bound).cloned().collect();
            if missing.is_empty() {
                continue;
            }
            let Some(a) = self.aggs.iter().find(|a| &a.var == v).cloned() else {
                continue;
            };
            let names: Vec<String> = missing
                .iter()
                .map(|m| {
                    rc.vars
                        .iter()
                        .find(|(_, low)| *low == m)
                        .map(|(src, _)| format!("`{src}`"))
                        .unwrap_or_else(|| format!("`{m}`"))
                })
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
            out.extend(missing);
        }
        out
    }

    /// A statement's rules with their aggregate bindings folded (see the
    /// module's table); an aggregate anywhere else is an error.
    pub(super) fn fold_aggregates(&mut self, stmts: Vec<Stmt>, span: Span) -> L<Vec<Stmt>> {
        let mut out = Vec::with_capacity(stmts.len());
        for s in stmts {
            match s {
                Stmt::Rule(r) => out.extend(self.fold_rule(r, span)),
                s if self.has_aggregate(&s) => return self.aggregate_outside(span),
                s => out.push(s),
            }
        }
        Ok(out)
    }

    /// An aggregate bound in `body`, a statement's that is no rule's: an
    /// error at `span`.
    pub(super) fn aggregate_in(&mut self, body: &[Lit], span: Span) -> L<()> {
        match self.aggs.is_empty() || self.agg_lits(body).is_empty() {
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

    fn has_aggregate(&self, s: &Stmt) -> bool {
        let body = match s {
            Stmt::Resource(r) => r.body.as_deref(),
            Stmt::Instance(i) | Stmt::Use(i) => i.body.as_deref(),
            _ => None,
        };
        body.is_some_and(|b| !self.agg_lits(b).is_empty())
    }

    /// [`Self::fold_rule1`], under `DFORM_CHECK_LOWER=1` also built as a
    /// rule item, its folds numbered from the same counter, lowered and
    /// compared (R-211 step 4).
    fn fold_rule(&mut self, r: RuleStmt, span: Span) -> Vec<Stmt> {
        if !crate::program::check::enabled() {
            return self.fold_rule1(r, span);
        }
        let results: Vec<String> = self
            .agg_lits(&r.body)
            .into_iter()
            .map(|(_, v, _)| v)
            .collect();
        let (written, counters) = (r.clone(), self.program.helpers);
        let out = self.fold_rule1(r, span);
        crate::program::check::fold(&written, &results, counters, span, &out);
        out
    }

    fn fold_rule1(&mut self, r: RuleStmt, span: Span) -> Vec<Stmt> {
        let found = self.agg_lits(&r.body);
        if found.is_empty() {
            return vec![Stmt::Rule(r)];
        }
        let results: BTreeSet<String> = found.iter().map(|(_, v, _)| v.clone()).collect();
        let reads_result = |l: &Lit| {
            let mut vs = BTreeSet::new();
            lit_vars(l, &mut vs);
            !vs.is_disjoint(&results)
        };
        let mut base = Vec::new();
        let mut post = Vec::new();
        for (i, l) in r.body.into_iter().enumerate() {
            if found.iter().any(|(j, ..)| *j == i) {
                continue;
            }
            if reads_result(&l) {
                post.push(l);
            } else {
                base.push(l);
            }
        }
        // One aggregate, its value a column of the head and read by nothing
        // else: the head applies it.
        if let [(_, v, call)] = found.as_slice()
            && post.is_empty()
            && r.head.record.is_none()
        {
            let at: Vec<usize> = r
                .head
                .args
                .iter()
                .enumerate()
                .filter(|(_, t)| matches!(t, Term::Var(x) if x == v))
                .map(|(i, _)| i)
                .collect();
            let mut elsewhere = BTreeSet::new();
            for (i, t) in r.head.args.iter().enumerate() {
                if !at.contains(&i) {
                    lit_vars(&Lit::Eq(t.clone(), t.clone()), &mut elsewhere);
                }
            }
            if let [i] = at.as_slice()
                && !elsewhere.contains(v)
            {
                let mut head = r.head;
                head.args[*i] = call.clone();
                return vec![Stmt::Rule(RuleStmt::new(head, base))];
            }
        }
        // The groups: the head's other variables and what the literals
        // after the fold read, as the body binds them.
        let bound = bound_vars(&base);
        let mut wanted = BTreeSet::new();
        for t in &r.head.args {
            lit_vars(&Lit::Eq(t.clone(), t.clone()), &mut wanted);
        }
        if let Some(rec) = &r.head.record {
            for t in rec.values() {
                lit_vars(&Lit::Eq(t.clone(), t.clone()), &mut wanted);
            }
        }
        for l in &post {
            lit_vars(l, &mut wanted);
        }
        let group: Vec<Term> = wanted
            .difference(&results)
            .filter(|v| bound.contains(*v))
            .map(|v| var(v))
            .collect();
        let mut out = Vec::new();
        let mut body = Vec::new();
        for (_, v, call) in &found {
            let pred = format!("__agg_{}", self.program.helpers.agg());
            let mut args = group.clone();
            args.push(call.clone());
            out.push(Stmt::Rule(RuleStmt::helper(
                Helper::Aggregate,
                atom_at(&pred, args, span),
                base.clone(),
            )));
            let mut args = group.clone();
            args.push(var(v));
            body.push(Lit::Pos(atom_at(&pred, args, span)));
        }
        body.extend(post);
        out.insert(0, Stmt::Rule(RuleStmt::new(r.head, body)));
        out
    }
}

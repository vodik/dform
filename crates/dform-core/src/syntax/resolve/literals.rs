//! A statement's body (R-10, R-211 steps 4, 11): each literal lowered
//! with the reads it hoists before it and its goal built of its nodes,
//! the clause put in binding order; `x.ready`, `has`, `in`, `not` and
//! a negation's helper (`__neg_N`), and a comparison with its direct
//! forms (`x = R.p`, `R.p == c`).

use super::*;

impl Lowerer<'_> {
    pub(super) fn body(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<Vec<Lit>> {
        let lits: Vec<SyntaxNode> = n.children().collect();
        self.lits(rc, &lits)
    }

    /// A statement's body (its clauses together), built once as written,
    /// checked for what binds, its clause in the order its goals hold
    /// (R-10): each after what binds what it reads, otherwise as written.
    /// A body nested in it is checked with it. The literals it returns are
    /// the resolver's own, as written.
    pub(super) fn lits(&mut self, rc: &mut Rc, lits: &[SyntaxNode]) -> L<Vec<Lit>> {
        self.lits_after(rc, lits, Vec::new())
    }

    /// [`Self::lits`] after `seed`, literals the statement holds before
    /// its clause: a let's parameters' binding (R-187), which a `not { }`
    /// helper's body takes with the other positive literals before it.
    pub(super) fn lits_after(
        &mut self,
        rc: &mut Rc,
        lits: &[SyntaxNode],
        seed: Vec<Lit>,
    ) -> L<Vec<Lit>> {
        if self.nested > 0 {
            return self.lits_as_written(rc, lits, seed);
        }
        let outer = rc.outer.clone();
        let out = self.lits_as_written(rc, lits, seed)?;
        let order = self.binding_order(rc, &outer_names(rc, &outer))?;
        if let Some(s) = &mut self.gather {
            s.order(&mut self.program, &order, &outer);
        }
        Ok(out)
    }

    /// The order the clause just built holds in (R-10,
    /// [`crate::program::bind`]), what is wrong with it said; `outer` the
    /// source names bound around it.
    pub(super) fn binding_order(&mut self, rc: &Rc, outer: &BTreeSet<String>) -> L<Vec<usize>> {
        let clause = self.gather.as_ref().and_then(|g| g.body());
        let Some(clause) = clause.filter(|_| !(self.lenient || self.core)) else {
            let n = clause.map_or(0, |c| self.program.clauses[c].goals.len());
            return Ok((0..n).collect());
        };
        let cx = crate::program::bind::Context {
            outer,
            scope: rc.scope,
        };
        let bound = crate::program::bind::order(&self.program, clause, &cx);
        if !bound.diags.is_empty() {
            self.diags.extend(bound.diags);
            return Err(Skip);
        }
        Ok(bound.order)
    }

    pub(super) fn lits_as_written(
        &mut self,
        rc: &mut Rc,
        lits: &[SyntaxNode],
        mut out: Vec<Lit>,
    ) -> L<Vec<Lit>> {
        // A statement's clause is gathered whole (R-211 step 4); a body
        // nested in a literal, with the literal.
        let whole = self.nested == 0 && self.gather.is_some() && !lits.is_empty();
        if self.nested == 0
            && let Some(s) = &mut self.gather
        {
            // The clause gathered is this body's, or none.
            s.take_clause();
            if whole {
                s.open();
            }
        }
        let mut failed = false;
        for l in lits {
            if self.lit(rc, l, &mut out).is_err() {
                failed = true;
            }
        }
        if whole {
            let span = self.span(&lits[0]);
            if let Some(s) = &mut self.gather {
                match failed {
                    true => s.failed(),
                    false => s.clause(&mut self.program, span),
                }
            }
        }
        if failed { Err(Skip) } else { Ok(out) }
    }

    /// One literal, with the reads it hoists before it, into `out`.
    pub(super) fn lit(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<()> {
        self.lit_as(rc, n, out, Self::lit1)
    }

    /// [`Self::lit`] of `n` lowered by `lower`: gathered, the goal it
    /// builds the literal's (R-211 step 7c).
    pub(super) fn lit_as(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        out: &mut Vec<Lit>,
        lower: impl FnOnce(&mut Self, &mut Rc, &SyntaxNode, &mut Vec<Lit>) -> L<()>,
    ) -> L<()> {
        let saved = std::mem::take(&mut self.after);
        let saved_goals = std::mem::take(&mut self.after_goals);
        let saved_tests = std::mem::take(&mut rc.tests);
        if let Some(s) = &mut self.gather {
            s.open();
        }
        let r = self.bind(true, |l| lower(l, rc, n, out));
        let tests = std::mem::replace(&mut rc.tests, saved_tests);
        out.append(&mut self.after);
        self.after = saved;
        let after = std::mem::replace(&mut self.after_goals, saved_goals);
        if !after.is_empty()
            && let Some(s) = &mut self.gather
            && let Some(g) = s.take_built()
        {
            let span = self.span(n);
            let kind = GoalKind::Group {
                goals: vec![g],
                after,
            };
            let g = self.goal(rc, span, kind);
            self.built(g);
        }
        match (r.is_ok(), &mut self.gather) {
            (true, Some(_)) => self.built_literal(n, &tests),
            (false, Some(s)) => s.failed(),
            _ => {}
        }
        r
    }

    /// The written literal `n`, made after the `tests`: the goal built of
    /// it gathered (R-211 step 4).
    pub(super) fn built_literal(&mut self, n: &SyntaxNode, tests: &[(Option<ExprId>, GoalId)]) {
        let span = self.span(n);
        let Some(s) = &mut self.gather else { return };
        s.literal(&mut self.program, span, tests, &self.helpers);
    }

    /// `v in PATH`, PATH with a `[_]` past its type's (R-162): `v` is
    /// each value the path reaches.
    pub(super) fn each_membership(&self, rc: &Rc, n: &SyntaxNode) -> bool {
        terms(n)
            .nth(1)
            .and_then(|t| Chain::of(&t))
            .is_some_and(|c| self.type_each(rc, c).ops.iter().any(each::is_each))
    }

    /// A membership, positive whatever `n` says: the body of the helper
    /// a negated one lowers through.
    pub(super) fn member_lit(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<()> {
        let (lit, g) = self.membership(rc, n, out)?;
        out.push(lit);
        if let Some(g) = g {
            self.built(g);
        }
        Ok(())
    }

    pub(super) fn lit1(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<()> {
        let span = self.span(n);
        match n.kind() {
            LIT_ATOM => {
                let a = terms(n).next().ok_or(Skip)?;
                let (atom, args, via) = self.atom_args(rc, &a, Pos::Content, out)?;
                self.core_read(&a, &atom)?;
                let g = self.rel_goal(rc, (&atom, via), args);
                self.built(g);
                out.push(Lit::Pos(atom));
            }
            LIT_TRUTH => self.truth_lit(rc, n, out, span)?,
            LIT_CMP => self.cmp(rc, n, out)?,
            LIT_IN => self.member_lit(rc, n, out)?,
            // `v not in PATH[_]`: no value the path reaches is `v`, a
            // helper over the membership (R-219).
            LIT_NOT_IN if self.each_membership(rc, n) => self.neg_helper(rc, n, out, span)?,
            LIT_NOT_IN => {
                let (lit, g) = self.membership(rc, n, out)?;
                out.push(negate(lit));
                if let Some(g) = g {
                    let g = self.building(rc, span, |b| b.not_one(g, Negated::NotIn));
                    self.built(g);
                }
            }
            LIT_HAS => self.has_lit(rc, n, out, span)?,
            LIT_NOT => {
                let inner = n.children().next().ok_or(Skip)?;
                self.not(rc, &inner, out, span)?;
            }
            LIT_NOT_BLOCK => {
                let b = node(n, BODY).ok_or(Skip)?;
                self.neg_helper(rc, &b, out, span)?;
            }
            k => return self.error(span, format!("unexpected {k:?} in a body")),
        }
        Ok(())
    }

    /// `x.ready`: the read with `true` in its value column, or the value
    /// compared with `true`.
    pub(super) fn truth_lit(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        out: &mut Vec<Lit>,
        span: Span,
    ) -> L<()> {
        let c = terms(n).next().and_then(|t| Chain::read(&t)).ok_or(Skip)?;
        let mark = out.len();
        let res = self.resolve(rc, &c, out)?;
        let e = match self.read_atom(&res, Term::Val(Value::Bool(true)), span) {
            Some(a) => {
                out.push(Lit::Pos(a));
                self.in_place(rc, &res, span).ok_or(Skip)?
            }
            None => {
                let local = out.split_off(mark);
                let id = self.realize_expr(rc, res, Pos::Content, (local, out), span)?;
                let t = self.lowered(id, out);
                out.push(Lit::Eq(t, Term::Val(Value::Bool(true))));
                id
            }
        };
        let g = self.goal(rc, span, GoalKind::Truth(e));
        self.built(g);
        Ok(())
    }

    /// `has r`, `has r.p`, `has x.f`: whether the resource exists, the
    /// attribute or the path into a value has one.
    pub(super) fn has_lit(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        out: &mut Vec<Lit>,
        span: Span,
    ) -> L<()> {
        let t = terms(n).next().ok_or(Skip)?;
        let c = Chain::read(&t).ok_or(Skip)?;
        if self.bare_resource(rc, &c) {
            let (typ, addr) = self.reference(rc, &c, out, span)?;
            let args = vec![self.term_of(typ), self.term_of(addr)];
            out.push(Lit::Pos(atom_at(IDENTITY, args, span)));
            let g = self.goal(rc, span, GoalKind::Has(Has::Resource { typ, addr }));
            self.built(g);
            return Ok(());
        }
        let res = self.resolve(rc, &c, out)?;
        match &res {
            Res::Ref { typ, addr, path } if path.is_empty() => {
                let args = vec![self.term_of(*typ), self.term_of(*addr)];
                let a = atom_at(IDENTITY, args, span);
                out.push(Lit::Pos(a));
                let (typ, addr) = (*typ, *addr);
                let g = self.goal(rc, span, GoalKind::Has(Has::Resource { typ, addr }));
                self.built(g);
                return Ok(());
            }
            Res::Ref { path, .. } if is_identity(path) => {
                return self.has_identity(&t, span);
            }
            _ => {}
        }
        self.has_path(rc, res, out, span)
    }

    /// `has R.p`, `has x.f`, `has R.p.q` of what `res` resolved: the read
    /// itself, or the walk to the path; a resource's attribute path marked
    /// as the test (R-106).
    pub(super) fn has_path(
        &mut self,
        rc: &mut Rc,
        res: Res,
        out: &mut Vec<Lit>,
        span: Span,
    ) -> L<()> {
        let start = out.len();
        let marked = self.has_atom(&res, span);
        let mark = self.has_mark(&res);
        // The reads the mark covers are the test's own: where the
        // schema answers it they are gone (R-106), so nothing
        // after it reads through them (R-204).
        let reads = marked.as_ref().map(|_| rc.reads.clone());
        let mut before = Vec::new();
        let test = match self.read_atom(&res, Term::Wildcard, span) {
            Some(a) => {
                out.push(Lit::Pos(a));
                // A marked read's resource is the mark's.
                let res = match mark {
                    Some(_) => self.settled(rc, res, span),
                    None => res,
                };
                Has::Read(self.in_place(rc, &res, span).ok_or(Skip)?)
            }
            // A nested path, or a field of a value: it has a value
            // when the walk to it does.
            None if matches!(res, Res::Ref { .. } | Res::Var { .. } | Res::Value { .. }) => {
                // The reads resolving it stay before the mark, or before
                // the walk when no mark holds them.
                if mark.is_none() {
                    before = self.settled_reads(rc, &res, span);
                }
                let res = self.settled(rc, res, span);
                let value = self.realize_expr(rc, res, Pos::Content, (Vec::new(), out), span)?;
                let t = self.lowered(value, out);
                let v = fresh(rc, HAS_VAR);
                out.push(Lit::Eq(var(&v), t));
                let var = self.building(rc, span, |b| b.var(&v));
                rc.inner.insert(v);
                Has::Walk { var, value }
            }
            None => {
                return self.error(
                    span,
                    "`has` takes an attribute of a resource (`has r.p`), a value name \
                     or a field of a value",
                );
            }
        };
        mark_has(out, start, marked, false);
        if let Some(reads) = reads {
            rc.reads = reads;
        }
        let mut g = self.goal(rc, span, GoalKind::Has(test));
        if let Some(mark) = mark {
            g = self.goal(rc, span, GoalKind::Marked { mark, goal: g });
        }
        let g = self.after(rc, span, before, g);
        self.built(g);
        Ok(())
    }

    /// `not L`: an atom, a read with a value, a membership; anything else
    /// through a helper relation.
    pub(super) fn not(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        out: &mut Vec<Lit>,
        span: Span,
    ) -> L<()> {
        match n.kind() {
            LIT_ATOM => {
                let a = terms(n).next().ok_or(Skip)?;
                let (atom, args, via) = self.atom_args(rc, &a, Pos::Content, out)?;
                self.core_read(&a, &atom)?;
                let g = self.rel_goal(rc, (&atom, via), args);
                let g = self.building(rc, span, |b| b.not_one(g, Negated::Literal));
                self.built(g);
                out.push(Lit::Not(atom));
                return Ok(());
            }
            LIT_IN
                if !n.descendants().any(|d| d.kind() == TUPLE) && !self.each_membership(rc, n) =>
            {
                let (lit, g) = self.membership(rc, n, out)?;
                out.push(negate(lit));
                if let Some(g) = g {
                    let g = self.building(rc, span, |b| b.not_one(g, Negated::Literal));
                    self.built(g);
                }
                return Ok(());
            }
            // `not has r` of a resource: a helper over its identity.
            LIT_HAS
                if terms(n)
                    .next()
                    .and_then(|t| Chain::read(&t))
                    .is_some_and(|c| self.bare_resource(rc, &c)) => {}
            LIT_TRUTH | LIT_HAS => {
                let t = terms(n).next().ok_or(Skip)?;
                let c = Chain::read(&t).ok_or(Skip)?;
                let value = if n.kind() == LIT_HAS {
                    Term::Wildcard
                } else {
                    Term::Val(Value::Bool(true))
                };
                let mut pre = Vec::new();
                let res = self.resolve(rc, &c, &mut pre)?;
                match &res {
                    Res::Ref { path, .. } if n.kind() == LIT_HAS && path.is_empty() => {
                        return self.neg_helper(rc, n, out, span);
                    }
                    Res::Ref { path, .. } if n.kind() == LIT_HAS && is_identity(path) => {
                        return self.has_identity(&t, span);
                    }
                    _ => {}
                }
                let marked = (n.kind() == LIT_HAS)
                    .then(|| self.has_atom(&res, span))
                    .flatten();
                if let Some(a) = self.read_atom(&res, value, span) {
                    out.extend(pre);
                    let start = out.len();
                    out.push(Lit::Not(a));
                    mark_has(out, start, marked, true);
                    let mark = (n.kind() == LIT_HAS).then(|| self.has_mark(&res)).flatten();
                    // A marked read's resource is the mark's.
                    let res = match mark {
                        Some(_) => self.settled(rc, res, span),
                        None => res,
                    };
                    let e = self.in_place(rc, &res, span).ok_or(Skip)?;
                    let kind = match n.kind() {
                        LIT_HAS => GoalKind::Has(Has::Read(e)),
                        _ => GoalKind::Truth(e),
                    };
                    let g = self.goal(rc, span, kind);
                    let mut g = self.building(rc, span, |b| b.not_one(g, Negated::Literal));
                    if let Some(mark) = mark {
                        g = self.goal(rc, span, GoalKind::Marked { mark, goal: g });
                    }
                    self.built(g);
                    return Ok(());
                }
                if marked.is_some() {
                    let start = out.len();
                    self.neg_helper(rc, n, out, span)?;
                    mark_has(out, start, marked, true);
                    let mark = self.has_mark(&res).map(|m| {
                        let (typ, addr) =
                            self.building(rc, span, |b| (b.settled(m.typ), b.settled(m.addr)));
                        HasMark { typ, addr, ..m }
                    });
                    if let (Some(mark), Some(s)) = (mark, &mut self.gather)
                        && let Some(g) = s.take_built()
                    {
                        let g = self.goal(rc, span, GoalKind::Marked { mark, goal: g });
                        self.built(g);
                    }
                    return Ok(());
                }
            }
            LIT_CMP => {
                let ts: Vec<SyntaxNode> = terms(n).collect();
                let ops: Vec<SyntaxKind> = tokens(n).map(|t| t.kind()).collect();
                if ts.len() == 2 && matches!(ops.as_slice(), [EQ | EQ2]) {
                    for (r, v) in [(&ts[0], &ts[1]), (&ts[1], &ts[0])] {
                        let Some(c) = Chain::of(r) else { continue };
                        let mut pre = Vec::new();
                        let mut rc2 = rc.clone();
                        let Ok(res) = self.probe(|l| l.resolve(&mut rc2, &c, &mut pre)) else {
                            continue;
                        };
                        if matches!(res, Res::Val(_) | Res::Var { .. } | Res::Type(_)) {
                            continue;
                        }
                        let mut vpre = Vec::new();
                        let Ok(id) = self.probe(|l| {
                            l.bind(false, |l| l.expr(&mut rc2, v, Pos::Content, &mut vpre))
                        }) else {
                            continue;
                        };
                        let value = self.lowered(id, &mut vpre);
                        if !vpre.is_empty() {
                            continue;
                        }
                        if let Some(a) = self.read_atom(&res, value, span) {
                            *rc = rc2;
                            out.extend(pre);
                            out.push(Lit::Not(a));
                            let e = self.in_place(rc, &res, span).ok_or(Skip)?;
                            let ops = vec![(CmpOp::Eq, id)];
                            let g = self.goal(rc, span, GoalKind::Compare { lhs: e, ops });
                            let g = self.building(rc, span, |b| b.not_one(g, Negated::Literal));
                            self.built(g);
                            return Ok(());
                        }
                    }
                }
            }
            _ => {}
        }
        // A body holding this one literal.
        self.neg_helper(rc, n, out, span)
    }

    /// `not { B }` (or a `not` no single literal can say): a helper
    /// `__neg_N(ȳ) :- P, B` over the variables ȳ the body so far binds,
    /// and `not __neg_N(ȳ)`.
    pub(super) fn neg_helper(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        out: &mut Vec<Lit>,
        span: Span,
    ) -> L<()> {
        let body_from = self.gather.as_ref().map(|s| s.goals());
        let inner = self.negated_body(rc, n)?;
        let outer_bound = {
            let mut b = bound_vars(out);
            b.extend(rc.bound_around().cloned());
            b
        };
        let mut used = BTreeSet::new();
        for l in &inner {
            lit_vars(l, &mut used);
        }
        let shared: Vec<String> = used.intersection(&outer_bound).cloned().collect();
        let number = self.program.helpers.neg();
        let pred = format!("__neg_{number}");
        let head = atom_at(&pred, shared.iter().map(|v| var(v)).collect(), span);
        // An aggregate's value is not the helper's: it is folded after.
        let results: BTreeSet<&String> = self.aggs.iter().map(|a| &a.var).collect();
        let reads_result = |l: &Lit| {
            let mut vs = BTreeSet::new();
            lit_vars(l, &mut vs);
            vs.iter().any(|v| results.contains(v))
        };
        if inner.iter().any(reads_result) {
            return self.error(
                span,
                "an aggregate's value is compared after the fold, not inside `not { }`",
            );
        }
        let mut body: Vec<Lit> = out
            .iter()
            .filter(|l| !matches!(l, Lit::Not(_)) && !reads_result(l))
            .cloned()
            .collect();
        body.extend(inner);
        self.helpers.push(Stmt::Rule(RuleStmt::helper(
            Helper::Negation,
            head.clone(),
            body,
        )));
        if let (Some(s), Some(from)) = (&mut self.gather, body_from) {
            s.negation(
                number,
                results.iter().map(|v| v.to_string()).collect(),
                from,
            );
            s.negated(&mut self.program, &shared, span, negated(n));
        }
        // The helper's own variables are bound in the helper.
        for v in used.difference(&outer_bound) {
            rc.inner.insert(v.clone());
        }
        out.push(Lit::Not(head));
        Ok(())
    }

    /// The body of a negation `n` (a block, a membership, a literal),
    /// lowered in a rule context of its own, its reads its own; the names
    /// it writes are the rule's (`rc`).
    fn negated_body(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<Vec<Lit>> {
        let mut inner = Vec::new();
        let mut rc2 = rc.clone();
        rc2.reads.clear();
        rc2.values.clear();
        self.nested += 1;
        let lowered = match n.kind() {
            BODY => self.body(&mut rc2, n).map(|b| inner = b),
            // `v not in PATH[_]`: the helper's body is the membership.
            LIT_NOT_IN => self.lit_as(&mut rc2, n, &mut inner, Self::member_lit),
            _ => self.lit(&mut rc2, n, &mut inner),
        };
        self.nested -= 1;
        lowered?;
        // Keep the variable table: names the helper introduced are its own.
        for (k, v) in &rc2.vars {
            rc.vars.entry(k.clone()).or_insert_with(|| v.clone());
        }
        rc.reserved.extend(rc2.reserved.iter().cloned());
        // The body is written once in the rule: its names count there.
        rc.uses = rc2.uses;
        for (k, at) in rc2.first {
            rc.first.entry(k).or_insert(at);
        }
        Ok(inner)
    }

    /// `a op b [op c]`, with the direct forms: `x = R.p` and `R.p == c`
    /// are the read itself.
    pub(super) fn cmp(&mut self, rc: &mut Rc, n: &SyntaxNode, out: &mut Vec<Lit>) -> L<()> {
        if let Some(r) = self.aggregate_binding(rc, n, out) {
            return r;
        }
        let span = self.span(n);
        let ts: Vec<SyntaxNode> = terms(n).collect();
        let ops: Vec<SyntaxKind> = tokens(n).map(|t| t.kind()).collect();
        // `P = e` with a tuple or object pattern on the left (R-58); an
        // element `P = e[i]` (or `e[i] = P`) is `member(e, i, P)` (H-9).
        let pattern = |t: &SyntaxNode| Self::is_pattern(t) || t.kind() == LIST;
        if ts.len() == 2 && ops.as_slice() == [EQ] && (pattern(&ts[0]) || pattern(&ts[1])) {
            let (p, c) = match pattern(&ts[0]) {
                true => (&ts[0], &ts[1]),
                false => (&ts[1], &ts[0]),
            };
            let element = Chain::of(c)
                .filter(|ch| matches!(ch.ops.last(), Some(Op::Index(ix, _)) if ix.len() == 1));
            if let Some(ch) = element
                && let Some(Op::Index(ix, _)) = ch.ops.last()
            {
                let ix = ix[0].clone();
                let mut list = ch.clone();
                list.ops.pop();
                let mark = out.len();
                let res = self.resolve(rc, &list, out)?;
                let local = out.split_off(mark);
                let lid = self.realize_expr(rc, res, Pos::Content, (local, out), span)?;
                let l = self.lowered(lid, out);
                let (i, iid) = self.bind(true, |x| x.arg_node(rc, &ix, Pos::Content, out))?;
                let (pat, pid) = self.pattern(rc, p, out)?;
                out.push(Lit::Pos(atom_at("member", vec![l, i, pat], span)));
                let g = self.building(rc, span, |b| {
                    let value = b.element_in_place(lid, iid);
                    let pat = b.pattern_of(pid);
                    b.bind_goal(span, pat, value)
                });
                self.built(g);
                return Ok(());
            }
            if pattern(&ts[0]) {
                return self.pattern_eq(rc, p, c, out);
            }
        }
        // An input that is a reference (`input role: iam.role`) compares
        // as the resource it names, not as its relation's row: `r == role`
        // compares addresses.
        let refs = ts.len() == 2
            && ts.iter().any(|t| {
                Chain::of(t).is_some_and(|c| {
                    c.is_bare()
                        && !rc.vars.contains_key(&c.head)
                        && self.input_ref(rc.scope, &c.head).is_some()
                })
            });
        if ts.len() == 2 && matches!(ops.as_slice(), [EQ | EQ2]) && !refs {
            for (r, v) in [(&ts[0], &ts[1]), (&ts[1], &ts[0])] {
                let Some(c) = Chain::of(r) else { continue };
                let mut rc2 = rc.clone();
                let mut pre = Vec::new();
                let Ok(res) = self.probe(|l| l.resolve(&mut rc2, &c, &mut pre)) else {
                    continue;
                };
                if matches!(res, Res::Val(_) | Res::Var { .. } | Res::Type(_)) {
                    continue;
                }
                if let Res::Ref { path, .. } = &res
                    && matches!(path.first(), Some(Seg::F(f)) if f == crate::schema::IDENTITY)
                {
                    return self.identity_read(r, self.span(r));
                }
                let mut vpre = Vec::new();
                let binding = ops[0] == EQ;
                let Ok(id) = self
                    .probe(|l| l.bind(binding, |l| l.expr(&mut rc2, v, Pos::Content, &mut vpre)))
                else {
                    continue;
                };
                let value = self.lowered(id, &mut vpre);
                if !vpre.is_empty() {
                    continue;
                }
                if let Some(a) = self.read_atom(&res, value, span) {
                    *rc = rc2;
                    out.extend(pre);
                    out.push(Lit::Pos(a));
                    let e = self.in_place(rc, &res, span).ok_or(Skip)?;
                    let g = self.building(rc, span, |b| match binding {
                        true => {
                            let pat = b.pattern_of(id);
                            b.bind_goal(span, pat, e)
                        }
                        false => {
                            let kind = GoalKind::Compare {
                                lhs: e,
                                ops: vec![(CmpOp::Eq, id)],
                            };
                            b.goal_node(span, kind)
                        }
                    });
                    self.built(g);
                    return Ok(());
                }
            }
        }
        // `r == "main"`: a reference is never a string (R-31).
        if ts.len() == 2 && matches!(ops.as_slice(), [EQ2 | NEQ]) {
            for (r, s) in [(&ts[0], &ts[1]), (&ts[1], &ts[0])] {
                if s.kind() == LITERAL
                    && tokens(s).any(|t| t.kind() == STRING)
                    && self.is_reference(rc, r)
                {
                    let text = r.text().to_string();
                    let lit = s.text().to_string();
                    let d = Diagnostic::error(
                        span,
                        format!("`{text}` is a reference and {lit} a string: they are never equal"),
                    )
                    .with_help(format!(
                        "compare with the resource: `{text} {op} {}`, or `{text} {op} T[{lit}]` \
                         (R-31)",
                        lit.trim_matches('"'),
                        op = if ops[0] == NEQ { "!=" } else { "==" },
                    ));
                    self.diags.push(d);
                    return Err(Skip);
                }
            }
        }
        // `r == main`, `r != T[e]`: a resource compares as a reference, and
        // so does the other side (R-42).
        if ts.len() == 2
            && matches!(ops.as_slice(), [EQ | EQ2 | NEQ])
            && (refs || ts.iter().any(|t| self.names_resource(rc, t)))
        {
            let mut ids = Vec::new();
            for (i, t) in ts.iter().enumerate() {
                let binding = i == 0 && ops[0] == EQ;
                ids.push(self.bind(binding, |l| l.ref_expr(rc, t, Pos::Content, out))?);
            }
            let (a, b) = (self.lowered(ids[0], out), self.lowered(ids[1], out));
            out.push(if ops[0] == NEQ {
                Lit::Neq(a, b)
            } else {
                Lit::Eq(a, b)
            });
            let pair = match ops[0] {
                NEQ => vec![(CmpOp::Ne, ids[0], ids[1])],
                _ => vec![(CmpOp::Eq, ids[0], ids[1])],
            };
            let g = self.compare_goal(rc, span, ops[0] == EQ, pair);
            self.built(g);
            return Ok(());
        }
        let mut lowered = Vec::new();
        let mut ids = Vec::new();
        for (i, t) in ts.iter().enumerate() {
            // `=` binds either side; `==`, `!=` and the orders test.
            let binding = ops.get(i.saturating_sub(1)) == Some(&EQ) && i <= 1
                || (i == 0 && ops.first() == Some(&EQ));
            let (t, id) = self.bind(binding, |l| l.arg_node(rc, t, Pos::Content, out))?;
            lowered.push(t);
            ids.push(id);
        }
        let mut pairs = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            let (a, b) = (lowered[i].clone(), lowered[i + 1].clone());
            let sign = match op {
                EQ => "=",
                EQ2 => "==",
                NEQ => "!=",
                LT => "<",
                LE => "<=",
                GT => ">",
                _ => ">=",
            };
            let (was_a, was_b) = (a.clone(), b.clone());
            let (a, b) = match crate::types::operands(sign, a, b) {
                Ok(ab) => ab,
                Err(why) => return self.error(span, why),
            };
            let ia = self.read_as(ids[i], &was_a, a.clone());
            let ib = self.read_as(ids[i + 1], &was_b, b.clone());
            let (lit, cmp) = match op {
                EQ | EQ2 => (Lit::Eq(a, b), CmpOp::Eq),
                NEQ => (Lit::Neq(a, b), CmpOp::Ne),
                LT => (Lit::Lt(a, b), CmpOp::Lt),
                LE => (Lit::Le(a, b), CmpOp::Le),
                GT => (Lit::Gt(a, b), CmpOp::Gt),
                _ => (Lit::Ge(a, b), CmpOp::Ge),
            };
            out.push(lit);
            pairs.push((cmp, ia, ib));
        }
        let g = self.compare_goal(rc, span, ops[0] == EQ, pairs);
        self.built(g);
        Ok(())
    }

    /// The goal of a comparison written at `span`, its pairs `pairs` as
    /// typed: `x = e` binds (`bind`, its first operator `=`); a chain whose
    /// middle terms each pair reads alike is one comparison, else a goal
    /// per pair.
    pub(super) fn compare_goal(
        &mut self,
        rc: &Rc,
        span: Span,
        bind: bool,
        pairs: Vec<(CmpOp, ExprId, ExprId)>,
    ) -> GoalId {
        let term = |l: &Self, id: ExprId| format!("{:?}", l.term_of(id));
        let shared = pairs
            .windows(2)
            .all(|p| term(self, p[0].2) == term(self, p[1].1));
        self.building(rc, span, |b| {
            if pairs.len() > 1 && shared {
                let lhs = pairs[0].1;
                let ops = pairs.iter().map(|(op, _, x)| (*op, *x)).collect();
                return b.goal_node(span, GoalKind::Compare { lhs, ops });
            }
            let mut goals: Vec<GoalId> = pairs
                .into_iter()
                .map(|(op, x, y)| match (op, bind) {
                    (CmpOp::Eq, true) => {
                        let pat = b.pattern_of(x);
                        b.bind_goal(span, pat, y)
                    }
                    _ => {
                        let kind = GoalKind::Compare {
                            lhs: x,
                            ops: vec![(op, y)],
                        };
                        b.goal_node(span, kind)
                    }
                })
                .collect();
            match goals.len() {
                1 => goals.pop().expect("one"),
                _ => {
                    let kind = GoalKind::Group {
                        goals,
                        after: Vec::new(),
                    };
                    b.goal_node(span, kind)
                }
            }
        })
    }

    /// Run `f` with the binding flag set to `binding`.
    pub(super) fn bind<T>(&mut self, binding: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.binding;
        self.binding = binding;
        let r = f(self);
        self.binding = saved;
        r
    }

    pub(super) fn calls<T>(&mut self, calls: Calls, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.calls, calls);
        let r = f(self);
        self.calls = saved;
        r
    }

    /// Run `f`, discarding the diagnostics it records when it fails.
    pub(super) fn probe<T>(&mut self, f: impl FnOnce(&mut Self) -> L<T>) -> L<T> {
        let n = self.diags.len();
        let helpers = self.helpers.len();
        let negs = self.program.helpers.negs;
        let r = f(self);
        if r.is_err() {
            self.diags.truncate(n);
            self.helpers.truncate(helpers);
            self.program.helpers.negs = negs;
        }
        r
    }

    /// `x in coll` at `span`, `x` the node `x` in a binding position.
    pub(super) fn member_goal(&mut self, rc: &Rc, span: Span, x: ExprId, coll: Coll) -> GoalId {
        self.building(rc, span, |b| {
            let pat = b.pattern_of(x);
            b.goal_node(span, GoalKind::Member { pat, coll })
        })
    }

    /// `x in list` at `span`: a list written as the element is a value,
    /// not a tuple (a tuple is a key and a value).
    pub(super) fn element_goal(&mut self, rc: &Rc, span: Span, x: ExprId, list: ExprId) -> GoalId {
        self.building(rc, span, |b| {
            let pat = b.element_of(x);
            let coll = Coll::Expr(list);
            b.goal_node(span, GoalKind::Member { pat, coll })
        })
    }

    /// A namespace's or a renamed type's test as the collection a
    /// membership ranges over: enumerated by a `want` after it, or a test
    /// alone.
    pub(super) fn tested(coll: Coll, enumerate: bool) -> Coll {
        match coll {
            Coll::Namespace { ns, types, typ, .. } => Coll::Namespace {
                ns,
                types,
                typ,
                enumerate,
            },
            Coll::ProviderType {
                typ, names, var, ..
            } => Coll::ProviderType {
                typ,
                names,
                var,
                enumerate,
            },
            coll => coll,
        }
    }

    /// The resource attribute `t` names, `(T, A, path)`, when it is one
    /// at a constant path (`w.spec.template.spec.containers`). Resolved on
    /// the side: the caller lowers `t` itself.
    pub(super) fn resource_list(&mut self, rc: &Rc, t: &SyntaxNode) -> Option<Elem> {
        let c = Chain::of(t).filter(|c| !c.is_bare())?;
        let (diags, helpers, negs) = (
            self.diags.len(),
            self.helpers.len(),
            self.program.helpers.negs,
        );
        let mut rc = rc.clone();
        let res = self.probe(|l| l.resolve(&mut rc, &c, &mut Vec::new()));
        self.diags.truncate(diags);
        self.helpers.truncate(helpers);
        self.program.helpers.negs = negs;
        match res {
            Ok(Res::Ref { typ, addr, path }) => {
                let list = self.path_string(&path).filter(|p| !p.is_empty())?;
                Some(Elem {
                    typ: (self.term_of(typ), typ),
                    addr: (self.term_of(addr), addr),
                    list,
                })
            }
            _ => None,
        }
    }
}

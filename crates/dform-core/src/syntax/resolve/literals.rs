//! A statement's body (R-10, R-211 steps 4, 11): each literal lowered
//! with the reads it hoists before it and its goal built of its nodes,
//! the clause put in binding order; `x.ready`, `has`, `in`, `not` and
//! a negation's helper (`__neg_N`), and a comparison with its direct
//! forms (`x = R.p`, `R.p == c`).

use super::*;

impl Lowerer<'_> {
    pub(super) fn body(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<()> {
        let lits: Vec<SyntaxNode> = n.children().collect();
        self.lits(rc, &lits)
    }

    /// A statement's body (its clauses together), built once as written,
    /// checked for what binds, its clause in the order its goals hold
    /// (R-10): each after what binds what it reads, otherwise as written.
    /// A body nested in it is checked with it.
    pub(super) fn lits(&mut self, rc: &mut Rc, lits: &[SyntaxNode]) -> L<()> {
        if self.nested > 0 {
            return self.lits_as_written(rc, lits);
        }
        self.lits_as_written(rc, lits)?;
        let order = self.binding_order(rc, &outer_names(rc, &rc.outer))?;
        if let Some(s) = &mut self.gather {
            let around = crate::program::build::Around {
                outer: &rc.outer,
                scope: rc.scope,
            };
            s.order(&mut self.program, &order, &around);
        }
        Ok(())
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

    pub(super) fn lits_as_written(&mut self, rc: &mut Rc, lits: &[SyntaxNode]) -> L<()> {
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
            if self.lit(rc, l).is_err() {
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
        if failed { Err(Skip) } else { Ok(()) }
    }

    /// One literal, its goal built of its terms' nodes.
    pub(super) fn lit(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<()> {
        self.lit_as(rc, n, Self::lit1)
    }

    /// [`Self::lit`] of `n` lowered by `lower`: gathered, the goal it
    /// builds the literal's (R-211 step 7c).
    pub(super) fn lit_as(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        lower: impl FnOnce(&mut Self, &mut Rc, &SyntaxNode) -> L<()>,
    ) -> L<()> {
        let saved_goals = std::mem::take(&mut self.after_goals);
        let saved_tests = std::mem::take(&mut rc.tests);
        if let Some(s) = &mut self.gather {
            s.open();
        }
        let r = self.bind(true, |l| lower(l, rc, n));
        let tests = std::mem::replace(&mut rc.tests, saved_tests);
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
    pub(super) fn member_lit(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<()> {
        if let Some(g) = self.membership(rc, n)? {
            self.built(g);
        }
        Ok(())
    }

    pub(super) fn lit1(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<()> {
        let span = self.span(n);
        match n.kind() {
            LIT_ATOM => {
                let a = terms(n).next().ok_or(Skip)?;
                let (atom, args, via) = self.atom_args(rc, &a, Pos::Content)?;
                self.core_read(&a, &atom)?;
                let g = self.rel_goal(rc, (&atom, via), args);
                self.built(g);
            }
            LIT_TRUTH => self.truth_lit(rc, n, span)?,
            LIT_CMP => self.cmp(rc, n)?,
            LIT_IN => self.member_lit(rc, n)?,
            // `v not in PATH[_]`: no value the path reaches is `v`, a
            // helper over the membership (R-219).
            LIT_NOT_IN if self.each_membership(rc, n) => self.neg_helper(rc, n, span)?,
            LIT_NOT_IN => {
                if let Some(g) = self.membership(rc, n)? {
                    let g = self.building(rc, span, |b| b.not_one(g, Negated::NotIn));
                    self.built(g);
                }
            }
            LIT_HAS => self.has_lit(rc, n, span)?,
            LIT_NOT => {
                let inner = n.children().next().ok_or(Skip)?;
                self.not(rc, &inner, span)?;
            }
            LIT_NOT_BLOCK => {
                let b = node(n, BODY).ok_or(Skip)?;
                self.neg_helper(rc, &b, span)?;
            }
            k => return self.error(span, format!("unexpected {k:?} in a body")),
        }
        Ok(())
    }

    /// `x.ready`: the read with `true` in its value column, or the value
    /// compared with `true`.
    pub(super) fn truth_lit(&mut self, rc: &mut Rc, n: &SyntaxNode, span: Span) -> L<()> {
        let c = terms(n).next().and_then(|t| Chain::read(&t)).ok_or(Skip)?;
        let res = self.resolve(rc, &c)?;
        let e = match Self::in_place_read(&res) {
            Some(_) => self.in_place(rc, &res, span).ok_or(Skip)?,
            None => self.realize_expr(rc, res, Pos::Content, span)?,
        };
        let g = self.goal(rc, span, GoalKind::Truth(e));
        self.built(g);
        Ok(())
    }

    /// `has r`, `has r.p`, `has x.f`: whether the resource exists, the
    /// attribute or the path into a value has one.
    pub(super) fn has_lit(&mut self, rc: &mut Rc, n: &SyntaxNode, span: Span) -> L<()> {
        let t = terms(n).next().ok_or(Skip)?;
        let c = Chain::read(&t).ok_or(Skip)?;
        if self.bare_resource(rc, &c) {
            let (typ, addr) = self.reference(rc, &c, span)?;
            let g = self.goal(rc, span, GoalKind::Has(Has::Resource { typ, addr }));
            self.built(g);
            return Ok(());
        }
        let res = self.resolve(rc, &c)?;
        match &res {
            Res::Ref { typ, addr, path } if path.is_empty() => {
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
        self.has_path(rc, res, span)
    }

    /// `has R.p`, `has x.f`, `has R.p.q` of what `res` resolved: the read
    /// itself, or the walk to the path; a resource's attribute path marked
    /// as the test (R-106).
    pub(super) fn has_path(&mut self, rc: &mut Rc, res: Res, span: Span) -> L<()> {
        let mark = self.has_mark(&res);
        // The reads the mark covers are the test's own: where the
        // schema answers it they are gone (R-106), so nothing
        // after it reads through them (R-204).
        let reads = mark.as_ref().map(|_| rc.reads.clone());
        let mut before = Vec::new();
        let test = match Self::in_place_read(&res) {
            Some(_) => {
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
                let value = self.realize_expr(rc, res, Pos::Content, span)?;
                let v = fresh(rc, HAS_VAR);
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
    pub(super) fn not(&mut self, rc: &mut Rc, n: &SyntaxNode, span: Span) -> L<()> {
        match n.kind() {
            LIT_ATOM => {
                let a = terms(n).next().ok_or(Skip)?;
                let (atom, args, via) = self.atom_args(rc, &a, Pos::Content)?;
                self.core_read(&a, &atom)?;
                let g = self.rel_goal(rc, (&atom, via), args);
                let g = self.building(rc, span, |b| b.not_one(g, Negated::Literal));
                self.built(g);
                return Ok(());
            }
            LIT_IN
                if !n.descendants().any(|d| d.kind() == TUPLE) && !self.each_membership(rc, n) =>
            {
                if let Some(g) = self.membership(rc, n)? {
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
                let res = self.resolve(rc, &c)?;
                match &res {
                    Res::Ref { path, .. } if n.kind() == LIT_HAS && path.is_empty() => {
                        return self.neg_helper(rc, n, span);
                    }
                    Res::Ref { path, .. } if n.kind() == LIT_HAS && is_identity(path) => {
                        return self.has_identity(&t, span);
                    }
                    _ => {}
                }
                let mark = (n.kind() == LIT_HAS).then(|| self.has_mark(&res)).flatten();
                if Self::in_place_read(&res).is_some() {
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
                if mark.is_some() {
                    self.neg_helper(rc, n, span)?;
                    let mark = mark.map(|m| {
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
                        let mut rc2 = rc.clone();
                        let Ok(res) = self.probe(|l| l.resolve(&mut rc2, &c)) else {
                            continue;
                        };
                        if matches!(res, Res::Val(_) | Res::Var { .. } | Res::Type(_)) {
                            continue;
                        }
                        let Ok(id) =
                            self.probe(|l| l.bind(false, |l| l.expr(&mut rc2, v, Pos::Content)))
                        else {
                            continue;
                        };
                        if self.reads(id) {
                            continue;
                        }
                        if Self::in_place_read(&res).is_some() {
                            *rc = rc2;
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
        self.neg_helper(rc, n, span)
    }

    /// `not { B }` (or a `not` no single literal can say): a helper
    /// `__neg_N(ȳ) :- P, B` over the variables ȳ its body shares with
    /// what is bound before it, and `not __neg_N(ȳ)` (`Gather::negated`).
    pub(super) fn neg_helper(&mut self, rc: &mut Rc, n: &SyntaxNode, span: Span) -> L<()> {
        let body_from = self.gather.as_ref().map(|s| s.goals());
        self.negated_body(rc, n)?;
        // An aggregate's value is not the helper's: it is folded after.
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let (Some(s), Some(from)) = (&mut self.gather, body_from) else {
            return Ok(());
        };
        let number = self.program.helpers.neg();
        if s.reads_any(&self.program, from, &results) {
            return self.error(
                span,
                "an aggregate's value is compared after the fold, not inside `not { }`",
            );
        }
        s.negation(number, results, from);
        let around = crate::program::build::Around {
            outer: &rc.outer,
            scope: rc.scope,
        };
        let own = s.negated(&mut self.program, span, negated(n), &around);
        // The helper's own variables are bound in the helper.
        rc.inner.extend(own);
        Ok(())
    }

    /// The body of a negation `n` (a block, a membership, a literal),
    /// lowered in a rule context of its own, its reads its own; the names
    /// it writes are the rule's (`rc`).
    fn negated_body(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<()> {
        let mut rc2 = rc.clone();
        rc2.reads.clear();
        rc2.values.clear();
        self.nested += 1;
        let lowered = match n.kind() {
            BODY => self.body(&mut rc2, n),
            // `v not in PATH[_]`: the helper's body is the membership.
            LIT_NOT_IN => self.lit_as(&mut rc2, n, Self::member_lit),
            _ => self.lit(&mut rc2, n),
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
        Ok(())
    }

    /// `a op b [op c]`, with the direct forms: `x = R.p` and `R.p == c`
    /// are the read itself.
    pub(super) fn cmp(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<()> {
        if let Some(r) = self.aggregate_binding(rc, n) {
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
                let res = self.resolve(rc, &list)?;
                let lid = self.realize_expr(rc, res, Pos::Content, span)?;
                let (_, iid) = self.bind(true, |x| x.arg_node(rc, &ix, Pos::Content))?;
                let (_, pid) = self.pattern(rc, p)?;
                let g = self.building(rc, span, |b| {
                    let value = b.element_in_place(lid, iid);
                    let pat = b.pattern_of(pid);
                    b.bind_goal(span, pat, value)
                });
                self.built(g);
                return Ok(());
            }
            if pattern(&ts[0]) {
                return self.pattern_eq(rc, p, c);
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
                let Ok(res) = self.probe(|l| l.resolve(&mut rc2, &c)) else {
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
                let binding = ops[0] == EQ;
                let Ok(id) = self.probe(|l| l.bind(binding, |l| l.expr(&mut rc2, v, Pos::Content)))
                else {
                    continue;
                };
                if self.reads(id) {
                    continue;
                }
                if Self::in_place_read(&res).is_some() {
                    *rc = rc2;
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
                ids.push(self.bind(binding, |l| l.ref_expr(rc, t, Pos::Content))?);
            }
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
            let (t, id) = self.bind(binding, |l| l.arg_node(rc, t, Pos::Content))?;
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
            let ia = self.read_as(ids[i], &was_a, a);
            let ib = self.read_as(ids[i + 1], &was_b, b);
            let cmp = match op {
                EQ | EQ2 => CmpOp::Eq,
                NEQ => CmpOp::Ne,
                LT => CmpOp::Lt,
                LE => CmpOp::Le,
                GT => CmpOp::Gt,
                _ => CmpOp::Ge,
            };
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
        let res = self.probe(|l| l.resolve(&mut rc, &c));
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

//! `set` through a relation (the decision of 2026-10-16): what a `set`
//! writes may be placed by a relation the program's rules define, `set
//! c.resources.requests = { .. } where pod(w, p), c in p.spec.containers`.
//! The `set` is then the `set` once per rule of the relation, the rule's
//! clause in the relation's place and its variables its own: with `pod(w,
//! p) where w in k8s.deployment, p = w.spec.template`, the `set` over `w in
//! k8s.deployment, c in w.spec.template.spec.containers`, a path the
//! lowering writes.
//!
//! A variable bound to a resource's attribute (`p = w.spec.template`)
//! where a `set` writes through it is a place: a chain on it is a path
//! into that resource, which the target writes below and the clause reads
//! through, and the literal that names it binds nothing, so a `set` below
//! the place does not read what it writes. No value holds a place: it is
//! a path the resolver knows while it lowers the statement, in the `set`'s
//! own clause as in a rule unfolded into it. A rule that reads what the
//! `set` writes (a `pod` chosen by a label the `set` writes) is the
//! partition's cycle, as for any write.
//!
//! A relation a fact gives a row places nothing, so a `set` through one
//! is refused, as is a relation whose rules place through the relation
//! itself, at the literal that does, and a rule that does not say where
//! the column is.

use super::*;

/// What a body places for a `set`: its relation literals unfolded, each
/// with the rule chosen, and its literals that name a place.
#[derive(Clone, Default)]
pub(super) struct Placing {
    unfold: Vec<Unfold>,
    /// Each literal that may name a place, and whether it must.
    places: Vec<(SyntaxNode, bool)>,
    /// The relation whose rule the body is, when it is one: there a
    /// literal that names a place must.
    through: Option<String>,
}

/// One relation literal unfolded: the rule of its relation chosen, and
/// what that rule's clause places.
#[derive(Clone)]
struct Unfold {
    atom: SyntaxNode,
    rule: RuleAt,
    inner: Placing,
}

/// A rule of the program: its statement, its file and the scope it is
/// written in.
#[derive(Clone)]
struct RuleAt {
    node: SyntaxNode,
    file: u32,
    scope: ScopeId,
}

/// A place: a resource, its type and its address as terms, and a path of
/// fields into it.
#[derive(Clone)]
pub(super) struct Place {
    typ: Term,
    addr: Term,
    path: Vec<String>,
}

/// A body's literals and the variables a `set` needs placed in it: what
/// it writes, and what each of those is placed from.
struct Needs<'a> {
    lits: &'a [SyntaxNode],
    vars: BTreeSet<String>,
    /// The variables a resource type is known for (`w in T`, a relation's
    /// reference column): each is that resource, placed already.
    typed: BTreeSet<String>,
    /// A rule's columns: a literal that binds one to a resource's
    /// attribute names a place, needed or not.
    columns: BTreeSet<String>,
}

impl Place {
    /// The place a resource's attribute is: its type and address
    /// constants or variables, its path fields.
    fn of((typ, addr): (Term, Term), path: &[Seg]) -> Option<Place> {
        let named = |t: &Term| matches!(t, Term::Val(_) | Term::Var(_));
        let path = path
            .iter()
            .map(|s| match s {
                Seg::F(f) => Some(f.clone()),
                Seg::I(_) | Seg::K(_) => None,
            })
            .collect::<Option<Vec<String>>>()?;
        (named(&typ) && named(&addr)).then_some(Place { typ, addr, path })
    }
}

impl Placing {
    /// The relation literal `lit` unfolded, when it is.
    fn unfolding(&self, lit: &SyntaxNode) -> Option<&Unfold> {
        self.unfold.iter().find(|u| u.atom == *lit)
    }

    /// Whether `lit` may name a place, and whether it must (a rule's
    /// literal that places a column the `set` needs).
    fn place(&self, lit: &SyntaxNode) -> Option<bool> {
        self.places
            .iter()
            .find(|(l, _)| l == lit)
            .map(|&(_, must)| must)
    }
}

impl Lowerer<'_> {
    /// The ways the `set` statement `n` places what it writes: one per
    /// choice of a rule for each relation literal of its clause that
    /// places a target (and, inside each rule, for its own); one with
    /// nothing unfolded when none does.
    pub(super) fn placings(
        &mut self,
        n: &SyntaxNode,
        (scope, outer): (ScopeId, &Rc),
    ) -> L<Vec<Placing>> {
        let Some(body) = node(n, BODY) else {
            return Ok(vec![Placing::default()]);
        };
        let lits: Vec<SyntaxNode> = body.children().collect();
        let block = node(n, BLOCK);
        let lines: Vec<SyntaxNode> = match &block {
            Some(b) => b.children().filter(|c| c.kind() == ASSIGN).collect(),
            None => vec![n.clone()],
        };
        let heads = lines
            .iter()
            .filter_map(|a| a.children().find(|c| c.kind() == CHAIN))
            .filter_map(|c| Chain::of(&c))
            .map(|c| c.head)
            .collect();
        let typed = self.rc(n, scope, outer).types.into_keys().collect();
        let needs = Needs::of(&lits, (heads, typed), BTreeSet::new());
        self.ways(&needs, scope, &mut Vec::new(), None)
    }

    /// The ways a body whose `needs` a `set` has places them, `through`
    /// the relations being unfolded around it.
    fn ways(
        &mut self,
        needs: &Needs,
        scope: ScopeId,
        through: &mut Vec<String>,
        rule_of: Option<&str>,
    ) -> L<Vec<Placing>> {
        let mut ways = vec![Placing {
            unfold: Vec::new(),
            places: needs.places(rule_of.is_some()),
            through: rule_of.map(str::to_string),
        }];
        for (atom, columns) in self.placing_atoms(needs, scope) {
            let alternatives = self.unfoldings(&atom, &columns, scope, through)?;
            ways = ways
                .iter()
                .flat_map(|w| {
                    alternatives.iter().map(move |u| {
                        let mut w = w.clone();
                        w.unfold.push(u.clone());
                        w
                    })
                })
                .collect();
        }
        Ok(ways)
    }

    /// The relation literals of a body that place what a `set` needs:
    /// each, and the columns at which it binds a variable needed that no
    /// other literal places; a relation the program's rules define.
    fn placing_atoms(&self, needs: &Needs, scope: ScopeId) -> Vec<(SyntaxNode, Vec<usize>)> {
        let unplaced = needs.unplaced();
        let mut out = Vec::new();
        for l in needs.lits.iter().filter(|l| l.kind() == LIT_ATOM) {
            let Some(call) = terms(l).next() else {
                continue;
            };
            let columns: Vec<usize> = atom_args(&call)
                .iter()
                .enumerate()
                .filter(|(_, a)| bare_var(a).is_some_and(|v| unplaced.contains(&v)))
                .map(|(i, _)| i)
                .collect();
            let arity = atom_args(&call).len();
            let defined = self
                .callee(&call)
                .is_some_and(|r| self.relation_decls(scope, &r, arity).is_some());
            if !columns.is_empty() && defined {
                out.push((l.clone(), columns));
            }
        }
        out
    }

    /// The statements of the relation `name` of `arity` columns a body
    /// in `scope` reads: its rules and facts, the innermost scope's that
    /// has any.
    fn relation_decls(&self, scope: ScopeId, name: &str, arity: usize) -> Option<Vec<SyntaxNode>> {
        let rule = |k: &DeclKind| *k == DeclKind::Rule { arity };
        let (at, _) = self.program.scopes.reaching(scope, name, rule)?;
        let decls = self.names(at).of(name).iter().filter(|d| rule(&d.kind));
        Some(decls.map(|d| d.node.clone()).collect())
    }

    /// The literal `atom` unfolded once per rule of its relation, each
    /// with what its clause places; `columns` the ones the `set` needs
    /// placed.
    fn unfoldings(
        &mut self,
        atom: &SyntaxNode,
        columns: &[usize],
        scope: ScopeId,
        through: &mut Vec<String>,
    ) -> L<Vec<Unfold>> {
        let span = self.span(atom);
        let call = terms(atom).next().ok_or(Skip)?;
        let rel = self.callee(&call).ok_or(Skip)?;
        if through.contains(&rel) {
            return self.error(
                span,
                format!(
                    "`set` writes through `{rel}`, and a rule of `{rel}` places what it writes \
                     through `{rel}` again: a rule places it from a resource's attribute, once"
                ),
            );
        }
        let args = self.joined_args(&call, &rel)?;
        let decls = self
            .relation_decls(scope, &rel, args.len())
            .unwrap_or_default();
        if let Some(fact) = decls.iter().find(|d| node(d, BODY).is_none()) {
            let at = self.site_of(fact);
            let d = Diagnostic::error(
                span,
                format!(
                    "`set` writes through `{rel}`, which a fact gives rows to: a fact names no \
                     resource, so it places nothing a `set` writes"
                ),
            )
            .with_help(format!(
                "give `{rel}` rules that place each row from a resource (`p = w.spec.template`), \
                 not the fact at {at}"
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        through.push(rel.clone());
        let mut out = Vec::new();
        let mut failed = false;
        for d in decls {
            match self.rule_unfoldings((atom, &args), (&rel, &d), columns, through) {
                Ok(us) => out.extend(us),
                Err(Skip) => failed = true,
            }
        }
        through.pop();
        if failed { Err(Skip) } else { Ok(out) }
    }

    /// The literal `atom` unfolded into the rule `stmt`: once per way its
    /// clause places the `columns` the `set` needs.
    fn rule_unfoldings(
        &mut self,
        (atom, args): (&SyntaxNode, &[SyntaxNode]),
        (rel, stmt): (&str, &SyntaxNode),
        columns: &[usize],
        through: &mut Vec<String>,
    ) -> L<Vec<Unfold>> {
        let rule = self.rule_at(stmt).ok_or(Skip)?;
        let head = stmt.children().find(|c| c.kind() == CALL).ok_or(Skip)?;
        let params = atom_args(&head);
        let lits: Vec<SyntaxNode> = node(stmt, BODY).iter().flat_map(|b| b.children()).collect();
        let saved = std::mem::replace(&mut self.file, rule.file);
        let r = (|| {
            let mut vars = BTreeSet::new();
            for (i, (a, p)) in args.iter().zip(&params).enumerate() {
                if bare_var(a).is_none_or(|x| x == "_") {
                    continue;
                }
                let v = self.column(p, rel)?;
                if columns.contains(&i) {
                    vars.insert(v);
                }
            }
            let all = params.iter().filter_map(bare_var).collect();
            let typed = self
                .rc(stmt, rule.scope, &Rc::default())
                .types
                .into_keys()
                .collect();
            let needs = Needs::of(&lits, (vars, typed), all);
            self.placed_columns(&needs, (rel, &head), columns)?;
            self.ways(&needs, rule.scope, through, Some(rel))
        })();
        self.file = saved;
        Ok(r?
            .into_iter()
            .map(|inner| Unfold {
                atom: atom.clone(),
                rule: rule.clone(),
                inner,
            })
            .collect())
    }

    /// The arguments of the literal `call` of `rel` a `set` writes
    /// through: each a variable (or `_`), which the rule's own joins.
    fn joined_args(&mut self, call: &SyntaxNode, rel: &str) -> L<Vec<SyntaxNode>> {
        let args = atom_args(call);
        let Some(a) = args.iter().find(|a| bare_var(a).is_none()) else {
            return Ok(args);
        };
        let d = Diagnostic::error(
            self.span(a),
            format!(
                "`set` writes through `{rel}`, which joins its rules by variables: `{}` is a \
                 value",
                a.text()
            ),
        )
        .with_help(format!(
            "write a variable in its place and compare it after `where`: `x == {}`",
            a.text()
        ));
        self.diags.push(d);
        Err(Skip)
    }

    /// The variable a rule's head writes in a column a `set` joins: one
    /// the clause binds, by name.
    fn column(&mut self, p: &SyntaxNode, rel: &str) -> L<String> {
        match bare_var(p) {
            Some(v) => Ok(v),
            None => {
                let d = Diagnostic::error(
                    self.span(p),
                    format!(
                        "`set` writes through `{rel}`, which joins its rules by variables: this \
                         rule writes `{}` in a column the `set` names",
                        p.text()
                    ),
                )
                .with_help(format!(
                    "write a variable in the head and bind it after `where`: `x = {}`",
                    p.text()
                ));
                self.diags.push(d);
                Err(Skip)
            }
        }
    }

    /// Every column of the rule `head` a `set` needs placed (`columns`)
    /// is placed by its clause: a resource (`w in T`), an element of a
    /// list (`c in p.L`), an attribute (`p = w.path`), or a relation that
    /// places it.
    fn placed_columns(
        &mut self,
        needs: &Needs,
        (rel, head): (&str, &SyntaxNode),
        columns: &[usize],
    ) -> L<()> {
        let params = atom_args(head);
        let placed = needs.placed();
        for &i in columns {
            let Some(v) = params.get(i).and_then(bare_var) else {
                continue;
            };
            let by_atom = needs.lits.iter().filter(|l| l.kind() == LIT_ATOM).any(|l| {
                terms(l).next().is_some_and(|c| {
                    atom_args(&c)
                        .iter()
                        .any(|a| bare_var(a).as_ref() == Some(&v))
                })
            });
            if placed.contains(&v) || by_atom {
                continue;
            }
            let d = Diagnostic::error(
                self.span(head),
                format!(
                    "`set` writes through `{rel}`, and this rule does not say where `{v}` is: a \
                     `set` writes what a rule places at a resource's attribute"
                ),
            )
            .with_help(format!(
                "bind `{v}` to the attribute in the rule's clause, `{v} = w.spec.template`"
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        Ok(())
    }

    /// Where the rule a `set` placed as `placing` is unfolded with is
    /// written: the first relation's.
    pub(super) fn through(&self, placing: &Placing) -> Option<Span> {
        let rule = &placing.unfold.first()?.rule;
        Some(self.span_in(rule.file, &rule.node))
    }

    /// The rule whose statement is `stmt`: its file and its scope.
    fn rule_at(&self, stmt: &SyntaxNode) -> Option<RuleAt> {
        let root = stmt.ancestors().last()?;
        let file = self.units.iter().find(|u| u.root == root)?.file;
        let scope = self
            .program
            .scopes
            .at(file, stmt.text_range().start().into())?;
        Some(RuleAt {
            node: stmt.clone(),
            file,
            scope,
        })
    }

    /// Where the statement `stmt` of any file is, as a message names it.
    fn site_of(&self, stmt: &SyntaxNode) -> String {
        let file = self.rule_at(stmt).map_or(self.file, |r| r.file);
        let span = self.span_in(file, stmt);
        crate::diag::at(span).unwrap_or_else(|| format!("`{}`", stmt.text()))
    }

    /// A body's literals, those that place first: its relation literals
    /// unfolded, then the literals that name a place, then the rest as
    /// written; whether any failed.
    pub(super) fn placed_lits(&mut self, rc: &mut Rc, lits: &[SyntaxNode]) -> bool {
        let spliced = self.splices(rc, lits);
        let (placed, named) = self.places(rc, lits);
        spliced | named | self.unplaced_lits(rc, lits, &placed)
    }

    /// The relation literals of `lits` unfolded, each spliced in; whether
    /// any failed.
    fn splices(&mut self, rc: &mut Rc, lits: &[SyntaxNode]) -> bool {
        let placing = rc.placing.clone();
        let mut failed = false;
        for u in lits.iter().filter_map(|l| placing.unfolding(l)) {
            failed |= self.splice(rc, u).is_err();
        }
        failed
    }

    /// The literals of `lits` that name a place, each made the place it
    /// names: those that do, and whether one a rule must place does not.
    fn places(&mut self, rc: &mut Rc, lits: &[SyntaxNode]) -> (Vec<SyntaxNode>, bool) {
        let placing = rc.placing.clone();
        let (mut placed, mut failed) = (Vec::new(), false);
        for (l, must) in lits.iter().filter_map(|l| Some((l, placing.place(l)?))) {
            match self.place_lit(rc, l) {
                true => placed.push(l.clone()),
                false if must => failed |= self.no_place(&placing, l),
                false => {}
            }
        }
        (placed, failed)
    }

    /// The literals of `lits` neither unfolded nor `placed`, lowered as
    /// written; whether any failed.
    fn unplaced_lits(&mut self, rc: &mut Rc, lits: &[SyntaxNode], placed: &[SyntaxNode]) -> bool {
        let placing = rc.placing.clone();
        let rest = lits
            .iter()
            .filter(|l| placing.unfolding(l).is_none() && !placed.contains(l));
        let mut failed = false;
        for l in rest {
            failed |= self.lit(rc, l).is_err();
        }
        failed
    }

    /// A rule's literal `lit`, `p = t`, where a `set` writes through `p`
    /// and `t` is no resource's attribute: the error; true.
    fn no_place(&mut self, placing: &Placing, lit: &SyntaxNode) -> bool {
        let rel = placing.through.as_deref().unwrap_or_default();
        let mut ts = terms(lit);
        let (lhs, rhs) = (ts.next(), ts.next());
        let (p, t) = (
            lhs.map(|t| t.text().to_string()).unwrap_or_default(),
            rhs.map(|t| t.text().to_string()).unwrap_or_default(),
        );
        let d = Diagnostic::error(
            self.span(lit),
            format!(
                "`set` writes through `{rel}`, and here `{p}` is `{t}`, which is no resource's \
                 attribute: a `set` writes a path into a resource"
            ),
        )
        .with_help(format!(
            "range a variable over the resources (`w in T`) and write `{p} = w.path`"
        ));
        self.diags.push(d);
        true
    }

    /// `p = w.path` where a `set` writes through `p`: `p` the place, when
    /// the chain is a resource's attribute; whether it is.
    fn place_lit(&mut self, rc: &mut Rc, lit: &SyntaxNode) -> bool {
        let ts: Vec<SyntaxNode> = terms(lit).collect();
        let (Some(var), Some(rhs)) = (ts.first().and_then(bare_var), ts.get(1)) else {
            return false;
        };
        // Probed first: a chain that is no resource's attribute is lowered
        // as written, and makes nothing here.
        let (Some(c), Some(_)) = (Chain::of(rhs), self.resource_path(rc, rhs)) else {
            return false;
        };
        let Ok(Res::Ref { typ, addr, path }) = self.resolve(rc, &c) else {
            return false;
        };
        let Some(place) = Place::of((self.term_of(typ), self.term_of(addr)), &path) else {
            return false;
        };
        let span = self.span(&ts[0]);
        rc.uses.add(&var, span);
        rc.first.entry(var.clone()).or_insert(span);
        rc.places.insert(var, place);
        true
    }

    /// The relation literal `u.atom` in `rc`'s body as the rule `u.rule`'s
    /// clause: its goals the clause's, its variables renamed apart, each
    /// column the literal names the rule's variable there (its type, the
    /// list it ranges over, the place it names, all of it).
    fn splice(&mut self, rc: &mut Rc, u: &Unfold) -> L<()> {
        let call = terms(&u.atom).next().ok_or(Skip)?;
        let head = u
            .rule
            .node
            .children()
            .find(|c| c.kind() == CALL)
            .ok_or(Skip)?;
        let mut rr = self.rc(&u.rule.node, u.rule.scope, &Rc::default());
        rr.placing = u.inner.clone();
        rr.taken = rc
            .reserved
            .iter()
            .chain(rc.vars.values())
            .chain(&rc.taken)
            .cloned()
            .collect();
        rr.reserved.extend(rr.taken.iter().cloned());
        let columns = self.columns(&call, &head);
        let lits: Vec<SyntaxNode> = node(&u.rule.node, BODY)
            .iter()
            .flat_map(|b| b.children())
            .collect();
        // A column the rule may place is the `set`'s variable only once it
        // does not: no variable holds a place.
        let may_place = |y: &str| {
            lits.iter()
                .any(|l| u.inner.place(l).is_some() && placing(l).is_some_and(|(v, ..)| v == y))
        };
        let (places, values): (Vec<_>, Vec<_>) = columns.iter().partition(|(_, y, _)| may_place(y));
        for (x, y, span) in values {
            self.join_column(rc, &mut rr, (x, y, *span));
        }
        let saved = std::mem::replace(&mut self.file, u.rule.file);
        let said = self.diags.len();
        let spliced = self.splices(&mut rr, &lits);
        let (placed, named) = self.places(&mut rr, &lits);
        for (x, y, span) in places {
            match rr.places.contains_key(y) {
                true => {
                    rc.uses.add(x, *span);
                    rc.first.entry(x.clone()).or_insert(*span);
                }
                false => self.join_column(rc, &mut rr, (x, y, *span)),
            }
        }
        let failed = spliced | named | self.unplaced_lits(&mut rr, &lits, &placed);
        self.file = saved;
        let rule_said = self.diags.split_off(said);
        self.spliced.extend(rule_said);
        let own: Vec<String> = rr
            .vars
            .iter()
            .filter(|(k, _)| !columns.iter().any(|(_, y, _)| y == *k))
            .map(|(_, v)| v.clone())
            .collect();
        rc.taken.extend(own);
        rc.reserved.extend(rr.reserved.iter().cloned());
        for (x, y, _) in &columns {
            rc.take_column(&rr, x, y);
        }
        if failed { Err(Skip) } else { Ok(()) }
    }

    /// The columns the literal `call` names by a variable: each the
    /// variable, the rule `head`'s there, and where the literal names it.
    fn columns(&self, call: &SyntaxNode, head: &SyntaxNode) -> Vec<(String, String, Span)> {
        let pairs = atom_args(call).into_iter().zip(atom_args(head));
        pairs
            .filter_map(|(a, p)| {
                let x = bare_var(&a).filter(|x| x != "_")?;
                Some((x, bare_var(&p)?, self.span(&a)))
            })
            .collect()
    }

    /// The rule's variable `y` (in `rr`) is the `set`'s `x`, written at
    /// `span`: the name it lowers to the `set`'s.
    fn join_column(&mut self, rc: &mut Rc, rr: &mut Rc, (x, y, span): (&String, &String, Span)) {
        let v = self.bind(true, |l| l.var_named(rc, x, span));
        rr.vars.insert(y.clone(), v);
    }

    /// What the walk said: what lowering a rule unfolded into a `set`
    /// said after the rest, once, and not where the rule's own lowering
    /// said it.
    pub(super) fn said(&mut self) -> Vec<Diagnostic> {
        let mut said = std::mem::take(&mut self.diags);
        for d in std::mem::take(&mut self.spliced) {
            if !said.contains(&d) {
                said.push(d);
            }
        }
        said
    }

    /// `c.p` of the place `place`: the path into its resource.
    pub(super) fn placed(&mut self, rc: &mut Rc, c: &Chain, place: &Place, span: Span) -> L<Res> {
        rc.uses.add(&c.head, span);
        let typ = self.name_node(rc, span, &place.typ);
        let addr = self.name_node(rc, span, &place.addr);
        let mut path: Vec<Seg> = place.path.iter().cloned().map(Seg::F).collect();
        path.extend(self.segs(rc, &c.ops)?);
        Ok(Res::Ref { typ, addr, path })
    }
}

impl Rc {
    /// The rule's variable `y` (in `rr`) is the `set`'s `x`: what `rr`
    /// knows of it.
    fn take_column(&mut self, rr: &Rc, x: &str, y: &str) {
        let x = x.to_string();
        if let Some(p) = rr.places.get(y) {
            self.places.insert(x.clone(), p.clone());
        }
        if let Some(t) = rr.types.get(y) {
            self.types.insert(x.clone(), t.clone());
        }
        if let Some(ns) = rr.namespaces.get(y) {
            self.namespaces.insert(x.clone(), ns.clone());
        }
        if let Some(e) = rr.elems.get(y) {
            self.elems.insert(x.clone(), e.clone());
        }
        if rr.row_refs.contains(y) {
            self.row_refs.insert(x);
        }
    }
}

impl<'a> Needs<'a> {
    /// What a `set` needs placed in `lits`: `vars`, and what a literal
    /// places one of them from (`c in p.L`, `(i, c) in p.L`, `c = p.L`
    /// need `p`).
    fn of(
        lits: &'a [SyntaxNode],
        (mut vars, typed): (BTreeSet<String>, BTreeSet<String>),
        columns: BTreeSet<String>,
    ) -> Needs<'a> {
        loop {
            let more: Vec<String> = lits
                .iter()
                .filter_map(placing)
                .filter(|(v, _, _)| vars.contains(v))
                .filter_map(|(_, from, _)| from)
                .filter(|f| !vars.contains(f))
                .collect();
            if more.is_empty() {
                return Needs {
                    lits,
                    vars,
                    typed,
                    columns,
                };
            }
            vars.extend(more);
        }
    }

    /// The variables needed that a literal of the body places, or whose
    /// resource type is known.
    fn placed(&self) -> BTreeSet<String> {
        let by_literal = self.lits.iter().filter_map(placing).map(|(v, _, _)| v);
        by_literal
            .chain(self.typed.iter().cloned())
            .filter(|v| self.vars.contains(v))
            .collect()
    }

    /// The variables needed that no literal of the body places: a
    /// relation must.
    fn unplaced(&self) -> BTreeSet<String> {
        let placed = self.placed();
        self.vars.difference(&placed).cloned().collect()
    }

    /// The literals that bind a variable needed, or a rule's column, to a
    /// chain, `p = w.spec.template`: each may name a place, and must when
    /// a rule's places one the `set` needs.
    fn places(&self, rule: bool) -> Vec<(SyntaxNode, bool)> {
        let mut out = Vec::new();
        for l in self.lits {
            let Some((v, _, true)) = placing(l) else {
                continue;
            };
            let needed = self.vars.contains(&v);
            if needed || self.columns.contains(&v) {
                out.push((l.clone(), rule && needed));
            }
        }
        out
    }
}

/// The variable a literal places, the head of the chain it places it
/// from when that is a name, and whether it binds it with `=`: `c in
/// p.L`, `(i, c) in p.L`, `w in T`, `c = p.L`.
fn placing(l: &SyntaxNode) -> Option<(String, Option<String>, bool)> {
    let mut ts = terms(l);
    let (lhs, rhs) = (ts.next()?, ts.next()?);
    let from = Chain::of(&rhs).map(|c| c.head);
    match l.kind() {
        LIT_IN => {
            let elem = match lhs.kind() {
                TUPLE => terms(&lhs).nth(1)?,
                _ => lhs,
            };
            Some((bare_var(&elem)?, from, false))
        }
        LIT_CMP if tokens(l).map(|t| t.kind()).eq([EQ]) => {
            Some((bare_var(&lhs)?, Some(from?), true))
        }
        _ => None,
    }
}

/// The name a term is when it is a bare name.
fn bare_var(t: &SyntaxNode) -> Option<String> {
    Chain::of(t).filter(Chain::is_bare).map(|c| c.head)
}

/// A call's positional arguments.
fn atom_args(call: &SyntaxNode) -> Vec<SyntaxNode> {
    node(call, ARG_LIST).iter().flat_map(terms).collect()
}

//! Rules, lets, outputs and checks (H-6, R-3, R-43, R-74, R-109): a
//! rule's head and body, a `let` (typed, literal, holding a reference),
//! a value or relation output, `deny`/`warn`/`check`; what a head may
//! write and a body read of the compiler's relations, and that every
//! name a statement writes is bound.

use super::*;

impl Lowerer<'_> {
    /// The clause of a block statement: the `where` body after its block.
    pub(super) fn clauses(&mut self, rc: &mut Rc, stmt: &SyntaxNode) -> L<Vec<Lit>> {
        let mut lits = Vec::new();
        for c in stmt.children().filter(|c| c.kind() == CLAUSE) {
            lits.extend(node(&c, BODY).ok_or(Skip)?.children());
        }
        self.lits(rc, &lits)
    }

    /// The assignments of a block, their reads appended to `reads`; with
    /// each, where its reads and the statement's helpers it made end.
    pub(super) fn fields(
        &mut self,
        rc: &mut Rc,
        block: &SyntaxNode,
        reads: &mut Vec<Lit>,
    ) -> L<Entries> {
        let mut out = Vec::new();
        let mut failed = false;
        for a in block.children().filter(|c| c.kind() == ASSIGN) {
            let r = (|| {
                let key = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
                let op = if tokens(&a).any(|t| t.kind() == PLUS_EQ) {
                    FieldOp::Add
                } else {
                    FieldOp::Assign
                };
                let (value, id) = self.entry_value(rc, &a, Pos::Value, reads)?;
                let f = FieldAssign {
                    key,
                    op,
                    value,
                    rank: self.rank_tok(&a)?,
                    span: self.span(&a),
                };
                Ok((f, id, (reads.len(), self.helpers.len())))
            })();
            match r {
                Ok(f) => out.push(f),
                Err(Skip) => failed = true,
            }
        }
        if failed { Err(Skip) } else { Ok(out) }
    }

    pub(super) fn opt_body(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<Vec<Lit>> {
        match node(n, BODY) {
            Some(b) => self.body(rc, &b),
            None => Ok(Vec::new()),
        }
    }

    /// `p(a, b) [@rank] [where B]` (a fact without `where`) built as a
    /// `rule` item (R-211 step 5): its head the relation and its
    /// arguments, the reads they hoisted the head's, its clause B's goals;
    /// `lower` writes it.
    pub(super) fn rule(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let r = self.rule_parts(n, scope, outer)?;
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let l = crate::program::build::RuleLowered {
            span: self.span(n),
            scope: self.item_scope,
            head: &r.head,
            rel: r.rel,
            args: r.args,
            rank: r.rank,
            clause: r.clause,
            body: &r.body,
            made: &self.helpers[r.made..],
            results: &results,
            ranked: &r.ranked,
        };
        let written: BTreeSet<&str> = r.written.iter().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, r.head.span, &written);
        let item = b.rule_item(l);
        gather.done(b);
        Ok(item)
    }

    /// A rule lowered: its head, its body and what binds it.
    pub(super) fn rule_parts(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
    ) -> L<RuleParts> {
        let span = self.span(n);
        let head_node = n.children().find(|c| c.kind() == CALL).ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let has_body = node(n, BODY).is_some();
        let clause = self.body_clause(n);
        let calls = match self.callee(&head_node).as_deref() {
            Some("type_refine") => Calls::Data,
            _ => Calls::Function,
        };
        // Another copy's or deployment's relation is read, never written.
        if let Some(c) = head_node.children().find_map(|c| Chain::of(&c))
            && !c.ops.is_empty()
            && (self.instance_in(scope, &c.head).is_some()
                || self.stack_in(scope, &c.head).is_some()
                || matches!(c.ops.first(), Some(Op::Index(..) | Op::Keyed(..))))
        {
            return self.error(
                span,
                format!(
                    "`{}` is another copy's relation, read after `where`; its rows are its own",
                    c.fields().join(".")
                ),
            );
        }
        let made = self.helpers.len();
        let (head, args, via) = self.calls(calls, |l| {
            l.atom_args(&mut rc, &head_node, Pos::Whole, &mut body)
        })?;
        let rel = RelRef::reached(head.pred.clone(), via, head.span);
        let args = self.rel_args(&rc, head.span, args);
        let rank = self.rank_tok(n)?;
        let mut ranked = head.clone();
        if let Some(rank) = rank {
            if head.pred != "arg" || head.args.len() != 4 || head.record.is_some() {
                return self.error(
                    span,
                    format!(
                        "a rank orders the writes to one attribute, `set r.p = v @override`: \
                         `{}` is a relation, whose rows are not ranked",
                        head.pred.rsplit("::").next().unwrap_or(&head.pred)
                    ),
                );
            }
            ranked.args.push(str_term(rank.name()));
        }
        self.core_head(&head_node, &ranked, has_body)?;
        self.check_bound(&rc, &body, &atom_terms(&ranked))?;
        Ok(RuleParts {
            clause,
            head,
            rel,
            args,
            ranked,
            rank,
            body,
            made,
            written: rc.vars.values().cloned().collect(),
        })
    }

    /// `let k = t [@rank] [where B]` (H-6, R-3): a contribution to the cell
    /// `k`, written `let(k, t, rank)` for `modules` to scope (the cell is
    /// `(let, SCOPE, k)`, read by name as `k(V)`). When `t` is a reference,
    /// `k`'s value is that reference and a dot on `k` reads through it.
    /// Built as a `let` item (R-211 step 5): its clause B's goals, its
    /// value the term's node; `lower` writes it.
    pub(super) fn let_stmt(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let span = self.span(n);
        let name = word_text(n, 1);
        if let Err(e) = self.value_type(scope, &name) {
            return self.error(span, e);
        }
        // `let NAME: T = t` (R-74): its rows agree on `T`, and a literal is
        // read as one, as in any typed position (R-31).
        let declared = node(n, TYPE_EXPR).map(|t| self.type_expr(&t));
        let typed_rows: Vec<SyntaxNode> = self
            .find_let(scope, &name)
            .map(|(_, rows)| rows)
            .unwrap_or_default()
            .iter()
            .filter_map(|r| r.parent().and_then(|l| node(&l, TYPE_EXPR)))
            .collect();
        if let (Some(here), Some(first)) = (node(n, TYPE_EXPR), typed_rows.first())
            && here.text() != first.text()
        {
            return self.error(
                self.span(&here),
                format!(
                    "`let {name}` is declared `{}` in one row and `{}` in another",
                    first.text(),
                    here.text()
                ),
            );
        }
        let ty = declared.as_ref().map(crate::types::of_expr);
        let want = match &ty {
            Some(crate::types::Ty::Ref(t)) => Want::Type(t.clone()),
            _ => Want::Nothing,
        };
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let clause = self.body_clause(n);
        let made = self.helpers.len();
        let t = terms(n).next().ok_or(Skip)?;
        let aggregate = t.kind() == CALL && self.aggregate_name(&t).is_some();
        let mut fold = None;
        let (value, id) = if aggregate {
            // `let n = count(x) where B`: one group (R-59).
            let (call, agg) = self.aggregate_call(&mut rc, &t, &mut body)?;
            let v = fresh(&mut rc, &capitalise(&name));
            self.aggs.push(aggregate::Agg {
                var: v.clone(),
                text: t.text().to_string(),
                span: self.span(&t),
            });
            body.push(Lit::Eq(var(&v), call));
            let at = self.span(&t);
            fold = Some(self.fold_goal(&rc, at, &v, agg));
            (var(&v), Some(self.var_node(&rc, at, &v)))
        } else {
            let (value, id) = self.wanting(want, |l| l.let_value(&mut rc, &t, &mut body))?;
            (value, Some(id))
        };
        let (value, id) = match (&ty, &declared) {
            (Some(ty), Some(d)) => {
                let typed = self.let_typed(scope, &name, (ty, d), &t, value.clone())?;
                let id = id.map(|id| self.retyped(&rc, id, &value, typed.clone()));
                (typed, id)
            }
            _ => (value, id),
        };
        let rank = self.rank_tok(n)?;
        let head = Atom {
            pred: crate::modules::LET.to_string(),
            args: vec![
                str_term(&name),
                value.clone(),
                str_term(rank.unwrap_or(Rank::Normal).name()),
            ],
            record: None,
            span,
        };
        self.check_bound(&rc, &body, &atom_terms(&head))?;
        // The cell's type is its reader's column, `decl NAME(NAME: T)`, as
        // R-34 types a relation: once, at the first typed row. A resource
        // type is the reference's own (`value_type`).
        let declares = matches!(&ty, Some(t) if !matches!(t, crate::types::Ty::Ref(_)))
            && typed_rows.first().and_then(|t| t.parent()).as_ref() == Some(n);
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let l = crate::program::build::LetLowered {
            name,
            ty: declared,
            rank,
            declares,
            span,
            scope: self.item_scope,
            clause,
            head: &head,
            body: &body,
            value: id,
            fold,
            made: &self.helpers[made..],
            results: &results,
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let at = self.span(&t);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, at, &written);
        let item = b.let_item(l);
        gather.done(b);
        let item = item.ok_or(Skip)?;
        Ok(item)
    }

    /// The clause of statement `n`'s body just lowered, gathered (empty
    /// when no literal is written in it); none without a body, or when
    /// nothing gathers.
    pub(super) fn body_clause(&mut self, n: &SyntaxNode) -> Option<crate::program::ClauseId> {
        self.clause_at(node(n, BODY))
    }

    /// The literal of `let NAME = LITERAL [@rank]` (R-211's first port): no
    /// type, no clause, a literal not interpolated.
    pub(super) fn literal_let(&self, n: &SyntaxNode) -> Option<SyntaxNode> {
        if n.kind() != LET
            || [TYPE_EXPR, BODY, PARAMS]
                .iter()
                .any(|&k| node(n, k).is_some())
        {
            return None;
        }
        let t = terms(n).next().filter(|t| t.kind() == LITERAL)?;
        let tok = tokens(&t).next()?;
        let interpolated = tok.kind() == STRING && !self.text && tok.text().contains("${");
        (!interpolated).then_some(t)
    }

    /// `let NAME = LITERAL [@rank]` built as a `let` item of the literal's
    /// node, read from the tree; `lower` writes it `let("NAME", v,
    /// "rank")`.
    pub(super) fn let_literal(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        t: &SyntaxNode,
    ) -> Option<ItemId> {
        self.let_item(n, scope, t).ok()
    }

    pub(super) fn let_item(&mut self, n: &SyntaxNode, scope: ScopeId, t: &SyntaxNode) -> L<ItemId> {
        let span = self.span(n);
        let name = word_text(n, 1);
        if let Err(e) = self.value_type(scope, &name) {
            return self.error(span, e);
        }
        let value = self.literal_expr(t)?;
        let rank = self.rank_tok(n)?;
        Ok(self.program.items.insert(crate::program::Item {
            span,
            scope: self.item_scope,
            kind: crate::program::ItemKind::Let {
                name,
                ty: None,
                value,
                clause: None,
                rank,
                declares: false,
            },
        }))
    }

    /// A typed `let`'s value `value` (lowered from `t`) checked against
    /// its type `ty` (declared `d`), a literal read as one (R-31, R-74),
    /// an object type's fields each as theirs (R-192).
    pub(super) fn let_typed(
        &mut self,
        scope: ScopeId,
        name: &str,
        (ty, d): (&crate::types::Ty, &crate::ast::TypeExpr),
        t: &SyntaxNode,
        value: Term,
    ) -> L<Term> {
        use crate::types::Ty;
        let span = self.span(t);
        // A reference: its type is the row's, read from its text.
        let got = match Chain::of(t) {
            Some(c) if c.is_bare() && !self.is_value(scope, &c.head) => {
                self.resource(scope, &c.head).map(|types| match ty {
                    Ty::Ref(want) if types.contains(want) => want.clone(),
                    _ => types[0].clone(),
                })
            }
            _ => match self.written_vtype(scope, t, 0) {
                Some(VType::Ref(typ)) => Some(typ),
                _ => None,
            },
        };
        match got {
            Some(typ) => {
                let r = func(
                    crate::address::REF,
                    vec![str_term(&typ), value.clone(), str_term("")],
                );
                match crate::types::mismatch(ty, &r) {
                    Some(why) => self.error(span, format!("let {name} {why}")),
                    None => Ok(value),
                }
            }
            None => crate::types::declared(d, value).or_else(|(path, why)| {
                let name = crate::types::dotted(name, &path);
                self.error(span, format!("let {name} {why}"))
            }),
        }
    }

    /// A `let`'s value: a reference is its key (a resource's address, a
    /// live object's name); anything else the term.
    pub(super) fn let_value(
        &mut self,
        rc: &mut Rc,
        t: &SyntaxNode,
        body: &mut Vec<Lit>,
    ) -> L<(Term, ExprId)> {
        if let Some(c) = Chain::of(t) {
            // A resource by its bare name.
            if c.is_bare()
                && !rc.vars.contains_key(&c.head)
                && !self.is_value(rc.scope, &c.head)
                && self.resource(rc.scope, &c.head).is_some()
            {
                let span = self.span(t);
                let (_, addr) = self.reference(rc, &c, body, span)?;
                return Ok((self.term_of(addr), addr));
            }
            let mut pre = Vec::new();
            let mut rc2 = rc.clone();
            if let Ok(res) = self.probe(|l| l.resolve(&mut rc2, &c, &mut pre)) {
                let key = match &res {
                    Res::Ref { addr, path, .. } if path.is_empty() => Some(*addr),
                    Res::World { addr, path, .. } if path.is_empty() => Some(*addr),
                    _ => None,
                };
                if let Some(k) = key {
                    *rc = rc2;
                    body.extend(pre);
                    return Ok((self.term_of(k), k));
                }
            }
        }
        self.arg_node(rc, t, Pos::Value, body)
    }

    /// `output k [: T] = t [where B]` (H-7): the declaration, when typed, and
    /// its value; a value that reads, or one with a condition, is the rule
    /// `output(k, t') :- B, reads`.
    pub(super) fn output(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let span = self.span(n);
        let name = word_text(n, 1);
        if node(n, ATTR_DECL).is_some() {
            return self.output_object(n, scope, outer);
        }
        // `output p`: the relation `p` exported (R-55), read as
        // `copy.p(..)`, `c[t].p(..)`, `stack[k=v].p(..)`.
        if node(n, TYPE_EXPR).is_none() && terms(n).next().is_none() {
            let arities: BTreeSet<usize> = self
                .chain_of(scope)
                .into_iter()
                .flat_map(|s| self.names(s).arities(&name))
                .collect();
            match arities.iter().collect::<Vec<_>>().as_slice() {
                [n] => {
                    // A column of references holds the copy's resources,
                    // and never leaves a deployment (R-204).
                    let ref_columns = self.published_refs(scope, &name, **n, span)?;
                    let rel = crate::program::node::RelRef::own(name, span);
                    let kind = crate::program::ItemKind::OutputRelation { rel, ref_columns };
                    return Ok(self.program.item(span, self.item_scope, kind));
                }
                [] => {}
                _ => {
                    return self.error(
                        span,
                        format!("output {name}: the relation {name} has several arities"),
                    );
                }
            }
        }
        // The declaration, once per scope (an output may have several rows);
        // with no type written, any.
        let ty = match node(n, TYPE_EXPR) {
            Some(t) => match self.resource_type(scope, &t) {
                Some(_) => TypeExpr::Name("addr".to_string()),
                None => self.type_expr(&t),
            },
            None => TypeExpr::Name("any".to_string()),
        };
        let addr_typed = matches!(&ty, TypeExpr::Name(n) if n == "addr");
        let declared = self.outputs.insert((scope, name.clone())).then_some(ty);
        let Some(t) = terms(n).next() else {
            let d =
                Diagnostic::error(span, format!("output {name} has no value")).with_help(format!(
                    "an output is one statement, `output {name}: T = term`; `output {name}` \
                     alone exports the relation {name}, which is declared or defined here"
                ));
            self.diags.push(d);
            return Err(Skip);
        };
        let mut rc = self.rc(n, scope, outer);
        let mut pre = self.opt_body(&mut rc, n)?;
        let clause = self.body_clause(n);
        let made = self.helpers.len();
        // An output typed by a resource type holds its address (`scoped` by
        // the module); any other output is a value, a resource in it the
        // reference (R-43).
        let (value, id) = match Chain::of(&t) {
            Some(c)
                if addr_typed
                    && c.is_bare()
                    && !rc.vars.contains_key(&c.head)
                    && self.resource(scope, &c.head).is_some() =>
            {
                (str_term(&c.head), self.str_node(self.span(&t), &c.head))
            }
            _ if addr_typed => self.arg_node(&mut rc, &t, Pos::Whole, &mut pre)?,
            _ => self.arg_node(&mut rc, &t, Pos::Value, &mut pre)?,
        };
        let head = atom_at("output", vec![str_term(&name), value], span);
        match pre.is_empty() && clause.is_none() {
            true => self.check_bound(&rc, &[], &[&head.args[1]])?,
            false => self.check_bound(&rc, &pre, &head.args.iter().collect::<Vec<_>>())?,
        }
        let parts = (name, declared, clause, id, made);
        self.output_item(n, &rc, parts, head, pre)
    }

    /// An output row's item, of what it lowered to: its name, its type
    /// where it declares the output, its clause, its value's node, made
    /// the statement's helpers from `made`.
    #[allow(clippy::type_complexity)]
    pub(super) fn output_item(
        &mut self,
        n: &SyntaxNode,
        rc: &Rc,
        (name, declared, clause, value, made): (
            String,
            Option<TypeExpr>,
            Option<crate::program::ClauseId>,
            ExprId,
            usize,
        ),
        head: Atom,
        pre: Vec<Lit>,
    ) -> L<ItemId> {
        let span = self.span(n);
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let o = crate::program::build::OutputLowered {
            name,
            ty: declared,
            span,
            scope: self.item_scope,
            clause,
            head: &head,
            body: &pre,
            value,
            made: &self.helpers[made..],
            results: &results,
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.output_item(o);
        gather.done(b);
        Ok(item)
    }

    /// `output k { f [: T] = t, g: { .. } } [where B]` (R-55): an object
    /// output by its fields, one value, typed by its fields' types (`any`
    /// where a field gives none).
    pub(super) fn output_object(
        &mut self,
        n: &SyntaxNode,
        scope: ScopeId,
        outer: &Rc,
    ) -> L<ItemId> {
        let span = self.span(n);
        let name = word_text(n, 1);
        let mut rc = self.rc(n, scope, outer);
        let mut pre = self.opt_body(&mut rc, n)?;
        let clause = self.body_clause(n);
        let made = self.helpers.len();
        let (value, id, ty) = self.output_fields(&mut rc, n, &mut pre)?;
        let declared = self.outputs.insert((scope, name.clone())).then_some(ty);
        let head = atom_at("output", vec![str_term(&name), value], span);
        self.check_bound(&rc, &pre, &head.args.iter().collect::<Vec<_>>())?;
        let parts = (name, declared, clause, id, made);
        self.output_item(n, &rc, parts, head, pre)
    }

    /// An object output's fields: the object term and its type.
    pub(super) fn output_fields(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pre: &mut Vec<Lit>,
    ) -> L<(Term, ExprId, TypeExpr)> {
        let mut value = BTreeMap::new();
        let mut parts = Vec::new();
        let mut types = Vec::new();
        for a in n.children().filter(|c| c.kind() == ATTR_DECL) {
            let span = self.span(&a);
            let field = self.block_path(&node(&a, BLOCK_PATH).ok_or(Skip)?)?;
            if field.contains('.') || field.contains('[') || value.contains_key(&field) {
                return self.error(
                    span,
                    format!("field {field}: a field is a name, given once"),
                );
            }
            let (v, id, t) = if node(&a, ATTR_DECL).is_some() {
                self.output_fields(rc, &a, pre)?
            } else {
                let Some(t) = terms(&a).next() else {
                    return self.error(span, format!("field {field} has no value: `{field} = t`"));
                };
                let (v, id) = self.arg_node(rc, &t, Pos::Value, pre)?;
                let ty = node(&a, TYPE_EXPR)
                    .map(|t| self.type_expr(&t))
                    .unwrap_or_else(|| TypeExpr::Name("any".into()));
                (v, id, ty)
            };
            types.push((field.clone(), t));
            parts.push(ObjPart::Field {
                key: field.clone(),
                key_span: span,
                value: id,
                pun: false,
            });
            value.insert(field, v);
        }
        let id = self.node(self.span(n), ExprKind::Object { parts });
        Ok((Term::Obj(value), id, TypeExpr::Object(types)))
    }

    /// A core relation written where a surface form says it (H-15): an
    /// error that prints the surface form. The core stays writable where
    /// nothing else reaches: a variable type or path, a read of the
    /// contributions before they merge (`arg` in a body).
    pub(super) fn core_head(&mut self, n: &SyntaxNode, head: &Atom, has_body: bool) -> L<()> {
        if self.lenient || self.any_type || self.text || self.core {
            return Ok(());
        }
        let a = arg_texts(n);
        let s = |i: usize| match head.args.get(i) {
            Some(Term::Val(Value::Str(s))) => Some(s.clone()),
            _ => None,
        };
        let cond = if has_body { " where .." } else { "" };
        let surface = match (head.pred.as_str(), head.args.len()) {
            ("want", 2) if s(0).is_some() => Some(format!(
                "a resource is declared by a block: `resource {} {} {{ .. }}`",
                s(0).unwrap(),
                a[1]
            )),
            ("arg" | "arg_add", 4 | 5) if s(0).is_some() && s(2).is_some() => {
                let op = if head.pred == "arg" { "=" } else { "+=" };
                let (t, p) = (s(0).unwrap(), s(2).unwrap());
                let target = match t.as_str() {
                    crate::modules::INPUT if s(1).as_deref() == Some("") => p,
                    crate::modules::INPUT => format!("{}.{p}", s(1).unwrap_or_default()),
                    _ => format!("{t}[{}]{}", a[1], crate::address::path_suffix(&p)),
                };
                Some(format!("`set {target} {op} {}{cond}`", a[3]))
            }
            ("output", 2) if s(0).is_some() => {
                Some(format!("`output {} = {}{cond}`", s(0).unwrap(), a[1]))
            }
            ("input", 2) if s(0).is_some() && has_body => {
                Some(format!("`set {} = {}{cond}`", s(0).unwrap(), a[1]))
            }
            ("input", 2) if s(0).is_some() => Some(format!(
                "a default, `input {} .. = {}`, or `--set {}=..`",
                s(0).unwrap(),
                a[1],
                s(0).unwrap()
            )),
            ("deny" | "warn", 1 | 2) if s(0).is_some() => Some(format!(
                "`{} {}{}{cond}`",
                head.pred,
                a[0],
                a.get(1)
                    .filter(|o| !o.is_empty())
                    .map(|o| format!(" {o}"))
                    .unwrap_or_default()
            )),
            _ => None,
        };
        let Some(surface) = surface else {
            return Ok(());
        };
        let d = Diagnostic::error(
            head.span,
            format!(
                "`{}` is the core's spelling of a surface form",
                n.text().to_string().trim()
            ),
        )
        .with_help(format!("write {surface} (H-15)"));
        self.diags.push(d);
        Err(Skip)
    }

    /// A body's read of a core relation a surface form says (H-15), of
    /// `member` (H-9), or of `deny`/`warn` (H-8).
    pub(super) fn core_read(&mut self, n: &SyntaxNode, atom: &Atom) -> L<()> {
        if self.lenient || self.any_type || self.text || self.core {
            return Ok(());
        }
        // A relation read through a copy or a deployment (R-55) is the
        // compiler's lowering, not a core relation written.
        if self.callee(n).as_deref() != Some(atom.pred.as_str()) {
            return Ok(());
        }
        let a = arg_texts(n);
        let s = |i: usize| match atom.args.get(i) {
            Some(Term::Val(Value::Str(s))) => Some(s.clone()),
            _ => None,
        };
        let (msg, help) = match (atom.pred.as_str(), atom.args.len()) {
            ("deny" | "warn", _) => (
                format!("a rule reads `{}`", atom.pred),
                "a deny or a warn is checked after evaluation: no rule may read deny/2 or \
                 warn/2 (H-8); read what the deny reads instead"
                    .to_string(),
            ),
            ("member", 2) => (
                "`member` is what `in` lowers to".to_string(),
                format!("write `{} in {}` (H-9)", a[1], a[0]),
            ),
            ("member", 3) => (
                "`member` is what an index lowers to".to_string(),
                format!("write `{} = {}[{}]` (H-9)", a[2], a[0], a[1]),
            ),
            ("want", 2) if s(0).is_some() => (
                format!("`{}` is the core's spelling of `in`", n.text()),
                format!("write `{} in {}` (H-15)", a[1], s(0).unwrap()),
            ),
            ("attr", 4) if s(0).is_some() && s(2).is_some() => (
                format!("`{}` is the core's spelling of a read", n.text()),
                format!(
                    "write `{} = {}[{}]{}` (H-15)",
                    a[3],
                    s(0).unwrap(),
                    a[1],
                    crate::address::path_suffix(&s(2).unwrap())
                ),
            ),
            ("output", 3) if s(0).is_some() && s(1).is_some() => (
                format!("`{}` is the core's spelling of a read", n.text()),
                format!(
                    "write `{} = {}.{}` (H-15)",
                    a[2],
                    s(0).unwrap(),
                    s(1).unwrap()
                ),
            ),
            ("cloud_attr", 4) if s(0).is_some() && s(2).is_some() => (
                format!("`{}` is the core's spelling of a read", n.text()),
                format!(
                    "write `{} = world.{}[{}]{}` (H-15)",
                    a[3],
                    s(0).unwrap(),
                    a[1],
                    crate::address::path_suffix(&s(2).unwrap())
                ),
            ),
            ("cloud_exists", 2) if s(0).is_some() => (
                format!("`{}` is the core's spelling of `in`", n.text()),
                format!("write `{} in world.{}` (H-15)", a[1], s(0).unwrap()),
            ),
            _ => return Ok(()),
        };
        let d = Diagnostic::error(atom.span, msg).with_help(help);
        self.diags.push(d);
        Err(Skip)
    }

    /// `deny "m" {o} where B`, `warn ...`.
    /// `deny "m" [{ .. }] [where B]`, `warn ..` built as a `Check` item
    /// (R-211 step 5): its message and detail the terms' nodes, its clause
    /// B's goals; `lower` writes `deny(m[, detail])`.
    pub(super) fn check(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let span = self.span(n);
        let kw = tokens(n).next().ok_or(Skip)?;
        let msg = tokens(n).find(|t| t.kind() == STRING).ok_or(Skip)?;
        let mut rc = self.rc(n, scope, outer);
        let mut body = self.opt_body(&mut rc, n)?;
        let clause = self.body_clause(n);
        let made = self.helpers.len();
        // The message is a string like any other: `${e}` reads the body's
        // variables (H-13).
        let message = self.string_expr(&mut rc, &msg, &mut body)?;
        let mut args = vec![self.lowered(message, &mut body)];
        let made_mid = self.helpers.len();
        let mut detail = None;
        if let Some(o) = node(n, OBJECT) {
            let (t, id) = self.arg_node(&mut rc, &o, Pos::Whole, &mut body)?;
            args.push(t);
            detail = Some(id);
        }
        let head = Atom {
            pred: kw.text().to_string(),
            args,
            record: None,
            span,
        };
        self.check_bound(&rc, &body, &atom_terms(&head))?;
        let kind = match kw.kind() {
            WARN_KW => crate::program::node::CheckKind::Warn,
            _ => crate::program::node::CheckKind::Deny,
        };
        let results: Vec<String> = self.aggs.iter().map(|a| a.var.clone()).collect();
        let c = crate::program::build::CheckLowered {
            kind,
            span,
            scope: self.item_scope,
            clause,
            head: &head,
            body: &body,
            message,
            detail,
            made: (&self.helpers[made..made_mid], &self.helpers[made_mid..]),
            results: &results,
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.check_item(c);
        gather.done(b);
        Ok(item)
    }

    /// Every variable written in the statement is bound: by a relation
    /// read, an equality, or an enclosing `for`/`when`. A name that is none
    /// of these was meant as a string.
    pub(super) fn check_bound(&mut self, rc: &Rc, body: &[Lit], heads: &[&Term]) -> L<()> {
        let unbound = self.unbound_aggregates(rc, body);
        let mut failed = !unbound.is_empty();
        let mut bound = bound_vars(body);
        // A comprehension binds its own variables, wherever it stands.
        let holder: Vec<Lit> = heads
            .iter()
            .map(|t| Lit::Neq((*t).clone(), (*t).clone()))
            .collect();
        bound.extend(bound_vars(&holder));
        bound.extend(rc.bound_around().cloned());
        for (src, low) in &rc.vars {
            if rc.outer.contains(low)
                || rc.inner.contains(low)
                || (bound.contains(low) && rc.binders.contains(src))
                || unbound.contains(low)
            {
                continue;
            }
            let Some(span) = rc.first.get(src).copied() else {
                continue;
            };
            failed = true;
            let near = self.scope_names(rc);
            let d = unknown_name(
                span,
                src,
                &format!("\"{src}\""),
                true,
                near.iter().map(String::as_str),
            );
            self.diags.push(d);
        }
        if failed {
            Err(Skip)
        } else {
            self.singletons(rc)
        }
    }
}

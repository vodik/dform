//! What a statement writes (R-3, R-104, R-126): a resource block and its
//! entries, a resource given as a value, `set` and `set .. from`, each
//! target's cell (an attribute, a list element by its key, an input) and
//! the block that owns it.

use super::*;

impl Lowerer<'_> {
    /// `resource T n { f = t ... } where B`, `T` a provider's type or one
    /// the program declares. Built as a `Resource` item (R-211 step 5):
    /// its header, each entry's value and the reads it hoisted, the clause
    /// B's goals; `lower` writes the statement.
    pub(super) fn block_stmt(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        use crate::program::build::{BodyLowered, EntryLowered, HeaderLowered};
        let span = self.span(n);
        let block = node(n, BLOCK);
        let header = self.header_token(n).ok_or(Skip)?;
        // Rows are a component's relation inputs (R-55); a type's
        // resource has attributes.
        if let Some(row) = block
            .iter()
            .flat_map(|b| b.children())
            .find(|c| matches!(c.kind(), RULE | FACT | INPUT_RELATION))
        {
            let typ = dotted_text(n, 1);
            return self.error(
                self.span(&row),
                format!(
                    "{typ} is no component, so its resource takes no rows: a row gives a \
                     relation a component takes (`input p`)"
                ),
            );
        }
        let mut rc = self.rc(n, scope, outer);
        self.clauses(&mut rc, n)?;
        let clause = self.gather.as_mut().and_then(|g| g.take_clause());
        let made = self.helpers.len();
        let (fields, ends, value) = match &block {
            Some(block) => {
                let fields = self.wanting(Want::Schema, |l| l.fields(&mut rc, block))?;
                let (fields, ends): (Vec<_>, Vec<_>) = fields
                    .into_iter()
                    .map(|(f, id, end)| ((f, id), end))
                    .unzip();
                (fields, ends, None)
            }
            None => {
                let (_, value, fields) = self.value_body(&mut rc, n)?;
                let end = self.helpers.len();
                let fields = fields.into_iter().map(|f| (f, value)).collect();
                (fields, vec![end], Some(value))
            }
        };
        let (fields, ids): (Vec<FieldAssign>, Vec<ExprId>) = fields.into_iter().unzip();
        let named_at = self.helpers.len();
        // The header: a string with holes is bound last, by `format`; a
        // bare name is the literal name, always (R-76). The name is one
        // segment of the address, quoted when it holds a dot (R-112).
        let interp = header.kind() == STRING && has_hole(header.text());
        let mut named = None;
        if interp {
            let id = self.name_from_clause(&mut rc, &header)?;
            named = Some(id);
        } else if header.kind() == STRING {
            self.string(&header)?;
        } else {
            let text = header.text();
            let bound = self.binds(&rc, clause, &[]);
            match rc.vars.get(text) {
                Some(v) if bound.contains(&source_name(v)) || rc.bound_around().any(|b| b == v) => {
                    let d = Diagnostic::error(
                        self.span_of(header.text_range()),
                        format!(
                            "`{text}` is bound by the clause, but a bare header name is the \
                             resource's literal name: a name from the clause is a string, \
                             `\"${{{text}}}\"`"
                        ),
                    )
                    .with_help(format!(
                        "write `resource {} \"${{{text}}}\" {{ .. }}` for the resource the \
                         clause names, or `\"{text}\"` quoted for the one named \"{text}\"",
                        dotted_text(n, 1)
                    ));
                    self.diags.push(d);
                    return Err(Skip);
                }
                _ if text == "_" => {
                    return self.error(
                        self.span_of(header.text_range()),
                        "a resource is written by its name: `_` names nothing; name it, or \
                         name it from the clause (`resource T \"${n}\" { .. } where p(n)`)",
                    );
                }
                _ => {}
            }
        }
        let values: Vec<bind::Term> = ids
            .iter()
            .copied()
            .chain(named)
            .map(bind::Term::Expr)
            .collect();
        self.check_bound(&rc, clause, &values)?;
        let rank = self.rank_tok(n)?;
        self.aggregate_in(clause, span)?;
        let typ = dotted_text(n, 1);
        let literal =
            (header.kind() == STRING && !interp).then(|| self.string(&header).unwrap_or_default());
        // Each entry's helpers, from where the one before ended.
        let starts = std::iter::once(made).chain(ends.iter().copied());
        let spans: Vec<std::ops::Range<usize>> =
            starts.zip(&ends).map(|(h0, &h1)| h0..h1).collect();
        let body_lowered = match value {
            Some(v) => BodyLowered::Value {
                value: v,
                made: &self.helpers[spans[0].clone()],
            },
            None => BodyLowered::Block(
                fields
                    .iter()
                    .zip(&ids)
                    .zip(&spans)
                    .map(|((f, id), h)| EntryLowered {
                        path: &f.key,
                        op: f.op,
                        value: *id,
                        rank: f.rank,
                        span: f.span,
                        made: &self.helpers[h.clone()],
                    })
                    .collect(),
            ),
        };
        let header_lowered = if let Some(value) = named {
            HeaderLowered::Interp {
                value,
                made: &self.helpers[named_at..],
            }
        } else if let Some(text) = literal {
            HeaderLowered::Literal(text)
        } else {
            HeaderLowered::Bare(header.text())
        };
        let r = crate::program::build::ResourceLowered {
            span,
            scope: self.item_scope,
            typ: crate::program::node::TypeRef { name: typ, span },
            name: header_lowered,
            rank,
            clause,
            body: body_lowered,
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.resource_item(r);
        gather.done(b);
        Ok(item)
    }

    /// `resource T N = VALUE` (R-126): the body is a value of the type, a
    /// document. An object written out is the block of its entries, one
    /// per key, checked as a block's are; any other value is one
    /// contribution at the root, an entry per key of the object it is when
    /// the rule runs (`transform::resource_to_stmts`). The value, and its
    /// entries.
    pub(super) fn value_body(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
    ) -> L<(Term, ExprId, Vec<FieldAssign>)> {
        let t = terms(n).next().ok_or(Skip)?;
        let span = self.span(&t);
        let (value, id) = self.wanting(Want::Schema, |l| l.arg_node(rc, &t, Pos::Value))?;
        match crate::program::lower::value_entries(value.clone(), span) {
            Some(entries) => Ok((value, id, entries)),
            None => self.error(
                span,
                format!(
                    "`resource T NAME = VALUE` takes an object, a value of the type: not `{}`",
                    t.text().to_string().trim()
                ),
            ),
        }
    }

    /// `set from DOC [@rank] [where B]` (R-38): every leaf of the document
    /// a contribution to the input at its path, `arg(input, "", P, V, Rank)
    /// :- B, reads, table.FORMAT.set(Path, At, P, V)`, which
    /// `tables::expand_set_from` makes one rule per input path, and a deny
    /// for a leaf at a path that is no input's.
    pub(super) fn set_from(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        let span = self.span(n);
        let mut rc = self.rc(n, scope, outer);
        self.opt_body(&mut rc, n)?;
        let clause = self.body_clause(n);
        let rank = self.rank_tok(n)?;
        let (path, value) = (fresh(&mut rc, "Path"), fresh(&mut rc, "Value"));
        let cols = [("path", "string"), ("value", "any")]
            .map(|(name, ty)| BindArg {
                input: false,
                name: name.into(),
                ty: Some(TypeExpr::Name(ty.into())),
            })
            .to_vec();
        let made = self.helpers.len();
        let (externs, rows) = self.doc_source(
            &mut rc,
            n,
            crate::tables::SET_DOC,
            cols,
            vec![var(&path), var(&value)],
        )?;
        self.check_bound(&rc, clause, &[])?;
        let s = crate::program::build::SetFromLowered {
            span,
            scope: self.item_scope,
            clause,
            rank,
            path: &path,
            value: &value,
            externs,
            rows,
            made: &self.helpers[made..],
        };
        let written: BTreeSet<&str> = rc.vars.values().map(String::as_str).collect();
        let written = |v: &str| written.contains(v);
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.set_from_item(s);
        gather.done(b);
        Ok(item)
    }

    /// `set chain (=|+=) t [@rank] [where B]` (H-5): a contribution to a
    /// resource's attribute (`arg(T, A, p, t)`) or an input (the stack's
    /// own, a field of an object one, a used module's, a copy's), normal
    /// unless ranked (R-38). `set { chain = t .. } [@rank] [where B]` is
    /// several under one clause and rank; `set from DOC [@rank] [where B]`
    /// a document's leaves, each to the input at its path. Built as a
    /// `Set` item (R-211 step 5): each line's target, its value and the
    /// reads they hoisted, the clause B's goals; `lower` writes each
    /// line's `arg`.
    pub(super) fn set(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> L<ItemId> {
        if tokens(n).nth(1).is_some_and(|t| t.text() == "from")
            && !tokens(n).any(|t| matches!(t.kind(), EQ | PLUS_EQ))
        {
            return self.set_from(n, scope, outer);
        }
        let span = self.span(n);
        let mut rc = self.rc(n, scope, outer);
        self.opt_body(&mut rc, n)?;
        let clause = self.body_clause(n);
        let rank = self.rank_tok(n)?;
        let block = node(n, BLOCK);
        let lines: Vec<SyntaxNode> = match &block {
            Some(b) => b.children().filter(|c| c.kind() == ASSIGN).collect(),
            None => vec![n.clone()],
        };
        let mut writes = Vec::new();
        let mut failed = false;
        for a in &lines {
            let r = (|| {
                let lhs = a.children().find(|c| c.kind() == CHAIN).ok_or(Skip)?;
                let rhs = terms(a).find(|t| *t != lhs).ok_or(Skip)?;
                let add = tokens(a).any(|t| t.kind() == PLUS_EQ);
                let rank = match block {
                    Some(_) => self.rank_tok(a)?.or(rank),
                    None => rank,
                };
                let span = self.span(a);
                let mut rc = rc.clone();
                let line = (&lhs, &rhs, add, rank, span);
                self.contribution(&mut rc, line, clause)
            })();
            match r {
                Ok(w) => writes.push(w),
                Err(Skip) if block.is_some() => failed = true,
                Err(Skip) => return Err(Skip),
            }
        }
        if failed {
            return Err(Skip);
        }
        let written: BTreeSet<&str> = writes
            .iter()
            .flat_map(|w| w.written.iter().map(String::as_str))
            .collect();
        let written = |v: &str| written.contains(v);
        let s = crate::program::build::SetLowered {
            span,
            scope: self.item_scope,
            clause,
            writes: writes.iter().map(|w| w.lowered(&self.helpers)).collect(),
        };
        let gather = self
            .gather
            .as_mut()
            .expect("a ported statement is gathered");
        let mut b = gather.builder(&mut self.program, span, &written);
        let item = b.set_item(s);
        gather.done(b);
        Ok(item)
    }

    /// One contribution of a `set`: `lhs (=|+=) rhs [@rank]` at `span`,
    /// under the statement's clause `clause`, when one is written.
    pub(super) fn contribution(
        &mut self,
        rc: &mut Rc,
        (lhs, rhs, add, rank, span): (&SyntaxNode, &SyntaxNode, bool, Option<Rank>, Span),
        clause: Option<ClauseId>,
    ) -> L<Contribution> {
        let has_body = clause.is_some();
        let c = Chain::of(lhs).ok_or(Skip)?;
        if add && rank.is_some() {
            return self.error(span, "a rank applies to `=`, not `+=`");
        }
        let made = self.helpers.len();
        let target = self.set_target(rc, &c, span)?;
        match &target {
            Target::Element(_, _, list, _, _, block) => {
                if add {
                    return self.error(
                        span,
                        "`+=` adds to a list or a set; an element of a keyed list is written \
                         with `=`",
                    );
                }
                if let Some(block) = block
                    && !has_body
                {
                    let d = Diagnostic::error(
                        span,
                        format!(
                            "`set {}` with no condition is an entry of `{block}`, declared in \
                             the same scope",
                            lhs.text()
                        ),
                    )
                    .with_help(format!("write the element in the block's `{list}`"));
                    self.diags.push(d);
                    return Err(Skip);
                }
            }
            Target::Input(k) => {
                // A stack input: `input(k, t)`, as `--set k=t` gives it.
                if !has_body {
                    let d = Diagnostic::error(
                        span,
                        format!("`set {k}` at the top of the program sets the program's own input"),
                    )
                    .with_help(format!(
                        "give `input {k}` a default, or pass `--set {k}=...` on the command line"
                    ));
                    self.diags.push(d);
                    return Err(Skip);
                }
                if add {
                    return self.error(span, "an input is set with `=`");
                }
            }
            Target::Cell(_, _, path, Some(block)) if !has_body => {
                let d = Diagnostic::error(
                    self.span(lhs),
                    format!(
                        "`set {}` with no condition is an entry of `{block}`, declared in the same \
                         scope",
                        lhs.text()
                    ),
                )
                .with_help(format!(
                    "write `{path} = ...` in the block `{block} {{ .. }}`"
                ));
                self.diags.push(d);
                return Err(Skip);
            }
            Target::Cell(..) => {}
        }
        let mid = self.helpers.len();
        // A contribution to an input's cell is normal unless ranked (R-38),
        // written to the cell itself, so that its condition may read
        // another input.
        let (_, value_node) = match &target {
            Target::Input(_) => self.arg_node(rc, rhs, Pos::Value)?,
            _ => self.wanting(Want::Schema, |l| l.arg_node(rc, rhs, Pos::Value))?,
        };
        let mut terms = match &target {
            Target::Cell(typ, addr, ..) => vec![typ.1, addr.1],
            Target::Element(typ, addr, _, key, ..) => vec![typ.1, addr.1, key.1],
            Target::Input(_) => Vec::new(),
        };
        terms.push(value_node);
        let terms: Vec<bind::Term> = terms.into_iter().map(bind::Term::Expr).collect();
        self.check_bound(rc, clause, &terms)?;
        Ok(Contribution {
            span,
            target,
            add,
            rank,
            value: value_node,
            made: (made, mid, self.helpers.len()),
            written: rc.vars.values().cloned().collect(),
        })
    }

    /// What a `set` sets: a cell `(T, A, path)` and, when the block that
    /// owns the cell is declared in the same scope, how that block is
    /// written; or a stack input.
    pub(super) fn set_target(&mut self, rc: &mut Rc, c: &Chain, span: Span) -> L<Target> {
        let scope = rc.scope;
        // `[_]`: a variable per step, the clause form (R-162).
        let each;
        let c = if c.ops.iter().any(each::is_each) {
            each = self.each_target(rc, c, span)?;
            &each
        } else {
            c
        };
        // A stack input, set by name, or a field of an object input by its
        // path (R-54); a field of an input that is a reference is the
        // referenced resource's attribute (`set role.policies` in a module
        // given `input role: iam.role`), as a `let` of one's is.
        let through_ref = !c.ops.is_empty() && self.input_ref(scope, &c.head).is_some();
        if c.ops.iter().all(|o| matches!(o, Op::Field(_)))
            && self.is_value(scope, &c.head)
            && self.find_let(scope, &c.head).is_none()
            && !through_ref
        {
            return Ok(Target::Input(c.fields().join(".")));
        }
        // A used module's input, `m.k` or a field of an object one,
        // `m.k.f` (R-55): its cell `(input, m, k)`.
        if !c.ops.is_empty()
            && c.ops.iter().all(|o| matches!(o, Op::Field(_)))
            && let Some(at) = self
                .chain_of(scope)
                .into_iter()
                .find(|&s| self.names(s).module(&c.head).is_some())
        {
            let own = at == self.decl_scope(scope);
            return Ok(Target::Cell(
                self.termed_str(span, crate::modules::INPUT),
                self.scoped_name(scope, at, &c.head, span),
                c.fields()[1..].join("."),
                own.then(|| format!("use {}", c.head)),
            ));
        }
        // An element of a keyed list a variable ranges over: `set c.p = v
        // where c in w.containers` writes `w.containers[c].p` (R-69).
        if let Some(Elem { typ, addr, list }) = rc.elems.get(&c.head).cloned()
            && (!c.ops.is_empty() || c.head.starts_with("_[_]"))
        {
            let mut rest = Vec::new();
            for op in &c.ops {
                match op {
                    Op::Field(f) => rest.push(f.clone()),
                    Op::Index(_, r) | Op::Keyed(_, r) => {
                        return self.error(
                            self.span_of(*r),
                            format!(
                                "`{}` is an element of `{list}`: what `set` writes in it is a \
                                 path of fields, `{}.p.q`",
                                c.head, c.head
                            ),
                        );
                    }
                }
            }
            let key = self.var_named(rc, &c.head, span);
            let key = (var(&key), self.var_node(rc, span, &key));
            let block = self.owning_block(scope, &typ.0, &addr.0);
            let (typ, addr) = (
                self.settled_part(rc, typ, span),
                self.settled_part(rc, addr, span),
            );
            let (addr, key) = (self.each_read(rc, addr), self.each_read(rc, key));
            return Ok(Target::Element(typ, addr, list, key, rest, block));
        }
        // An instance's input: `n.k`.
        if let [Op::Field(k)] = c.ops.as_slice()
            && let Some((at, path)) = self.instance_in(scope, &c.head)
        {
            let own = at == self.decl_scope(scope);
            return Ok(Target::Cell(
                self.termed_str(span, crate::modules::INPUT),
                self.scoped_name(scope, at, &c.head, span),
                k.clone(),
                own.then(|| format!("resource {path} {}", c.head)),
            ));
        }
        self.target_named(rc, c)?;
        let res = self.resolve(rc, c)?;
        let (typ, addr, path) = match res {
            Res::Ref { typ, addr, path } if !path.is_empty() => {
                let addr = self.each_read(rc, (self.term_of(addr), addr));
                ((self.term_of(typ), typ), addr, path)
            }
            _ => {
                return self.error(
                    span,
                    "`set` sets a resource's attribute (`r.tags`, `T[e].p`) or an input",
                );
            }
        };
        // `containers[k]`: the element of a keyed list whose key is `k`, any
        // term but an integer, which stays the position (R-35, R-69).
        let keyed = path.iter().position(|s| match s {
            Seg::I(t) => !matches!(self.term_of(*t), Term::Val(Value::Int(_))),
            Seg::K(_) => true,
            Seg::F(_) => false,
        });
        if let Some(i) = keyed
            && !matches!(&typ.0, Term::Val(Value::Str(t)) if t == "settings")
        {
            let (Seg::I(key) | Seg::K(key)) = path[i].clone() else {
                unreachable!("found above")
            };
            let key = (self.term_of(key), key);
            let Some(list) = self.path_string(&path[..i]).filter(|l| !l.is_empty()) else {
                return self.error(span, "a contribution's path is constant");
            };
            let mut rest = Vec::new();
            for s in &path[i + 1..] {
                match s {
                    Seg::F(f) => rest.push(f.clone()),
                    Seg::I(_) | Seg::K(_) => {
                        return self.error(
                            span,
                            format!(
                                "one index per written path: below the element of `{list}` \
                                 the path is fields"
                            ),
                        );
                    }
                }
            }
            let block = self.owning_block(scope, &typ.0, &addr.0);
            return Ok(Target::Element(typ, addr, list, key, rest, block));
        }
        let Some(path) = self.path_string(&path) else {
            return self.error(span, "a contribution's path is constant");
        };
        let block = self.owning_block(scope, &typ.0, &addr.0);
        Ok(Target::Cell(typ, addr, path, block))
    }

    /// The string `s`, its term and its node at `span`.
    pub(super) fn termed_str(&mut self, span: Span, s: &str) -> Termed {
        (str_term(s), self.str_node(span, s))
    }

    /// The name `name` of a scope declared in `declared`, as read in
    /// `scope` ([`Self::scoped`]): its term and its node.
    pub(super) fn scoped_name(
        &mut self,
        scope: ScopeId,
        declared: ScopeId,
        name: &str,
        span: Span,
    ) -> Termed {
        let n = self.str_node(span, name);
        let id = self.scoped(scope, declared, n, span);
        (self.term_of(id), id)
    }

    /// `part`, resolved elsewhere, where a target writes it: a copy whose
    /// reads are made already.
    pub(super) fn settled_part(&mut self, rc: &Rc, (t, id): Termed, span: Span) -> Termed {
        (t, self.building(rc, span, |b| b.settled(id)))
    }

    /// A `set` target's part: the read of the `[_]` that binds it, at its
    /// first use ([`Rc::each_reads`]), else itself.
    pub(super) fn each_read(&mut self, rc: &mut Rc, (t, id): Termed) -> Termed {
        let read = match &self.program.exprs[id].kind {
            ExprKind::Var(v) => rc.each_reads.remove(&self.program.vars[*v].lowered),
            _ => None,
        };
        (t, read.unwrap_or(id))
    }

    /// A `set`'s path starts with what it writes: a name in scope (a
    /// resource, an input, a copy, a variable of the clause) or a type,
    /// `T[k]`. One that starts with no name is the error at the target,
    /// whatever follows it (`web.spec`, `web.containers["web"]`).
    pub(super) fn target_named(&mut self, rc: &Rc, c: &Chain) -> L<()> {
        let h = c.head.as_str();
        let named = c.is_bare()
            || c.call.is_some()
            || rc.vars.contains_key(h)
            || rc.types.contains_key(h)
            || rc.instances.contains_key(h)
            || self.declares(rc.scope, h)
            || self.decls.namespaces.contains(h)
            || self.decls.providers.contains(h)
            || self.decls.types.contains(h)
            || matches!(h, "world" | "settings" | "super" | "_")
            || self.lenient
            || self.any_type;
        if named {
            return Ok(());
        }
        let near = self.scope_names(rc);
        let help = match crate::diag::nearest(h, near.iter().map(String::as_str)) {
            Some(n) => format!("`{n}` is in scope"),
            None => format!(
                "a resource is named as its block declares it (`resource T {h}`), or by its \
                 type and key, `T[\"{h}\"]`"
            ),
        };
        let d = Diagnostic::error(
            self.span_of(c.range),
            format!(
                "`set` writes a resource's attribute or an input: nothing in scope is named `{h}`"
            ),
        )
        .with_help(help);
        self.diags.push(d);
        Err(Skip)
    }

    /// The block that owns the cells of `(T, A)` when it is declared in
    /// `scope`: `resource T A`.
    pub(super) fn owning_block(&self, scope: ScopeId, typ: &Term, addr: &Term) -> Option<String> {
        let s = self.names(self.decl_scope(scope));
        match (typ, addr) {
            (Term::Val(Value::Str(t)), Term::Val(Value::Str(a))) => s
                .resources(a)
                .is_some_and(|ts| ts.contains(t))
                .then(|| format!("resource {t} {a}")),
            _ => None,
        }
    }
}

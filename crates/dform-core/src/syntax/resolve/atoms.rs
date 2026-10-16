//! A relation's atom (H-11, H-12, R-58, R-185): the relation it names
//! and how the read reaches it, its arguments by position or by column
//! name, a reference where a column holds one (a resource by its name, a
//! typed variable, `T[e]`), a copy's exported relation (R-55), and the
//! checks on what it calls (an aggregate, a function).

use super::*;

impl Lowerer<'_> {
    /// A chain that must name one resource: its type and address.
    pub(super) fn reference(&mut self, rc: &mut Rc, c: &Chain, span: Span) -> L<(ExprId, ExprId)> {
        if c.is_bare()
            && let Some(types) = self.resource(rc.scope, &c.head)
        {
            let typ = match &self.want {
                _ if types.len() == 1 => &types[0],
                Want::Type(t) if types.contains(t) => t,
                _ => return self.ambiguous(&c.head, &types, span),
            };
            let typ = typ.clone();
            let typ = self.str_node(span, &typ);
            return Ok((typ, self.resource_addr(rc.scope, &c.head, span)));
        }
        match self.resolve(rc, c)? {
            Res::Ref { typ, addr, path } if path.is_empty() => Ok((typ, addr)),
            _ => self.error(span, "expected a resource: a name in scope, or `T[e]`"),
        }
    }

    /// `f` with `want` picking among the resources a bare name shares.
    pub(super) fn wanting<T>(&mut self, want: Want, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.want, want);
        let r = f(self);
        self.want = saved;
        r
    }

    pub(super) fn ambiguous<T>(&mut self, name: &str, types: &[String], span: Span) -> L<T> {
        self.error(span, crate::types::ambiguous_resource(name, types))
    }

    /// A resource's attribute given a bare name two types share: every
    /// candidate, for the schema's `ref(T)` to pick (`types::read`, R-74).
    pub(super) fn deferred_ref(&mut self, rc: &Rc, name: &str, span: Span) -> Option<ExprId> {
        let types = self.resource(rc.scope, name)?;
        if self.want != Want::Schema || types.len() < 2 {
            return None;
        }
        let int = |l: &mut Self, i: u32| l.node(span, ExprKind::Lit(Value::Int(i64::from(i))));
        let mut args = vec![
            self.str_node(span, name),
            int(self, span.file),
            int(self, span.start),
            int(self, span.end),
        ];
        for t in &types {
            let typ = self.str_node(span, t);
            let addr = self.resource_addr(rc.scope, name, span);
            let empty = self.str_node(span, "");
            let r = vec![typ, addr, empty];
            args.push(self.building(rc, span, |b| b.form(crate::address::REF, r)));
        }
        Some(self.building(rc, span, |b| b.form(crate::types::AMBIGUOUS_REF, args)))
    }

    /// A relation atom, its arguments lowered at `pos`: positional, or
    /// named by the relation's columns (`p(a: x)`, H-12); its arguments'
    /// nodes, and how the read reaches the relation.
    pub(super) fn atom_args(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
    ) -> L<(Atom, AtomArgs, Reach)> {
        let span = self.span(n);
        if let Some((a, args)) = self.exported_relation(rc, n, pos)? {
            return Ok((a, args, Reach::Own));
        }
        let (pred, via) = self.relation_named(rc, n, span)?;
        let list = node(n, ARG_LIST);
        let named: Vec<SyntaxNode> = list
            .iter()
            .flat_map(|l| l.children().filter(|c| c.kind() == NAMED_ARG))
            .collect();
        if !named.is_empty() {
            if list.iter().any(|l| terms(l).next().is_some()) {
                return self.error(
                    span,
                    "a relation's arguments are all positional or all named",
                );
            }
            let (a, args) = self.named_args(rc, pred, &named, pos, span)?;
            return Ok((a, args, via));
        }
        let terms_of = |l: &Option<SyntaxNode>| -> Vec<SyntaxNode> {
            l.as_ref().map(|l| terms(l).collect()).unwrap_or_default()
        };
        let (a, args) = match crate::zset::REF_RELATIONS.iter().find(|(p, ..)| *p == pred) {
            Some(_) => self.ref_relation_args(rc, pred, &terms_of(&list), pos, span)?,
            // A program relation's reference column (R-185): a resource
            // there is the reference, as in the plan's relations.
            None if !self
                .ref_columns(
                    rc.scope,
                    &pred,
                    list.as_ref().map_or(0, |l| terms(l).count()),
                )
                .is_empty() =>
            {
                let (args, nodes) = self.ref_args(rc, &pred, list.as_ref(), pos)?;
                let a = Atom {
                    pred,
                    args,
                    record: None,
                    span,
                };
                (a, AtomArgs::Positional(nodes))
            }
            None if pos == Pos::Content => {
                self.content_args(rc, pred, &terms_of(&list), pos, span)?
            }
            None => {
                let (args, nodes) = self.args(rc, n, pos)?;
                Self::positional(pred, args.into_iter().zip(nodes).collect(), span)
            }
        };
        Ok((a, args, via))
    }

    /// The relation the atom `n` names, and how the read reaches it: a
    /// used module's (`releases.release(..)`, the activation's
    /// `releases::release`, R-65), the scope's around a component's
    /// (`super.p(..)`, R-186), the module's own by its name in its file,
    /// or one in scope by its name.
    pub(super) fn relation_named(
        &mut self,
        rc: &Rc,
        n: &SyntaxNode,
        span: Span,
    ) -> L<(String, Reach)> {
        let Some(pred) = self.callee(n) else {
            return self.error(span, "a relation is named by a plain name (`p` or `m.p`)");
        };
        if let Some((m, p)) = pred.split_once('.')
            && let Some(path) = self.use_in(rc.scope, m)
        {
            let Some(scope) = self.module_at(&path).map(|m| m.0) else {
                return Err(Skip);
            };
            if !self.names(scope).is_relation(p) {
                return self.error(span, format!("the module {path} has no relation `{p}`"));
            }
            return Ok((format!("{m}::{p}"), Reach::Own));
        }
        if let Some(c) = n.children().find_map(|c| Chain::of(&c))
            && c.head == "super"
        {
            let (from, rest) = self.super_chain(rc, &c, span)?;
            let p = &rest.head;
            let known = self.chain_of(from).into_iter().any(|s| {
                let sc = self.names(s);
                sc.is_relation(p) || sc.takes(p)
            });
            if !rest.ops.is_empty() || !known {
                let around = self.scope_name(from);
                return self.error(span, format!("`super.{p}`: {around} has no relation `{p}`"));
            }
            return Ok((p.clone(), self.relation_reach(rc.scope, from, p)));
        }
        if let Some((m, p)) = pred.split_once('.')
            && let Some(from) = self.self_module(rc.scope, m)
            && self.names(from).is_relation(p)
        {
            // The instance's the read is in (R-186).
            return Ok((p.to_string(), self.relation_reach(rc.scope, from, p)));
        }
        if !pred.contains('.') {
            self.undeclared_relation(rc, &pred, span)?;
            let via = self.relation_reach(rc.scope, rc.scope, &pred);
            return Ok((pred, via));
        }
        Ok((pred, Reach::Own))
    }

    /// `pred(a: x, ..)`: the arguments `named` by the relation's columns.
    pub(super) fn named_args(
        &mut self,
        rc: &mut Rc,
        pred: String,
        named: &[SyntaxNode],
        pos: Pos,
        span: Span,
    ) -> L<(Atom, AtomArgs)> {
        let mut fields = BTreeMap::new();
        let mut nodes = Vec::new();
        for f in named {
            let key = tokens(f).next().ok_or(Skip)?.text().to_string();
            let (value, id) = self.arg_node(rc, &terms(f).next().ok_or(Skip)?, pos)?;
            if fields.insert(key.clone(), value).is_some() {
                return self.error(self.span(f), format!("column `{key}` given twice"));
            }
            nodes.push((key, id));
        }
        let a = Atom {
            pred,
            args: Vec::new(),
            record: Some(fields),
            span,
        };
        Ok((a, AtomArgs::Record(nodes)))
    }

    /// A relation of the plan's whose one column holds a reference
    /// (`zset::REF_RELATIONS`): that argument the reference.
    pub(super) fn ref_relation_args(
        &mut self,
        rc: &mut Rc,
        pred: String,
        list: &[SyntaxNode],
        pos: Pos,
        span: Span,
    ) -> L<(Atom, AtomArgs)> {
        if let Err(e) = crate::zset::ref_arity(&pred, list.len()) {
            return self.error(span, e);
        }
        let at = crate::zset::ref_column(&pred, list.len());
        let mut args = Vec::new();
        for (i, t) in list.iter().enumerate() {
            args.push(if Some(i) == at {
                let id = self.ref_expr(rc, t, pos)?;
                (self.term_of(id), id)
            } else {
                self.arg_node(rc, t, pos)?
            });
        }
        // `moved(T, "old-address", r)`: the old address is a path too
        // (R-112).
        if pred == "moved" {
            self.address_key(&args[1].0, span)?;
        }
        Ok(Self::positional(pred, args, span))
    }

    /// In a body an argument is a pattern (R-58): `pair((a, b))`; a
    /// relation of several named columns takes one object pattern, `zone({
    /// name, index })`, its record pattern.
    pub(super) fn content_args(
        &mut self,
        rc: &mut Rc,
        pred: String,
        list: &[SyntaxNode],
        pos: Pos,
        span: Span,
    ) -> L<(Atom, AtomArgs)> {
        if let [o] = list
            && o.kind() == OBJECT
            && self.named_columns(rc.scope, &pred)
        {
            let (record, nodes) = self.record_pattern(rc, o)?;
            let a = Atom {
                pred,
                args: Vec::new(),
                record: Some(record),
                span,
            };
            return Ok((a, nodes));
        }
        let mut args = Vec::new();
        for t in list {
            args.push(match t.kind() {
                TUPLE => self.pattern(rc, t)?,
                _ => self.arg_node(rc, t, pos)?,
            });
        }
        Ok(Self::positional(pred, args, span))
    }

    /// The atom `pred(args)` at `span`, each argument its term and node.
    pub(super) fn positional(
        pred: String,
        args: Vec<(Term, ExprId)>,
        span: Span,
    ) -> (Atom, AtomArgs) {
        let (args, nodes): (Vec<Term>, Vec<ExprId>) = args.into_iter().unzip();
        let a = Atom {
            pred,
            args,
            record: None,
            span,
        };
        (a, AtomArgs::Positional(nodes))
    }

    /// A relation's argument at `pos`: its term, the reads it makes into
    /// `pre`, and its node.
    pub(super) fn arg_node(&mut self, rc: &mut Rc, t: &SyntaxNode, pos: Pos) -> L<(Term, ExprId)> {
        let id = self.expr(rc, t, pos)?;
        Ok((self.term_of(id), id))
    }

    /// The goal of the atom `a` whose arguments are `args`: its relation's.
    pub(super) fn rel_goal(&mut self, rc: &Rc, (a, via): (&Atom, Reach), args: AtomArgs) -> GoalId {
        let span = a.span;
        let rel = RelRef::reached(a.pred.clone(), via, span);
        let args = self.rel_args(rc, span, args);
        self.goal(rc, span, GoalKind::Rel { rel, args })
    }

    /// The atom's arguments `args` at `span`, each in a binding position.
    pub(super) fn rel_args(&mut self, rc: &Rc, span: Span, args: AtomArgs) -> RelArgs {
        self.building(rc, span, |b| match args {
            AtomArgs::Positional(xs) => {
                RelArgs::Positional(xs.into_iter().map(|x| b.pattern_of(x)).collect())
            }
            AtomArgs::Record(xs) => {
                RelArgs::Record(xs.into_iter().map(|(k, x)| (k, b.pattern_of(x))).collect())
            }
        })
    }

    /// A relation another copy or deployment exports, `output p` (R-55):
    /// a copy's, `blue.p(x, y)`, the copy's rows `__rows(blue, "p", [x,
    /// y])`; every copy's, `c[t].p(x, y)`, those of each copy `t` of `c`;
    /// a deployment's, `platform[env=e].p(x, y)`, a row of what it
    /// published, `output("platform[env=e]", "p", Rows), member(Rows, [x,
    /// y])`. `None` for any other call.
    pub(super) fn exported_relation(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
    ) -> L<Option<(Atom, AtomArgs)>> {
        let span = self.span(n);
        let Some(c) = n.children().find_map(|c| Chain::of(&c)) else {
            return Ok(None);
        };
        let Some(Op::Field(p)) = c.ops.last() else {
            return Ok(None);
        };
        let p = p.clone();
        if rc.vars.contains_key(&c.head) {
            return Ok(None);
        }
        if let Some(d) = self.stack_in(rc.scope, &c.head) {
            let Res::Output { inst, key, of, .. } = self.deployed_path(rc, &c, &d, span)? else {
                return Err(Skip);
            };
            let read = PlanRead::Output {
                copy: inst,
                key,
                of,
            };
            let rows = self.read_var(rc, "output", read, 2, &p, span);
            let (cols, nodes) = self.args(rc, n, pos)?;
            let parts = nodes.into_iter().map(ListPart::Elem).collect();
            let list = self.node(span, ExprKind::List { parts });
            let a = atom_at("member", vec![self.term_of(rows), Term::List(cols)], span);
            return Ok(Some((a, AtomArgs::Positional(vec![rows, list]))));
        }
        let named = self
            .instance_in(rc.scope, &c.head)
            .filter(|_| c.ops.len() == 1);
        let every = matches!(c.ops.first(), Some(Op::Index(..)));
        if named.is_none() && !every {
            return Ok(None);
        }
        if let Some((_, path)) = &named {
            let exports = self
                .module_at(path)
                .is_some_and(|(m, _)| self.names(m).exports(&p));
            if !exports {
                let d = Diagnostic::error(
                    span,
                    format!("{} exports no relation {p}", path),
                )
                .with_help(format!(
                    "a copy's relations are its own; `output {p}` in {path} exports it, read as \
                     `{}.{p}(..)`",
                    c.head
                ));
                self.diags.push(d);
                return Err(Skip);
            }
        }
        let mut prefix = c.clone();
        prefix.ops.pop();
        let Some(Res::Val(inst)) = self.scope_path(rc, &prefix, span)? else {
            return Ok(None);
        };
        let inst_term = self.term_of(inst);
        // A column the component's relation holds references in is the
        // reference here too (R-204): `s` in `blue.subnet(s)` is the
        // subnet, its address what `s in net.subnet` reads.
        let list: Vec<SyntaxNode> = node(n, ARG_LIST)
            .map(|l| terms(&l).collect())
            .unwrap_or_default();
        let (cols, nodes) = match self.exported_ref_columns(rc, n, list.len()) {
            Some(refs) if !refs.is_empty() => self.ref_args_by(rc, &p, &refs, &list, pos)?,
            _ => {
                let mut cols = Vec::new();
                for t in &list {
                    cols.push(self.arg_node(rc, t, pos)?);
                }
                cols.into_iter().unzip()
            }
        };
        let a = atom_at(
            crate::modules::ROWS,
            vec![inst_term, str_term(&p), Term::List(cols)],
            span,
        );
        let parts = nodes.into_iter().map(ListPart::Elem).collect();
        let list = self.node(span, ExprKind::List { parts });
        let args = AtomArgs::Positional(vec![inst, self.str_node(span, &p), list]);
        Ok(Some((a, args)))
    }

    /// Is `n` a reference: a resource, a typed variable, or a variable a
    /// reference column binds?
    pub(super) fn is_reference(&mut self, rc: &Rc, n: &SyntaxNode) -> bool {
        if let Some(c) = Chain::of(n)
            && c.is_bare()
            && (rc.types.contains_key(&c.head) || rc.untyped_refs.contains(&c.head))
        {
            return true;
        }
        self.names_resource(rc, n)
    }

    /// Does `n` name one resource, by its name in scope or `T[e]`?
    pub(super) fn names_resource(&mut self, rc: &Rc, n: &SyntaxNode) -> bool {
        let Some(c) = Chain::of(n) else {
            return false;
        };
        if c.is_bare() {
            return !rc.vars.contains_key(&c.head)
                && !rc.types.contains_key(&c.head)
                && self.resource(rc.scope, &c.head).is_some();
        }
        if rc.types.contains_key(&c.head) {
            return false;
        }
        let mut rc2 = rc.clone();
        matches!(
            self.probe(|l| l.resolve(&mut rc2, &c)),
            Ok(Res::Ref { ref path, .. }) if path.is_empty()
        )
    }

    /// A resource where a relation takes one (R-42): by its name in scope,
    /// `T[e]`, or a variable, as a reference value. A variable `in T` binds
    /// is `ref(T, A, "")`: the reference taken apart, its address `A` what
    /// `r in T` and `r.path` read.
    /// The component a membership's right side names (`x in network`,
    /// `x in net.vpc`): its path.
    pub(super) fn component_of(&self, rc: &Rc, c: &Chain) -> Option<String> {
        if rc.vars.contains_key(&c.head) || !c.ops.iter().all(|o| matches!(o, Op::Field(..))) {
            return None;
        }
        let fields = c.fields();
        let mut at = self
            .component_in(rc.scope, &c.head)
            .or_else(|| self.use_in(rc.scope, &c.head))?;
        for f in &fields[1..] {
            at = format!("{at}.{f}");
        }
        self.module_at(&at).is_some_and(|m| m.1).then_some(at)
    }

    /// The scope whose copies of the component `path` a read in this body
    /// sees: the body's own when it makes one, else its user's (the
    /// `instance_of` user column, as `c[t]` reads it).
    pub(super) fn copies_scope(&mut self, rc: &Rc, path: &str, span: Span) -> ExprId {
        let own = self
            .own_scopes(rc.scope)
            .iter()
            .find(|s| {
                self.names(**s)
                    .instances()
                    .map(|(_, c)| c)
                    .any(|p| p == path)
            })
            .copied();
        let user = self.str_node(span, "");
        self.scoped(rc.scope, own.unwrap_or(self.root()), user, span)
    }

    /// A copy where a resource is taken (R-67): its name in scope (`blue`)
    /// or a variable `x in network` binds, as a reference `ref(PATH, name,
    /// "")`, the copy's address `network["blue"]`.
    pub(super) fn instance_ref(&mut self, rc: &mut Rc, c: &Chain, span: Span) -> Option<ExprId> {
        if !c.is_bare() {
            return None;
        }
        let (path, name) = if let Some(path) = rc.instances.get(&c.head).cloned() {
            let v = self.var_named(rc, &c.head, span);
            (path, self.var_node(rc, span, &v))
        } else {
            if rc.vars.contains_key(&c.head) || self.resource(rc.scope, &c.head).is_some() {
                return None;
            }
            let (at, path) = self.instance_in(rc.scope, &c.head)?;
            let name = self.str_node(span, &c.head);
            (path, self.scoped(rc.scope, at, name, span))
        };
        let args = vec![self.str_node(span, &path), name, self.str_node(span, "")];
        Some(self.building(rc, span, |b| b.form(crate::address::REF, args)))
    }

    /// A resource where a relation takes one (R-42) as its node, the reads
    /// no node of it holds into `pre`.
    pub(super) fn ref_expr(&mut self, rc: &mut Rc, n: &SyntaxNode, pos: Pos) -> L<ExprId> {
        let span = self.span(n);
        if n.kind() == LITERAL {
            return self.error(
                span,
                "a resource here, not a value: its name in scope, `T[\"a\"]`, or a variable \
                 (R-42)",
            );
        }
        let Some(c) = Chain::of(n) else {
            return self.expr(rc, n, pos);
        };
        if let Some(r) = self.instance_ref(rc, &c, span) {
            return Ok(r);
        }
        if c.is_bare()
            && !rc.vars.contains_key(&c.head)
            && self.resource(rc.scope, &c.head).is_some()
        {
            let (typ, addr) = self.reference(rc, &c, span)?;
            let empty = self.str_node(span, "");
            let args = vec![typ, addr, empty];
            return Ok(self.building(rc, span, |b| b.form(crate::address::REF, args)));
        }
        match self.resolve(rc, &c)? {
            Res::Ref { typ, addr, path } if path.is_empty() => {
                let empty = self.str_node(span, "");
                let args = vec![typ, addr, empty];
                let id = self.building(rc, span, |b| b.form(crate::address::REF, args));
                Ok(id)
            }
            res => {
                if c.is_bare()
                    && let Res::Val(v) = &res
                    && matches!(self.program.exprs[*v].kind, ExprKind::Var(_))
                {
                    rc.untyped_refs.insert(c.head.clone());
                }
                self.realize_expr(rc, res, pos, span)
            }
        }
    }

    /// A function's literal arguments read as its parameters' types (R-31):
    /// `inet.subnet("10.0.0.0/16", 8, 1)` takes an `inet`; a literal that
    /// cannot be one is an error at the call. Only the scalar types a
    /// literal can be checked against are; a `string` or `any` parameter
    /// takes what it is given.
    pub(super) fn typed_args(&mut self, name: &str, args: Vec<Term>, span: Span) -> L<Vec<Term>> {
        // Read leniently (a refinement's text), a name is its own string.
        let Some(f) = crate::functions::get(name).filter(|_| !self.lenient && !self.text) else {
            return Ok(args);
        };
        // A call takes the arguments its signature declares (R-134: no
        // function takes any number of values but `format`).
        let lowering = f.internal;
        if !lowering && !f.takes(args.len()) {
            return self.error(
                span,
                format!(
                    "`{name}` is called with {} arguments: its signature is `{}`",
                    args.len(),
                    f.signature
                ),
            );
        }
        let mut out = Vec::with_capacity(args.len());
        for (i, a) in args.into_iter().enumerate() {
            let p = f
                .params
                .get(i)
                .or(if f.variadic { f.params.last() } else { None });
            let ty = match p.map(|p| crate::types::Ty::parse(&p.ty)) {
                Some(t @ crate::types::Ty::Scalar(_)) if p.is_some_and(|p| p.ty != "string") => t,
                _ => {
                    out.push(a);
                    continue;
                }
            };
            // A literal is read as the parameter's type at compile time; a
            // computed value is the body's to read (`engine::as_params`).
            let a = match a {
                Term::Val(_) => a,
                Term::Func { ref name, .. } if name == crate::types::AMBIGUOUS => a,
                a => {
                    out.push(a);
                    continue;
                }
            };
            match crate::types::literal(&ty, a) {
                Ok(a) => out.push(a),
                Err(why) => {
                    let p = p.map(|p| p.name.as_str()).unwrap_or("");
                    return self.error(span, format!("`{name}`'s argument `{p}` {why}"));
                }
            }
        }
        Ok(out)
    }

    /// `ref(r)`: the reference to the resource `r` (its name in scope,
    /// `T[e]`, a typed variable), written out where an attribute that is no
    /// `ref(T)` takes one: the provider gives it the resource's id (R-43).
    /// It lowers to `ref(ref(T, A, ""))`, the reference marked as asked
    /// for (`types` lets it into a `string` attribute); evaluated, it is
    /// the reference.
    pub(super) fn ref_call(&mut self, rc: &mut Rc, list: &SyntaxNode, span: Span) -> L<ExprId> {
        let arg = terms(list).next().ok_or(Skip)?;
        let r = self.ref_expr(rc, &arg, Pos::Content)?;
        match self.term_of(r) {
            Term::Func { .. } => {
                Ok(self.building(rc, span, |b| b.form(crate::address::REF, vec![r])))
            }
            _ => self.error(
                span,
                "`ref(r)` takes a resource: its name in scope, `T[\"a\"]`, or a variable `in T`",
            ),
        }
    }

    /// Whether the bare name `c` is a resource in scope, not a variable, a
    /// type or a value name: `has warm_cache`.
    pub(super) fn bare_resource(&self, rc: &Rc, c: &Chain) -> bool {
        c.is_bare()
            && c.head != "_"
            && !rc.vars.contains_key(&c.head)
            && !rc.types.contains_key(&c.head)
            && !self.is_value(rc.scope, &c.head)
            && self.resource(rc.scope, &c.head).is_some()
    }

    /// `has x.id`: an error naming `has x` (R-152), which asks whether the
    /// resource's identity is known.
    pub(super) fn has_identity<T>(&mut self, n: &SyntaxNode, span: Span) -> L<T> {
        let text = n.text().to_string();
        let r = text
            .trim()
            .strip_suffix(&format!(".{}", crate::schema::IDENTITY))
            .unwrap_or("r")
            .to_string();
        let d = Diagnostic::error(
            span,
            format!(
                "`has {}`: a program does not read an id; write `has {r}`",
                text.trim()
            ),
        )
        .with_help(format!(
            "`has {r}` holds once `{r}` exists: its identity is known (R-152)"
        ));
        self.diags.push(d);
        Err(Skip)
    }

    /// `x.id`: an error naming the reference (R-43). A program never reads
    /// a resource's id; the provider resolves a reference to it.
    pub(super) fn identity_read<T>(&mut self, n: &SyntaxNode, span: Span) -> L<T> {
        let text = n.text().to_string();
        let r = text
            .trim()
            .strip_suffix(&format!(".{}", crate::schema::IDENTITY))
            .unwrap_or("r")
            .to_string();
        let d = Diagnostic::error(
            span,
            format!(
                "`{}`: a program does not read an id; a reference is the resource",
                text.trim()
            ),
        )
        .with_help(format!(
            "write `{r}` itself where an attribute takes the resource, or `ref({r})` where an \
             attribute that is not a `ref(T)` needs its id (R-43)"
        ));
        self.diags.push(d);
        Err(Skip)
    }

    /// `sum` of a value known here not to be an int, `min`/`max` of one
    /// neither an int nor a string: an error at the call rather than a
    /// deny of every group.
    pub(super) fn check_aggregated(&mut self, name: &str, args: &[Term], span: Span) {
        if self.lenient {
            return;
        }
        let kind = match args {
            [Term::Val(Value::Int(_))] => "an int",
            [Term::Val(Value::Str(_))] => "a string",
            [Term::Func { name, .. }]
                if crate::functions::get(name).is_some_and(|f| f.ret == "string") =>
            {
                "a string"
            }
            [Term::Val(Value::Bool(_))] => "a bool",
            [Term::Val(Value::List(_)) | Term::List(_) | Term::ListComp { .. }] => "a list",
            [Term::Val(Value::Obj(_)) | Term::Obj(_)] => "an object",
            _ => return,
        };
        let ok: &[&str] = match name {
            "sum" => &["an int"],
            "min" | "max" => &["an int", "a string"],
            _ => return,
        };
        if !ok.contains(&kind) {
            self.diags.push(Diagnostic::error(
                span,
                format!(
                    "`{name}` aggregates {}, not {kind}",
                    if ok.len() == 1 {
                        "ints"
                    } else {
                        "ints or strings"
                    }
                ),
            ));
        }
    }

    /// A call to a function the evaluator does not have would have no value
    /// and fail its literal quietly: an error at the call. A refinement's
    /// calls are `refine::check_rest`'s, with the refinement's own message.
    pub(super) fn check_function(&mut self, name: &str, span: Span) {
        if self.lenient || self.calls == Calls::Data || crate::functions::callable(name) {
            return;
        }
        if crate::partition::AGGREGATES.contains(&name) {
            self.diags.push(
                Diagnostic::error(
                    span,
                    format!(
                        "`{name}` is an aggregate: name its result after `where`, `n = \
                         {name}(x)`"
                    ),
                )
                .with_help(format!(
                    "`p(k, n) where n = {name}(x), ..` gives one `n` per `k`; `let n = \
                     {name}(x) where ..` gives one in all"
                )),
            );
            return;
        }
        self.diags.push(crate::functions::unknown(span, name));
    }

    /// The name a call or record is applied by: its chain's dotted text.
    pub(super) fn callee(&self, n: &SyntaxNode) -> Option<String> {
        let c = n.children().find_map(|c| Chain::of(&c))?;
        if c.ops.iter().any(|o| !matches!(o, Op::Field(..))) {
            return None;
        }
        Some(c.fields().join("."))
    }

    /// `n`'s positional arguments at `pos`: each its term and node
    /// ([`Self::arg_node`]).
    pub(super) fn args(
        &mut self,
        rc: &mut Rc,
        n: &SyntaxNode,
        pos: Pos,
    ) -> L<(Vec<Term>, Vec<ExprId>)> {
        let mut out = Vec::new();
        for t in node(n, ARG_LIST).iter().flat_map(terms) {
            out.push(self.arg_node(rc, &t, pos)?);
        }
        Ok(out.into_iter().unzip())
    }

    /// The arguments of `n` as parts of what a chain resolves to
    /// ([`Self::part`]).
    pub(super) fn arg_parts(&mut self, rc: &mut Rc, n: &SyntaxNode) -> L<Vec<ExprId>> {
        let Some(list) = node(n, ARG_LIST) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for t in terms(&list) {
            out.push(self.part(rc, &t)?);
        }
        Ok(out)
    }
}

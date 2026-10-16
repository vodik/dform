//! A dotted chain to what it denotes (docs/grammar.md "Names"): a
//! variable, a value name (through a reference it holds), `settings`,
//! `world`, a resource, a call's result, a type's resource by its key, a
//! relation or extern lookup; a bare name a variable unless it names
//! what no variable may.

use super::*;

impl Lowerer<'_> {
    /// In a field's `check`, a bare name of a field of an enclosing object
    /// (not the field checked, nor a variable): the chain read by the
    /// object's path, `lo` as `db.lo`.
    pub(super) fn field_in_check(&self, rc: &Rc, c: &Chain) -> Option<Chain> {
        let h = c.head.as_str();
        if self.checked.as_deref().is_none_or(|n| n == h) || rc.vars.contains_key(h) {
            return None;
        }
        let (path, _) = self
            .objects
            .iter()
            .rev()
            .find(|(_, f)| f.iter().any(|f| f == h))?;
        let mut words = path.split('.');
        let head = words.next()?.to_string();
        let mut ops: Vec<Op> = words.map(|w| Op::Field(w.to_string())).collect();
        ops.push(Op::Field(h.to_string()));
        ops.extend(c.ops.iter().cloned());
        Some(Chain {
            head,
            head_kind: IDENT,
            call: None,
            range: c.range,
            ops,
        })
    }

    /// A variable's lowered name, recording it.
    pub(super) fn var_named(&mut self, rc: &mut Rc, name: &str, span: Span) -> String {
        if self.binding {
            rc.binders.insert(name.to_string());
        }
        rc.uses.add(name, span);
        if let Some(v) = rc.vars.get(name) {
            return v.clone();
        }
        let mut v = capitalise(name);
        if rc.vars.values().any(|w| *w == v) {
            v = fresh(rc, &v);
        }
        rc.reserved.insert(v.clone());
        rc.vars.insert(name.to_string(), v.clone());
        rc.first.insert(name.to_string(), span);
        v
    }

    /// What a chain denotes (section "Names" of docs/grammar.md). Index
    /// terms are lowered in `rc`, their reads into `pre`.
    pub(super) fn resolve(&mut self, rc: &mut Rc, c: &Chain, pre: &mut Vec<Lit>) -> L<Res> {
        let span = self.span_of(c.range);
        if let Some(call) = &c.call {
            return self.call_read(rc, call, &c.ops, pre);
        }
        let field = self.field_in_check(rc, c);
        let c = field.as_ref().unwrap_or(c);
        let h = c.head.as_str();
        if self.lenient && !rc.vars.contains_key(h) && self.checked.as_ref().is_none_or(|n| n == h)
        {
            if c.ops.iter().any(|o| !matches!(o, Op::Field(..))) {
                return self.error(span, "a name here is a path: `a.b.c`");
            }
            let name = c.fields().join(".");
            return Ok(Res::Val(self.str_node(span, &name)));
        }
        if h == "_" && c.is_bare() {
            return Ok(Res::Val(self.node(span, ExprKind::Hole)));
        }
        if h == "_" {
            return self.error(
                span,
                "`_` is a placeholder and is never accessed: `_.p` and `_[k]` read nothing; \
                 name it (`env.p`)",
            );
        }
        // `super.x`: the scope around this component (R-186).
        if h == "super" && !rc.vars.contains_key(h) {
            return self.super_read(rc, c, pre, span);
        }
        // A let with parameters has no value until called (R-187).
        if !rc.vars.contains_key(h)
            && let Some((_, def)) = self.function_def(rc.scope, h)
        {
            return self.not_called(span, h, &def);
        }
        // A copy whose `instance` is the error reads nothing, and says so
        // there.
        if !rc.vars.contains_key(h) && self.unbound_instance(rc.scope, h) {
            return Err(Skip);
        }
        // A copy `x in network` binds (R-67): its name, `x.k` its output.
        if let Some(path) = rc.instances.get(h).cloned() {
            let v = self.var_named(rc, h, span);
            let inst = self.var_node(rc, span, &v);
            return self.output_of(rc, (inst, None), &path, &c.ops, pre, span);
        }
        // A typed variable: a reference.
        if let Some(t) = rc.types.get(h).cloned() {
            if let Some(Op::Field(first)) = c.ops.first() {
                self.namespace_attr(rc, h, first, span)?;
            }
            let v = self.var_named(rc, h, span);
            let path = self.segs(rc, &c.ops, pre)?;
            let name = self.var_node(rc, span, &v);
            let addr = match rc.row_refs.contains(h) {
                true => {
                    let via = Reach::Absolute;
                    self.node(span, ExprKind::Scoped { name, via })
                }
                false => name,
            };
            let typ = self.name_node(rc, span, &t);
            return Ok(Res::Ref { typ, addr, path });
        }
        // A name a resource shares, read as the other thing's (R-76); in a
        // module's or a component's body, the body's own (R-101).
        let own = match rc.vars.contains_key(h) {
            true => None,
            false => self.own_wins(rc.scope, h),
        };
        let shared = !rc.vars.contains_key(h)
            && match own {
                Some(resource) => !resource,
                None => self.shared_name(rc.scope, c, span)?,
            };
        if !rc.vars.contains_key(h) {
            self.undeclared(rc, h, span)?;
            if self.is_value(rc.scope, h) && own != Some(true) {
                return self.value(rc, rc.scope, c, pre, span);
            }
            // `settings.x` names a resource called `settings` in scope; a
            // settings row, `settings[e]`, is gone (R-38).
            let resource =
                matches!(c.ops.first(), Some(Op::Field(_))) && self.resource(rc.scope, h).is_some();
            if c.head_kind == SETTINGS_KW && !c.is_bare() && !resource {
                return self.settings_read(c, span);
            }
            if h == "world" && !c.ops.is_empty() {
                return self.world(rc, c, pre, span);
            }
        }
        if c.is_bare() {
            if c.head_kind == SETTINGS_KW && !rc.vars.contains_key(h) {
                return self.settings_read(c, span);
            }
            return self.bare(rc, h, span);
        }
        if !rc.vars.contains_key(h) {
            if let Some(types) = self.resource(rc.scope, h).filter(|_| !shared) {
                if types.len() > 1 {
                    return self.ambiguous(h, &types, span);
                }
                let path = self.segs(rc, &c.ops, pre)?;
                return Ok(Res::Ref {
                    typ: self.str_node(span, &types[0]),
                    addr: self.resource_addr(rc.scope, h, span),
                    path,
                });
            }
            if let Some(r) = self.scope_path(rc, c, pre, span)? {
                return Ok(r);
            }
            if let Some(r) = self.typed_path(rc, c, pre, span)? {
                return Ok(r);
            }
        }
        if rc.untyped_refs.contains(h) {
            let d = Diagnostic::error(
                span,
                format!("`{h}` is a reference of no known type: `{h}.path` reads nothing"),
            )
            .with_help(format!(
                "bind its type first, `{h} in T`, and `{h}.path` reads that resource (R-43)"
            ));
            self.diags.push(d);
            return Err(Skip);
        }
        // A variable's fields: `x.a.b`, `x[0]`.
        if rc.vars.contains_key(h)
            || (rc.candidates.contains(h) && !self.decls.namespaces.contains(h))
        {
            let v = self.var_named(rc, h, span);
            let path = self.segs(rc, &c.ops, pre)?;
            let var = self.var_node(rc, span, &v);
            return Ok(Res::Var { var, path });
        }
        // A dotted name in a type namespace is a type's name, and must be a
        // known type (H-10): a typo is an error, never a string.
        if c.ops.iter().all(|o| matches!(o, Op::Field(..)))
            && (self.decls.namespaces.contains(h) || self.any_type)
        {
            let name = c.fields().join(".");
            if self.any_type
                || self.decls.types.contains(&name)
                || (!c.ops.is_empty() && !self.decls.closed.contains(h))
            {
                return Ok(Res::Type(name));
            }
            return self.error(
                span,
                format!(
                    "unknown type `{name}`: no resource header, `type` block or provider schema \
                     declares it"
                ),
            );
        }
        let quoted = format!("\"{}\"", c.fields().join("."));
        let fix = c.ops.iter().all(|o| matches!(o, Op::Field(..)));
        let near = self.scope_names(rc);
        self.diags.push(unknown_name(
            span,
            h,
            &quoted,
            fix,
            near.iter().map(String::as_str),
        ));
        Err(Skip)
    }

    /// `f(x).p[i]` (R-71): the call bound to a variable, once per rule, as
    /// `v = f(x)` would, and the path read from it.
    pub(super) fn call_read(
        &mut self,
        rc: &mut Rc,
        call: &SyntaxNode,
        ops: &[Op],
        pre: &mut Vec<Lit>,
    ) -> L<Res> {
        let span = self.span(call);
        let id = self.bind(false, |l| l.part(rc, call, pre))?;
        let t = self.term_of(id);
        let key = format!("call {}", self.read_key_of(id));
        let var = match rc.reads.get(&key) {
            Some(Term::Var(v)) => {
                let v = v.clone();
                self.var_node(rc, span, &v)
            }
            _ => {
                let name = self.callee(call).unwrap_or_default();
                let base = capitalise(name.rsplit('.').next().unwrap_or_default());
                let v = fresh(rc, &base);
                pre.push(Lit::Eq(var(&v), t));
                rc.reads.insert(key, var(&v));
                self.building(rc, span, |b| b.bound_into(id, &v))
            }
        };
        let path = self.segs(rc, ops, pre)?;
        Ok(Res::Var { var, path })
    }

    /// A value name: `k(V)`, read once per rule. A `let` holding a
    /// reference (H-6) reads through it: `cfg.x` is `cfg(E), setting(E,
    /// "x", V)`. Looked up from `from` outward: `rc.scope`, or the scope
    /// `super` names.
    pub(super) fn value(
        &mut self,
        rc: &mut Rc,
        from: ScopeId,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Res> {
        let name = c.head.clone();
        let (decl, via) = self.value_decl(rc.scope, from, &name);
        let key = read_key(&name, &via);
        if !c.is_bare()
            && let Some(typ) = self.input_ref(from, &name)
        {
            return self.through_ref_input(rc, (c, &decl, via), typ, pre, span);
        }
        let ty = match self.value_type(from, &name) {
            Ok(t) => t,
            Err(e) => return self.error(span, e),
        };
        // A `let` holding a live object is its name alone; one holding a
        // resource is the reference (R-43).
        let Some(ty) = ty.filter(|t| !c.is_bare() || matches!(t, VType::Ref(_))) else {
            let path = self.segs(rc, &c.ops, pre)?;
            return Ok(Res::Value { decl, via, path });
        };
        let key = match rc.values.get(&key) {
            Some(v) => {
                let v = v.clone();
                self.var_node(rc, span, &v)
            }
            None => {
                let v = fresh(rc, &capitalise(&name));
                let read = self.value_read(rc, &v, (decl, via), pre, span);
                rc.values.insert(key, v.clone());
                read
            }
        };
        match ty {
            VType::Ref(typ) => {
                let path = self.segs(rc, &c.ops, pre)?;
                Ok(Res::Ref {
                    typ: self.str_node(span, &typ),
                    addr: key,
                    path,
                })
            }
            VType::World(typ) => {
                let segs = self.segs(rc, &c.ops, pre)?;
                let Some(path) = self.path_string(&segs) else {
                    return self.error(span, "a live object's path is constant");
                };
                Ok(Res::World {
                    typ,
                    addr: key,
                    path,
                })
            }
        }
    }

    /// `k.p` of an input `k` typed `ref(T)` (`c` the chain, `decl` and
    /// `via` the input as read): it holds the reference itself, so a dot
    /// reads through it, the address taken out of the reference (R-101),
    /// read once per rule. It is its user's, so no copy's scope goes in
    /// front of it.
    pub(super) fn through_ref_input(
        &mut self,
        rc: &mut Rc,
        (c, decl, via): (&Chain, &DeclRef, Reach),
        typ: String,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Res> {
        let key = format!("ref {}", read_key(&decl.name, &via));
        let read = match rc.values.get(&key) {
            Some(v) => {
                let v = v.clone();
                self.var_node(rc, span, &v)
            }
            None => {
                let name = fresh(rc, &capitalise(&decl.name));
                let rel = via.relation(&decl.name);
                let id = self.building(rc, span, |b| b.address_into((&rel, via), &typ, &name));
                self.hoisted_into(id, pre);
                rc.values.insert(key, name.clone());
                id
            }
        };
        let path = self.segs(rc, &c.ops, pre)?;
        let via = Reach::Absolute;
        Ok(Res::Ref {
            typ: self.str_node(span, &typ),
            addr: self.node(span, ExprKind::Scoped { name: read, via }),
            path,
        })
    }

    /// A bare name: a variable, unless it names something no variable may.
    pub(super) fn bare(&mut self, rc: &mut Rc, h: &str, span: Span) -> L<Res> {
        // A type named by one word (a program's own, `widget`) is that type.
        if !rc.vars.contains_key(h) && self.decls.types.contains(h) {
            return Ok(Res::Type(h.to_string()));
        }
        if !rc.vars.contains_key(h) {
            let what = if self.resource(rc.scope, h).is_some() {
                Some("the resource")
            } else if self.instance_in(rc.scope, h).is_some() {
                Some("the component's resource")
            } else if self.use_in(rc.scope, h).is_some() || self.stack_in(rc.scope, h).is_some() {
                Some("the module")
            } else if self.component_in(rc.scope, h).is_some() {
                Some("the component")
            } else if self.decls.namespaces.contains(h) {
                Some("the type namespace")
            } else if h == "world" {
                Some("the inventory")
            } else if h == "settings" {
                Some("the settings")
            } else {
                None
            };
            if let Some(what) = what {
                return self.error(
                    span,
                    format!(
                        "variable `{h}` shadows {what} `{h}`: rename the variable, or write \
                         `{h}.path` to read it"
                    ),
                );
            }
        }
        let v = self.var_named(rc, h, span);
        Ok(Res::Val(self.var_node(rc, span, &v)))
    }

    /// The parts after a reference or value, lowered.
    pub(super) fn segs(&mut self, rc: &mut Rc, ops: &[Op], pre: &mut Vec<Lit>) -> L<Vec<Seg>> {
        let mut out = Vec::new();
        for op in ops {
            match op {
                Op::Field(f) => out.push(Seg::F(f.clone())),
                Op::Index(ts, r) => {
                    if ts.len() != 1 {
                        return self.error(self.span_of(*r), "an index takes one term");
                    }
                    let keyed = match ts[0].kind() {
                        LITERAL => tokens(&ts[0]).next().is_some_and(|t| t.kind() == STRING),
                        CALL => true,
                        _ => Chain::of(&ts[0]).is_some_and(|c| !c.is_bare()),
                    };
                    let t = self.bind(true, |l| l.part(rc, &ts[0], pre))?;
                    out.push(if keyed { Seg::K(t) } else { Seg::I(t) });
                }
                Op::Keyed(_, r) => {
                    return self.error(
                        self.span_of(*r),
                        "`[k=v]` names a stack's deployment by its keys, after the name a \
                         `use` of the stack binds: `platform[env=e].output`",
                    );
                }
            }
        }
        Ok(out)
    }

    /// `settings[e].path`, `settings` alone: the settings rows and their
    /// pseudo-type are gone (R-38); a setting is an input, read by its name.
    pub(super) fn settings_read(&mut self, c: &Chain, span: Span) -> L<Res> {
        let path: Vec<String> = c
            .ops
            .iter()
            .skip_while(|o| !matches!(o, Op::Field(_)))
            .filter_map(|o| match o {
                Op::Field(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        let read = match path.is_empty() {
            true => "the input by its name".to_string(),
            false => format!("the input by its name, `{}`", path.join(".")),
        };
        let d = Diagnostic::error(span, "settings rows are gone (R-38): a setting is an input")
            .with_help(format!(
                "declare it, `input k: T = default`, give it per deployment with `set k = v \
                 where env == \"prod\"`, and read {read}"
            ));
        self.diags.push(d);
        Err(Skip)
    }

    /// `world.T[e].path`: the provider's inventory.
    pub(super) fn world(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Res> {
        let fields = c.fields();
        let typ = fields[1..].join(".");
        let rest = &c.ops[fields.len() - 1..];
        let Some(Op::Index(ts, _)) = rest.first() else {
            return self.error(span, "a live object is `world.T[name]`");
        };
        if ts.len() != 1 {
            return self.error(span, "a live object is `world.T[name]`");
        }
        let addr = self.bind(true, |l| l.part(rc, &ts[0], pre))?;
        let segs = self.segs(rc, &rest[1..], pre)?;
        let Some(path) = self.path_string(&segs) else {
            return self.error(span, "a live object's path is constant");
        };
        Ok(Res::World { typ, addr, path })
    }

    /// `T[e].path` for a resource of type `T` by its key, and the lookups
    /// `p[a, b]` and `ext[a]`. `T.n`, a resource by a dot after its type,
    /// is an error that names the spelling (H-10).
    pub(super) fn typed_path(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Option<Res>> {
        let fields = c.fields();
        let k = fields.len();
        // `T.n`: the longest `T` with a resource `n` of that type in scope.
        for i in (1..k).rev() {
            let typ = fields[..i].join(".");
            if self.resource_of_type(rc.scope, &typ, &fields[i]) {
                let n = &fields[i];
                let unique = self.resource(rc.scope, n).is_some_and(|ts| ts.len() == 1);
                let write = if unique {
                    n.clone()
                } else {
                    crate::address::Address {
                        typ,
                        name: n.clone(),
                    }
                    .to_string()
                };
                let d = Diagnostic::error(
                    span,
                    format!(
                        "`{}` names a resource by a dot after its type",
                        fields[..=i].join(".")
                    ),
                )
                .with_help(format!(
                    "a resource in scope is named `{write}` (H-10); `.` is static, `[ ]` a key"
                ));
                self.diags.push(d);
                return Err(Skip);
            }
        }
        let Some(Op::Index(ts, _)) = c.ops.get(k - 1) else {
            return Ok(None);
        };
        let name = fields.join(".");
        let rest = &c.ops[k..];
        if let Some(cols) = self.program.scopes.externs.get(&name).cloned() {
            let outs: Vec<usize> = cols
                .iter()
                .enumerate()
                .filter(|(_, (input, _))| !input)
                .map(|(i, _)| i)
                .collect();
            if outs.len() != 1 || ts.len() != cols.len() - 1 {
                return self
                    .error(
                        span,
                        format!(
                            "extern {name} is looked up with its inputs in brackets; it needs \
                             exactly one output"
                        ),
                    )
                    .map(Some);
            }
            let mut args = Vec::new();
            for t in ts {
                args.push(self.bind(true, |l| l.part(rc, t, pre))?);
            }
            let path = self.segs(rc, rest, pre)?;
            return Ok(Some(Res::Lookup {
                pred: name,
                args,
                out: outs[0],
                path,
            }));
        }
        if k == 1
            && self.program.scopes.declares_relation(&name)
            && !self.decls.types.contains(&name)
        {
            let mut args = Vec::new();
            for t in ts {
                args.push(self.bind(true, |l| l.part(rc, t, pre))?);
            }
            let path = self.segs(rc, rest, pre)?;
            let out = args.len();
            return Ok(Some(Res::Lookup {
                pred: name,
                args,
                out,
                path,
            }));
        }
        // A dotted name in no namespace a type or a provider `use` names
        // is no type: `web.spec.containers["web"]` reads the name `web`,
        // as `web.spec` does.
        let namespace = [&self.decls.namespaces, &self.decls.providers]
            .iter()
            .any(|ns| ns.contains(&c.head));
        if k > 1 && !namespace && !self.any_type {
            return Ok(None);
        }
        // `T[e]` in a namespace no declaration or builtin schema closes: a
        // provider schema's type the compiler does not read.
        let foreign = !self.decls.closed.contains(&c.head);
        if self.decls.types.contains(&name) || (k > 1 && (self.any_type || foreign)) {
            if ts.len() != 1 {
                return self.error(span, "a resource is `T[key]`").map(Some);
            }
            // `T["n"]` for a resource `n` in scope: it is named `n` (H-10),
            // unless `n` names something else too (R-76).
            if !self.any_type
                && let Some(LITERAL) = ts.first().map(|t| t.kind())
                && let Some(s) = tokens(&ts[0]).find(|t| t.kind() == STRING)
                && let Ok(n) = string_value(s.text())
                && !has_hole(s.text())
                && self
                    .resource(rc.scope, &n)
                    .is_some_and(|types| types == vec![name.clone()])
                && !self.names_other(rc.scope, &n)
            {
                let d = Diagnostic::error(
                    span,
                    format!("`{name}[\"{n}\"]` names the resource `{n}` in scope"),
                )
                .with_help(format!("write `{n}` (H-10)"));
                self.diags.push(d);
                return Err(Skip);
            }
            let addr = self.bind(true, |l| l.part(rc, &ts[0], pre))?;
            self.address_key(&self.term_of(addr), span)?;
            let path = self.segs(rc, rest, pre)?;
            return Ok(Some(Res::Ref {
                typ: self.str_node(span, &name),
                addr,
                path,
            }));
        }
        if k > 1 && self.decls.closed.contains(&c.head) {
            return self
                .error(
                    span,
                    format!(
                        "unknown type `{name}`: no resource header, `type` block or provider \
                         schema declares it"
                    ),
                )
                .map(Some);
        }
        self.error(span, format!("unknown relation or type `{name}`"))
            .map(Some)
    }

    /// An address written as a constant, `T["blue/vpc"]` or `"blue/vpc"
    /// in T`, is a path (R-112): `/` (and R-72's `::` before it) is an
    /// error naming the dot form, `"blue.vpc"`.
    pub(super) fn address_key(&mut self, addr: &Term, span: Span) -> L<()> {
        if let Term::Val(Value::Str(a)) = addr
            && let Some(fixed) = crate::address::old_scope(a)
        {
            let d = Diagnostic::error(
                span,
                format!("\"{a}\": an address is a path, its scope separated by `.`, not `/`"),
            )
            .with_help(format!("write \"{fixed}\" (R-112)"));
            self.diags.push(d);
            return Err(Skip);
        }
        Ok(())
    }
}

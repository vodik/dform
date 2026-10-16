//! A chain through a component or a stack (R-67, R-191, R-200, R-208):
//! a copy by its name or its key (`network[t].vpc`), a used module's
//! item, a copy's resource or output, a deployed stack's output; each
//! named alike in every declaration it has, and the test that a copy
//! named by its key exists before what reads it.

use super::*;

impl Lowerer<'_> {
    /// What a chain whose head names a scope reads (R-65): `n.k`, an
    /// output of the instance `n` (`n` alone, its scope); `c[t].k`, an
    /// output of each instance of the component `c` (`instance_of(c, T),
    /// output(T, k, V)`); `m.x`, the value `x` of a used module `m`; and a
    /// stack's deployment, `platform[env=e].k`.
    pub(super) fn scope_path(&mut self, rc: &mut Rc, c: &Chain, span: Span) -> L<Option<Res>> {
        let h = c.head.as_str();
        // Inside a module's file the module reads itself by its own name
        // (`backups.repository` in its component): its value or resource
        // is the one of the instance the read is in, as a bare read's is
        // (R-186).
        if let Some(from) = self.self_module(rc.scope, h)
            && let Some(Op::Field(x)) = c.ops.first()
        {
            let rest = Chain {
                head: x.clone(),
                head_kind: IDENT,
                call: None,
                range: c.range,
                ops: c.ops[1..].to_vec(),
            };
            if let Some(r) = self.item_read(rc, from, false, &rest, span)? {
                return Ok(Some(r));
            }
        }
        // Its other items under the name its user's `use` binds it to
        // (`use backups as b`).
        let alias = self.alias_in(rc.scope, h);
        let h = alias.as_deref().unwrap_or(h);
        if let Some(Op::Field(x)) = c.ops.first() {
            self.alternatives_agree(rc.scope, h, x, span)?;
        }
        if let Some((at, path)) = self.instance_in(rc.scope, h) {
            let name = self.str_node(span, h);
            let inst = self.scoped(rc.scope, at, name, span);
            return self
                .output_of(rc, (inst, None), &path, &c.ops, span)
                .map(Some);
        }
        if let Some(d) = self.stack_in(rc.scope, h) {
            return self.deployed_path(rc, c, &d, span).map(Some);
        }
        // A used stack by its full name, its module path (R-200):
        // `stacks.platform[env].out`, as `platform[env].out` reads it.
        if let Some((d, n)) = self.stack_by_path(rc.scope, h, &c.ops) {
            let short = Chain {
                head: d.name.clone(),
                head_kind: IDENT,
                call: None,
                range: c.range,
                ops: c.ops[n..].to_vec(),
            };
            return self.deployed_path(rc, &short, &d, span).map(Some);
        }
        // A component by a name in scope (`network`), a used module's
        // (`net.vpc`), or by its path from the root (`modules.net.vpc`).
        let mut ops = &c.ops[..];
        let mut path = match self.component_in(rc.scope, h) {
            Some(p) => Some(p),
            None => self.use_in(rc.scope, h),
        };
        if path.is_none() {
            let mut at = h.to_string();
            while let Some(Op::Field(f)) = ops.first() {
                if self.module_at(&at).is_some() {
                    break;
                }
                at = format!("{at}.{f}");
                ops = &ops[1..];
            }
            if !self.module_at(&at).is_some() {
                return Ok(None);
            }
            path = Some(at);
        }
        let Some(mut at) = path.take() else {
            return Ok(None);
        };
        while let Some(Op::Field(f)) = ops.first() {
            let next = format!("{at}.{f}");
            if !self.module_at(&next).is_some_and(|m| m.1) {
                break;
            }
            at = next;
            ops = &ops[1..];
        }
        let component = self.module_at(&at).is_some_and(|m| m.1);
        match ops.first() {
            Some(Op::Index(ts, _)) if component && ts.len() == 1 => {
                let mut t = self.bind(true, |l| l.part(rc, &ts[0]))?;
                // `c[_]` binds each copy (R-162): a name its resources'
                // addresses are made of (`c[_].vm`, R-191).
                if matches!(self.program.exprs[t].kind, ExprKind::Hole) {
                    let copy = fresh(rc, "Copy");
                    t = self.var_node(rc, span, &copy);
                }
                // The copies of `at` this body makes, or its user's.
                let own = self
                    .own_scopes(rc.scope)
                    .iter()
                    .find(|s| {
                        self.names(**s)
                            .instances()
                            .map(|(_, c)| c)
                            .any(|p| *p == at)
                    })
                    .copied();
                let at_scope = own.unwrap_or(self.root());
                let user = self.str_node(span, "");
                let parent = self.scoped(rc.scope, at_scope, user, span);
                self.copy_test(rc, (&at, parent, t), span);
                let inst = self.scoped(rc.scope, at_scope, t, span);
                // The test is the read's when it names the copy as the
                // read does: the body's own copies.
                let of = (inst == t).then(|| (at.clone(), parent));
                self.output_of(rc, (inst, of), &at, &ops[1..], span)
                    .map(Some)
            }
            _ if component => self
                .error(
                    span,
                    format!(
                        "{} is a component: read a copy's outputs by its name, `NAME.output`, \
                         or every copy's, `{}[t].output`",
                        at,
                        c.fields()[..c.fields().len() - ops.len().min(c.fields().len() - 1)]
                            .join(".")
                    ),
                )
                .map(Some),
            // A used module's item: a value (`config.region`, read as
            // `config::region`), an output, or a resource (`synapse.vm`,
            // the address `synapse::vm`).
            Some(Op::Field(x))
                if self.use_in(rc.scope, h).is_some() && ops.len() == c.ops.len() =>
            {
                let Some((module, _)) = self.module_at(&at) else {
                    return Err(Skip);
                };
                let declared = self
                    .chain_of(rc.scope)
                    .into_iter()
                    .find(|s| self.names(*s).module(h).is_some())
                    .unwrap_or(self.root());
                let own = self.names(module);
                if let Some(at) = own.value_at(x) {
                    let path = self.segs(rc, &ops[1..])?;
                    let decl = DeclRef {
                        scope: module,
                        name: x.clone(),
                        at,
                    };
                    let via = Reach::Use(h.to_string());
                    return Ok(Some(Res::Value { decl, via, path }));
                }
                if own.output(x).is_some() {
                    let name = self.str_node(span, h);
                    let inst = self.scoped(rc.scope, declared, name, span);
                    return self.output_of(rc, (inst, None), &at, ops, span).map(Some);
                }
                if let Some(types) = own.resources(x) {
                    if types.len() > 1 {
                        return self.ambiguous(x, &types, span).map(Some);
                    }
                    let name = self.str_node(span, h);
                    let scope = self.scoped(rc.scope, declared, name, span);
                    let path = self.segs(rc, &ops[1..])?;
                    let x = self.str_node(span, &crate::address::name_segment(x));
                    return Ok(Some(Res::Ref {
                        typ: self.str_node(span, &types[0]),
                        addr: self
                            .building(rc, span, |b| b.form(crate::address::SCOPED, vec![scope, x])),
                        path,
                    }));
                }
                self.error(
                    span,
                    format!("the module {at} has no value, output or resource `{x}`"),
                )
                .map(Some)
            }
            _ => self
                .error(
                    span,
                    format!("{at} is a module: `use {at}` to read its items, `NAME.x`"),
                )
                .map(Some),
        }
    }

    /// Each declaration of `h` in `s`, when it is declared more than once
    /// (R-104): the module or component path each binds it to, and its
    /// statement. A copy whose component names nothing is its own error.
    pub(super) fn alternatives(&self, s: ScopeId, h: &str) -> Option<Vec<(String, SyntaxNode)>> {
        let bound: Vec<_> = self.names(s).bound(h).collect();
        if bound.len() < 2 {
            return None;
        }
        let alts = bound.into_iter().filter_map(|d| match &d.kind {
            DeclKind::Use { written, .. } => {
                Some((self.module_path_of(s, written), d.node.clone()))
            }
            DeclKind::Copy { written, .. } => {
                let path = self.component_path(s, written).ok()?;
                Some((path, d.node.clone()))
            }
            _ => None,
        });
        Some(alts.collect())
    }

    /// `h.x` where `h` is declared more than once, each under a clause
    /// (R-104): every declaration has `x`, of one type, else the read is
    /// an error naming every declaration.
    pub(super) fn alternatives_agree(
        &mut self,
        scope: ScopeId,
        h: &str,
        x: &str,
        span: Span,
    ) -> L<()> {
        let Some(alts) = self
            .chain_of(scope)
            .into_iter()
            .find(|s| self.names(*s).bound(h).next().is_some())
            .and_then(|s| self.alternatives(s, h))
        else {
            return Ok(());
        };
        let mut found = Vec::new();
        for (path, n) in &alts {
            let what = match n.kind() {
                USE => format!("use {}", use_parts(n).0),
                _ => {
                    let (c, name) = copy_parts(n);
                    format!("resource {c} {name}")
                }
            };
            let item = self.module_at(path).and_then(|(m, _)| {
                let sc = self.names(m);
                if sc.output(x).is_some() {
                    let ty = sc.output_type(x).map_or("any", String::as_str);
                    return Some(format!("output {x}: {ty}"));
                }
                if let Some(i) = sc.input(x) {
                    let ty = node(i, TYPE_EXPR)
                        .map(|t| t.text().to_string().replace(char::is_whitespace, ""))
                        .unwrap_or_default();
                    return Some(format!("input {x}: {ty}"));
                }
                if sc.is_value(x) {
                    return Some(format!("let {x}"));
                }
                sc.resources(x)
                    .map(|ts| format!("resource {}", ts.join(", ")))
            });
            found.push((what, item, self.span(n)));
        }
        if let Some((what, _, _)) = found.iter().find(|(_, i, _)| i.is_none()) {
            let mut d = Diagnostic::error(span, format!("`{h}.{x}`: `{what}` has no `{x}`"));
            for (w, i, at) in &found {
                d = d.with_label(*at, format!("{w}: {}", i.as_deref().unwrap_or("none")));
            }
            self.diags.push(d.with_help(format!(
                "each declaration of `{h}` must have `{x}`: a component signature says what \
                 they all have, `type T = component {{ output {x}: TYPE }}`"
            )));
            return Err(Skip);
        }
        let first = &found[0].1;
        if found.iter().any(|(_, i, _)| i != first) {
            let mut d = Diagnostic::error(
                span,
                format!("`{h}.{x}` has another type in each declaration of `{h}`"),
            );
            for (w, i, at) in &found {
                d = d.with_label(*at, format!("{w}: {}", i.as_deref().unwrap_or("")));
            }
            self.diags.push(d.with_help(
                "a component signature unifies them: `type T = component { .. }` declares the \
                 inputs and outputs, and `component C: T { .. }` is checked against it",
            ));
            return Err(Skip);
        }
        Ok(())
    }

    /// The test that `copy` is a copy of `component` made in `scope`,
    /// `instance_of(component, scope, copy)`, before what reads the copy
    /// by its key: its goal, a test of the rule context.
    pub(super) fn copy_test(
        &mut self,
        rc: &mut Rc,
        (component, scope, copy): (&str, ExprId, ExprId),
        span: Span,
    ) {
        let pred = crate::modules::INSTANCE_OF;
        let of = self.str_node(span, component);
        let g = self.building(rc, span, |b| {
            let args = [of, b.settled(scope), b.settled(copy)]
                .into_iter()
                .map(|a| b.pattern_of(a))
                .collect();
            let rel = RelRef::own(pred, span);
            let args = RelArgs::Positional(args);
            b.goal_node(span, GoalKind::Rel { rel, args })
        });
        rc.tests.push((Some(scope), g));
    }

    /// `.k.path` after an instance's scope `inst`, of the component at
    /// `path`: its output `k`, a reference when the output is typed by a
    /// resource type; `of`, the test that the copy exists, made before the
    /// read when it is named by its key.
    pub(super) fn output_of(
        &mut self,
        rc: &mut Rc,
        (inst, of): (ExprId, Option<(String, ExprId)>),
        path: &str,
        ops: &[Op],
        span: Span,
    ) -> L<Res> {
        match ops.first() {
            None => Ok(Res::Val(inst)),
            Some(Op::Field(k)) => {
                let segs = self.segs(rc, &ops[1..])?;
                // A resource of the copy (R-191), `c[t].vm`: its address
                // under the copy's, `__scoped(t, "vm")`, as a used
                // module's `k3s.admin` is.
                if let Some(types) = self.copy_resource(path, k) {
                    if types.len() > 1 {
                        return self.ambiguous(k, &types, span);
                    }
                    let x = self.str_node(span, &crate::address::name_segment(k));
                    return Ok(Res::Ref {
                        typ: self.str_node(span, &types[0]),
                        addr: self
                            .building(rc, span, |b| b.form(crate::address::SCOPED, vec![inst, x])),
                        path: segs,
                    });
                }
                let typed = self
                    .module_at(path)
                    .and_then(|(m, _)| self.names(m).output(k).flatten().cloned());
                if let Some(t) = &typed
                    && !segs.is_empty()
                {
                    // A typed output: the address it holds, then a reference.
                    let read = PlanRead::Output {
                        copy: inst,
                        key: k.clone(),
                        of,
                    };
                    let v = self.read_var(rc, "output", read, 2, k, span);
                    return Ok(Res::Ref {
                        typ: self.str_node(span, t),
                        addr: v,
                        path: segs,
                    });
                }
                Ok(Res::Output {
                    inst,
                    key: k.clone(),
                    path: segs,
                    typ: typed,
                    of,
                })
            }
            Some(_) => self.error(span, "after a component's resource: `.output`"),
        }
    }

    /// The types of the resource `k` the component at `path` makes, when
    /// `k` names one of them and no output of it.
    pub(super) fn copy_resource(&self, path: &str, k: &str) -> Option<Vec<String>> {
        let (m, _) = self.module_at(path).filter(|m| m.1)?;
        let sc = self.names(m);
        if sc.output(k).is_some() {
            return None;
        }
        sc.resources(k)
    }

    /// `k` read of a deployment of `d` is one of its outputs (R-208): the
    /// stack's file is at hand, so a name it does not output is an error
    /// at the read, naming what it does; a key is the deployment's name.
    pub(super) fn deployed_output(&mut self, d: &Deployed, k: &str, span: Span) -> L<()> {
        let Some(outputs) = &d.outputs else {
            return Ok(());
        };
        if outputs.iter().any(|o| o == k) {
            return Ok(());
        }
        let (m, short) = (&d.path, &d.name);
        let d = match d.keys.iter().any(|x| x == k) {
            true => Diagnostic::error(
                span,
                format!(
                    "`{k}` is a key of {m}, not an output: it names the deployment, \
                     `{short}[{k}=..]`, and what is read of it is an output"
                ),
            ),
            false => Diagnostic::error(span, format!("{m} has no output `{k}`")),
        };
        let d = match outputs.as_slice() {
            [] => d.with_note(format!("{m} declares no output")),
            _ => d.with_note(format!("its outputs: {}", outputs.join(", "))),
        };
        self.diags.push(d);
        Err(Skip)
    }

    /// `NAME[k=v, ..].out.path`, or `NAME.out.path` unkeyed: an output of
    /// a stack's deployment (R-65), the instance `NAME[k=v,..]`, read from
    /// what it published. Each key is given once.
    pub(super) fn deployed_path(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        d: &Deployed,
        span: Span,
    ) -> L<Res> {
        let m = &c.head;
        let spelled = |keys: &[String]| {
            let ks: Vec<String> = keys.iter().map(|k| format!("{k}=..")).collect();
            format!("{m}[{}].OUTPUT", ks.join(", "))
        };
        let keys = d.keys.clone();
        // `NAME[env]`, every entry a bare name: the pun `NAME[env = env]`
        // (R-33), as beside a `k = v`.
        let punned: Option<Op> = match c.ops.first() {
            Some(Op::Index(ts, r)) if !keys.is_empty() => ts
                .iter()
                .map(|t| Some((bare_name(t)?, t.clone())))
                .collect::<Option<Vec<_>>>()
                .map(|entries| Op::Keyed(entries, *r)),
            _ => None,
        };
        let (name, rest) = match punned.as_ref().or(c.ops.first()) {
            Some(Op::Keyed(entries, r)) => {
                let given: BTreeSet<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
                let want: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
                if given != want || given.len() != entries.len() {
                    return self.error(
                        self.span_of(*r),
                        format!(
                            "a deployment of {} is named by each of its keys once: `{}`",
                            d.path,
                            spelled(&keys)
                        ),
                    );
                }
                let mut values = Vec::new();
                for k in &keys {
                    let (_, t) = entries.iter().find(|(x, _)| x == k).expect("checked");
                    let v = self.bind(true, |l| l.part(rc, t))?;
                    let e = &self.program.exprs[v];
                    let escaped = match (&e.kind, e.hoisted) {
                        (ExprKind::Lit(Value::Str(s)), None) => Some(crate::stack::escape(s)),
                        (ExprKind::Lit(v), None) => Some(crate::stack::escape(&spell::bare(v))),
                        _ => None,
                    };
                    values.push(match escaped {
                        Some(s) => self.str_node(span, &s),
                        None => v,
                    });
                }
                let text = |vs: Vec<String>| {
                    let kv: Vec<String> = keys
                        .iter()
                        .zip(vs)
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    format!("{}[{}]", d.name, kv.join(","))
                };
                let constant: Option<Vec<String>> = values
                    .iter()
                    .map(|v| match &self.program.exprs[*v].kind {
                        ExprKind::Lit(Value::Str(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                let name = match constant {
                    Some(vs) => self.str_node(span, &text(vs)),
                    None => {
                        let holes = text(vec!["%s".to_string(); keys.len()]);
                        let holes = self.str_node(span, &holes);
                        let args = std::iter::once(holes).chain(values).collect();
                        self.building(rc, span, |b| b.form(crate::address::FORMAT, args))
                    }
                };
                (name, &c.ops[1..])
            }
            Some(Op::Field(_)) if keys.is_empty() => (self.str_node(span, &d.name), &c.ops[..]),
            _ if keys.is_empty() => {
                return self.error(
                    span,
                    format!(
                        "{} is deployed: what is read of it is an output, `{m}.OUTPUT`",
                        d.path
                    ),
                );
            }
            _ => {
                return self.error(
                    span,
                    format!(
                        "{} is deployed, keyed by {}: read an output of one deployment, `{}`",
                        d.path,
                        keys.join(", "),
                        spelled(&keys)
                    ),
                );
            }
        };
        let Some(Op::Field(k)) = rest.first() else {
            return self.error(
                span,
                format!(
                    "{} is deployed: what is read of it is an output, `{m}[..].OUTPUT`",
                    d.path
                ),
            );
        };
        self.deployed_output(d, k, span)?;
        let path = self.segs(rc, &rest[1..])?;
        // The keyed read of a copy's output (`network[t].k`), the
        // deployment the instance: `instance_of(PATH, "", NAME), output(NAME,
        // k, V)`, both served from what it published (I-modules section 6).
        let root = self.root();
        let inst = self.scoped(rc.scope, root, name, span);
        let user = self.str_node(span, "");
        let user = self.scoped(rc.scope, root, user, span);
        self.copy_test(rc, (&d.path, user, inst), span);
        Ok(Res::Output {
            inst,
            key: k.clone(),
            path,
            typ: None,
            of: Some((d.path.clone(), user)),
        })
    }
}

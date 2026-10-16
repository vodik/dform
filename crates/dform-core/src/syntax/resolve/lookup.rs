//! A name from a scope outward (R-186, R-205): the scope tree the
//! program's names are written in, what each scope declares, how a read
//! in one scope reaches a declaration of another (its own, an enclosing
//! definition's, its user's), `super`, and what a name in scope is: a
//! value, a resource, an instance, a used module or stack, a component.

use super::*;

impl Lowerer<'_> {
    /// The scope a statement lowered in `scope` declares into: a file's top
    /// level declares into the program.
    pub(super) fn decl_scope(&self, scope: ScopeId) -> ScopeId {
        self.program.scopes.declaring(scope)
    }

    /// What `scope` declares.
    pub(super) fn names(&self, scope: ScopeId) -> &Names {
        &self.program.scopes.names[scope]
    }

    pub(super) fn names_mut(&mut self, scope: ScopeId) -> &mut Names {
        &mut self.program.scopes.names[scope]
    }

    /// The program's scope.
    pub(super) fn root(&self) -> ScopeId {
        self.program.scopes.root
    }

    /// The path of the module whose file `file` is (R-65); `None` for an
    /// entry file.
    pub(super) fn module_path(&self, file: u32) -> Option<String> {
        let scope = self.program.scopes.of_file(file)?;
        match self.program.scopes.definition(scope) {
            Some((path, false)) => Some(path.clone()),
            _ => None,
        }
    }

    /// The top level of `file`, collected.
    pub(super) fn file_scope(&self, file: u32) -> ScopeId {
        self.program
            .scopes
            .of_file(file)
            .expect("every file is collected")
    }

    /// The body of the component whose statement is at `start` in `file`,
    /// collected.
    pub(super) fn block_scope(&self, file: u32, start: u32) -> ScopeId {
        let scopes = &self.program.scopes;
        scopes
            .of_block(file, start)
            .expect("every component is collected")
    }

    /// The body of the module or component at `path`, and whether it is a
    /// component's.
    pub(super) fn module_at(&self, path: &str) -> Option<(ScopeId, bool)> {
        let s = self.program.scopes.by_path(path)?;
        Some((s, self.program.scopes.is_component(s)))
    }

    /// The names a rule at `rc` may read: its variables and what its
    /// scopes declare (inputs, `let`s, resources, copies, components,
    /// modules), for the name an unknown one is nearest.
    pub(super) fn scope_names(&self, rc: &Rc) -> Vec<String> {
        let mut out: Vec<String> = rc.vars.keys().cloned().collect();
        for s in self.chain_of(rc.scope) {
            let s = self.names(s);
            out.extend(s.names(DeclKind::is_value).cloned());
            out.extend(s.names(|k| matches!(k, DeclKind::Resource(_))).cloned());
            out.extend(s.instances().map(|(n, _)| n.clone()));
            out.extend(s.names(|k| matches!(k, DeclKind::Component(_))).cloned());
            out.extend(s.uses().map(|(n, _)| n.clone()));
        }
        out
    }

    pub(super) fn chain_of(&self, scope: ScopeId) -> Vec<ScopeId> {
        self.program.scopes.chain(scope)
    }

    /// The scopes from `scope` out to the body it is in ([`Scopes::own`]).
    pub(super) fn own_scopes(&self, scope: ScopeId) -> Vec<ScopeId> {
        self.program.scopes.own(scope)
    }

    /// An instance's scope `name`, written at `span`, as a read in `scope`
    /// of what `declared` declares reaches it: the name itself when the
    /// copy is the body's own (the copies being instanced put the body's
    /// scope in front of it), else as a scope no copy of the body is taken
    /// from reaches it (`Reach::Absolute`).
    pub(super) fn scoped(
        &mut self,
        scope: ScopeId,
        declared: ScopeId,
        name: ExprId,
        span: Span,
    ) -> ExprId {
        match self.own_scopes(scope).contains(&declared) {
            true => name,
            false => {
                let via = Reach::Absolute;
                self.node(span, ExprKind::Scoped { name, via })
            }
        }
    }

    /// The address of the resource `name` in scope, as a read in `scope`
    /// writes it, at `span`: the body's own is relative (expansion scopes
    /// it), its user's reached as written (`scoped`), an enclosing body's
    /// its instance's (`lexical`).
    pub(super) fn resource_addr(&mut self, scope: ScopeId, name: &str, span: Span) -> ExprId {
        self.resource_addr_from(scope, scope, name, span)
    }

    /// [`Self::resource_addr`] of `name` looked up from `from` outward:
    /// `super.x` looks up from the enclosing scope (R-186).
    pub(super) fn resource_addr_from(
        &mut self,
        scope: ScopeId,
        from: ScopeId,
        name: &str,
        span: Span,
    ) -> ExprId {
        let declared = self
            .chain_of(from)
            .into_iter()
            .find(|s| self.names(*s).resources(name).is_some())
            .unwrap_or(from);
        let segment = crate::address::name_segment(name);
        let seg = self.node(span, ExprKind::Lit(Value::Str(segment.into_owned())));
        match self.lexical(scope, declared) {
            Some(body) if !body.is_empty() => {
                let via = Reach::Lexical(body);
                self.node(span, ExprKind::Scoped { name: seg, via })
            }
            _ => self.scoped(scope, declared, seg, span),
        }
    }

    /// The definition whose body `declared` is, when a read in `scope`
    /// reaches it past its own body ([`Scopes::lexical`], R-186).
    pub(super) fn lexical(&self, scope: ScopeId, declared: ScopeId) -> Option<String> {
        self.program.scopes.lexical(scope, declared)
    }

    /// The value `name` read in `scope`, looked up from `from` outward:
    /// the scope that declares it, and how the read reaches it, its own
    /// or an enclosing body's (`lexical`), whose relation is that body's
    /// item marked for expansion ([`Reach::relation`]).
    pub(super) fn value_decl(&self, scope: ScopeId, from: ScopeId, name: &str) -> (DeclRef, Reach) {
        let declared = self
            .chain_of(from)
            .into_iter()
            .find(|s| self.names(*s).is_value(name));
        let via = match declared.and_then(|d| self.lexical(scope, d)) {
            Some(body) => Reach::Lexical(body),
            None => Reach::Own,
        };
        let decl = DeclRef {
            scope: declared.unwrap_or(from),
            name: name.to_string(),
            at: declared
                .and_then(|d| self.names(d).value_at(name))
                .unwrap_or(0),
        };
        (decl, via)
    }

    /// How a read in `scope` of the relation `p`, looked up from `from`
    /// outward, reaches it: an enclosing body's own relation lexically, as
    /// [`Self::value_decl`] reaches a value; anything else as its own.
    pub(super) fn relation_reach(&self, scope: ScopeId, from: ScopeId, p: &str) -> Reach {
        if !crate::modules::is_private(p) {
            return Reach::Own;
        }
        let declared = self.chain_of(from).into_iter().find(|s| {
            let sc = self.names(*s);
            sc.is_relation(p) || sc.takes(p)
        });
        match declared.and_then(|d| self.lexical(scope, d)) {
            Some(body) => Reach::Lexical(body),
            None => Reach::Own,
        }
    }

    /// The scope `super` names in `scope` (R-186): the one around the
    /// component the read is in, one level per `super`, the program's for
    /// a component of the stack's file. A module's body has none: a
    /// module never reaches its user.
    /// `outer` counts the `super`s before this one: `super.super.x` past a
    /// component's module is the module's error too.
    pub(super) fn super_scope(
        &mut self,
        scope: ScopeId,
        item: &str,
        outer: usize,
        span: Span,
    ) -> L<ScopeId> {
        let body = self.program.scopes.body(scope);
        let module = self
            .program
            .scopes
            .definition(body)
            .map(|(p, c)| (p.clone(), c));
        match module {
            Some((_, true)) => {}
            Some((path, false)) => {
                let help = match self.is_value(body, item) || self.resource(body, item).is_some() {
                    true => format!(
                        "read module {path}'s own as `{}{item}`",
                        "super.".repeat(outer)
                    ),
                    false => format!(
                        "a module never reaches its user: declare `input {item}: TYPE` in it and \
                         give it in the `use`, `use {path} {{ {item} = .. }}`"
                    ),
                };
                let d = Diagnostic::error(
                    span,
                    format!("`super` in module {path}: a module has no scope around it"),
                )
                .with_help(help);
                self.diags.push(d);
                return Err(Skip);
            }
            None => {
                let d = Diagnostic::error(
                    span,
                    "`super` at the stack's top level: it names the scope around a component",
                )
                .with_help(format!("read `{item}` bare"));
                self.diags.push(d);
                return Err(Skip);
            }
        }
        Ok(self.program.scopes.around(body))
    }

    /// The scope a chain's leading `super`s name and the chain after
    /// them (`super.repository.metadata.name`: the module's scope and
    /// `repository.metadata.name`).
    pub(super) fn super_chain(&mut self, rc: &Rc, c: &Chain, span: Span) -> L<(ScopeId, Chain)> {
        let supers = c
            .ops
            .iter()
            .take_while(|o| matches!(o, Op::Field(f) if f == "super"))
            .count();
        let Some(Op::Field(item)) = c.ops.get(supers) else {
            return self.error(
                span,
                "`super` is the scope around this component: read an item of it, `super.NAME`",
            );
        };
        let mut from = rc.scope;
        for outer in 0..=supers {
            from = self.super_scope(from, item, outer, span)?;
        }
        let rest = Chain {
            head: item.clone(),
            head_kind: IDENT,
            call: None,
            range: c.range,
            ops: c.ops[supers + 1..].to_vec(),
        };
        Ok((from, rest))
    }

    /// `super.x.path` (R-186): what the scope around this component reads
    /// `x` as, a value or a resource, when the component's own `x` shadows
    /// it.
    pub(super) fn super_read(
        &mut self,
        rc: &mut Rc,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Res> {
        let (from, rest) = self.super_chain(rc, c, span)?;
        if let Some(r) = self.item_read(rc, from, true, &rest, pre, span)? {
            return Ok(r);
        }
        let (x, around) = (&rest.head, self.scope_name(from));
        self.error(
            span,
            format!("`super.{x}`: {around} has no value or resource `{x}`"),
        )
    }

    /// The value or the resource `c.head` of the scope `from` (and, when
    /// `outward`, the scopes around it), read in `rc.scope` with `c`'s
    /// path: `super.x` and a module's read of itself by its name (R-186).
    /// `None` when it declares neither.
    pub(super) fn item_read(
        &mut self,
        rc: &mut Rc,
        from: ScopeId,
        outward: bool,
        c: &Chain,
        pre: &mut Vec<Lit>,
        span: Span,
    ) -> L<Option<Res>> {
        let x = c.head.as_str();
        let here = self.names(from);
        if (outward && self.is_value(from, x)) || here.is_value(x) {
            return self.value(rc, from, c, pre, span).map(Some);
        }
        let types = match outward {
            true => self.resource(from, x),
            false => here.resources(x),
        };
        let Some(types) = types else {
            return Ok(None);
        };
        if types.len() > 1 {
            return self.ambiguous(x, &types, span).map(Some);
        }
        let path = self.segs(rc, &c.ops, pre)?;
        Ok(Some(Res::Ref {
            typ: self.str_node(span, &types[0]),
            addr: self.resource_addr_from(rc.scope, from, x, span),
            path,
        }))
    }

    /// The scope of the module `h` names when the read in `scope` is in
    /// that module's own file and no `use` there binds `h`: `backups` in
    /// backups.df reads the module itself, the instance the read is in.
    pub(super) fn self_module(&self, scope: ScopeId, h: &str) -> Option<ScopeId> {
        let module = self.module_at(h).filter(|(_, c)| !c)?.0;
        for s in self.chain_of(scope) {
            if self.names(s).module(h).is_some() {
                return None;
            }
            if s == module {
                return Some(s);
            }
        }
        None
    }

    /// A scope as a message names it: `module backups`, `component
    /// backups.volume`, `the stack`.
    pub(super) fn scope_name(&self, scope: ScopeId) -> String {
        self.program.scopes.describe(scope)
    }

    /// The instance `name` in scope: the scope that declares it and its
    /// component's path.
    pub(super) fn instance_in(&self, scope: ScopeId, name: &str) -> Option<(ScopeId, String)> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.names(s).instance(name).map(|p| (s, p.clone())))
    }

    /// The stack `name` a `use` in scope binds.
    pub(super) fn stack_in(&self, scope: ScopeId, name: &str) -> Option<Deployed> {
        self.chain_of(scope).into_iter().find_map(|s| {
            self.names(s)
                .stack(name)
                .map(|i| self.decls.deployed[i].clone())
        })
    }

    /// The stack a `use` in scope binds that `head.f1.f2..` names by its
    /// module path (`stacks.platform`), and how many of `ops` the path
    /// takes.
    pub(super) fn stack_by_path(
        &self,
        scope: ScopeId,
        head: &str,
        ops: &[Op],
    ) -> Option<(Deployed, usize)> {
        let mut at = head.to_string();
        for (i, op) in ops.iter().enumerate() {
            let Op::Field(f) = op else { return None };
            at = format!("{at}.{f}");
            let found = self.chain_of(scope).into_iter().find_map(|s| {
                self.names(s)
                    .stacks()
                    .map(|i| &self.decls.deployed[i])
                    .find(|d| d.path == at)
                    .cloned()
            });
            if let Some(d) = found {
                return Some((d, i + 1));
            }
        }
        None
    }

    /// The module `name` a `use` in scope binds: its path.
    pub(super) fn use_in(&self, scope: ScopeId, name: &str) -> Option<String> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.names(s).module(name).cloned())
    }

    /// The name a `use` in scope binds the module `path` to, when `scope`
    /// is in that module's file and `path` is bound by no `use` of its
    /// own name (`use backups as b`: `b`).
    pub(super) fn alias_in(&self, scope: ScopeId, path: &str) -> Option<String> {
        let module = self.module_at(path)?.0;
        let chain = self.chain_of(scope);
        if !chain.contains(&module) || self.use_in(scope, path).is_some() {
            return None;
        }
        chain.into_iter().find_map(|s| {
            let mut uses = self.names(s).uses();
            uses.find(|(_, p)| *p == path).map(|(n, _)| n.clone())
        })
    }

    /// The component `name` reads as in scope: its path.
    pub(super) fn component_in(&self, scope: ScopeId, name: &str) -> Option<String> {
        self.chain_of(scope).into_iter().find_map(|s| {
            let sc = self.names(s);
            sc.component(name).or_else(|| sc.copied(name)).cloned()
        })
    }

    /// Whether `name` is an instance in scope whose component names
    /// nothing: its `instance` statement is the error.
    pub(super) fn unbound_instance(&self, scope: ScopeId, name: &str) -> bool {
        self.chain_of(scope)
            .into_iter()
            .any(|s| self.names(s).unbound(name))
    }

    /// A `let`'s rows and the scope that declares it.
    pub(super) fn find_let(
        &self,
        scope: ScopeId,
        name: &str,
    ) -> Option<(ScopeId, Vec<SyntaxNode>)> {
        self.chain_of(scope).into_iter().find_map(|s| {
            let rows: Vec<SyntaxNode> = self
                .names(s)
                .lets(name)
                .filter_map(|n| terms(n).next())
                .collect();
            (!rows.is_empty()).then_some((s, rows))
        })
    }

    /// The input `name` in scope when its type is a resource type, `ref(T)`
    /// or `T` (R-101): `T`.
    pub(super) fn input_ref(&self, scope: ScopeId, name: &str) -> Option<String> {
        let n = self
            .chain_of(scope)
            .into_iter()
            .find_map(|s| self.names(s).input(name).cloned())?;
        let ty = node(&n, TYPE_EXPR)?;
        let typ = dotted_text(&ty, 0);
        let typ = match node(&ty, TYPE_EXPR) {
            Some(inner) if typ == "ref" => dotted_text(&inner, 0),
            Some(_) => return None,
            None => typ,
        };
        (typ.contains('.') && self.decls.types.contains(&typ)).then_some(typ)
    }

    /// The static type of a value name whose value is a reference (H-6):
    /// what every row of its `let` names, read from the rows' text. `Err`
    /// names the rows' types when they disagree.
    pub(super) fn value_type(&self, scope: ScopeId, name: &str) -> Result<Option<VType>, String> {
        self.value_type_depth(scope, name, 0)
    }

    pub(super) fn value_type_depth(
        &self,
        scope: ScopeId,
        name: &str,
        depth: usize,
    ) -> Result<Option<VType>, String> {
        let Some((at, rows)) = self.find_let(scope, name) else {
            return Ok(None);
        };
        if depth > 8 {
            return Ok(None);
        }
        let mut ty: Option<Option<VType>> = None;
        for row in &rows {
            let t = self.term_vtype(at, row, depth);
            match &ty {
                None => ty = Some(t),
                Some(u) if *u == t => {}
                Some(u) => {
                    let show = |v: &Option<VType>| match v {
                        None => "a value".to_string(),
                        Some(VType::Ref(t)) => format!("a {t} reference"),
                        Some(VType::World(t)) => format!("a live {t}"),
                    };
                    return Err(format!(
                        "`let {name}` is {} in one row and {} in another",
                        show(u),
                        show(&t)
                    ));
                }
            }
        }
        Ok(ty.flatten())
    }

    /// The reference a `let` row's term names, if it names one: a resource
    /// (by name in scope or `T[e]`), a live object, or another `let`
    /// holding one.
    pub(super) fn term_vtype(&self, scope: ScopeId, t: &SyntaxNode, depth: usize) -> Option<VType> {
        match declared_ref(t).filter(|typ| !self.names_alias(scope, typ)) {
            Some(typ) => Some(VType::Ref(typ)),
            None => self.written_vtype(scope, t, depth),
        }
    }

    /// The reference a `let` row's term names as written, whatever the
    /// row declares.
    pub(super) fn written_vtype(
        &self,
        scope: ScopeId,
        t: &SyntaxNode,
        depth: usize,
    ) -> Option<VType> {
        let c = Chain::of(t)?;
        let index_then_end = |ops: &[Op]| matches!(ops, [Op::Index(ts, _)] if ts.len() == 1);
        if c.head == "world" {
            let fields = c.fields();
            let rest = &c.ops[fields.len() - 1..];
            return (fields.len() > 1 && index_then_end(rest))
                .then(|| VType::World(fields[1..].join(".")));
        }
        if c.is_bare() {
            if self.is_value(scope, &c.head) {
                return self
                    .value_type_depth(scope, &c.head, depth + 1)
                    .ok()
                    .flatten();
            }
            return match self.resource(scope, &c.head) {
                Some(types) if types.len() == 1 => Some(VType::Ref(types[0].clone())),
                _ => None,
            };
        }
        let fields = c.fields();
        let rest = &c.ops[fields.len() - 1..];
        let typ = fields.join(".");
        (self.decls.types.contains(&typ) && index_then_end(rest)).then_some(VType::Ref(typ))
    }

    /// A read whose head names a resource and something else in scope is
    /// an error (R-76): a scope's reads are one namespace, so `nodes` with
    /// `input nodes` and `resource net.vpc nodes` names two things. A
    /// module's item (`config.base_domain`), an instance's output and a
    /// stack's or a component's copy (`platform[env]`) read the other one;
    /// the resource always reads by its type, `net.vpc["nodes"]`.
    pub(super) fn shared_name(&mut self, scope: ScopeId, c: &Chain, span: Span) -> L<bool> {
        let h = c.head.as_str();
        let Some(types) = self.resource(scope, h) else {
            return Ok(false);
        };
        let field = match c.ops.first() {
            Some(Op::Field(f)) => Some(f.as_str()),
            _ => None,
        };
        let indexed = matches!(c.ops.first(), Some(Op::Index(..) | Op::Keyed(..)));
        let items = |sc: &Names| -> Vec<String> {
            sc.names(DeclKind::is_value)
                .chain(sc.names(|k| matches!(k, DeclKind::Output { .. })))
                .chain(sc.names(|k| matches!(k, DeclKind::Resource(_))))
                .cloned()
                .collect()
        };
        let (what, other) = if let Some(s) = self
            .chain_of(scope)
            .into_iter()
            .find(|s| self.names(*s).is_value(h))
        {
            let kind = if self.names(s).input(h).is_some() {
                "the input"
            } else {
                "the let"
            };
            (kind.to_string(), None)
        } else if let Some(path) = self.use_in(scope, h) {
            let own = self
                .module_at(&path)
                .map(|(m, _)| items(self.names(m)))
                .unwrap_or_default();
            if field.is_some_and(|f| own.iter().any(|i| i == f)) {
                return Ok(true);
            }
            (
                "the module".to_string(),
                own.first().map(|i| format!("`{h}.{i}`")),
            )
        } else if let Some((_, path)) = self.instance_in(scope, h) {
            let outputs: Vec<String> = self
                .module_at(&path)
                .map(|(m, _)| {
                    let output = |k: &DeclKind| matches!(k, DeclKind::Output { .. });
                    self.names(m).names(output).cloned().collect()
                })
                .unwrap_or_default();
            if field.is_some_and(|f| outputs.iter().any(|o| o == f)) {
                return Ok(true);
            }
            (
                format!("the resource {h} of the component {path}"),
                outputs.first().map(|o| format!("`{h}.{o}`")),
            )
        } else if self.stack_in(scope, h).is_some() || self.component_in(scope, h).is_some() {
            // A stack or a component is read by a copy, `platform[env]`,
            // which a resource never is: no read is ambiguous.
            return Ok(indexed);
        } else {
            return Ok(false);
        };
        let rest: String = c
            .ops
            .iter()
            .map(|o| match o {
                Op::Field(f) => format!(".{f}"),
                Op::Index(ts, _) => format!(
                    "[{}]",
                    ts.iter()
                        .map(|t| t.text().to_string().trim().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Op::Keyed(kv, _) => format!(
                    "[{}]",
                    kv.iter()
                        .map(|(k, v)| format!("{k}={}", v.text().to_string().trim()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })
            .collect();
        let resources = types
            .iter()
            .map(|t| format!("{t}[\"{h}\"]"))
            .collect::<Vec<_>>();
        let resource = match &resources[..] {
            [one] => format!("the resource {one}"),
            _ => format!("the resources {}", resources.join(", ")),
        };
        let read = resources
            .iter()
            .map(|r| format!("{r}{rest}"))
            .collect::<Vec<_>>()
            .join(" or ");
        let help = match other {
            Some(o) => format!("read {o} or {read}"),
            None => format!("read {read}, or name one of them otherwise"),
        };
        self.error(span, format!("`{h}` names {what} and {resource}: {help}"))
    }

    /// Inside a module's or a component's body, what the body declares
    /// wins a read over what its user's scope brings in (R-101): the
    /// module's own `resource k8s.namespace traefik` over its user's `use
    /// traefik`. `Some(true)`: the body's resource is read; `Some(false)`:
    /// the body's value, module or copy is; `None`: neither is the body's
    /// alone (the program's top level, where R-76's ambiguity stands).
    pub(super) fn own_wins(&self, scope: ScopeId, name: &str) -> Option<bool> {
        let own = self.own_scopes(scope);
        let names = |s: &Names| {
            s.is_value(name)
                || s.module(name).is_some()
                || s.instance(name).is_some()
                || s.stack(name).is_some()
        };
        let resource = |s: &Names| s.resources(name).is_some();
        let (res_own, other_own) = own.iter().fold((false, false), |(r, o), s| {
            let sc = self.names(*s);
            (r || resource(sc), o || names(sc))
        });
        if res_own && !other_own && self.names_other(scope, name) {
            return Some(true);
        }
        if other_own && !res_own && self.resource(scope, name).is_some() {
            return Some(false);
        }
        None
    }

    /// Whether `name` names a value, a used module or an instance in
    /// scope: what a resource's bare name may share ambiguously.
    pub(super) fn names_other(&self, scope: ScopeId, name: &str) -> bool {
        self.is_value(scope, name)
            || self.use_in(scope, name).is_some()
            || self.instance_in(scope, name).is_some()
    }

    pub(super) fn is_value(&self, scope: ScopeId, name: &str) -> bool {
        self.chain_of(scope)
            .into_iter()
            .any(|s| self.names(s).is_value(name))
    }

    /// The types a resource name has in scope (the innermost scope that
    /// declares it).
    pub(super) fn resource(&self, scope: ScopeId, name: &str) -> Option<Vec<String>> {
        self.chain_of(scope)
            .into_iter()
            .find_map(|s| self.names(s).resources(name))
    }

    pub(super) fn resource_of_type(&self, scope: ScopeId, typ: &str, name: &str) -> bool {
        self.chain_of(scope).into_iter().any(|s| {
            self.names(s)
                .resources(name)
                .is_some_and(|ts| ts.iter().any(|t| t == typ))
        })
    }
}

//! Each file's statements, one item each (R-211 step 5): a file's items
//! or the module it is, the statement dispatched by its kind (a
//! component's body in its own scope), a statement's rule context, the
//! placeholders (`_`) a statement may hold, a key in a module, and the
//! spans and errors the resolver says.

use super::*;

impl Lowerer<'_> {
    /// The items of the file `i`: its statements', or, a module's file,
    /// the module it is (R-65); then its doc comments'.
    pub(super) fn unit(&mut self, i: usize) -> Vec<ItemId> {
        let unit = &self.units[i];
        let (file, root) = (unit.file, unit.root.clone());
        let saved = (self.file, self.item_scope);
        self.file = file;
        let scope = self.file_scope(file);
        let module = self.module_path(file);
        self.item_scope = scope;
        let mut items = Vec::new();
        for n in root.children() {
            match n.kind() {
                // `edition` is a syntax error (R-68).
                ERROR | EDITION => {}
                _ => items.extend(self.stmt(&n, scope, &Rc::default())),
            }
        }
        // A module's file lowers to the module it is (R-65), named by its
        // path.
        if let Some(path) = module {
            let r = root.text_range();
            let span = Span {
                end: u32::from(r.start()),
                ..self.span_of(r)
            };
            items = vec![self.program.module(path, scope, items, span)];
        }
        // Each doc comment is a `doc(Kind, Name, Key, Value)` fact per pair
        // (docs/grammar.md "Doc comments").
        if !self.text && !self.any_type {
            for d in crate::syntax::doc::collect(&root) {
                if d.pairs.is_empty() {
                    continue;
                }
                let span = self.span_of(d.range);
                let (kind, name, pairs) = (d.kind, d.name, d.pairs);
                let kind = crate::program::ItemKind::Doc { kind, name, pairs };
                items.push(self.program.item(span, self.item_scope, kind));
            }
        }
        (self.file, self.item_scope) = saved;
        items
    }

    pub(super) fn stmts(
        &mut self,
        block: Option<SyntaxNode>,
        scope: ScopeId,
        outer: &Rc,
    ) -> Vec<ItemId> {
        let Some(block) = block else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for n in block.children().filter(|n| n.kind() != ERROR) {
            out.extend(self.stmt(&n, scope, outer));
        }
        out
    }

    /// A statement's item: what it lowers to, with the helper rules it
    /// generated after it. `outer` carries the variables an enclosing
    /// `for`/`when` binds.
    pub(super) fn stmt(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> Vec<ItemId> {
        if n.kind() == COMPONENT {
            return vec![self.component(n, scope, outer)];
        }
        let gathered = self.gather.as_mut().map(|g| g.begin());
        let item = self.stmt_item(n, scope, outer);
        if let (Some(g), Some(outer)) = (&mut self.gather, gathered) {
            g.resume(outer);
        }
        item
    }

    /// [`Self::stmt`], the statement's variables and clause gathered on
    /// their own: a `set` through a relation is several items.
    pub(super) fn stmt_item(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> Vec<ItemId> {
        match n.kind() {
            SET => self.ported_all(n, |l| l.set(n, scope, outer)),
            _ => self.one_item(n, scope, outer).into_iter().collect(),
        }
    }

    /// [`Self::stmt_item`] of a statement that is one item.
    fn one_item(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> Option<ItemId> {
        if let Some(value) = self.literal_let(n) {
            return self.let_literal(n, scope, &value);
        }
        match n.kind() {
            LET if node(n, PARAMS).is_none() => self.ported(n, |l| l.let_stmt(n, scope, outer)),
            LET => self.ported(n, |l| l.function_stmt(n, scope, outer)),
            RULE | FACT => self.ported(n, |l| l.rule(n, scope, outer)),
            CHECK => self.ported(n, |l| l.check(n, scope, outer)),
            OUTPUT_DECL => self.ported(n, |l| l.output(n, scope, outer)),
            INPUT => self.ported(n, |l| l.input(n, scope, outer)),
            DECL => Some(self.decl(n, scope)),
            EXTERN => self.extern_item(n).ok(),
            TYPE_DECL => self.ported(n, |l| l.type_block(n, scope)),
            USE if provider_use(n, self.units, &self.decls.deployed).is_some() => {
                self.ported(n, |l| {
                    l.redeclared(n, &use_parts(n).1)?;
                    l.provider(n, scope, outer)
                })
            }
            INPUT_RELATION => self.ported(n, |l| l.relation_input(n, scope, outer)),
            RESOURCE if self.decls.project.contains(&self.file) => {
                self.ported(n, |l| l.deployment(n, scope, outer))
            }
            RESOURCE if !self.is_copy(n) => self.ported(n, |l| l.block_stmt(n, scope, outer)),
            RESOURCE => self.ported(n, |l| l.instance(n, scope, outer)),
            USE => self.ported_maybe(n, |l| l.use_stmt(n, scope, outer)),
            // An alias lowers to nothing: each use is its type.
            TYPE_ALIAS => None,
            k => {
                let _: L<()> = self.error(self.span(n), format!("unexpected {k:?}"));
                None
            }
        }
    }

    /// A ported statement's item (R-211 step 5), built while its body's
    /// goals are gathered, with the helper rules and aggregates of its
    /// own; checked for `_` as what it lowers to.
    pub(super) fn ported(
        &mut self,
        n: &SyntaxNode,
        build: impl FnOnce(&mut Self) -> L<ItemId>,
    ) -> Option<ItemId> {
        self.ported_maybe(n, |l| build(l).map(Some))
    }

    /// [`Self::ported`] of a statement that may lower to nothing (`use std.x`,
    /// `use` of a stack's deployments).
    pub(super) fn ported_maybe(
        &mut self,
        n: &SyntaxNode,
        build: impl FnOnce(&mut Self) -> L<Option<ItemId>>,
    ) -> Option<ItemId> {
        let items = self.ported_all(n, |l| build(l).map(|i| i.into_iter().collect()));
        items.into_iter().next()
    }

    /// [`Self::ported`] of a statement that is any number of items.
    fn ported_all(
        &mut self,
        n: &SyntaxNode,
        build: impl FnOnce(&mut Self) -> L<Vec<ItemId>>,
    ) -> Vec<ItemId> {
        let saved = std::mem::take(&mut self.helpers);
        let aggs = std::mem::take(&mut self.aggs);
        let own = self.gather.is_none();
        if own {
            self.gather = Some(Box::default());
        }
        let items = build(self);
        if own {
            self.gather = None;
        }
        self.aggs = aggs;
        self.helpers = saved;
        let Ok(items) = items else {
            return Vec::new();
        };
        if !(self.core || self.lenient || self.text || self.any_type) {
            for &item in &items {
                let stmts = crate::program::lower_item(&self.program, item);
                if self.placeholders(&stmts, self.span(n)).is_err() {
                    return Vec::new();
                }
            }
        }
        items
    }

    /// `component NAME [: S] { .. }`: its statements' items in a scope of
    /// its own.
    pub(super) fn component(&mut self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> ItemId {
        let span = self.span(n);
        let start: u32 = n.text_range().start().into();
        let inner = self.block_scope(self.file, start);
        let name = word_text(n, 1);
        let path = self
            .names(self.decl_scope(scope))
            .component(&name)
            .cloned()
            .unwrap_or(name);
        if let Some(t) = node(n, TYPE_EXPR) {
            self.check_signature(n, &t, scope);
        }
        let saved = std::mem::replace(&mut self.item_scope, inner);
        let body = self.stmts(node(n, STMT_BLOCK), inner, outer);
        let item = self.program.module(path, inner, body, span);
        self.item_scope = saved;
        item
    }

    /// `_` stands where a variable could and is never accessed: not as a
    /// head's column (it has no value to derive), a field's value, a
    /// function's argument (an interpolation's included) or a comparison's
    /// side. Checked on what a statement lowered to.
    pub(super) fn placeholders(&mut self, stmts: &[Stmt], span: Span) -> L<()> {
        fn has(t: &Term) -> bool {
            match t {
                Term::Wildcard => true,
                Term::Func { args, .. } | Term::List(args) => args.iter().any(has),
                Term::Obj(m) => m.values().any(has),
                Term::ListComp { item, body } => has(item) || lits(body),
                _ => false,
            }
        }
        // `_` inside a function's arguments, anywhere in `t`.
        fn in_func(t: &Term) -> bool {
            match t {
                Term::Func { args, .. } => args.iter().any(has),
                Term::List(xs) => xs.iter().any(in_func),
                Term::Obj(m) => m.values().any(in_func),
                Term::ListComp { item, body } => has(item) || lits(body),
                _ => false,
            }
        }
        // A tuple pattern on the left of `=` may hold `_` (R-58).
        fn side(t: &Term) -> bool {
            match t {
                Term::List(_) => in_func(t),
                t => has(t),
            }
        }
        fn lits(body: &[Lit]) -> bool {
            body.iter().any(|l| match l {
                Lit::Pos(a) | Lit::Not(a) => {
                    a.args.iter().any(in_func)
                        || a.record.as_ref().is_some_and(|r| r.values().any(in_func))
                }
                Lit::Eq(a, b) => side(a) || has(b),
                Lit::Neq(a, b) | Lit::Gt(a, b) | Lit::Ge(a, b) | Lit::Lt(a, b) | Lit::Le(a, b) => {
                    has(a) || has(b)
                }
            })
        }
        let bad = |l: &mut Self, at: Span, msg: String, fix: String| {
            l.diags.push(Diagnostic::error(at, msg).with_help(fix));
        };
        let before = self.diags.len();
        for st in stmts {
            let (head, body): (Option<&Atom>, &[Lit]) = match st {
                Stmt::Fact(a) => (Some(a), &[]),
                Stmt::Rule(r) => (Some(&r.head), &r.body),
                Stmt::Resource(r) => {
                    for f in r.fields.iter().filter(|f| has(&f.value)) {
                        bad(
                            self,
                            f.span,
                            format!("`{}` is given `_`, which has no value", f.key),
                            format!("give `{}` a value, or leave its line out", f.key),
                        );
                    }
                    (None, r.body.as_deref().unwrap_or_default())
                }
                Stmt::Instance(i) => {
                    for (k, _, at) in i.inputs.iter().filter(|(_, v, _)| has(v)) {
                        bad(
                            self,
                            *at,
                            format!("input `{k}` is given `_`, which has no value"),
                            format!("give `{k}` a value, or leave it out for its default"),
                        );
                    }
                    (None, i.body.as_deref().unwrap_or_default())
                }
                Stmt::Output(o) => {
                    if o.value.as_ref().is_some_and(has) {
                        bad(
                            self,
                            o.span,
                            format!("output `{}` is `_`, which has no value", o.name),
                            format!("write its value, `output {} = ..`", o.name),
                        );
                    }
                    (None, &[])
                }
                _ => (None, &[]),
            };
            if let Some(h) = head {
                let columns: Vec<(String, &Term)> = match &h.record {
                    Some(r) => r.iter().map(|(k, v)| (format!("`{k}`"), v)).collect(),
                    None => h
                        .args
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (format!("{}", i + 1), v))
                        .collect(),
                };
                if let Some((c, _)) = columns.iter().find(|(_, v)| has(v)) {
                    let p = h.pred.rsplit("::").next().unwrap_or(&h.pred);
                    bad(
                        self,
                        h.span,
                        format!("`_` is `{p}`'s column {c}, which then has no value"),
                        format!(
                            "name the column and give it a value after `where`: `{p}(x) \
                             where .., x in ..`"
                        ),
                    );
                }
            }
            if lits(body) {
                bad(
                    self,
                    span,
                    "`_` as a value: a function's argument, an interpolation or a comparison \
                     reads it"
                        .to_string(),
                    "`_` matches anything and holds nothing: name what it stands for (`x`, \
                     `env.p`)"
                        .to_string(),
                );
            }
        }
        if self.diags.len() > before {
            Err(Skip)
        } else {
            Ok(())
        }
    }

    /// A fresh rule context for a statement in `scope`.
    pub(super) fn rc(&self, n: &SyntaxNode, scope: ScopeId, outer: &Rc) -> Rc {
        let mut rc = Rc {
            scope,
            vars: outer.vars.clone(),
            types: outer.types.clone(),
            instances: outer.instances.clone(),
            row_refs: outer.row_refs.clone(),
            outer: outer.vars.values().cloned().collect(),
            ..Rc::default()
        };
        rc.reserved.extend(outer.vars.values().cloned());
        for c in n.descendants() {
            if let Some(ch) = Chain::of(&c)
                && ch.is_bare()
                && !self.is_value(scope, &ch.head)
                && self.find_let(scope, &ch.head).is_none()
            {
                rc.reserved.insert(capitalise(&ch.head));
                rc.candidates.insert(ch.head.clone());
            }
        }
        rc.candidates.extend(outer.vars.keys().cloned());
        // `x in T`: x has the static type T wherever it is used.
        for c in n
            .descendants()
            .filter(|c| matches!(c.kind(), LIT_IN | LIT_NOT_IN))
        {
            let mut ts = terms(&c);
            let Some(lhs) = ts.next().and_then(|t| Chain::of(&t)) else {
                continue;
            };
            // A resource named in scope is its address, not a variable.
            if !lhs.is_bare()
                || rc.types.contains_key(&lhs.head)
                || self.resource(scope, &lhs.head).is_some()
            {
                continue;
            }
            // `r not in T`, `r` a reference column's (R-42): the column
            // binds its type, which the test says is not `T` (R-219).
            let negated = c.kind() == LIT_NOT_IN || c.parent().is_some_and(|p| p.kind() == LIT_NOT);
            if negated && Self::ref_bound(&c, &lhs.head) {
                let typed = terms(&c)
                    .nth(1)
                    .and_then(|t| Chain::of(&t))
                    .is_some_and(|r| {
                        let r = self.type_each(&rc, r);
                        self.chain_type(&rc, &r).is_some() || self.namespace_of(&rc, &r).is_some()
                    });
                if typed {
                    let tv = fresh(&mut rc, "Type");
                    rc.types.insert(lhs.head, var(&tv));
                    continue;
                }
            }
            if c.kind() == LIT_NOT_IN {
                continue;
            }
            if tokens(&c).any(|t| t.kind() == RESOURCE_KW) {
                let tv = fresh(&mut rc, "Type");
                rc.types.insert(lhs.head, var(&tv));
            } else if let Some(rhs) = ts.next().and_then(|t| Chain::of(&t)) {
                let rhs = self.type_each(&rc, rhs);
                if let Some(t) = self.chain_type(&rc, &rhs) {
                    // A type under several names (R-115): which one is
                    // the resource's.
                    let typ = match self.covering(&t).len() {
                        1 => str_term(&t),
                        _ => var(&fresh(&mut rc, "Type")),
                    };
                    rc.types.insert(lhs.head, typ);
                } else if let Some(path) = self.component_of(&rc, &rhs) {
                    rc.instances.insert(lhs.head, path);
                } else if let Some(ns) = self.namespace_of(&rc, &rhs) {
                    let tv = fresh(&mut rc, "Type");
                    rc.types.insert(lhs.head.clone(), var(&tv));
                    rc.namespaces.insert(lhs.head, ns);
                }
            }
        }
        self.ref_column_vars(n, scope, &mut rc);
        rc
    }

    /// The type a chain names, when it names one (the right side of `in`).
    pub(super) fn chain_type(&self, rc: &Rc, c: &Chain) -> Option<String> {
        if c.ops.iter().any(|o| !matches!(o, Op::Field(..))) {
            return None;
        }
        let name = c.fields().join(".");
        if self.decls.types.contains(&name) {
            return Some(name);
        }
        if c.head == "world" || c.head_kind == SETTINGS_KW {
            return None;
        }
        let local = self.find_let(rc.scope, &c.head).is_some()
            || self.is_value(rc.scope, &c.head)
            || rc.types.contains_key(&c.head)
            || rc.vars.contains_key(&c.head)
            || self.resource(rc.scope, &c.head).is_some()
            || self.module_at(&c.head).is_some()
            || rc.candidates.contains(&c.head);
        // `T.n.path` is a read of a resource, not a type.
        let fields = c.fields();
        let resource = (1..fields.len())
            .any(|i| self.resource_of_type(rc.scope, &fields[..i].join("."), &fields[i]));
        // A namespace no declaration or builtin schema names is a provider
        // schema's the compiler does not read (`--provider`, a plugin's).
        let namespace = self.decls.namespaces.contains(&c.head)
            || self.any_type
            || !self.program.scopes.declares_relation(&c.head);
        (!local && !resource && namespace && !c.ops.is_empty()).then_some(name)
    }

    /// `key NAME` in the module `path`, a file the program loads, not the
    /// one the tool runs (R-208): the error at its line, its help the
    /// input to declare (the key's own type text) and the `use` to give it
    /// in, labelled where the module is used.
    pub(super) fn key_in_module<T>(
        &mut self,
        n: &SyntaxNode,
        name: &str,
        path: &str,
        span: Span,
    ) -> L<T> {
        let ty = node(n, TYPE_EXPR).map_or_else(|| "TYPE".to_string(), |t| t.text().to_string());
        let site = self.program.scopes.names.values().find_map(|sc| {
            let used = sc.uses().any(|(_, m)| m == path);
            sc.iter().flat_map(|(_, ds)| ds).find_map(|d| {
                let is_use = matches!(d.kind, DeclKind::Use { .. });
                (is_use && used && use_parts(&d.node).0 == path).then(|| d.node.clone())
            })
        });
        let used = site.as_ref().map_or_else(
            || path.rsplit('.').next().unwrap_or(path).to_string(),
            |u| use_parts(u).1,
        );
        let shown = match used == path.rsplit('.').next().unwrap_or(path) {
            true => path.to_string(),
            false => format!("{path} as {used}"),
        };
        let mut d = Diagnostic::error(
            span,
            format!(
                "`key {name}` in a file that is not an entrypoint: a key is a deployment's \
                 identity; a module takes `input {name}` instead"
            ),
        )
        .with_help(format!(
            "declare `input {name}: {ty}` here, and give it in the use: `use {shown} {{ {name} }}`"
        ));
        if let Some(u) = &site
            && let Some(file) = self.file_of_node(u)
        {
            d = d.with_label(
                Span {
                    file,
                    start: u.text_range().start().into(),
                    end: u.text_range().end().into(),
                    origin: 0,
                },
                format!("{path} is used here, not deployed"),
            );
        }
        self.diags.push(d);
        Err(Skip)
    }

    /// The `diag` source id of the file a node of the program is in.
    pub(super) fn file_of_node(&self, n: &SyntaxNode) -> Option<u32> {
        let root = n.ancestors().last()?;
        self.units.iter().find(|u| u.root == root).map(|u| u.file)
    }

    pub(super) fn span_of(&self, r: rowan::TextRange) -> Span {
        Span {
            file: self.file,
            start: u32::from(r.start()) + self.offset,
            end: u32::from(r.end()) + self.offset,
            origin: 0,
        }
    }

    pub(super) fn span(&self, n: &SyntaxNode) -> Span {
        self.span_of(n.text_range())
    }

    /// Where `n` of the file `file` is.
    pub(super) fn span_in(&self, file: u32, n: &SyntaxNode) -> Span {
        let r = n.text_range();
        Span {
            file,
            start: r.start().into(),
            end: r.end().into(),
            origin: 0,
        }
    }

    pub(super) fn error<T>(&mut self, span: Span, msg: impl Into<String>) -> L<T> {
        self.diags.push(Diagnostic::error(span, msg));
        Err(Skip)
    }
}

//! What each scope declares, collected before anything lowers (R-65,
//! R-186): every file's statements walked into the scope tree, each copy
//! bound to the instance it is named through, each resource name's types
//! classified, and the paths of modules and components.

use super::*;

impl Lowerer<'_> {
    /// The types `x in t` ranges over: `t`, and, `t` a provider's type
    /// (`ovh.instance`), the same type under each name a `use .. as`
    /// gives the provider (`ca.instance`, R-115).
    pub(super) fn covering(&self, t: &str) -> Vec<String> {
        let mut out = vec![t.to_string()];
        if let Some((ns, rest)) = t.split_once('.') {
            for a in self.decls.renamed.get(ns).into_iter().flatten() {
                out.push(format!("{a}.{rest}"));
            }
        }
        out
    }

    /// Each scope's instances, their components' paths resolved, and
    /// each instanced component readable by its last segment there
    /// (`postgres[t]`).
    pub(super) fn bind_instances(&mut self) {
        let scopes: Vec<ScopeId> = self.program.scopes.ids().collect();
        for &s in &scopes {
            let names: Vec<String> = self
                .names(s)
                .names(|k| matches!(k, DeclKind::Use { .. }))
                .cloned()
                .collect();
            for name in names {
                for i in 0..self.names(s).of(&name).len() {
                    let DeclKind::Use { written, .. } = &self.names(s).of(&name)[i].kind else {
                        continue;
                    };
                    let path = self.module_path_of(s, &written.clone());
                    if let DeclKind::Use { module, .. } =
                        &mut self.names_mut(s).of_mut(&name)[i].kind
                    {
                        *module = path;
                    }
                }
            }
        }
        self.classify_resources();
        // A copy of a name declared more than once (R-104) makes its
        // component readable by its last segment.
        for &s in &scopes {
            let mut copied = Vec::new();
            for (_, decls) in self.names(s).iter() {
                let bound: Vec<&scopes::Decl> = decls
                    .iter()
                    .filter(|d| {
                        matches!(
                            d.kind,
                            DeclKind::Use { .. } | DeclKind::Copy { binds: true, .. }
                        )
                    })
                    .collect();
                if bound.len() < 2 {
                    continue;
                }
                for d in bound {
                    if let DeclKind::Copy { written, .. } = &d.kind {
                        copied.push((written.clone(), d.node.clone()));
                    }
                }
            }
            for (written, n) in copied {
                let Ok(path) = self.component_path(s, &written) else {
                    continue;
                };
                self.copy_by_last(s, &written, path, n);
            }
        }
        for &s in &scopes {
            let names: Vec<String> = self
                .names(s)
                .names(|k| matches!(k, DeclKind::Copy { .. }))
                .cloned()
                .collect();
            for name in names {
                let Some(d) = self
                    .names(s)
                    .of(&name)
                    .iter()
                    .find(|d| matches!(d.kind, DeclKind::Copy { .. }))
                else {
                    continue;
                };
                let (DeclKind::Copy { written: path, .. }, n) = (d.kind.clone(), d.node.clone())
                else {
                    continue;
                };
                let Ok(full) = self.component_path(s, &path) else {
                    continue;
                };
                // A used module of the name is read by it, and its
                // component's copies by the path (`k3s.k3s[t]`).
                self.copy_by_last(s, &path, full.clone(), n);
                let copy = self
                    .names_mut(s)
                    .of_mut(&name)
                    .iter_mut()
                    .find(|d| matches!(d.kind, DeclKind::Copy { .. }));
                if let Some(scopes::Decl {
                    kind: DeclKind::Copy { component, .. },
                    ..
                }) = copy
                {
                    *component = Some(full);
                }
            }
        }
    }

    /// The component at `path`, copied in `s` by the path `written`,
    /// readable there by its last segment (`postgres[t]`) unless a used
    /// module or stack of that name is read by it.
    pub(super) fn copy_by_last(&mut self, s: ScopeId, written: &str, path: String, n: SyntaxNode) {
        let last = written.rsplit('.').next().unwrap_or(written);
        if self.use_in(s, last).is_some() || self.stack_in(s, last).is_some() {
            return;
        }
        if self.names(s).copied(last).is_none() {
            self.names_mut(s).declare(last, DeclKind::Copied(path), n);
        }
    }

    /// Each `resource T n` collected, by what `T` names (R-113): a
    /// component's path (or a signature's, a module's, a stack's, which
    /// `instance` reports) makes a copy, bound as the scope's other names
    /// are; any other is a resource of the type `T`.
    pub(super) fn classify_resources(&mut self) {
        for (decl, file, n) in std::mem::take(&mut self.decls.pending) {
            let typ = dotted_text(&n, 1);
            let full = self.module_path_of(decl, &typ);
            // A path from the root that is also a type the program or a
            // built-in schema declares is the type: `net.vpc` in net.df
            // is the fake cloud's, not net.df's `component vpc`.
            let head = typ.split('.').next().unwrap_or(&typ);
            let bound = self.chain_of(decl).into_iter().any(|s| {
                let sc = self.names(s);
                sc.component(head).is_some() || sc.module(head).is_some()
            });
            let typed = schema_types().contains(&typ) || self.decls.types.contains(&typ);
            // A project module's resource of a stack is a deployment
            // (R-114), a resource of the stack's type.
            let deployment = self.decls.project.contains(&file)
                && self.decls.deployed.iter().any(|d| d.path == full);
            let copy = !deployment
                && (bound || !typed)
                && (self.module_at(&full).is_some()
                    || self.decls.deployed.iter().any(|d| d.path == full)
                    || self.signature(decl, &typ).is_some());
            if copy {
                let (path, name) = copy_parts(&n);
                self.decls
                    .copies
                    .insert((file, n.text_range().start().into()));
                // A copy named by its clause binds no name (R-191): its
                // copies are read `C[t]`, as a provider type's are `T[t]`.
                let kind = DeclKind::Copy {
                    written: path,
                    component: None,
                    binds: !named_by_clause(&n),
                };
                self.names_mut(decl).declare(name, kind, n.clone());
                continue;
            }
            self.decls.types.insert(typ.clone());
            if let Some(name) = self.static_header(&n) {
                self.names_mut(decl)
                    .declare(name, DeclKind::Resource(typ), n.clone());
            }
        }
    }

    /// Whether `n`, a statement of the file being lowered, makes a copy of
    /// a component: `resource C n { .. }` (R-113).
    pub(super) fn is_copy(&self, n: &SyntaxNode) -> bool {
        self.is_copy_in(self.file, n)
    }

    /// [`Self::is_copy`] for a statement of `file`.
    pub(super) fn is_copy_in(&self, file: u32, n: &SyntaxNode) -> bool {
        n.kind() == RESOURCE
            && self
                .decls
                .copies
                .contains(&(file, n.text_range().start().into()))
    }

    /// The module path a path written in `scope` names: its first segment
    /// a component or a used module in scope, else the path from the root.
    pub(super) fn module_path_of(&self, scope: ScopeId, written: &str) -> String {
        let (head, rest) = match written.split_once('.') {
            Some((h, r)) => (h, Some(r)),
            None => (written, None),
        };
        let base = self.chain_of(scope).into_iter().find_map(|s| {
            let sc = self.names(s);
            sc.component(head).or_else(|| sc.module(head)).cloned()
        });
        match (base, rest) {
            (Some(b), Some(r)) => format!("{b}.{r}"),
            (Some(b), None) => b,
            (None, _) => written.to_string(),
        }
    }

    /// The component a path written in `scope` names, or the error that
    /// says what it names instead.
    pub(super) fn component_path(
        &self,
        scope: ScopeId,
        written: &str,
    ) -> Result<String, Box<Diagnostic>> {
        let full = self.module_path_of(scope, written);
        match self.module_at(&full) {
            Some((_, true)) => Ok(full),
            Some(_) => {
                let mut d =
                    Diagnostic::error(Span::default(), format!("{written} is a module; `use` it"))
                        .with_note(
                            "a module, a file, is imported once by `use`; a component, an item \
                     `component NAME { .. }` of a module, is a type: `resource C NAME { .. }` \
                     makes one",
                        );
                // `resource k3s cluster` for k3s.df's `component k3s`.
                let items: Vec<String> = self
                    .program
                    .scopes
                    .paths()
                    .filter(|(p, s)| {
                        self.program.scopes.is_component(*s)
                            && p.strip_prefix(&full)
                                .and_then(|r| r.strip_prefix('.'))
                                .is_some_and(|r| !r.contains('.'))
                    })
                    .map(|(p, _)| format!("`{written}{}`", &p[full.len()..]))
                    .collect();
                if !items.is_empty() {
                    d = d.with_help(format!(
                        "its components are types by their path: {}",
                        items.join(", ")
                    ));
                }
                Err(Box::new(d))
            }
            None if self.decls.deployed.iter().any(|d| d.path == full) => Err(Box::new(
                Diagnostic::error(
                    Span::default(),
                    format!("{written} is deployed by the tool; `use` it"),
                )
                .with_note(
                    "a stack is a module the tool uses, one deployment per key: `use` binds to its \
                 deployments, and `NAME[k=v].output` reads one",
                ),
            )),
            None => Err(Box::new(
                Diagnostic::error(Span::default(), format!("no component `{written}`"))
                    .with_note("a component is an item of a module, `component NAME { .. }`"),
            )),
        }
    }

    /// Record the declarations of a statement list in `decl`; a component
    /// block nests in `outer`.
    pub(super) fn collect(
        &mut self,
        file: u32,
        parent: &SyntaxNode,
        decl: ScopeId,
        outer: ScopeId,
    ) {
        let arity = |n: &SyntaxNode| n.children().filter(|c| c.kind() == BIND_ARG).count();
        for n in parent.children() {
            match n.kind() {
                INPUT => {
                    let name = word_text(&n, 1);
                    self.names_mut(decl)
                        .declare(name, DeclKind::Input, n.clone());
                }
                // A let with parameters is a relation (R-187): its
                // parameters' columns, then its value's.
                LET if node(&n, PARAMS).is_some() => {
                    let name = word_text(&n, 1);
                    let arity = function::params(&n).map_or(0, |p| p.len()) + 1;
                    self.names_mut(decl)
                        .declare(name, DeclKind::LetFn { arity }, n.clone());
                }
                LET => {
                    let name = word_text(&n, 1);
                    self.names_mut(decl).declare(name, DeclKind::Let, n.clone());
                }
                // `input p from ..` or `input p`: rows of `p`, which its
                // `decl` declares (R-55).
                INPUT_RELATION => {
                    let name = word_text(&n, 1);
                    let given = terms(&n).next().is_none();
                    self.names_mut(decl).declare(
                        name,
                        DeclKind::RelationInput { given },
                        n.clone(),
                    );
                }
                // `output p` alone: a relation exported (R-55).
                OUTPUT_DECL
                    if node(&n, TYPE_EXPR).is_none()
                        && terms(&n).next().is_none()
                        && node(&n, ATTR_DECL).is_none() =>
                {
                    let k = word_text(&n, 1);
                    self.names_mut(decl)
                        .declare(k, DeclKind::RelationOutput, n.clone());
                }
                OUTPUT_DECL => {
                    let ty = node(&n, TYPE_EXPR).and_then(|t| self.resource_type(decl, &t));
                    let k = word_text(&n, 1);
                    let written = node(&n, TYPE_EXPR)
                        .map(|t| t.text().to_string().replace(char::is_whitespace, ""));
                    let kind = DeclKind::Output { typ: ty, written };
                    self.names_mut(decl).declare(k, kind, n.clone());
                }
                EXTERN => {
                    let name = dotted_text(&n, 1);
                    let cols = n
                        .children()
                        .filter(|c| c.kind() == BIND_ARG)
                        .map(|b| {
                            let input = tokens(&b).next().is_some_and(|t| t.kind() == PLUS);
                            (input, word_text(&b, 0))
                        })
                        .collect();
                    self.program.scopes.externs.insert(name, cols);
                }
                DECL => {
                    let name = dotted_text(&n, 1);
                    let kind = DeclKind::Decl { arity: arity(&n) };
                    self.names_mut(decl).declare(name, kind, n.clone());
                }
                TYPE_DECL => {
                    self.decls.types.insert(dotted_text(&n, 1));
                }
                TYPE_ALIAS if node(&n, SIGNATURE).is_some() => {
                    let name = word_text(&n, 1);
                    let kind = DeclKind::Signature { file };
                    self.names_mut(decl).declare(name, kind, n.clone());
                }
                // A resource of a provider's type, or a copy of a component
                // (R-113): which, once every module's components and every
                // scope's `use`s are known (`classify_resources`).
                RESOURCE => self.decls.pending.push((decl, file, n.clone())),
                // A provider's `use` binds its namespace, not a module.
                USE if provider_use(&n, self.units, &self.decls.deployed).is_some() => {}
                USE => {
                    // The path as written; `bind_instances` resolves it.
                    let (path, name) = use_parts(&n);
                    match self.decls.deployed.iter().position(|d| d.path == path) {
                        Some(i) => {
                            self.names_mut(decl)
                                .declare(name, DeclKind::Stack(i), n.clone());
                        }
                        None if path.split('.').next() != Some("std") => {
                            let kind = DeclKind::Use {
                                written: path.clone(),
                                module: path,
                            };
                            self.names_mut(decl).declare(name, kind, n.clone());
                        }
                        None => {}
                    }
                }
                COMPONENT => {
                    let start: u32 = n.text_range().start().into();
                    let name = word_text(&n, 1);
                    let path = match self.module_path(file) {
                        Some(m) => format!("{m}.{name}"),
                        None => name.clone(),
                    };
                    let scope = self.program.scopes.component(file, start, outer, &path);
                    self.names_mut(decl)
                        .declare(name, DeclKind::Component(path), n.clone());
                    if let Some(b) = node(&n, STMT_BLOCK) {
                        self.collect(file, &b, scope, scope);
                    }
                }
                RULE | FACT => {
                    self.decls.rules.push((outer, n.clone()));
                    if let Some(h) = n.children().find(|c| c.kind() == CALL)
                        && let Some(name) = self.callee(&h)
                    {
                        // A schema's `type_provider(T, ...)`, `type_attr(T, ...)`
                        // rows declare T.
                        let first = node(&h, ARG_LIST).and_then(|a| terms(&a).next());
                        if name.starts_with("type_")
                            && let Some(t) = &first
                        {
                            if let Some(c) = Chain::of(t)
                                && c.ops.iter().all(|o| matches!(o, Op::Field(..)))
                            {
                                self.decls.types.insert(c.fields().join("."));
                            } else if t.kind() == LITERAL
                                && let Some(s) = tokens(t).find(|x| x.kind() == STRING)
                                && let Ok(s) = unescape(s.text())
                            {
                                self.decls.types.insert(s);
                            }
                        }
                        let n_args = node(&h, ARG_LIST).map_or(0, |a| {
                            a.children()
                                .filter(|c| is_term(c.kind()) || c.kind() == NAMED_ARG)
                                .count()
                        });
                        let kind = DeclKind::Rule { arity: n_args };
                        self.names_mut(decl).declare(name.clone(), kind, n.clone());
                    }
                }
                _ => {}
            }
        }
    }

    /// A resource header's name when it is static: a string with no holes,
    /// or a bare name (R-76). A bare name the block's clauses bind is the
    /// error `block_stmt` reports, and names nothing here.
    pub(super) fn static_header(&self, n: &SyntaxNode) -> Option<String> {
        let t = self.header_token(n)?;
        if t.kind() == STRING {
            let text = t.text();
            if has_hole(text) {
                return None;
            }
            return string_value(text).ok();
        }
        let name = t.text().to_string();
        let bound = n
            .children()
            .filter(|c| c.kind() == CLAUSE)
            .flat_map(|c| c.descendants().collect::<Vec<_>>())
            .filter_map(|c| Chain::of(&c))
            .any(|c| c.is_bare() && c.head == name);
        (!bound).then_some(name)
    }

    /// The name token of a resource header.
    pub(super) fn header_token(&self, n: &SyntaxNode) -> Option<SyntaxToken> {
        header_name(n)
    }

    /// `T` in `output k: T`, written in `scope`, when it names a resource
    /// type: a dotted name that is no type alias (`types.environment` is
    /// one by its module's path, R-208).
    pub(super) fn resource_type(&self, scope: ScopeId, t: &SyntaxNode) -> Option<String> {
        if node(t, TYPE_EXPR).is_some() {
            return None;
        }
        let name = dotted_text(t, 0);
        (name.contains('.') && !self.names_alias(scope, &name)).then_some(name)
    }
}

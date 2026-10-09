//! A module reads only what it declares (R-205): scope is lexical
//! everywhere, so a module's file is a scope with none around it, and a
//! name its body does not declare is never its user's. A read of a name
//! the module's user declares, `env` in a policy pack the stack keys by
//! `env`, is the error naming the input to declare and the `use` to give
//! it in, once per name and file.

use super::*;

/// What a module's user declares a name as: the input the module takes
/// it by.
enum UserDecl {
    /// A value or a resource: the type the module's input is declared
    /// with, as the user's declaration writes it (an alias expanded).
    Value(String),
    /// A relation, of this many columns: `input p` and its `decl`.
    Relation(usize),
    /// A copy, a used module or a stack: what the module reads of it is
    /// a value the user gives (`NAME = blue.x`).
    Scope,
}

impl UserDecl {
    /// `input env: T` in FILE; `decl p(..)` and `input p` in FILE.
    fn declare(&self, h: &str) -> String {
        match self {
            UserDecl::Value(ty) => format!("`input {h}: {ty}`"),
            UserDecl::Relation(n) => {
                let cols: Vec<String> = (1..=*n).map(|i| format!("c{i}: T")).collect();
                format!("`decl {h}({})` and `input {h}`", cols.join(", "))
            }
            UserDecl::Scope => format!("what it reads of `{h}`, `input NAME: TYPE`,"),
        }
    }

    /// What the `use` or the copy gives: the value by its name, the rows
    /// by a rule over the user's.
    fn give(&self, h: &str) -> String {
        match self {
            UserDecl::Value(_) => h.to_string(),
            UserDecl::Relation(n) => {
                let vs: Vec<String> = (1..=*n).map(|i| format!("x{i}")).collect();
                let vs = vs.join(", ");
                format!("{h}({vs}) where {h}({vs})")
            }
            UserDecl::Scope => format!("NAME = {h}.x"),
        }
    }
}

impl Lowerer<'_> {
    /// The error for `h`, read in `rc.scope`, when the read is in a
    /// module's file, nothing there declares `h`, and the module's user
    /// does: the module would read its user's (R-205).
    pub(super) fn undeclared(&mut self, rc: &Rc, h: &str, span: Span) -> L<()> {
        if self.declares(rc.scope, h) {
            return Ok(());
        }
        self.not_declared(rc.scope, h, false, span)
    }

    /// The error for the relation `p` called in `rc.scope`, as
    /// [`Self::undeclared`]: a module's call of its user's relation.
    pub(super) fn undeclared_relation(&mut self, rc: &Rc, p: &str, span: Span) -> L<()> {
        let defines = self.chain_of(rc.scope).into_iter().any(|s| {
            let sc = self.names(s);
            sc.is_relation(p) || sc.takes(p)
        });
        if defines || !crate::modules::is_private(p) || self.decls.externs.contains_key(p) {
            return Ok(());
        }
        self.not_declared(rc.scope, p, true, span)
    }

    fn not_declared(&mut self, scope: ScopeId, h: &str, relation: bool, span: Span) -> L<()> {
        let Some(module) = self.module_of(scope) else {
            return Ok(());
        };
        // The body the read is in: a component's input is given in each
        // copy, a module's in its `use`.
        let body = self.program.scopes.body(scope);
        let component = self
            .program
            .scopes
            .definition(body)
            .filter(|(_, c)| *c)
            .map(|(p, _)| p.clone());
        let Some(user) = self.user_decl(&module, component.as_deref(), h, relation) else {
            return Ok(());
        };
        let file = self.module_file(&module);
        let d = match component {
            Some(c) => {
                let name = c.rsplit('.').next().unwrap_or(&c);
                Diagnostic::error(
                    span,
                    format!(
                        "`{h}` is not declared in component {c}: a component reads only what \
                         it and its module declare"
                    ),
                )
                .with_help(format!(
                    "take it as an input: {} in component {name} ({file}), and give it in each \
                     copy: `resource {c} NAME {{ {} }}`",
                    user.declare(h),
                    user.give(h)
                ))
            }
            None => Diagnostic::error(
                span,
                format!(
                    "`{h}` is not declared in module {module}: a module reads only what it \
                     declares"
                ),
            )
            .with_help(format!(
                "take it as an input: {} in {file}, and give it in the use: `use {module} {{ {} \
                 }}`",
                user.declare(h),
                user.give(h)
            )),
        };
        // Each name once per file: its first read says it.
        if !self.diags.iter().any(|x| x.message == d.message) {
            self.diags.push(d);
        }
        Err(Skip)
    }

    /// The module whose file `scope` is in: the module itself, or a
    /// component of it. `None` in a stack's file.
    fn module_of(&self, scope: ScopeId) -> Option<String> {
        self.program.scopes.module_of(scope).cloned()
    }

    /// Whether `h` names anything in scope: a value, a resource, a used
    /// module or stack, a copy, a component, the module itself in its
    /// file, or a path from the root (`modules.net.vpc[t]`).
    fn declares(&self, scope: ScopeId, h: &str) -> bool {
        let root = |p: &String| p == h || p.strip_prefix(h).is_some_and(|r| r.starts_with('.'));
        self.self_module(scope, h).is_some()
            || self.program.scopes.paths().any(|(p, _)| root(p))
            || self.is_value(scope, h)
            || self.resource(scope, h).is_some()
            || self.use_in(scope, h).is_some()
            || self.stack_in(scope, h).is_some()
            || self.instance_in(scope, h).is_some()
            || self.component_in(scope, h).is_some()
    }

    /// The module's file name, for the help.
    fn module_file(&self, module: &str) -> String {
        let scopes = &self.program.scopes;
        let file = scopes.by_path(module).and_then(|s| scopes.file_of(s));
        file.and_then(|f| {
            crate::diag::source_of(Span {
                file: f,
                ..Span::default()
            })
        })
        .map_or_else(|| format!("{module}.df"), |(name, _)| name)
    }

    /// What the module's users declare `h` as: the stack's top level, each
    /// scope that `use`s the module or copies the component, out to its
    /// own file's.
    fn user_decl(
        &self,
        module: &str,
        component: Option<&str>,
        h: &str,
        relation: bool,
    ) -> Option<UserDecl> {
        let uses = |s: &Names| {
            s.uses().map(|(_, m)| m).any(|p| p == module)
                || component.is_some_and(|c| s.instances().map(|(_, c)| c).any(|p| p == c))
        };
        let users = std::iter::once(self.root())
            .chain(self.program.scopes.ids().filter(|s| uses(self.names(*s))));
        for user in users {
            for s in self.chain_of(user) {
                let sc = self.names(s);
                if relation {
                    match sc.arities(h).first() {
                        Some(n) => return Some(UserDecl::Relation(*n)),
                        None => continue,
                    }
                }
                if let Some(n) = sc.input(h) {
                    let ty = node(n, TYPE_EXPR).map_or("TYPE".into(), |t| self.unaliased(&t));
                    return Some(UserDecl::Value(ty));
                }
                if sc.is_value(h) {
                    return Some(UserDecl::Value("TYPE".into()));
                }
                if let Some(types) = sc.resources(h) {
                    return Some(UserDecl::Value(format!("ref({})", types[0])));
                }
                if sc.instance(h).is_some() || sc.module(h).is_some() || sc.stack(h).is_some() {
                    return Some(UserDecl::Scope);
                }
            }
        }
        None
    }

    /// A type as written, a one-word alias of its file expanded: the
    /// module does not see its user's aliases.
    fn unaliased(&self, t: &SyntaxNode) -> String {
        let text = |n: &SyntaxNode| {
            let t = n.text().to_string();
            t.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let written = text(t);
        let root = t.ancestors().last();
        let alias = root.iter().flat_map(|r| r.children()).find(|n| {
            n.kind() == TYPE_ALIAS && node(n, SIGNATURE).is_none() && word_text(n, 1) == written
        });
        match alias.and_then(|a| node(&a, TYPE_EXPR)) {
            Some(ty) => text(&ty),
            None => written,
        }
    }
}

//! A module reads only what it declares (R-205): scope is lexical
//! everywhere, so a module's file is a scope with none around it, and a
//! name its body does not declare is never its user's. A read of a name
//! the module's user declares, `env` in a policy pack the stack keys by
//! `env`, is the error naming the input to declare and the `use` to give
//! it in, once per name and file.

use super::*;

/// What a module's user declares a name as: the input the module takes
/// it by.
struct UserDecl {
    /// The type the module's input is declared with, as the user's
    /// declaration writes it (an alias expanded).
    ty: String,
}

impl Lowerer<'_> {
    /// The error for `h`, read in `rc.scope`, when the read is in a
    /// module's file, nothing there declares `h`, and the module's user
    /// does: the module would read its user's (R-205).
    pub(super) fn undeclared(&mut self, rc: &Rc, h: &str, span: Span) -> L<()> {
        let Some(module) = self.module_of(rc.scope) else {
            return Ok(());
        };
        if self.declares(rc.scope, h) {
            return Ok(());
        }
        // The body the read is in: a component's input is given in each
        // copy, a module's in its `use`.
        let body = self.own_scopes(rc.scope).last().copied().unwrap_or(PROGRAM);
        let component = self
            .decls
            .modules
            .iter()
            .find(|(_, m)| m.scope == body && m.component)
            .map(|(p, _)| p.clone());
        let Some(user) = self.user_decl(&module, component.as_deref(), h) else {
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
                    "take it as an input: `input {h}: {}` in component {name} ({file}), and give \
                     it in each copy: `resource {c} NAME {{ {h} }}`",
                    user.ty
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
                "take it as an input: `input {h}: {}` in {file}, and give it in the use: `use \
                 {module} {{ {h} }}`",
                user.ty
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
    fn module_of(&self, scope: usize) -> Option<String> {
        let root = *self.chain_of(scope).last()?;
        self.decls
            .modules
            .iter()
            .find(|(_, m)| m.scope == root && !m.component)
            .map(|(p, _)| p.clone())
    }

    /// Whether `h` names anything in scope: a value, a resource, a used
    /// module or stack, a copy, a component.
    fn declares(&self, scope: usize, h: &str) -> bool {
        self.is_value(scope, h)
            || self.resource(scope, h).is_some()
            || self.use_in(scope, h).is_some()
            || self.stack_in(scope, h).is_some()
            || self.instance_in(scope, h).is_some()
            || self.component_in(scope, h).is_some()
    }

    /// The module's file name, for the help.
    fn module_file(&self, module: &str) -> String {
        let file = self
            .decls
            .paths
            .iter()
            .find(|(_, p)| *p == module)
            .map(|(f, _)| *f);
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
    fn user_decl(&self, module: &str, component: Option<&str>, h: &str) -> Option<UserDecl> {
        let uses = |s: &Scope| {
            s.uses.values().any(|p| p == module)
                || component.is_some_and(|c| s.instances.values().any(|p| p == c))
        };
        let users = std::iter::once(PROGRAM)
            .chain((0..self.decls.scopes.len()).filter(|s| uses(&self.decls.scopes[*s])));
        for user in users {
            for s in self.chain_of(user) {
                let sc = &self.decls.scopes[s];
                if let Some(n) = sc.input_nodes.get(h) {
                    let ty = node(n, TYPE_EXPR).map_or("TYPE".into(), |t| self.unaliased(&t));
                    return Some(UserDecl { ty });
                }
                if sc.values.contains(h) {
                    return Some(UserDecl { ty: "TYPE".into() });
                }
                if let Some(types) = sc.resources.get(h) {
                    return Some(UserDecl {
                        ty: format!("ref({})", types[0]),
                    });
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

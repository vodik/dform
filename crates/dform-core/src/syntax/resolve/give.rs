//! What a `use` block gives a module's inputs, checked where it is written
//! (R-208): a value whose declared enum has a member the input's enum has
//! not is an error naming both types, at compile time, not a violation in
//! the one deployment that has the member; and the pun `{ env }` with no
//! `env` in scope says it is the pun.

use super::*;

impl Lowerer<'_> {
    /// The entries of the block of `n`, a `use` or a copy of `module` in
    /// `scope`, that give an input a value the input cannot hold, or pun a
    /// name nothing declares: each an error at the entry.
    pub(super) fn check_gives(&mut self, n: &SyntaxNode, scope: ScopeId, module: &str) -> L<()> {
        let Some(block) = node(n, BLOCK) else {
            return Ok(());
        };
        let Some(inner) = self.module_at(module).map(|m| m.0) else {
            return Ok(());
        };
        let mut failed = false;
        for a in block.children().filter(|c| c.kind() == ASSIGN) {
            let Some(key) = node(&a, BLOCK_PATH).map(|p| p.text().to_string()) else {
                continue;
            };
            let (given, pun) = match terms(&a).next() {
                Some(t) => match bare_name(&t) {
                    Some(name) => (name, false),
                    None => continue,
                },
                None if !key.contains('.') => (key.clone(), true),
                None => continue,
            };
            let input = self.decls.scopes[inner].input_nodes.get(&key).cloned();
            let declared = self
                .chain_of(scope)
                .into_iter()
                .find(|s| self.decls.scopes[*s].values.contains(&given));
            let Some(declared) = declared else {
                if pun && self.resource(scope, &given).is_none() && !clause_binds(n, &given) {
                    self.pun_of_nothing(&a, &key, module, input.as_ref());
                    failed = true;
                }
                continue;
            };
            let (Some(input), Some(value)) = (
                input,
                self.decls.scopes[declared].input_nodes.get(&given).cloned(),
            ) else {
                continue;
            };
            failed |= self.enum_given(&a, module, &key, &input, &given, &value);
        }
        if failed { Err(Skip) } else { Ok(()) }
    }

    /// `{ env }` where nothing in scope is `env`: the pun's error, its
    /// help the declaration to write here, typed as the input is.
    fn pun_of_nothing(
        &mut self,
        a: &SyntaxNode,
        key: &str,
        module: &str,
        input: Option<&SyntaxNode>,
    ) {
        let shown = module.rsplit('.').next().unwrap_or(module);
        let ty = input
            .and_then(|i| node(i, TYPE_EXPR))
            .map_or_else(|| "TYPE".to_string(), |t| t.text().to_string());
        // A module takes what it gives on as an input; the file the tool
        // runs declares the deployment's key.
        let declare = match self.module_path(self.file).is_some() {
            true => format!("`input {key}: {ty}`"),
            false => format!("`key {key}: {ty}`"),
        };
        let d = Diagnostic::error(
            self.span(a),
            format!(
                "`{key}` in the block of `use {shown}` is `{key} = {key}`, and nothing here \
                 declares `{key}`"
            ),
        )
        .with_help(format!(
            "declare it in this file, {declare}, or give a value, `{key} = ..`"
        ));
        self.diags.push(d);
    }

    /// `{ key = given }`, `given` declared by `value`, an input or a key:
    /// when both are enums and `given`'s has a member the input's has not,
    /// the error naming both types. Whether it reported one.
    fn enum_given(
        &mut self,
        a: &SyntaxNode,
        module: &str,
        key: &str,
        input: &SyntaxNode,
        given: &str,
        value: &SyntaxNode,
    ) -> bool {
        let (Some(to), Some(from)) = (self.enum_of(input), self.enum_of(value)) else {
            return false;
        };
        let missing: Vec<&String> = from.1.iter().filter(|m| !to.1.contains(m)).collect();
        if missing.is_empty() {
            return false;
        }
        let what = if is_key(value) { "key" } else { "input" };
        let members: Vec<String> = missing.iter().map(|m| format!("`{m}`")).collect();
        let mut d = Diagnostic::error(
            self.span(a),
            format!(
                "{what} {given}, {}, is given to input {module}.{key}, {}: {} {} not one of \
                 its members",
                from.0,
                to.0,
                members.join(", "),
                if missing.len() == 1 { "is" } else { "are" }
            ),
        )
        .with_help(format!(
            "declare the input with the {what}'s type: `input {key}: {}` in module {module}",
            self.written_type(value)
        ));
        for (n, label) in [
            (value, format!("{given}: {} declared here", from.0)),
            (input, format!("{key}: {} declared here", to.0)),
        ] {
            if let Some(file) = self.file_of_node(n) {
                let r = n.text_range();
                d = d.with_label(
                    Span {
                        file,
                        start: r.start().into(),
                        end: r.end().into(),
                        origin: 0,
                    },
                    label,
                );
            }
        }
        self.diags.push(d);
        true
    }

    /// The type of the input or key `n` as written, `config.environment`.
    fn written_type(&self, n: &SyntaxNode) -> String {
        node(n, TYPE_EXPR).map_or_else(String::new, |t| {
            t.text()
                .to_string()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
    }

    /// The input or key `n`'s type, when it is an enum: as written (an
    /// alias with its members, `config.environment = enum(lab, prod)`), and
    /// its members. Read in the declaration's own file, where its aliases
    /// are in scope.
    fn enum_of(&mut self, n: &SyntaxNode) -> Option<(String, Vec<String>)> {
        let t = node(n, TYPE_EXPR)?;
        let file = self.file_of_node(n)?;
        let saved = std::mem::replace(&mut self.file, file);
        let before = self.diags.len();
        let ty = self.type_expr(&t);
        // Its errors are its declaration's, reported where it is lowered.
        self.diags.truncate(before);
        self.file = saved;
        let TypeExpr::Apply(name, args) = &ty else {
            return None;
        };
        if name != "enum" {
            return None;
        }
        let members = args
            .iter()
            .map(|a| match a {
                TypeExpr::Str(s) | TypeExpr::Name(s) => s.clone(),
                other => crate::inputs::type_text(other),
            })
            .collect();
        // An alias is shown with what it is.
        let expanded = crate::inputs::type_text(&ty);
        let shown = match dotted_text(&t, 0).as_str() {
            "enum" => expanded,
            alias => format!("{alias} = {expanded}"),
        };
        Some((shown, members))
    }
}

/// Whether the clause of `n` (`.. } where run("blue", image, v)`) names
/// `name`: a variable it binds, which the block may pun.
fn clause_binds(n: &SyntaxNode, name: &str) -> bool {
    n.children()
        .filter(|c| c.kind() == CLAUSE)
        .flat_map(|c| c.descendants_with_tokens().collect::<Vec<_>>())
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == IDENT && t.text() == name)
}

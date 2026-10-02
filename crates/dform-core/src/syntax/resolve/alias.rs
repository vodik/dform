//! Type aliases (docs/grammar.md "Type aliases"): `type NAME = TYPE` at a
//! file's top level or in a module or policy. An alias is
//! transparent: each use is its type, expanded while lowering, so nothing
//! after the resolver sees one.
//!
//! Scope: a file's aliases are in scope in the file and in every file that
//! imports it (transitively: an import inlines the file); a module's in
//! the module, and, once it says `export type NAME`, in its file (and so
//! wherever that file is imported). Two aliases of one name in one scope
//! are an error listing both; an alias that reaches itself is an error
//! naming the cycle.

use super::*;

/// Built-in type names an alias may not take.
const BUILTIN: &[&str] = &[
    "int", "string", "bool", "inet", "symbol", "addr", "any", "enum", "list", "set", "secret",
    "ref",
];

/// One `type NAME = TYPE`.
struct Def {
    name: String,
    /// The alias's `diag` source id and the span of its statement.
    file: u32,
    span: Span,
    /// Its `TYPE_EXPR`.
    ty: SyntaxNode,
}

#[derive(Default)]
pub(super) struct Aliases {
    defs: Vec<Def>,
    /// Scope -> name -> the aliases of that name visible there: declared
    /// in it, imported into it, exported into it by one of its modules.
    visible: BTreeMap<usize, BTreeMap<String, BTreeSet<usize>>>,
    /// Each alias's expansion, once made (`None`: it is in error).
    expanded: BTreeMap<usize, Option<TypeExpr>>,
    /// The aliases being expanded, outermost first.
    expanding: Vec<usize>,
}

impl Lowerer<'_> {
    /// Collect every unit's aliases and exports, and what each scope sees.
    pub(super) fn collect_aliases(&mut self) {
        let mut own: BTreeMap<usize, BTreeMap<String, BTreeSet<usize>>> = BTreeMap::new();
        for u in self.units {
            let fs = self.decls.files[&u.file];
            self.alias_stmts(u.file, &u.root, fs, fs, &mut own);
        }
        // A file sees its own aliases and every imported file's.
        for (i, u) in self.units.iter().enumerate() {
            let fs = self.decls.files[&u.file];
            let mut seen = BTreeSet::from([i]);
            let mut stack = u.links.clone();
            let mut vis = own.get(&fs).cloned().unwrap_or_default();
            while let Some(j) = stack.pop() {
                if !seen.insert(j) {
                    continue;
                }
                let other = self.decls.files[&self.units[j].file];
                for (name, ids) in own.get(&other).into_iter().flatten() {
                    vis.entry(name.clone()).or_default().extend(ids);
                }
                stack.extend(self.units[j].links.iter().copied());
            }
            own.insert(fs, vis);
        }
        self.aliases.visible = own;
        self.duplicate_aliases();
    }

    /// The aliases and `export type`s of a statement list: `decl` is the
    /// scope its aliases land in, `file_scope` its file's.
    fn alias_stmts(
        &mut self,
        file: u32,
        parent: &SyntaxNode,
        decl: usize,
        file_scope: usize,
        own: &mut BTreeMap<usize, BTreeMap<String, BTreeSet<usize>>>,
    ) {
        let mut exports = Vec::new();
        for n in parent.children() {
            match n.kind() {
                TYPE_ALIAS => {
                    let Some(ty) = node(&n, TYPE_EXPR) else {
                        continue;
                    };
                    let name = word_text(&n, 1);
                    let span = self.span_in(file, &n);
                    if BUILTIN.contains(&name.as_str()) {
                        self.diags.push(Diagnostic::error(
                            span,
                            format!("type alias `{name}` takes the name of a built-in type"),
                        ));
                        continue;
                    }
                    let id = self.aliases.defs.len();
                    self.aliases.defs.push(Def {
                        name: name.clone(),
                        file,
                        span,
                        ty,
                    });
                    own.entry(decl)
                        .or_default()
                        .entry(name)
                        .or_default()
                        .insert(id);
                }
                EXPORT if tokens(&n).nth(1).is_some_and(|t| t.kind() == TYPE_KW) => {
                    let span = self.span_in(file, &n);
                    if decl == file_scope {
                        self.diags.push(
                            Diagnostic::error(span, "`export type` is a module's")
                                .with_note("a file's aliases are in scope wherever it is imported"),
                        );
                        continue;
                    }
                    exports.push((word_text(&n, 2), span));
                }
                MODULE | POLICY => {
                    let start: u32 = n.text_range().start().into();
                    let inner = self.decls.blocks[&(file, start)];
                    if let Some(b) = node(&n, STMT_BLOCK) {
                        self.alias_stmts(file, &b, inner, file_scope, own);
                    }
                }
                _ => {}
            }
        }
        for (name, span) in exports {
            match own.get(&decl).and_then(|m| m.get(&name)).cloned() {
                Some(ids) => {
                    own.entry(file_scope)
                        .or_default()
                        .entry(name)
                        .or_default()
                        .extend(ids);
                }
                None => self.diags.push(Diagnostic::error(
                    span,
                    format!("`export type {name}`: the module declares no alias {name}"),
                )),
            }
        }
    }

    fn span_in(&self, file: u32, n: &SyntaxNode) -> Span {
        let r = n.text_range();
        Span {
            file,
            start: r.start().into(),
            end: r.end().into(),
            origin: 0,
        }
    }

    /// The aliases of `name` in scope at `scope`.
    fn aliases_at(&self, scope: usize, name: &str) -> BTreeSet<usize> {
        self.chain_of(scope)
            .into_iter()
            .filter_map(|s| self.aliases.visible.get(&s)?.get(name))
            .flatten()
            .copied()
            .collect()
    }

    /// Two aliases of one name in one scope: an error listing both, once
    /// per set of aliases.
    fn duplicate_aliases(&mut self) {
        let scopes: Vec<usize> = self.aliases.visible.keys().copied().collect();
        let mut reported = BTreeSet::new();
        for s in scopes {
            let names: Vec<String> = self.aliases.visible[&s].keys().cloned().collect();
            for name in names {
                let ids = self.aliases_at(s, &name);
                if ids.len() < 2 || !reported.insert(ids.clone()) {
                    continue;
                }
                let ids: Vec<usize> = ids.into_iter().collect();
                let last = &self.aliases.defs[*ids.last().unwrap()];
                let mut d = Diagnostic::error(
                    last.span,
                    format!("{} type aliases named `{name}` are in scope", ids.len()),
                );
                for &i in &ids[..ids.len() - 1] {
                    d = d.with_label(self.aliases.defs[i].span, "and this one");
                }
                self.diags.push(d.with_help(
                    "an alias is in scope in its file, in every file that imports it, and \
                     (with `export type`) in its module's file: rename one",
                ));
            }
        }
    }

    /// The scope a node of the current file is in: its module's or
    /// policy's, else the file's.
    fn scope_of_node(&self, n: &SyntaxNode) -> usize {
        n.ancestors()
            .find(|a| matches!(a.kind(), MODULE | POLICY))
            .and_then(|a| {
                let start: u32 = a.text_range().start().into();
                self.decls.blocks.get(&(self.file, start)).copied()
            })
            .unwrap_or_else(|| self.decls.files.get(&self.file).copied().unwrap_or(PROGRAM))
    }

    /// `name` at the type node `at`, expanded if it is an alias in scope
    /// there.
    pub(super) fn alias(&mut self, at: &SyntaxNode, name: &str) -> Option<TypeExpr> {
        if self.aliases.defs.is_empty() || name.contains('.') {
            return None;
        }
        let ids = self.aliases_at(self.scope_of_node(at), name);
        // Two in scope: reported once, by `duplicate_aliases`.
        let &id = ids.iter().next()?;
        if ids.len() > 1 {
            return Some(TypeExpr::Name(name.to_string()));
        }
        Some(
            self.expand_alias(id)
                .unwrap_or_else(|| TypeExpr::Name(name.to_string())),
        )
    }

    fn expand_alias(&mut self, id: usize) -> Option<TypeExpr> {
        if let Some(t) = self.aliases.expanded.get(&id) {
            return t.clone();
        }
        if let Some(at) = self.aliases.expanding.iter().position(|&x| x == id) {
            let cycle: Vec<usize> = self.aliases.expanding[at..].to_vec();
            let names: Vec<&str> = cycle
                .iter()
                .chain([&id])
                .map(|&i| self.aliases.defs[i].name.as_str())
                .collect();
            let mut d = Diagnostic::error(
                self.aliases.defs[id].span,
                format!("type alias cycle: {}", names.join(" -> ")),
            );
            for &i in &cycle[1..] {
                d = d.with_label(self.aliases.defs[i].span, "in the cycle");
            }
            if cycle.len() == 1 {
                d = d.with_note("an alias may not name itself");
            }
            self.diags.push(d);
            for i in cycle {
                self.aliases.expanded.insert(i, None);
            }
            return None;
        }
        self.aliases.expanding.push(id);
        let (file, ty) = {
            let d = &self.aliases.defs[id];
            (d.file, d.ty.clone())
        };
        let saved = std::mem::replace(&mut self.file, file);
        let before = self.diags.len();
        let t = self.type_expr(&ty);
        self.file = saved;
        self.aliases.expanding.pop();
        // A cycle through this alias has already recorded it as in error.
        if matches!(self.aliases.expanded.get(&id), Some(None)) || self.diags.len() > before {
            self.aliases.expanded.insert(id, None);
            return None;
        }
        self.aliases.expanded.insert(id, Some(t.clone()));
        Some(t)
    }
}

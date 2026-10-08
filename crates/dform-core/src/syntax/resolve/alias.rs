//! Type aliases (docs/grammar.md "Type aliases"): `type NAME = TYPE` at a
//! file's top level or in a component. An alias is transparent: each use
//! is its type, expanded while lowering, so nothing after the resolver
//! sees one.
//!
//! Scope: a file's aliases are in scope in the file, a component's in the
//! component; another module's are public, read by its path or the name a
//! `use` binds, `postgres.conn`, `net.cidr` (R-65). Two aliases of one name
//! in one scope are an error listing both; an alias that reaches itself is
//! an error naming the cycle.

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
    /// Collect every unit's aliases, and what each scope sees.
    pub(super) fn collect_aliases(&mut self) {
        let mut own: BTreeMap<usize, BTreeMap<String, BTreeSet<usize>>> = BTreeMap::new();
        for u in self.units {
            let fs = self.decls.files[&u.file];
            self.alias_stmts(u.file, &u.root, fs, &mut own);
        }
        self.aliases.visible = own;
        self.duplicate_aliases();
    }

    /// The aliases of a statement list: `decl` is the scope they land in.
    fn alias_stmts(
        &mut self,
        file: u32,
        parent: &SyntaxNode,
        decl: usize,
        own: &mut BTreeMap<usize, BTreeMap<String, BTreeSet<usize>>>,
    ) {
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
                COMPONENT => {
                    let start: u32 = n.text_range().start().into();
                    let inner = self.decls.blocks[&(file, start)];
                    if let Some(b) = node(&n, STMT_BLOCK) {
                        self.alias_stmts(file, &b, inner, own);
                    }
                }
                _ => {}
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
                self.diags.push(
                    d.with_help("an alias is in scope in its file or its component: rename one"),
                );
            }
        }
    }

    /// The scope a node of the current file is in: its component's, else
    /// the file's.
    fn scope_of_node(&self, n: &SyntaxNode) -> usize {
        n.ancestors()
            .find(|a| a.kind() == COMPONENT)
            .and_then(|a| {
                let start: u32 = a.text_range().start().into();
                self.decls.blocks.get(&(self.file, start)).copied()
            })
            .unwrap_or_else(|| self.decls.files.get(&self.file).copied().unwrap_or(PROGRAM))
    }

    /// `name` at the type node `at`, expanded if it is an alias in scope
    /// there.
    pub(super) fn alias(&mut self, at: &SyntaxNode, name: &str) -> Option<TypeExpr> {
        self.alias_def(at, name).map(|(t, _)| t)
    }

    /// `alias`, and where the alias is declared (its statement).
    pub(super) fn alias_def(&mut self, at: &SyntaxNode, name: &str) -> Option<(TypeExpr, Span)> {
        let scope = self.scope_of_node(at);
        let ids = self.alias_ids(scope, name);
        // Two in scope: reported once, by `duplicate_aliases`.
        let &id = ids.iter().next()?;
        let span = self.aliases.defs[id].span;
        if ids.len() > 1 {
            return Some((TypeExpr::Name(name.to_string()), span));
        }
        let t = self
            .expand_alias(id)
            .unwrap_or_else(|| TypeExpr::Name(name.to_string()));
        Some((t, span))
    }

    /// The aliases `name`, written in `scope`, names: one in scope there,
    /// or another module's by a name a `use` there binds or its path from
    /// the root (R-65), its own, not what is visible there. Lexical: a
    /// module another file uses is not in scope here (R-208).
    fn alias_ids(&self, scope: usize, name: &str) -> BTreeSet<usize> {
        if self.aliases.defs.is_empty() {
            return BTreeSet::new();
        }
        match name.rsplit_once('.') {
            Some((m, alias)) => {
                let path = self.module_path_of(scope, m);
                self.decls
                    .modules
                    .get(&path)
                    .and_then(|d| self.aliases.visible.get(&d.scope)?.get(alias))
                    .cloned()
                    .unwrap_or_default()
            }
            None => self.aliases_at(scope, name),
        }
    }

    /// Whether `name`, written in `scope`, is a type alias: a dotted name
    /// is one before it is a resource type (`types.environment`).
    pub(super) fn names_alias(&self, scope: usize, name: &str) -> bool {
        !self.alias_ids(scope, name).is_empty()
    }

    /// An output typed by an alias by path holds a value, not a
    /// reference: `collect` met it before the aliases were known.
    pub(super) fn unalias_outputs(&mut self) {
        for scope in 0..self.decls.scopes.len() {
            let aliased: Vec<String> = self.decls.scopes[scope]
                .outputs
                .iter()
                .filter(|(_, t)| t.as_deref().is_some_and(|t| self.names_alias(scope, t)))
                .map(|(k, _)| k.clone())
                .collect();
            for k in aliased {
                self.decls.scopes[scope].outputs.insert(k, None);
            }
        }
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

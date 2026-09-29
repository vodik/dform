//! Rename: every place that denotes a name (`refs`), rewritten. A
//! resource, an instance or a module whose addresses have state in the
//! selected deployment also gets, in the same edit, a `moved(T, "old",
//! "new")` fact per address beside its declaration, so the rename plans as
//! a move and not a destroy and a create. Keywords, builtins, schema types
//! and what a provider owns (attribute paths, its relations) are refused.

use crate::refs::{self, Decls, Parsed, Project, Symbol, What};
use crate::text;
use anyhow::{Result, anyhow, bail};
use dform_core::ast::Term;
use dform_core::syntax::{SyntaxKind, SyntaxToken};
use dform_core::value::Value;
use lsp_types::{Range, TextEdit};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// `textDocument/prepareRename`: the name's range and text, or why it
/// cannot be renamed.
pub fn prepare(p: &Project, path: &Path, at: usize) -> Result<(Range, String)> {
    let files = p.parse();
    let d = Decls::of(files.iter().map(|f| &f.tree));
    let (f, t, what) = refs::at(&d, &files, path, at).ok_or_else(|| anyhow!("no name here"))?;
    renameable(&what, &t)?;
    let r = t.text_range();
    Ok((
        text::range(&f.text, r.start().into(), r.end().into()),
        t.text().to_string(),
    ))
}

/// What `t` denotes, when a rename may change it.
fn renameable(what: &What, t: &SyntaxToken) -> Result<Symbol> {
    let name = t.text();
    if t.kind().is_keyword() {
        bail!("`{name}` is a keyword");
    }
    match what {
        What::Name(Symbol::Predicate(n), _) if builtin(n) => bail!("`{n}` is a builtin"),
        What::Name(Symbol::Predicate(n), _) if dform_core::loader::is_core_pred(n) => {
            bail!("`{n}` is dform's own relation (the compiler's or a provider's)")
        }
        What::Name(s, _) => Ok(s.clone()),
        What::Type => bail!("`{name}` is part of a schema type's name: its provider's"),
        What::Path => bail!("`{name}` is an attribute path of the provider's schema"),
        What::Provider => bail!("`{name}` is a provider's name"),
        What::Other => bail!(
            "`{name}` is not a name a rename changes (a variable, an output, a key or a string)"
        ),
    }
}

/// The builtins: functions, aggregates, `env_var`.
fn builtin(n: &str) -> bool {
    dform_core::engine::FUNCTIONS.contains(&n)
        || dform_core::ir::ops::is_builtin_pred(n)
        || matches!(n, "count" | "collect" | "collect_set" | "collect_list")
        || n == dform_core::syntax::resolve::ENV_VAR
}

fn describe(s: &Symbol) -> String {
    match s {
        Symbol::Predicate(_) => "a relation".into(),
        Symbol::Value(Some(scope), _) => format!("a value name in {scope}"),
        Symbol::Value(None, _) => "a value name".into(),
        Symbol::Let(Some(scope), _) => format!("a let in {scope}"),
        Symbol::Let(None, _) => "a let".into(),
        Symbol::Alias(_) => "a type alias".into(),
        Symbol::Module(_) => "a module".into(),
        Symbol::Instance(m, _) => format!("an instance of module {m}"),
        Symbol::Resource(Some(m), _) => format!("a resource in module {m}"),
        Symbol::Resource(None, _) => "a resource".into(),
        Symbol::Settings(_) => "a settings row".into(),
        Symbol::Policy(_) => "a policy".into(),
    }
}

/// `textDocument/rename` of the name at `at` to `new`: a workspace edit
/// of `changes`.
pub fn rename(p: &Project, path: &Path, at: usize, new: &str) -> Result<Json> {
    let files = p.parse();
    let d = Decls::of(files.iter().map(|f| &f.tree));
    let (_, t, what) = refs::at(&d, &files, path, at).ok_or_else(|| anyhow!("no name here"))?;
    let sym = renameable(&what, &t)?;
    let old = t.text().to_string();
    let lexed = dform_core::syntax::parser::parse(new).syntax();
    if lexed
        .first_token()
        .map(|x| (x.kind(), x.text().to_string()))
        != Some((SyntaxKind::IDENT, new.to_string()))
    {
        bail!("`{new}` is not a name (a keyword, or not one word)");
    }
    let mut changes: BTreeMap<PathBuf, Vec<TextEdit>> = BTreeMap::new();
    if new == old {
        return Ok(json!({ "changes": {} }));
    }
    if d.taken(&sym, new) {
        bail!("`{new}` is already {}", describe(&sym));
    }
    let found = refs::occurrences(&d, &files, &sym);
    for (f, t, _) in &found {
        let r = t.text_range();
        changes
            .entry(f.path.clone())
            .or_default()
            .push(TextEdit::new(
                text::range(&f.text, r.start().into(), r.end().into()),
                new.to_string(),
            ));
    }
    let moves = moves(p, &d, &sym, &old, new);
    if let Some((f, at, indent)) = beside(&found)
        && !moves.is_empty()
    {
        let facts: String = moves
            .iter()
            .map(|(typ, a, b)| format!("\n{indent}moved({typ:?}, {a:?}, {b:?})"))
            .collect();
        let pos = text::range(&f.text, at, at);
        changes
            .entry(f.path.clone())
            .or_default()
            .push(TextEdit::new(pos, facts));
    }
    let by_uri: serde_json::Map<String, Json> = changes
        .into_iter()
        .map(|(f, edits)| {
            Ok((
                text::uri_of(&f).as_str().to_string(),
                serde_json::to_value(edits)?,
            ))
        })
        .collect::<Result<_>>()?;
    Ok(json!({ "changes": by_uri }))
}

/// Where the `moved` facts go: just after the declaration's block, at its
/// indentation.
fn beside<'p>(found: &[(&'p Parsed, SyntaxToken, bool)]) -> Option<(&'p Parsed, usize, String)> {
    let (f, t, _) = found.iter().find(|(_, _, is_decl)| *is_decl)?;
    let node = t.parent()?;
    if !matches!(
        node.kind(),
        SyntaxKind::RESOURCE | SyntaxKind::INSTANCE | SyntaxKind::MODULE
    ) {
        return None;
    }
    let start: usize = node.text_range().start().into();
    let line = f.text[..start].rfind('\n').map_or(0, |i| i + 1);
    let indent: String = f.text[line..start]
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    Some((f, node.text_range().end().into(), indent))
}

/// The `(type, old, new)` addresses the rename of `sym` from `old` to
/// `new` changes that have state in the selected deployment (and whose
/// new address has none: `moved` applies only then).
fn moves(
    p: &Project,
    d: &Decls,
    sym: &Symbol,
    old: &str,
    new: &str,
) -> Vec<(String, String, String)> {
    let held = identities(p);
    let renamed = |typ: &str, a: &str| -> Option<String> {
        match sym {
            Symbol::Resource(None, _) => {
                (Some(typ) == d.type_of(&None, old) && a == old).then(|| new.to_string())
            }
            Symbol::Resource(Some(m), _) => {
                let (inst, local) = a.split_once("::")?;
                (Some(typ) == d.type_of(&Some(m.clone()), old)
                    && local == old
                    && inst.split_once('.')?.0 == m)
                    .then(|| format!("{inst}::{new}"))
            }
            Symbol::Instance(m, _) => {
                let rest = a.strip_prefix(&format!("{m}.{old}::"))?;
                Some(format!("{m}.{new}::{rest}"))
            }
            Symbol::Module(_) => {
                let (inst, rest) = a.split_once("::")?;
                let i = inst.strip_prefix(&format!("{old}."))?;
                Some(format!("{new}.{i}::{rest}"))
            }
            _ => None,
        }
    };
    held.iter()
        .filter_map(|(typ, a)| {
            let b = renamed(typ, a)?;
            (!held.contains(&(typ.clone(), b.clone()))).then(|| (typ.clone(), a.clone(), b))
        })
        .collect()
}

/// Every `(type, address)` the selected deployment's state maps to a live
/// object: the evaluations' `identity` facts.
fn identities(p: &Project) -> BTreeSet<(String, String)> {
    let s = |t: Option<&Term>| match t {
        Some(Term::Val(Value::Str(s))) => Some(s.clone()),
        _ => None,
    };
    p.evaluated
        .iter()
        .flat_map(|e| &e.res.facts)
        .filter(|a| a.pred == "identity")
        .filter_map(|a| Some((s(a.args.first())?, s(a.args.get(1))?)))
        .collect()
}

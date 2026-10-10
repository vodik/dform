//! Rename: every place that denotes a name (`refs`), rewritten. A
//! resource, an instance or a module whose addresses have state in the
//! selected deployment also gets, in the same edit, a `moved(T, "old",
//! new)` fact per address beside its declaration, so the rename plans as
//! a move and not a destroy and a create. Keywords, builtins, schema types
//! and what a provider owns (attribute paths, its relations) are refused.

use crate::analysis::{Outcome, Severity};
use crate::refs::Project;
use crate::text;
use anyhow::{Result, anyhow, bail};
use dform_core::address::{scope_split, scoped};
use dform_core::ast::Term;
use dform_core::names::{self, Parsed, Symbol, What};
use dform_core::syntax::{SyntaxKind, SyntaxToken};
use dform_core::value::Value;
use lsp_types::{Range, TextEdit};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// `textDocument/prepareRename`: the name's range and text, or why it
/// cannot be renamed; `None` where no word is (a literal, a comment,
/// whitespace, punctuation).
pub fn prepare(p: &Project, path: &Path, at: usize) -> Result<Option<(Range, String)>> {
    let files = p.parse();
    let d = p.decls(&files);
    let Some(names::Named {
        file: f,
        token: t,
        range: r,
        what,
    }) = d.at(&files, path, at).filter(|n| n.token.kind().is_word())
    else {
        return Ok(None);
    };
    // A component's resource whose name is also a string an `m[e]`
    // reads: the rename would not change the string, and `m[e]` would no
    // longer find it.
    if let Symbol::Instance(path, i) = renameable(&what, &t)?
        && let m = path.rsplit('.').next().unwrap_or(&path).to_string()
        && names::indexed(&files, &m)
    {
        let places: Vec<String> = names::strings(&files, &i)
            .into_iter()
            .map(|(f, s)| {
                let at = text::position(&f.text, s.text_range().start().into());
                let name = f.path.strip_prefix(&p.dir).unwrap_or(&f.path);
                format!("{}:{}:{}", name.display(), at.line + 1, at.character + 1)
            })
            .collect();
        if !places.is_empty() {
            bail!(
                "resource {path} {i} is also the string \"{i}\" at {}, and `{m}[..]` reads \
                 the component's resources by such strings: a rename would not change the \
                 string",
                places.join(", ")
            );
        }
    }
    Ok(Some((
        text::range(&f.text, r.start().into(), r.end().into()),
        t.text().to_string(),
    )))
}

/// What `t` denotes, when a rename may change it.
fn renameable(what: &What, t: &SyntaxToken) -> Result<Symbol> {
    let name = t.text();
    if t.kind().is_keyword() {
        bail!("`{name}` is a keyword");
    }
    if let Some(f) = called_function(t) {
        bail!("`{f}` is a builtin");
    }
    match what {
        What::Name(Symbol::Predicate(_, n), _) if builtin(n) => bail!("`{n}` is a builtin"),
        What::Name(Symbol::Predicate(_, n), _) if dform_core::loader::is_core_pred(n) => {
            bail!("`{n}` is dform's own relation (the compiler's or a provider's)")
        }
        What::Name(Symbol::Function(f) | Symbol::Package(f), _) => bail!("`{f}` is a builtin"),
        What::Name(Symbol::File(f), _) => bail!("`{f}` is a file's path: rename the file"),
        What::Name(s, _) => Ok(s.clone()),
        What::Names(ss) => bail!(
            "`{name}` names {} resources of different types here: rename it at its declaration",
            ss.len()
        ),
        What::Type(_) => bail!("`{name}` is part of a schema type's name: its provider's"),
        What::Path => bail!("`{name}` is an attribute path of the provider's schema"),
        What::Provider => bail!("`{name}` is a provider's name"),
        What::Variable | What::Key | What::Other => {
            bail!("`{name}` is not a name a rename changes (a variable, a key or a string)")
        }
    }
}

/// The function a call names when `t` is a segment of its name
/// (`inet` in `inet.host(..)`).
fn called_function(t: &SyntaxToken) -> Option<String> {
    let chain = t.parent().filter(|c| c.kind() == SyntaxKind::CHAIN)?;
    chain.parent().filter(|c| c.kind() == SyntaxKind::CALL)?;
    let name: String = chain
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|x| !x.kind().is_trivia())
        .map(|x| x.text().to_string())
        .collect();
    dform_core::functions::get(&name).map(|_| name)
}

/// The builtins: functions, aggregates, `env.var`.
fn builtin(n: &str) -> bool {
    dform_core::functions::get(n).is_some()
        || matches!(n, "count" | "collect_set" | "collect_list")
        || n == dform_core::syntax::resolve::ENV_VAR
}

fn describe(s: &Symbol) -> String {
    match s {
        Symbol::Predicate(Some(scope), _) => format!("a relation of {scope}"),
        Symbol::Predicate(None, _) => "a relation".into(),
        Symbol::Value(Some(scope), _) => format!("a value name in {scope}"),
        Symbol::Value(None, _) => "a value name".into(),
        Symbol::Let(Some(scope), _) => format!("a let in {scope}"),
        Symbol::Let(None, _) => "a let".into(),
        Symbol::Alias(_) => "a type alias".into(),
        Symbol::Module(_) => "a component or a used module".into(),
        Symbol::Instance(m, _) => format!("a resource of the component {m}"),
        Symbol::Field(_, i, _) => format!("a field of input {i}"),
        Symbol::Output(Some(scope), _) => format!("an output of {scope}"),
        Symbol::Output(None, _) => "an output".into(),
        Symbol::Resource(Some(scope), _, _) => format!("a resource in {scope}"),
        Symbol::Resource(None, _, _) => "a resource".into(),
        Symbol::Function(_) | Symbol::Package(_) => "a builtin".into(),
        Symbol::File(_) => "a file".into(),
    }
}

/// Bytes `start..end` of a text replaced.
type Splice = (usize, usize, String);

/// A rename: the edit, per file its edits in bytes and its text after
/// them, and how it renames addresses.
pub struct Renaming {
    sym: Symbol,
    old: String,
    new: String,
    /// A renamed resource's type.
    typ: Option<String>,
    files: BTreeMap<PathBuf, (String, Vec<Splice>)>,
}

/// `textDocument/rename` of the name at `at` to `new`.
pub fn rename(p: &Project, path: &Path, at: usize, new: &str) -> Result<Renaming> {
    let files = p.parse();
    let d = p.decls(&files);
    let here = d
        .at(&files, path, at)
        .ok_or_else(|| anyhow!("no name here"))?;
    let t = here.token;
    let sym = renameable(&here.what, &t)?;
    let old = t.text().to_string();
    let lexed = dform_core::syntax::parser::parse(new).syntax();
    if lexed
        .first_token()
        .map(|x| (x.kind(), x.text().to_string()))
        != Some((SyntaxKind::IDENT, new.to_string()))
    {
        bail!("`{new}` is not a name (a keyword, or not one word)");
    }
    let typ = match &sym {
        Symbol::Resource(_, t, _) => Some(t.clone()),
        _ => None,
    };
    let mut r = Renaming {
        sym: sym.clone(),
        old: old.clone(),
        new: new.to_string(),
        typ,
        files: BTreeMap::new(),
    };
    if new == old {
        return Ok(r);
    }
    if d.taken(&sym, new) {
        bail!("`{new}` is already {}", describe(&sym));
    }
    let found = d.occurrences(&files, &sym);
    let moves = moves(p, &r);
    // An address written as a string, `T["m/i/n"]` (H-16).
    let strings: Vec<_> = names::addresses(&files)
        .into_iter()
        .filter_map(|(f, t, typ, a)| Some((f, t, format!("{:?}", r.address(&typ, &a)?))))
        .collect();
    let mut edit = |f: &Parsed, s: usize, e: usize, t: String| {
        r.files
            .entry(f.path.clone())
            .or_insert_with(|| (f.text.clone(), Vec::new()))
            .1
            .push((s, e, t));
    };
    for n in &found {
        // A pun keeps its key: `vpc` becomes `vpc = net0`.
        let to = match n.token.parent() {
            Some(p) if names::is_pun(&p, &n.token) => format!("{old} = {new}"),
            _ => new.to_string(),
        };
        edit(n.file, n.range.start().into(), n.range.end().into(), to);
    }
    for (f, t, to) in strings {
        let range = t.text_range();
        edit(f, range.start().into(), range.end().into(), to);
    }
    if let Some((f, at, indent)) = beside(&found)
        && !moves.is_empty()
    {
        let facts: String = moves
            .iter()
            .map(|(typ, a, b)| format!("\n{indent}moved({typ}, {a:?}, {})", reference(typ, b)))
            .collect();
        edit(f, at, at, facts);
    }
    Ok(r)
}

impl Renaming {
    /// The workspace edit, as `changes`.
    pub fn edit(&self) -> Result<Json> {
        let mut by_uri = serde_json::Map::new();
        for (f, (text, edits)) in &self.files {
            let edits: Vec<TextEdit> = edits
                .iter()
                .map(|(s, e, t)| TextEdit::new(text::range(text, *s, *e), t.clone()))
                .collect();
            by_uri.insert(
                text::uri_of(f).as_str().to_string(),
                serde_json::to_value(edits)?,
            );
        }
        Ok(json!({ "changes": by_uri }))
    }

    /// Each file the edit changes, and its text after it.
    pub fn texts(&self) -> BTreeMap<PathBuf, String> {
        self.files
            .iter()
            .map(|(f, (text, edits))| {
                let mut out = text.clone();
                let mut edits = edits.clone();
                edits.sort_by_key(|(s, _, _)| std::cmp::Reverse(*s));
                for (s, e, t) in edits {
                    out.replace_range(s..e, &t);
                }
                (f.clone(), out)
            })
            .collect()
    }

    /// The address `a` of type `typ` becomes, when the rename changes it.
    fn address(&self, typ: &str, a: &str) -> Option<String> {
        let (old, new) = (&self.old, &self.new);
        match &self.sym {
            Symbol::Resource(None, _, _) => {
                (Some(typ) == self.typ.as_deref() && a == old).then(|| new.clone())
            }
            // An address of a copy is its path, `instance.name` (R-65,
            // R-112): the copy does not say its component.
            Symbol::Resource(Some(_), _, _) => {
                let (inst, local) = scope_split(a)?;
                (Some(typ) == self.typ.as_deref() && local == old).then(|| scoped(inst, new))
            }
            Symbol::Instance(_, _) => {
                let rest = a.strip_prefix(&scoped(old, ""))?;
                Some(scoped(new, rest))
            }
            _ => None,
        }
    }

    /// Refuse the rename unless the program means what it meant: each
    /// stack evaluated `after` it has no diagnostic it had not `before`
    /// (but for the new name in place of the old), and plans what it
    /// planned, at the renamed addresses (the moves the edit adds keep a
    /// renamed object undeformed).
    pub fn verify(&self, before: &[&Outcome], after: &[Outcome]) -> Result<()> {
        let mut changed = Vec::new();
        for (b, a) in before.iter().zip(after) {
            let known: BTreeSet<String> = b
                .problems
                .iter()
                .flat_map(|p| [p.message.clone(), p.message.replace(&self.old, &self.new)])
                .collect();
            for p in &a.problems {
                if !known.contains(&p.message) {
                    changed.push(format!("a new {}: {}", severity(p.severity), p.message));
                }
            }
            let deployment = a
                .evaluated
                .as_ref()
                .or(b.evaluated.as_ref())
                .map_or(String::new(), |e| e.deployment.clone());
            let planned_before: BTreeSet<(String, String, String)> = deformations(b)
                .into_iter()
                .map(|(k, t, x)| {
                    let x = self.address(&t, &x).unwrap_or(x);
                    (k, t, x)
                })
                .collect();
            let planned_after = deformations(a);
            let at = |t: &String, x: &String| dform_core::address::Address {
                typ: t.clone(),
                name: x.clone(),
            };
            for (k, t, x) in planned_before.difference(&planned_after) {
                changed.push(format!(
                    "{deployment} would no longer plan {k} {}",
                    at(t, x)
                ));
            }
            for (k, t, x) in planned_after.difference(&planned_before) {
                changed.push(format!("{deployment} would plan {k} {}", at(t, x)));
            }
        }
        if !changed.is_empty() {
            bail!(
                "renaming `{}` to `{}` would change what the program means:\n- {}",
                self.old,
                self.new,
                changed.join("\n- ")
            );
        }
        Ok(())
    }
}

fn severity(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
    }
}

/// The new side of a `moved` fact (R-42): a resource of the stack by its
/// name, one of a module instance by its address, which a module body
/// does not scope again.
fn reference(typ: &str, b: &str) -> String {
    if dform_core::address::is_scoped(b) {
        dform_core::address::Address {
            typ: typ.to_string(),
            name: b.to_string(),
        }
        .to_string()
    } else {
        b.to_string()
    }
}

/// The plan of an evaluation: its policy pass's `deformation(Kind, r, _)`
/// facts, as (Kind, T, A).
fn deformations(o: &Outcome) -> BTreeSet<(String, String, String)> {
    let Some(e) = &o.evaluated else {
        return BTreeSet::new();
    };
    e.res
        .facts
        .iter()
        .filter(|a| a.pred == "deformation")
        .filter_map(|a| {
            let r = dform_core::zset::referenced(a.args.get(1)?)?;
            Some((str_of(a.args.first())?, r.typ, r.name))
        })
        .collect()
}

fn str_of(t: Option<&Term>) -> Option<String> {
    match t {
        Some(Term::Val(Value::Str(s))) => Some(s.clone()),
        _ => None,
    }
}

/// Where the `moved` facts go: just after the declaration's block, at its
/// indentation.
fn beside<'p>(found: &[names::Named<'p>]) -> Option<(&'p Parsed, usize, String)> {
    let decl = found.iter().find(|n| n.is_declaration())?;
    let f = decl.file;
    let node = decl.token.parent()?;
    if !matches!(node.kind(), SyntaxKind::RESOURCE | SyntaxKind::COMPONENT) {
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

/// The `(type, old, new)` addresses the rename changes that have state in
/// the selected deployment (and whose new address has none: `moved`
/// applies only then).
fn moves(p: &Project, r: &Renaming) -> Vec<(String, String, String)> {
    let held = identities(p);
    held.iter()
        .filter_map(|(typ, a)| {
            let b = r.address(typ, a)?;
            (!held.contains(&(typ.clone(), b.clone()))).then(|| (typ.clone(), a.clone(), b))
        })
        .collect()
}

/// Every `(type, address)` the selected deployment's state maps to a live
/// object: the evaluations' `identity` facts.
fn identities(p: &Project) -> BTreeSet<(String, String)> {
    p.evaluated
        .iter()
        .flat_map(|e| &e.res.facts)
        .filter(|a| a.pred == "identity")
        .filter_map(|a| Some((str_of(a.args.first())?, str_of(a.args.get(1))?)))
        .collect()
}

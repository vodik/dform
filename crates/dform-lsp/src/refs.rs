//! References: what a name at a place denotes, read off the syntax trees
//! of every file of the project in the resolver's order (docs/grammar.md
//! "Names"), and every place that denotes the same; on an attribute path,
//! the contributions to its cell the evaluation found (the hover's list).

use crate::analysis::{self, Evaluated, Where};
use crate::nav::{declared_name, head_name};
use crate::{explain, text};
use dform_core::circuit::View;
use dform_core::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use lsp_types::Location;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The block a name is declared in, innermost: `module network`, `policy
/// baseline`, `scenario dev`; `None` for the program's top.
pub type Scope = Option<String>;

/// A name the program declares.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Symbol {
    /// A relation, an extern or a builtin: `p(..)`, `p[..]`, `decl p/N`.
    Predicate(String),
    /// An input, a module input or a value rule `k = t`, in its scope.
    Value(Scope, String),
    /// `let a = CHAIN`, in its scope.
    Let(Scope, String),
    /// `type NAME = TYPE`.
    Alias(String),
    Module(String),
    /// `instance m i`: module and instance.
    Instance(String, String),
    /// `resource T n`: the module it is declared in, and its name.
    Resource(Option<String>, String),
    /// `settings n`.
    Settings(String),
    Policy(String),
}

/// What a name token is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    /// A declared name, and whether this is its declaration.
    Name(Symbol, bool),
    /// A schema type's name, or a segment of one.
    Type,
    /// An attribute path: a field of a resource or settings block, a
    /// segment after a reference.
    Path,
    /// A provider block's name.
    Provider,
    /// A variable, an output's name, a key: nothing references follow.
    Other,
}

/// The declarations of every file of a project, which the order of
/// resolution consults.
#[derive(Debug, Default)]
pub struct Decls {
    lets: BTreeSet<(Scope, String)>,
    values: BTreeSet<(Scope, String)>,
    /// By module and name: each static resource's type.
    resources: BTreeMap<(Option<String>, String), String>,
    modules: BTreeSet<String>,
    predicates: BTreeSet<String>,
    aliases: BTreeSet<String>,
    /// Resource headers' and `type` blocks' types.
    types: BTreeSet<String>,
    policies: BTreeSet<String>,
    settings: BTreeSet<String>,
    instances: BTreeSet<(String, String)>,
}

impl Decls {
    pub fn of<'a>(trees: impl IntoIterator<Item = &'a SyntaxNode>) -> Decls {
        let mut d = Decls::default();
        for root in trees {
            for n in root.descendants() {
                let scope = || scope_of(&n.parent().unwrap_or_else(|| n.clone()));
                let name = || declared_name(&n).map(|t| t.text().to_string());
                match n.kind() {
                    SyntaxKind::LET => d.lets.extend(name().map(|x| (scope(), x))),
                    SyntaxKind::INPUT | SyntaxKind::VALUE_RULE => {
                        d.values.extend(name().map(|x| (scope(), x)))
                    }
                    SyntaxKind::TYPE_ALIAS => d.aliases.extend(name()),
                    SyntaxKind::MODULE => d.modules.extend(name()),
                    SyntaxKind::POLICY => d.policies.extend(name()),
                    SyntaxKind::INSTANCE => {
                        let ids = idents(&n);
                        if let [m, i, ..] = ids.as_slice() {
                            d.instances
                                .insert((m.text().to_string(), i.text().to_string()));
                        }
                    }
                    SyntaxKind::RESOURCE => {
                        if let Some(h) = header(&n) {
                            d.types.insert(h.typ.clone());
                            if h.is_static {
                                d.resources.insert(
                                    (module_of(&scope()), h.name.text().to_string()),
                                    h.typ,
                                );
                            }
                        }
                    }
                    SyntaxKind::SETTINGS => {
                        if let Some(h) = header(&n)
                            && h.is_static
                        {
                            d.settings.insert(h.name.text().to_string());
                        }
                    }
                    SyntaxKind::TYPE_DECL => {
                        let t = dotted_after_keyword(&n);
                        if !t.is_empty() {
                            d.types.insert(t);
                        }
                    }
                    SyntaxKind::RULE | SyntaxKind::FACT => d
                        .predicates
                        .extend(head_name(&n).map(|t| t.text().to_string())),
                    SyntaxKind::DECL | SyntaxKind::EXTERN | SyntaxKind::INPUT_RELATION => d
                        .predicates
                        .extend(relation_name(&n).map(|t| t.text().to_string())),
                    _ => {}
                }
            }
        }
        d
    }

    /// The value, let or resource `name` seen from `scope`: its own, else
    /// the program's.
    fn lookup<T: Ord>(
        set: &BTreeSet<(Option<String>, String)>,
        scope: &Scope,
        name: &str,
        wrap: impl Fn(Scope, String) -> T,
    ) -> Option<T> {
        [scope.clone(), None]
            .into_iter()
            .find(|s| set.contains(&(s.clone(), name.to_string())))
            .map(|s| wrap(s, name.to_string()))
    }

    fn value(&self, scope: &Scope, name: &str) -> Option<Symbol> {
        Decls::lookup(&self.values, scope, name, Symbol::Value)
    }

    fn let_(&self, scope: &Scope, name: &str) -> Option<Symbol> {
        Decls::lookup(&self.lets, scope, name, Symbol::Let)
    }

    /// The resource `name` (of type `typ`, when given) seen from `scope`.
    fn resource(&self, scope: &Scope, name: &str, typ: Option<&str>) -> Option<Symbol> {
        let module = module_of(scope);
        [module, None].into_iter().find_map(|m| {
            let t = self.resources.get(&(m.clone(), name.to_string()))?;
            typ.is_none_or(|x| x == t)
                .then(|| Symbol::Resource(m, name.to_string()))
        })
    }

    /// A resource's type.
    pub fn type_of(&self, module: &Option<String>, name: &str) -> Option<&str> {
        self.resources
            .get(&(module.clone(), name.to_string()))
            .map(String::as_str)
    }

    /// Whether `sym`'s kind already has a declaration named `name` where
    /// `sym` is (a rename onto it would merge the two).
    pub fn taken(&self, sym: &Symbol, name: &str) -> bool {
        let n = name.to_string();
        match sym {
            Symbol::Predicate(_) => self.predicates.contains(name),
            Symbol::Value(s, _) => self.values.contains(&(s.clone(), n)),
            Symbol::Let(s, _) => self.lets.contains(&(s.clone(), n)),
            Symbol::Alias(_) => self.aliases.contains(name),
            Symbol::Module(_) => self.modules.contains(name),
            Symbol::Instance(m, _) => self.instances.contains(&(m.clone(), n)),
            Symbol::Resource(m, _) => self.resources.contains_key(&(m.clone(), n)),
            Symbol::Settings(_) => self.settings.contains(name),
            Symbol::Policy(_) => self.policies.contains(name),
        }
    }
}

/// The innermost module, policy or scenario `node` is in (itself
/// included).
pub fn scope_of(node: &SyntaxNode) -> Scope {
    node.ancestors().find_map(|a| {
        let kw = match a.kind() {
            SyntaxKind::MODULE => "module",
            SyntaxKind::POLICY => "policy",
            SyntaxKind::SCENARIO => "scenario",
            _ => return None,
        };
        Some(format!("{kw} {}", declared_name(&a)?.text()))
    })
}

/// The module of a scope.
pub fn module_of(scope: &Scope) -> Option<String> {
    scope
        .as_deref()
        .and_then(|s| s.strip_prefix("module "))
        .map(str::to_string)
}

/// A node's own tokens, trivia left out.
fn own_tokens(n: &SyntaxNode) -> Vec<SyntaxToken> {
    n.children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
        .collect()
}

fn idents(n: &SyntaxNode) -> Vec<SyntaxToken> {
    own_tokens(n)
        .into_iter()
        .filter(|t| t.kind() == SyntaxKind::IDENT)
        .collect()
}

/// The dotted name right after a node's keyword: `net.vpc` of `type
/// net.vpc {`.
fn dotted_after_keyword(n: &SyntaxNode) -> String {
    let mut out = String::new();
    for t in own_tokens(n).into_iter().skip(1) {
        match t.kind() {
            SyntaxKind::IDENT | SyntaxKind::DOT => out.push_str(t.text()),
            _ => break,
        }
    }
    out
}

/// The name a `decl`, `extern` or `input relation` declares, when it is
/// one name (`decl p/2`; not `decl type ..` nor a dotted extern).
fn relation_name(n: &SyntaxNode) -> Option<SyntaxToken> {
    let ts = own_tokens(n);
    if ts.iter().any(|t| t.kind() == SyntaxKind::TYPE_KW) {
        return None;
    }
    let mut ids = ts.iter().skip_while(|t| !t.kind().is_keyword()).skip(1);
    let first = if n.kind() == SyntaxKind::INPUT_RELATION {
        // `input relation NAME`: `relation` is a name the parser reads.
        ids.find(|t| t.kind() == SyntaxKind::IDENT && t.text() != "relation")?
    } else {
        ids.next()?
    };
    let next = ts
        .iter()
        .skip_while(|t| *t != first)
        .nth(1)
        .map(SyntaxToken::kind);
    (first.kind() == SyntaxKind::IDENT && next != Some(SyntaxKind::DOT)).then(|| first.clone())
}

/// A resource or settings header: its type (a resource's), its name's
/// token, and whether that name is static (no clause of the block binds
/// it).
pub struct Header {
    pub typ: String,
    pub name: SyntaxToken,
    pub is_static: bool,
    /// The type's tokens.
    pub type_tokens: Vec<SyntaxToken>,
}

pub fn header(n: &SyntaxNode) -> Option<Header> {
    let ts: Vec<SyntaxToken> = own_tokens(n)
        .into_iter()
        .filter(|t| t.kind() != SyntaxKind::RANK)
        .collect();
    let (name, rest) = ts.split_last()?;
    if !matches!(name.kind(), SyntaxKind::IDENT | SyntaxKind::STRING) {
        return None;
    }
    let type_tokens: Vec<SyntaxToken> = rest.iter().skip(1).cloned().collect();
    let typ = type_tokens.iter().map(|t| t.text()).collect::<String>();
    let block = n.children().find(|c| c.kind() == SyntaxKind::BLOCK);
    let bound = block.is_some_and(|b| {
        b.children()
            .filter(|c| c.kind() == SyntaxKind::CLAUSE)
            .flat_map(|c| c.descendants_with_tokens())
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == SyntaxKind::IDENT && t.text() == name.text())
    });
    Some(Header {
        typ,
        name: name.clone(),
        is_static: name.kind() == SyntaxKind::IDENT && !bound,
        type_tokens,
    })
}

/// A chain's parts, in order.
#[derive(Debug)]
enum Part {
    Name(SyntaxToken),
    Settings,
    Dot,
    Slash,
    Index,
}

fn parts(chain: &SyntaxNode) -> Vec<Part> {
    chain
        .children_with_tokens()
        .filter_map(|e| match e {
            rowan::NodeOrToken::Node(n) => (n.kind() == SyntaxKind::INDEX).then_some(Part::Index),
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::IDENT => Some(Part::Name(t)),
                SyntaxKind::SETTINGS_KW => Some(Part::Settings),
                SyntaxKind::DOT => Some(Part::Dot),
                SyntaxKind::SLASH => Some(Part::Slash),
                _ => None,
            },
        })
        .collect()
}

/// What the name token `t` is.
pub fn classify(d: &Decls, t: &SyntaxToken) -> What {
    if t.kind() != SyntaxKind::IDENT {
        return What::Other;
    }
    let Some(parent) = t.parent() else {
        return What::Other;
    };
    let name = t.text().to_string();
    let is_declared = || declared_name(&parent).as_ref() == Some(t);
    let scope = scope_of(&parent.parent().unwrap_or_else(|| parent.clone()));
    let decl = |s: Symbol| What::Name(s, true);
    let used = |s: Option<Symbol>| s.map_or(What::Other, |s| What::Name(s, false));
    match parent.kind() {
        SyntaxKind::LET if is_declared() => decl(Symbol::Let(scope, name)),
        SyntaxKind::INPUT | SyntaxKind::VALUE_RULE if is_declared() => {
            decl(Symbol::Value(scope, name))
        }
        SyntaxKind::TYPE_ALIAS if is_declared() => decl(Symbol::Alias(name)),
        SyntaxKind::MODULE if is_declared() => decl(Symbol::Module(name)),
        SyntaxKind::POLICY if is_declared() => decl(Symbol::Policy(name)),
        SyntaxKind::APPLY => used(Some(Symbol::Policy(name))),
        SyntaxKind::PROVIDER => What::Provider,
        SyntaxKind::DECL | SyntaxKind::EXTERN | SyntaxKind::INPUT_RELATION
            if relation_name(&parent).as_ref() == Some(t) =>
        {
            decl(Symbol::Predicate(name))
        }
        SyntaxKind::EXPORT => {
            if own_tokens(&parent)
                .iter()
                .any(|x| x.kind() == SyntaxKind::TYPE_KW)
            {
                used(Some(Symbol::Alias(name)))
            } else {
                used(Some(Symbol::Predicate(name)))
            }
        }
        SyntaxKind::INSTANCE => match idents(&parent).as_slice() {
            [m, i, ..] if i == t => decl(Symbol::Instance(m.text().to_string(), name)),
            [m, ..] if m == t => used(Some(Symbol::Module(name))),
            _ => What::Other,
        },
        SyntaxKind::WITH => used(d.value(&scope, &name)),
        SyntaxKind::RESOURCE | SyntaxKind::SETTINGS => match header(&parent) {
            Some(h) if &h.name == t && h.is_static => {
                if parent.kind() == SyntaxKind::RESOURCE {
                    decl(Symbol::Resource(module_of(&scope), name))
                } else {
                    decl(Symbol::Settings(name))
                }
            }
            Some(h) if h.type_tokens.contains(t) => What::Type,
            _ => What::Other,
        },
        SyntaxKind::TYPE_EXPR => {
            let dotted = own_tokens(&parent)
                .iter()
                .any(|x| x.kind() == SyntaxKind::DOT);
            if !dotted && d.aliases.contains(&name) {
                used(Some(Symbol::Alias(name)))
            } else {
                What::Type
            }
        }
        SyntaxKind::BLOCK_PATH => {
            // `k = ..` in an instance block: the module's input `k`.
            let block = parent.parent().and_then(|a| a.parent());
            let first = idents(&parent).first() == Some(t);
            match block.and_then(|b| b.parent()) {
                Some(i) if i.kind() == SyntaxKind::INSTANCE && first => match idents(&i).first() {
                    Some(m) => used(Some(Symbol::Value(
                        Some(format!("module {}", m.text())),
                        name,
                    ))),
                    None => What::Other,
                },
                Some(i) if i.kind() == SyntaxKind::INSTANCE => What::Other,
                _ => What::Path,
            }
        }
        SyntaxKind::RECORD_ATOM if idents(&parent).first() == Some(t) => {
            used(Some(Symbol::Predicate(name)))
        }
        SyntaxKind::CHAIN => chain(d, &parent, t, &scope),
        _ => What::Other,
    }
}

/// A name in a chain, by the order of resolution.
fn chain(d: &Decls, c: &SyntaxNode, t: &SyntaxToken, scope: &Scope) -> What {
    let ps = parts(c);
    let Some(k) = ps.iter().position(|p| matches!(p, Part::Name(x) if x == t)) else {
        return What::Other;
    };
    let name_at = |i: usize| match ps.get(i) {
        Some(Part::Name(x)) => Some(x.text().to_string()),
        _ => None,
    };
    let context = c.parent().map(|p| p.kind());
    // A relation's name: `p(..)`, a head, `contributes p`.
    if matches!(context, Some(SyntaxKind::CALL | SyntaxKind::CONTRIBUTES)) && ps.len() == 1 {
        let head = c
            .parent()
            .and_then(|call| call.parent())
            .is_some_and(|h| matches!(h.kind(), SyntaxKind::RULE | SyntaxKind::FACT))
            && context == Some(SyntaxKind::CALL);
        return What::Name(Symbol::Predicate(t.text().to_string()), head);
    }
    let path = |start: usize| if k >= start { What::Path } else { What::Other };
    let Some(first) = ps.first() else {
        return What::Other;
    };
    // `settings.n.p`: the row `n`.
    if matches!(first, Part::Settings) {
        return match (ps.get(1), k) {
            (Some(Part::Dot), 2) => What::Name(Symbol::Settings(t.text().to_string()), false),
            _ => path(3),
        };
    }
    let Some(name0) = name_at(0) else {
        return What::Other;
    };
    let only = ps.len() == 1;
    // 1, 2: a `let` alias, a value name.
    if let Some(s) = d.let_(scope, &name0).or_else(|| d.value(scope, &name0)) {
        return if k == 0 {
            What::Name(s, false)
        } else {
            What::Path
        };
    }
    if name0 == "world" {
        return What::Other;
    }
    // 4: a resource in scope (bare, only where a bare name is an address).
    let addressed = matches!(
        context,
        Some(SyntaxKind::OUTPUT_DECL | SyntaxKind::LIT_EXISTS)
    ) || (context == Some(SyntaxKind::LIT_IN)
        && c.parent().and_then(|p| p.first_child()).as_ref() == Some(c));
    if (!only || addressed)
        && !matches!(ps.get(1), Some(Part::Index))
        && let Some(s) = d.resource(scope, &name0, None)
    {
        return if k == 0 {
            What::Name(s, false)
        } else {
            What::Path
        };
    }
    // 5: a module's instance, `m.i`, `m[e]`, `m.i/n`.
    if d.modules.contains(&name0) && matches!(ps.get(1), Some(Part::Dot | Part::Index)) {
        if k == 0 {
            return What::Name(Symbol::Module(name0), false);
        }
        if matches!(ps.get(1), Some(Part::Index)) {
            // `m[e].k`: an output.
            return path(4);
        }
        if k == 2 {
            return What::Name(Symbol::Instance(name0, t.text().to_string()), false);
        }
        return match (ps.get(3), k) {
            (Some(Part::Slash), 4) => {
                What::Name(Symbol::Resource(Some(name0), t.text().to_string()), false)
            }
            (Some(Part::Slash), _) => path(5),
            // `m.i.k`: an output; after it, a path.
            _ => path(5),
        };
    }
    // 6: a type followed by `.n` (the longest such type), a relation's
    // `p[..]`.
    if matches!(ps.get(1), Some(Part::Index)) && d.predicates.contains(&name0) {
        return if k == 0 {
            What::Name(Symbol::Predicate(name0), false)
        } else {
            What::Path
        };
    }
    let mut typ = String::new();
    let mut best: Option<(usize, String)> = None;
    for (i, p) in ps.iter().enumerate() {
        match p {
            Part::Name(x) => {
                typ.push_str(x.text());
                if d.types.contains(&typ) {
                    best = Some((i, typ.clone()));
                }
            }
            Part::Dot => typ.push('.'),
            _ => break,
        }
    }
    if let Some((end, typ)) = best {
        if k <= end {
            return What::Type;
        }
        if let (Some(Part::Dot), Some(n)) = (ps.get(end + 1), name_at(end + 2))
            && let Some(s) = d.resource(scope, &n, Some(&typ))
        {
            return match k.cmp(&(end + 2)) {
                std::cmp::Ordering::Equal => What::Name(s, false),
                _ => What::Path,
            };
        }
        return What::Other;
    }
    // 7: a variable; a dot on it is a path.
    path(1)
}

/// A project as references read it: its files' texts (open buffers
/// included) and its stacks' evaluations in the selected environment.
pub struct Project<'a> {
    /// Where the evaluations ran: their places are relative to it.
    pub dir: PathBuf,
    pub files: Vec<(PathBuf, String)>,
    pub evaluated: Vec<&'a Evaluated>,
}

/// One file, parsed.
pub struct Parsed {
    pub path: PathBuf,
    pub text: String,
    pub tree: SyntaxNode,
}

impl Project<'_> {
    pub fn parse(&self) -> Vec<Parsed> {
        self.files
            .iter()
            .map(|(path, text)| Parsed {
                path: path.clone(),
                text: text.clone(),
                tree: dform_core::syntax::parser::parse(text).syntax(),
            })
            .collect()
    }
}

/// Every token of the parsed files that denotes `sym`, with whether it is
/// a declaration, by file.
pub fn occurrences<'p>(
    d: &Decls,
    files: &'p [Parsed],
    sym: &Symbol,
) -> Vec<(&'p Parsed, SyntaxToken, bool)> {
    let mut out = Vec::new();
    for f in files {
        for t in f
            .tree
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
        {
            if t.kind() != SyntaxKind::IDENT {
                continue;
            }
            if let What::Name(s, is_decl) = classify(d, &t)
                && &s == sym
            {
                out.push((f, t, is_decl));
            }
        }
    }
    out
}

/// The name token at byte `at` of `path`, and what it is.
pub fn at<'p>(
    d: &Decls,
    files: &'p [Parsed],
    path: &Path,
    at: usize,
) -> Option<(&'p Parsed, SyntaxToken, What)> {
    let f = files.iter().find(|f| f.path == path)?;
    let t = crate::nav::token_at(&f.tree, at)?;
    let what = classify(d, &t);
    Some((f, t, what))
}

/// `textDocument/references` at byte `at` of `path`.
pub fn references(p: &Project, path: &Path, at: usize, declarations: bool) -> Vec<Location> {
    let files = p.parse();
    let d = Decls::of(files.iter().map(|f| &f.tree));
    let Some((_, _, what)) = self::at(&d, &files, path, at) else {
        return Vec::new();
    };
    match what {
        What::Name(sym, _) => occurrences(&d, &files, &sym)
            .into_iter()
            .filter(|(_, _, is_decl)| declarations || !is_decl)
            .map(|(f, t, _)| {
                let r = t.text_range();
                Location::new(
                    text::uri_of(&f.path),
                    text::range(&f.text, r.start().into(), r.end().into()),
                )
            })
            .collect(),
        What::Path => contributors(p, path, at),
        _ => Vec::new(),
    }
}

/// Where each contribution to the attributes the code at `at` is about is
/// written: every rule, module instance's or pack's, contributing to
/// the cell, as the hover lists them.
pub fn contributors(p: &Project, path: &Path, at: usize) -> Vec<Location> {
    let mut out: Vec<Location> = Vec::new();
    for e in &p.evaluated {
        let in_file = |id: u32| e.files.get(&id).is_some_and(|f| f == path);
        let c = &e.res.circuit;
        for n in explain::targets(e, &in_file, at) {
            let View::Fact { fact, alts, .. } = c.view(n) else {
                continue;
            };
            if fact.pred != "attr" {
                continue;
            }
            for a in alts {
                let View::Times { children, .. } = c.view(*a) else {
                    continue;
                };
                for ch in children {
                    if !matches!(c.view(*ch), View::Fact { fact, .. } if fact.pred == "arg") {
                        continue;
                    }
                    let Some(l) = analysis::written(e, *ch).and_then(|w| locate(p, e, w)) else {
                        continue;
                    };
                    if !out.contains(&l) {
                        out.push(l);
                    }
                }
            }
        }
    }
    out
}

/// A place an evaluation names, as a location.
fn locate(p: &Project, e: &Evaluated, w: Where) -> Option<Location> {
    let text_of = |f: &Path| {
        p.files
            .iter()
            .find(|(g, _)| g == f)
            .map(|(_, t)| t.clone())
            .or_else(|| std::fs::read_to_string(f).ok())
    };
    match w {
        Where::Span(s) => {
            let f = e.files.get(&s.file)?;
            let t = text_of(f)?;
            Some(Location::new(
                text::uri_of(f),
                text::range(&t, s.start as usize, s.end as usize),
            ))
        }
        Where::Place(place) => {
            // `modules/network.df:15:5 (arg)`.
            let at = place.split(' ').next()?;
            let mut it = at.rsplitn(3, ':');
            let (col, line, name) = (it.next()?, it.next()?, it.next()?);
            let f = p.dir.join(name);
            let f = std::fs::canonicalize(&f).unwrap_or(f);
            let t = text_of(&f)?;
            Some(Location::new(
                text::uri_of(&f),
                text::line_range(&t, line.parse().ok()?, col.parse().ok()?),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"edition 2026
input env: string = "staging"
let cfg = settings[env]
module network {
  input vpc_net: inet
  output vpc: net.vpc
  resource net.vpc vpc {
    cidr = vpc_net
    tags = { env: env }
  }
  output vpc = vpc
  q(x) if zone_index[x] = 1, vpc.cidr == x
}
instance network main { vpc_net = inet(cfg.a) }
resource compute.vm bastion { private_ip = 1 }
p(a) if a = network.main/vpc.id, b = network[a].vpc, compute.vm.bastion.id == 1, exists bastion
zone_index("a", 0)
"#;

    fn names(sym: &Symbol) -> Vec<(u32, bool)> {
        let files = vec![Parsed {
            path: PathBuf::from("/x.df"),
            text: SRC.into(),
            tree: dform_core::syntax::parser::parse(SRC).syntax(),
        }];
        let d = Decls::of(files.iter().map(|f| &f.tree));
        occurrences(&d, &files, sym)
            .into_iter()
            .map(|(f, t, decl)| {
                (
                    text::position(&f.text, t.text_range().start().into()).line,
                    decl,
                )
            })
            .collect()
    }

    #[test]
    fn a_resource_is_found_by_every_spelling_of_its_address() {
        assert_eq!(
            names(&Symbol::Resource(Some("network".into()), "vpc".into())),
            vec![(6, true), (10, false), (11, false), (15, false)]
        );
        assert_eq!(
            names(&Symbol::Resource(None, "bastion".into())),
            vec![(14, true), (15, false), (15, false)]
        );
    }

    #[test]
    fn values_modules_instances_and_predicates() {
        // The module's input, from its body and from its instance block.
        assert_eq!(
            names(&Symbol::Value(
                Some("module network".into()),
                "vpc_net".into()
            )),
            vec![(4, true), (7, false), (13, false)]
        );
        // The program's input, read inside the module and by the let.
        assert_eq!(
            names(&Symbol::Value(None, "env".into())),
            vec![(1, true), (2, false), (8, false)]
        );
        assert_eq!(
            names(&Symbol::Let(None, "cfg".into())),
            vec![(2, true), (13, false)]
        );
        assert_eq!(
            names(&Symbol::Module("network".into())),
            vec![(3, true), (13, false), (15, false), (15, false)]
        );
        assert_eq!(
            names(&Symbol::Instance("network".into(), "main".into())),
            vec![(13, true), (15, false)]
        );
        assert_eq!(
            names(&Symbol::Predicate("zone_index".into())),
            vec![(11, false), (16, true)]
        );
    }
}

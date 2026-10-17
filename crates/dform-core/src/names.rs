//! What a name at a place denotes in a project's files, without
//! evaluating them: the declaration it is, and every place that denotes
//! the same. The language server's definition, references, rename and
//! hover read it; so may any tool that names a declaration by a place.
//!
//! The names are the front end's: the project's scope tree
//! (`syntax::resolve::scopes`, every file at once, built past any error)
//! answers what a read reaches through the queries of
//! `program::scope::query` (R-211 step 12). What stays here is reading
//! the syntax at a token: whether it is a chain's head, a block's key, a
//! declaration's name, a segment of a type or a path, and so which query
//! answers it. A file a `use` or a copy names is a module (`module a.b`,
//! R-65); a component's names are its own (`component a.b`, by its
//! path); a stack's file, used by another stack or not, is the program's
//! top level. A name a body does not declare reads outward through its
//! own file, never through a `use` (R-205).

use crate::program::scope::query::defines_relation;
use crate::program::scope::{Decl, DeclKind, ScopeId, Scopes};
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The scope a name is declared in, as the tree names it: `component
/// a.b` and `module a.b` by their paths; `None` for the program's (a
/// stack's file's top level).
pub type Scope = Option<String>;

/// A name the program declares.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Symbol {
    /// A relation, an extern or a builtin: `p(..)`, `p[..]`, `decl p/N`;
    /// the program's (`None`), or private to the component or module file
    /// that defines it, as `modules::expand` scopes it.
    Predicate(Scope, String),
    /// An input, a key or a component's input, in its scope.
    Value(Scope, String),
    /// A field of an object input, `nodes.count`: the input's scope, its
    /// name and the field's path.
    Field(Scope, String, String),
    /// `let a = t`, in its scope.
    Let(Scope, String),
    /// A parameter of a let with parameters (R-187), `n` of `let cidr(n)
    /// = ..`: the let's scope, its name and the parameter's.
    Param(Scope, String, String),
    /// `type NAME = TYPE`.
    Alias(String),
    /// A component, or the name a `use` binds (R-65).
    Module(String),
    /// `resource c n`: the component's path and the instance's name.
    Instance(String, String),
    /// `resource T n` with a static name: its scope, type and name.
    Resource(Scope, String, String),
    /// `output k: T` or `output k = t`, in its scope.
    Output(Scope, String),
    /// A function of the registry (`inet.subnet`, `int`), by its name.
    Function(String),
    /// A function package, `inet` of `inet.subnet`.
    Package(String),
    /// A file a path names (R-65), by its dotted path from the root.
    File(String),
}

/// What a name token is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    /// A declared name, and whether this is its declaration.
    Name(Symbol, bool),
    /// A bare resource name several types share where no `ref(T)` picks
    /// one (R-74): each candidate.
    Names(Vec<Symbol>),
    /// A schema type's name, or a segment of one: the whole type.
    Type(String),
    /// An attribute path: a field of a resource block, a segment after a
    /// reference or a value.
    Path,
    /// A provider block's name.
    Provider,
    /// A variable of a rule.
    Variable,
    /// A key of an object, a named argument or a pattern: no declaration.
    Key,
    /// A word of the syntax written as a name (`world`, `check`, `as`).
    Other,
}

/// Where a symbol is declared.
#[derive(Debug, Clone)]
pub enum Site<'p> {
    /// A name in one of the project's files: its bytes.
    Text(&'p Parsed, rowan::TextRange),
    /// A file, as a whole.
    File(PathBuf),
    /// A line (from 1) of a signature file shipped with dform,
    /// `functions::SOURCES`' path and text.
    Std(&'static str, &'static str, usize),
}

/// A token of a file and what it is. A name in an interpolation hole is a
/// token of the hole's own tree; `range` is its place in the file.
#[derive(Debug, Clone)]
pub struct Named<'p> {
    pub file: &'p Parsed,
    pub token: SyntaxToken,
    pub range: rowan::TextRange,
    pub what: What,
}

impl Named<'_> {
    /// Whether this is the declaration of what it names.
    pub fn is_declaration(&self) -> bool {
        matches!(self.what, What::Name(_, true))
    }
}

/// The interpolation holes of a string token, each parsed as the term it
/// is (`parser::parse_term`) with the byte of the file it starts at; a
/// string inside a hole gives its own holes too (R-175).
pub fn holes(t: &SyntaxToken) -> Vec<(rowan::TextSize, SyntaxNode)> {
    let mut out = Vec::new();
    holes_at(t.text(), usize::from(t.text_range().start()), &mut out);
    out
}

/// `holes` of the string `text` starting at byte `base` of the file.
fn holes_at(text: &str, base: usize, out: &mut Vec<(rowan::TextSize, SyntaxNode)>) {
    let Some(pieces) = crate::syntax::resolve::pieces(text) else {
        return;
    };
    for p in pieces {
        let crate::syntax::resolve::Piece::Hole(src, at) = p else {
            continue;
        };
        let lead = src.len() - src.trim_start().len();
        let start = base + at + lead;
        let tree = crate::syntax::parser::parse_term(src.trim()).syntax();
        let Ok(at) = u32::try_from(start) else {
            continue;
        };
        for s in tree
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .filter(|x| x.kind() == SyntaxKind::STRING)
        {
            holes_at(s.text(), start + usize::from(s.text_range().start()), out);
        }
        out.push((rowan::TextSize::from(at), tree));
    }
}

/// One file, parsed.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub path: PathBuf,
    pub text: String,
    pub tree: SyntaxNode,
}

impl Parsed {
    pub fn new(path: PathBuf, text: String) -> Parsed {
        let tree = crate::syntax::parser::parse(&text).syntax();
        Parsed { path, text, tree }
    }
}

/// A project's names: its scope tree as the front end builds it
/// (`syntax::resolve::scopes`, every file at once, past any error), and
/// what the syntax around a name needs besides (the files by path, the
/// types and the providers).
#[derive(Debug, Default)]
pub struct Decls {
    tree: Scopes,
    /// Each file's tree root and its source id in `tree`.
    roots: Vec<(SyntaxNode, u32)>,
    /// Each file by its dotted path.
    files: BTreeMap<String, PathBuf>,
    /// Each file's source id by its dotted path.
    ids: BTreeMap<String, u32>,
    /// The providers `use`s import (R-112): a path of one segment that
    /// names no file of the project.
    providers: BTreeSet<String>,
    /// Resource headers', `type` blocks' and the schema's types.
    types: BTreeSet<String>,
    /// (type, attribute path) -> the type its `ref(T)` takes (R-74).
    refs: BTreeMap<(String, String), String>,
}

/// What a read from outside a definition looks into: a module's or a
/// component's scope; a stack another stack uses (`use stacks.platform`),
/// the program's scope, its file's declarations alone.
#[derive(Debug, Clone, Copy)]
struct Body {
    scope: ScopeId,
    file: Option<u32>,
}

/// A file's dotted path from `root`: `stacks/platform.df` is
/// `stacks.platform`.
fn module_path(root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?.with_extension("");
    let segs: Vec<&str> = rel.iter().map(|s| s.to_str()).collect::<Option<_>>()?;
    (!segs.is_empty()).then(|| segs.join("."))
}

/// The first source id of a project's files in its tree: past any the
/// process has made, no file's message ever names one.
const FIRST_FILE: u32 = 1 << 30;

impl Decls {
    /// The project file a `use` statement names, by its module path
    /// (`use backups` is backups.df); `None` for std, a provider, a
    /// package's module.
    pub fn used_file(&self, use_stmt: &SyntaxNode) -> Option<&Path> {
        let (path, _) = crate::syntax::resolve::use_parts(use_stmt);
        self.files.get(&path).map(PathBuf::as_path)
    }

    /// The names of a project's files under `root`: a file a `use` or a
    /// copy names is a module (R-65), a stack's file and any other the
    /// program's top level.
    pub fn of_files(root: &Path, files: &[Parsed]) -> Decls {
        let mut d = Decls::default();
        let stacks = crate::project::Manifest::parse(
            &root.join(crate::project::MANIFEST),
            &std::fs::read_to_string(root.join(crate::project::MANIFEST)).unwrap_or_default(),
        )
        .map(|m| m.stacks)
        .unwrap_or_default();
        let paths: Vec<Option<String>> = files.iter().map(|f| module_path(root, &f.path)).collect();
        for (f, p) in files.iter().zip(&paths) {
            if let Some(p) = p {
                d.files.insert(p.clone(), f.path.clone());
            }
        }
        let named = Modules::of(files, &d.files);
        d.providers = named.providers.clone();
        let mut units = Vec::new();
        let mut entries = Vec::new();
        for (i, (f, p)) in files.iter().zip(&paths).enumerate() {
            let file = FIRST_FILE + i as u32;
            let stack = crate::project::is_stack(root, &f.path, &stacks);
            if let Some(p) = p {
                d.ids.insert(p.clone(), file);
            }
            let module = p.clone().filter(|p| !stack && named.is_module(p));
            if module.is_none() {
                entries.push(i);
            }
            units.push(crate::syntax::resolve::Unit {
                file,
                root: f.tree.clone(),
                path: module,
                project: crate::loader::is_project_module(&f.path),
            });
            d.roots.push((f.tree.clone(), file));
            d.declare_types(&f.tree);
        }
        d.tree = crate::syntax::resolve::scopes(&units, &entries, &[]);
        d
    }

    /// The source id of the file `n` is in.
    fn file_of(&self, n: &SyntaxNode) -> Option<u32> {
        let root = n.ancestors().last()?;
        self.roots.iter().find(|(r, _)| *r == root).map(|(_, f)| *f)
    }

    /// The scope a name at `n` is read in: the innermost component whose
    /// statement holds it, else its file's top level.
    fn scope_at(&self, n: &SyntaxNode) -> Option<ScopeId> {
        let file = self.file_of(n)?;
        match n.parent() {
            None => self.tree.of_file(file),
            Some(_) => self.tree.at(file, n.text_range().start().into()),
        }
    }

    /// The scope a statement at `n` declares into.
    fn declaring_at(&self, n: &SyntaxNode) -> Option<ScopeId> {
        Some(self.tree.declaring(self.scope_at(n)?))
    }

    /// A scope as a symbol names it: `component a.b` and `module a.b` by
    /// their paths, `None` for the program's.
    fn describe(&self, s: ScopeId) -> Scope {
        match self.tree.definition(s)? {
            (p, true) => Some(format!("component {p}")),
            (p, false) => Some(format!("module {p}")),
        }
    }

    /// The scope a symbol's scope names.
    fn scope_id(&self, s: &Scope) -> Option<ScopeId> {
        match s.as_deref() {
            None => Some(self.tree.root),
            Some(s) => {
                let path = s.strip_prefix("component ").or(s.strip_prefix("module "))?;
                self.tree.by_path(path)
            }
        }
    }

    /// The symbol a declaration of `name` in `s` is, as a read names it.
    fn symbol(&self, s: ScopeId, name: &str, d: &Decl) -> Option<Symbol> {
        let (scope, name) = (self.describe(s), name.to_string());
        Some(match &d.kind {
            DeclKind::Input => Symbol::Value(scope, name),
            DeclKind::Let | DeclKind::LetFn { .. } => Symbol::Let(scope, name),
            DeclKind::Resource(t) => Symbol::Resource(scope, t.clone(), name),
            DeclKind::Output { .. } => Symbol::Output(scope, name),
            _ => return None,
        })
    }

    /// The `use` or the copy the statement `stmt` makes.
    fn made_by(&self, stmt: &SyntaxNode) -> Option<(&str, &Decl)> {
        let bound = |k: &DeclKind| matches!(k, DeclKind::Use { .. } | DeclKind::Copy { .. });
        self.tree.made_by(self.scope_at(stmt)?, stmt, bound)
    }

    /// Whether `n` makes a copy of a component (R-113): the front end
    /// declared it one.
    fn is_copy(&self, n: &SyntaxNode) -> bool {
        n.kind() == SyntaxKind::RESOURCE
            && self
                .made_by(n)
                .is_some_and(|(_, d)| matches!(d.kind, DeclKind::Copy { .. }))
    }

    /// The body of the definition a `use` or a copy statement names.
    fn bound_body(&self, stmt: &SyntaxNode) -> Option<Body> {
        match &self.made_by(stmt)?.1.kind {
            DeclKind::Use { module, .. } => self.module_body(module),
            DeclKind::Copy {
                component: Some(c), ..
            } => self.tree.by_path(c).map(|scope| Body { scope, file: None }),
            _ => None,
        }
    }

    /// The body of the module at `path`: a module's file, or a stack's
    /// file another stack uses, whose top level is the program's.
    fn module_body(&self, path: &str) -> Option<Body> {
        if let Some(scope) = self.tree.by_path(path) {
            return Some(Body { scope, file: None });
        }
        Some(Body {
            scope: self.tree.root,
            file: Some(*self.ids.get(path)?),
        })
    }

    /// The declarations of `x` in the body `b`, each with its kind.
    fn in_body(&self, b: Body, x: &str) -> Vec<&Decl> {
        self.tree.names[b.scope]
            .of(x)
            .iter()
            .filter(|d| b.file.is_none() || self.file_of(&d.node) == b.file)
            .collect()
    }

    /// What a read of `name` at `at` reaches (`Scopes::reaching`); in the
    /// program's scope, which every stack's file declares into, the
    /// reading file's own declaration before another stack's.
    fn reach(
        &self,
        at: &SyntaxNode,
        name: &str,
        pick: impl Fn(&DeclKind) -> bool,
    ) -> Option<(ScopeId, &Decl)> {
        let (found, d) = self.tree.reaching(self.scope_at(at)?, name, &pick)?;
        if found != self.tree.root {
            return Some((found, d));
        }
        let file = self.file_of(at);
        let own = self.tree.names[found]
            .of(name)
            .iter()
            .find(|x| pick(&x.kind) && self.file_of(&x.node) == file);
        Some((found, own.unwrap_or(d)))
    }

    /// The use `name` binds, read at `at`: the module's path.
    fn use_at(&self, at: &SyntaxNode, name: &str) -> Option<&str> {
        let used = |k: &DeclKind| matches!(k, DeclKind::Use { .. });
        match &self.reach(at, name, used)?.1.kind {
            DeclKind::Use { module, .. } => Some(module),
            _ => None,
        }
    }

    /// The copy `name` read at `at`: its component's path as written and
    /// as resolved.
    fn copy_at(&self, at: &SyntaxNode, name: &str) -> Option<(&str, &str)> {
        let copy = |k: &DeclKind| {
            matches!(
                k,
                DeclKind::Copy {
                    component: Some(_),
                    ..
                }
            )
        };
        match &self.reach(at, name, copy)?.1.kind {
            DeclKind::Copy {
                written,
                component: Some(c),
                ..
            } => Some((written, c)),
            _ => None,
        }
    }

    /// Whether `n` is a `use` of a provider (R-112).
    fn provider_use(&self, n: &SyntaxNode) -> bool {
        crate::syntax::resolve::maybe_provider_use(n).is_some_and(|p| self.providers.contains(&p))
    }

    /// Take the schema's types and the types its `ref(T)` attributes take,
    /// which pick among resources of one name (R-74).
    pub fn with_schema(mut self, schema: &crate::schema::Schema) -> Decls {
        use crate::types::Ty;
        for ((typ, attr), spec) in &schema.attrs {
            self.types.insert(typ.clone());
            let mut ty = Ty::parse(&spec.ty);
            while let Ty::List(t) | Ty::Secret(t) = ty {
                ty = *t;
            }
            if let Ty::Ref(want) = ty {
                self.refs.insert((typ.clone(), attr.clone()), want);
            }
        }
        self.types.extend(schema.provider_of.keys().cloned());
        self
    }

    /// The types the program's resource headers and `type` blocks name.
    fn declare_types(&mut self, root: &SyntaxNode) {
        for n in root.descendants() {
            let t = match n.kind() {
                SyntaxKind::RESOURCE => header(&n).map(|h| h.typ),
                SyntaxKind::TYPE_DECL => Some(dotted_after_keyword(&n)),
                _ => None,
            };
            self.types.extend(t.filter(|t| !t.is_empty()));
        }
    }

    /// The let, the input or the resource `name` read at `at` past
    /// `supers` enclosing components: `super.x` (R-186).
    fn item_from(&self, at: &SyntaxNode, supers: usize, name: &str) -> Option<Symbol> {
        let s = self.scope_at(at)?;
        let item = |k: &DeclKind| k.is_value() || matches!(k, DeclKind::Resource(_));
        let (s, d) = self.tree.past(s, supers, name, item)?;
        self.symbol(s, name, d)
    }

    /// A `let` (with parameters or not) `name` read at `at`.
    fn let_(&self, at: &SyntaxNode, name: &str) -> Option<Symbol> {
        let lets = |k: &DeclKind| matches!(k, DeclKind::Let | DeclKind::LetFn { .. });
        let (s, d) = self.reach(at, name, lets)?;
        self.symbol(s, name, d)
    }

    /// The `let` or the input `name` read bare at `at`: the innermost
    /// scope's that declares either (`Scopes::reaching`), and its
    /// declaration.
    fn value_decl(&self, at: &SyntaxNode, name: &str) -> Option<(Symbol, &Decl)> {
        let (s, d) = self.reach(at, name, DeclKind::is_value)?;
        Some((self.symbol(s, name, d)?, d))
    }

    fn value_read(&self, at: &SyntaxNode, name: &str) -> Option<Symbol> {
        self.value_decl(at, name).map(|(s, _)| s)
    }

    /// The parameter `name` of the let with parameters `at` is in (its
    /// head or its body), as `Scopes::parameter` finds it in the let's head.
    fn param(&self, at: &SyntaxNode, name: &str) -> Option<Symbol> {
        let stmt = at.ancestors().find(|a| a.kind() == SyntaxKind::LET)?;
        crate::program::scope::query::parameters(&stmt)
            .iter()
            .any(|p| p.text() == name)
            .then_some(())?;
        let f = declared_name(&stmt)?.text().to_string();
        let scope = self.describe(self.declaring_at(&stmt)?);
        Some(Symbol::Param(scope, f, name.to_string()))
    }

    /// The resources `name` seen from `at`: the innermost scope's that
    /// declares one, each type's.
    fn resources_at(&self, at: &SyntaxNode, name: &str) -> Vec<Symbol> {
        let resource = |k: &DeclKind| matches!(k, DeclKind::Resource(_));
        let Some((s, _)) = self.reach(at, name, resource) else {
            return Vec::new();
        };
        let body = Body {
            scope: s,
            file: None,
        };
        self.resources_in(body, name)
    }

    /// The resources `name` the body `b` declares, one per type.
    fn resources_in(&self, b: Body, name: &str) -> Vec<Symbol> {
        let mut out: Vec<Symbol> = Vec::new();
        for d in self.in_body(b, name) {
            if let Some(sym @ Symbol::Resource(..)) = self.symbol(b.scope, name, d)
                && !out.contains(&sym)
            {
                out.push(sym);
            }
        }
        out
    }

    /// A resource name read at `at`: the one candidate; among several, the
    /// one the attribute's `ref(T)` takes or the type on the right of
    /// `in` names; else each.
    fn pick(&self, c: &SyntaxNode, cs: Vec<Symbol>) -> What {
        if cs.len() > 1
            && let Some(want) = self.wanted_type(c)
        {
            let of: Vec<&Symbol> = cs
                .iter()
                .filter(|s| matches!(s, Symbol::Resource(_, t, _) if *t == want))
                .collect();
            if let [one] = of.as_slice() {
                return What::Name((*one).clone(), false);
            }
        }
        match <[Symbol; 1]>::try_from(cs) {
            Ok([one]) => What::Name(one, false),
            Err(cs) if cs.is_empty() => What::Other,
            Err(cs) => What::Names(cs),
        }
    }

    /// The type a reference at chain `c` must have: the resource block
    /// attribute's `ref(T)` it is the value of (in a list too), or the
    /// type on the right of `in`.
    fn wanted_type(&self, c: &SyntaxNode) -> Option<String> {
        let up = c
            .ancestors()
            .skip(1)
            .find(|a| !matches!(a.kind(), SyntaxKind::LIST | SyntaxKind::LITERAL))?;
        match up.kind() {
            SyntaxKind::ASSIGN => {
                let path = up.children().find(|x| x.kind() == SyntaxKind::BLOCK_PATH)?;
                let resource = up.ancestors().find(|a| a.kind() == SyntaxKind::RESOURCE)?;
                let typ = header(&resource)?.typ;
                let attr = path.text().to_string().replace(char::is_whitespace, "");
                self.refs.get(&(typ, attr)).cloned()
            }
            SyntaxKind::LIT_IN if up.first_child().as_ref() == Some(c) => {
                let rhs = up.children().nth(1)?;
                Some(dotted_names(&rhs))
            }
            // `output vpc: net.vpc = main`.
            SyntaxKind::OUTPUT_DECL => {
                let ty = up.children().find(|x| x.kind() == SyntaxKind::TYPE_EXPR)?;
                Some(dotted_names(&ty))
            }
            _ => None,
        }
    }

    /// The body a `use` or a copy block's statement at `at` gives rows
    /// to: the module's or the component's.
    fn block_body(&self, at: &SyntaxNode) -> Option<Body> {
        let owner = at
            .ancestors()
            .find(|a| a.kind() == SyntaxKind::USE || self.is_copy(a))?;
        self.bound_body(&owner)
    }

    /// The relation `name` read or defined at `at`: the innermost module's
    /// or component's that defines it (in a `use` or a copy's block, the
    /// module's or the component's first), else the program's.
    fn predicate(&self, at: &SyntaxNode, name: &str) -> Symbol {
        let scope = match crate::loader::is_core_name(name) {
            true => None,
            false => {
                let block = self.block_body(at).map(|b| b.scope);
                let around = self.scope_at(at).map(|s| self.tree.chain(s));
                let scopes = block.into_iter().chain(around.into_iter().flatten());
                self.tree
                    .private_relation(scopes, name)
                    .and_then(|s| self.describe(s))
            }
        };
        Symbol::Predicate(scope, name.to_string())
    }

    /// Whether `sym`'s kind already has a declaration named `name` where
    /// `sym` is (a rename onto it would merge the two).
    pub fn taken(&self, sym: &Symbol, name: &str) -> bool {
        let here = |s: &Scope, pick: &dyn Fn(&DeclKind) -> bool| {
            self.scope_id(s)
                .is_some_and(|s| self.tree.names[s].of(name).iter().any(|d| pick(&d.kind)))
        };
        let anywhere = |pick: &dyn Fn(&DeclKind) -> bool| {
            self.tree
                .every()
                .any(|(_, n, d)| n == name && pick(&d.kind))
        };
        match sym {
            // Any relation of the name, anywhere: a private one would
            // shadow a global one, or the other way round.
            Symbol::Predicate(..) => self.tree.is_relation(name),
            Symbol::Value(s, _) => here(s, &|k| *k == DeclKind::Input),
            Symbol::Field(s, i, p) => {
                let p = match p.rsplit_once('.') {
                    Some((init, _)) => format!("{init}.{name}"),
                    None => name.to_string(),
                };
                self.scope_id(s)
                    .and_then(|s| self.tree.names[s].input(i))
                    .is_some_and(|n| input_fields(n).iter().any(|(f, _)| *f == p))
            }
            Symbol::Let(s, _) => here(s, &|k| matches!(k, DeclKind::Let | DeclKind::LetFn { .. })),
            Symbol::Param(s, f, _) => self
                .scope_id(s)
                .and_then(|s| self.tree.names[s].function(f))
                .is_some_and(|n| {
                    crate::program::scope::query::parameters(n)
                        .iter()
                        .any(|p| p.text() == name)
                }),
            Symbol::Output(s, _) => here(s, &|k| matches!(k, DeclKind::Output { .. })),
            Symbol::Alias(_) => anywhere(&|k| matches!(k, DeclKind::Alias(_))),
            Symbol::Module(_) => {
                anywhere(&|k| matches!(k, DeclKind::Component(_) | DeclKind::Use { .. }))
            }
            Symbol::Instance(_, _) => anywhere(&|k| matches!(k, DeclKind::Copy { .. })),
            Symbol::Resource(s, _, _) => here(s, &|k| matches!(k, DeclKind::Resource(_))),
            Symbol::Function(_) | Symbol::Package(_) | Symbol::File(_) => true,
        }
    }

    /// The names the scope `scope`'s copies or bindings have: each copy's
    /// name for a component's, each name a `use` binds for a module
    /// file's; `None` for the program's.
    fn bindings(&self, scope: &Scope) -> Option<Vec<String>> {
        let s = scope.as_deref()?;
        let (path, component) = match (s.strip_prefix("component "), s.strip_prefix("module ")) {
            (Some(c), _) => (c, true),
            (_, Some(m)) => (m, false),
            _ => return Some(Vec::new()),
        };
        let bound = self.tree.bound_to(path, component);
        Some(bound.into_iter().map(str::to_string).collect())
    }

    /// How an evaluation names the cells of a scope's inputs, lets and
    /// outputs (`attr("input", S, k, V)`): `""` for the program's (a
    /// stack's own included), each copy's name for a component's, each
    /// name a `use` binds for a module file's.
    pub fn cell_scopes(&self, scope: &Scope) -> Vec<String> {
        self.bindings(scope).unwrap_or_else(|| vec![String::new()])
    }

    /// The addresses a resource has, as strings name it (H-16, R-112): its
    /// name in the program's scope; its path `n.name` in each copy `n` of
    /// its component or under each name its module file is used by.
    pub fn addresses_of(&self, sym: &Symbol) -> Vec<(String, String)> {
        let Symbol::Resource(scope, typ, name) = sym else {
            return Vec::new();
        };
        let name = crate::address::name_segment(name).into_owned();
        match self.bindings(scope) {
            None => vec![(typ.clone(), name)],
            Some(bound) => bound
                .iter()
                .map(|n| (typ.clone(), crate::address::scoped(n, &name)))
                .collect(),
        }
    }

    /// What the name token `t` is.
    pub fn classify(&self, t: &SyntaxToken) -> What {
        self.classify_in(t, None)
    }

    /// What the name token `t` is; `host`, for a token of an interpolation
    /// hole's tree, the node of the string it is in, where its names are
    /// looked up.
    fn classify_in(&self, t: &SyntaxToken, host: Option<&SyntaxNode>) -> What {
        if t.kind() != SyntaxKind::IDENT {
            return What::Other;
        }
        let Some(parent) = t.parent() else {
            return What::Other;
        };
        let here = host.cloned().unwrap_or_else(|| parent.clone());
        let name = t.text().to_string();
        let is_declared = || declared_name(&parent).as_ref() == Some(t);
        // A declaration's scope is where its statement stands.
        let outer = parent.parent().unwrap_or_else(|| parent.clone());
        let scope = || self.declaring_at(&outer).and_then(|s| self.describe(s));
        let decl = |s: Symbol| What::Name(s, true);
        let used = |s: Option<Symbol>| s.map_or(What::Other, |s| What::Name(s, false));
        match parent.kind() {
            SyntaxKind::LET if is_declared() => decl(Symbol::Let(scope(), name)),
            SyntaxKind::INPUT if is_declared() => decl(Symbol::Value(scope(), name)),
            SyntaxKind::OUTPUT_DECL if is_declared() => {
                if is_value_output(&parent) {
                    decl(Symbol::Output(scope(), name))
                } else {
                    // `output p` exports the relation `p` (R-55).
                    used(Some(self.predicate(&parent, &name)))
                }
            }
            SyntaxKind::TYPE_ALIAS if is_declared() => decl(Symbol::Alias(name)),
            // `n` of `let cidr(n) = ..`: the parameter's declaration.
            SyntaxKind::PARAM if is_declared() => match self.param(&parent, &name) {
                Some(p) => decl(p),
                None => What::Other,
            },
            SyntaxKind::COMPONENT if is_declared() => decl(Symbol::Module(name)),
            SyntaxKind::USE if self.provider_use(&parent) => What::Provider,
            SyntaxKind::USE => self.statement_path(&parent, t),
            SyntaxKind::RESOURCE if self.is_copy(&parent) => self.statement_path(&parent, t),
            SyntaxKind::DECL | SyntaxKind::EXTERN | SyntaxKind::INPUT_RELATION
                if relation_name(&parent).as_ref() == Some(t) =>
            {
                What::Name(self.predicate(&parent, &name), true)
            }
            SyntaxKind::RESOURCE => match header(&parent) {
                Some(h) if &h.name == t && h.is_static => {
                    decl(Symbol::Resource(scope(), h.typ, name))
                }
                Some(h) if h.type_tokens.contains(t) => What::Type(h.typ),
                _ => What::Other,
            },
            SyntaxKind::TYPE_DECL => What::Type(dotted_after_keyword(&parent)),
            SyntaxKind::TYPE_EXPR => self.type_name(&parent, t),
            SyntaxKind::BLOCK_PATH => self.block_path(&parent, t),
            SyntaxKind::CHAIN => self.chain(&parent, t, &here),
            // `{ env }`: the field's value is the name's; `{ k = v }`, a key.
            SyntaxKind::OBJECT_FIELD
                if !own_tokens(&parent)
                    .iter()
                    .any(|x| matches!(x.kind(), SyntaxKind::EQ | SyntaxKind::COLON)) =>
            {
                self.object_pun(&parent, &here, &name)
            }
            // A field of an object, a named argument, a column of a `decl`.
            SyntaxKind::OBJECT_FIELD | SyntaxKind::NAMED_ARG | SyntaxKind::BIND_ARG => What::Key,
            // What a dot reads off a call's value, `str.split(s, ":")[0]`.
            SyntaxKind::CALL_CHAIN => What::Path,
            _ => What::Other,
        }
    }

    /// A name in a type: an alias, bare or read through its module
    /// (`network.subnets`, R-65), else the type it is part of.
    fn type_name(&self, ty: &SyntaxNode, t: &SyntaxToken) -> What {
        let toks = own_tokens(ty);
        let dotted = toks.iter().any(|x| x.kind() == SyntaxKind::DOT);
        let last = toks.iter().rev().find(|x| x.kind() == SyntaxKind::IDENT);
        let alias = self
            .tree
            .every()
            .any(|(_, n, d)| n == t.text() && matches!(d.kind, DeclKind::Alias(_)));
        if alias && (!dotted || last == Some(t)) {
            What::Name(Symbol::Alias(t.text().to_string()), false)
        } else {
            What::Type(dotted_names(ty))
        }
    }

    /// `{ name }` in `field`, read at `here`: the field's value is the
    /// name's, a let's parameter, a let, an input or a resource; in a
    /// pattern, a variable of its name.
    fn object_pun(&self, field: &SyntaxNode, here: &SyntaxNode, name: &str) -> What {
        match self
            .param(here, name)
            .or_else(|| self.value_read(here, name))
        {
            Some(s) => What::Name(s, false),
            None => match self.pick(field, self.resources_at(here, name)) {
                What::Other => What::Variable,
                w => w,
            },
        }
    }

    /// A segment of a `use`'s path or a copy's component path, or the
    /// name it binds: a file the path names so far, a component of one,
    /// the binding.
    fn statement_path(&self, n: &SyntaxNode, t: &SyntaxToken) -> What {
        if crate::syntax::resolve::bound_token(n).as_ref() == Some(t) {
            return match n.kind() {
                SyntaxKind::USE => What::Name(Symbol::Module(t.text().to_string()), true),
                _ => {
                    let (path, name) = crate::syntax::resolve::copy_parts(n);
                    What::Name(Symbol::Instance(path, name), true)
                }
            };
        }
        let Some(prefix) = path_prefix(n, t) else {
            return What::Other;
        };
        if self.files.contains_key(&prefix) {
            return What::Name(Symbol::File(prefix), false);
        }
        // `resource network.vpc main`: the component `vpc` of network.df;
        // `resource vpc main`, a component of the program.
        if self.tree.component_named(t.text()).is_some() {
            return What::Name(Symbol::Module(t.text().to_string()), false);
        }
        What::Other
    }

    /// The input `name` of the module or the component the `use` or the
    /// copy `stmt` binds, which its block's entry `name = ..` fills.
    fn filled(&self, stmt: &SyntaxNode, name: &str) -> Option<Symbol> {
        let body = self.bound_body(stmt)?;
        let input = self
            .in_body(body, name)
            .into_iter()
            .find(|d| d.kind == DeclKind::Input)?;
        self.symbol(body.scope, name, input)
    }

    /// The input a pun in a `use` or a copy's block fills (`env` of `use
    /// baseline { env }` is the stack's `env` it reads, and fills
    /// baseline's input `env`).
    pub fn fills(&self, t: &SyntaxToken) -> Option<Symbol> {
        let path = t.parent().filter(|p| is_pun(p, t))?;
        let stmt = path.parent()?.parent()?.parent()?;
        let bound = stmt.kind() == SyntaxKind::USE || self.is_copy(&stmt);
        self.filled(&stmt, t.text())
            .filter(|_| bound && !self.provider_use(&stmt))
    }

    /// A name in a block entry's path.
    fn block_path(&self, path: &SyntaxNode, t: &SyntaxToken) -> What {
        let name = t.text().to_string();
        let first = idents(path).first() == Some(t);
        let entry = path.parent();
        // `input nodes { count: int }`: the field's declaration.
        if let Some(attr) = entry.as_ref().filter(|e| e.kind() == SyntaxKind::ATTR_DECL)
            && let Some(input) = attr.ancestors().find(|a| a.kind() == SyntaxKind::INPUT)
            && let Some(i) = declared_name(&input)
        {
            let s = self.declaring_at(&input).and_then(|x| self.describe(x));
            let field = field_path(attr, path, t);
            return What::Name(Symbol::Field(s, i.text().to_string(), field), true);
        }
        let block = entry.as_ref().and_then(|e| e.parent());
        // `k = ..` in an instance or a use block: the component's or the
        // module's input `k`; a provider's `use` block holds its settings.
        // A pun there, `{ env }`, is the user's value it reads: the input
        // it fills is `fills`'.
        if let Some(i) = block
            .and_then(|b| b.parent())
            .filter(|s| s.kind() == SyntaxKind::USE || self.is_copy(s))
            .filter(|s| !self.provider_use(s))
            .filter(|_| !is_pun(path, t))
        {
            return match self.filled(&i, &name).filter(|_| first) {
                Some(s) => What::Name(s, false),
                None => What::Path,
            };
        }
        // An entry that is only a name is the pun `k = k` (R-33): the name
        // is also its value, a `let`, an input or a resource (the
        // reference, R-43).
        if is_pun(path, t) {
            return match self.value_read(path, &name) {
                Some(s) => What::Name(s, false),
                None => match self.pick(path, self.resources_at(path, &name)) {
                    // In a `use` or a copy's block, what no declaration
                    // gives is a variable of its clause (`resource app
                    // "${c}" { c } where run(c)`).
                    What::Other if self.fills(t).is_some() => What::Variable,
                    What::Other => What::Path,
                    w => w,
                },
            };
        }
        What::Path
    }

    /// A name in a chain, by the order of resolution, its names looked up
    /// from `at`: the first step that knows it says what it is, else it
    /// is a variable (a dot on one a path).
    fn chain(&self, c: &SyntaxNode, t: &SyntaxToken, at: &SyntaxNode) -> What {
        let Some(r) = Read::of(c, t, at) else {
            return What::Other;
        };
        self.param_read(&r)
            .or_else(|| self.called(&r))
            .or_else(|| self.value_head(&r))
            .or_else(|| self.refined_field(&r))
            .or_else(|| self.super_read(&r))
            .or_else(|| self.world_read(&r))
            .or_else(|| self.scope_read(&r))
            .or_else(|| self.file_read(&r))
            .or_else(|| self.relation_or_type(&r))
            .unwrap_or(match r.k {
                0 => What::Variable,
                _ => What::Path,
            })
    }

    /// A parameter of the let the chain is in, read in its body: a
    /// variable of the let's clause, bound around it.
    fn param_read(&self, r: &Read) -> Option<What> {
        let p = self.param(r.at, &r.name0).filter(|_| !r.call)?;
        Some(r.head_or_path(p))
    }

    /// A call's name: a relation's, `p(..)`, a head; a function's,
    /// `int(..)`; a let with parameters', `f(x, y)` (R-187). `m.p(..)`,
    /// the relation `p` of the module `m` binds; `n.p(..)`, the relation
    /// the copy `n` exports; `inet.subnet(..)`, a function.
    fn called(&self, r: &Read) -> Option<What> {
        if !r.call {
            return None;
        }
        let (name0, at) = (&r.name0, r.at);
        if r.ps.len() == 1 {
            let head =
                r.c.parent()
                    .and_then(|call| call.parent())
                    .is_some_and(|h| matches!(h.kind(), SyntaxKind::RULE | SyntaxKind::FACT));
            if !head && !self.tree.is_relation(name0) && crate::functions::callable(name0) {
                return Some(What::Name(Symbol::Function(name0.clone()), false));
            }
            if !head && let Some(s) = self.let_(at, name0) {
                return Some(What::Name(s, false));
            }
            return Some(What::Name(self.predicate(at, name0), head));
        }
        if r.ps.len() != 3 || !r.dot(1) {
            return None;
        }
        let name2 = r.name_at(2).unwrap_or_default();
        let (first, second) = if let Some(path) = self.use_at(at, name0) {
            let module = match self.module_body(path) {
                Some(b) => self.describe(b.scope),
                None => Some(format!("module {path}")),
            };
            (
                Symbol::Module(name0.clone()),
                Symbol::Predicate(module, name2),
            )
        } else if let Some((written, c)) = self.copy_at(at, name0) {
            let component = Some(format!("component {c}"));
            let copy = Symbol::Instance(written.to_string(), name0.clone());
            (copy, Symbol::Predicate(component, name2))
        } else {
            let full = format!("{name0}.{name2}");
            crate::functions::callable(&full).then_some(())?;
            (Symbol::Package(name0.clone()), Symbol::Function(full))
        };
        Some(What::Name(if r.k == 0 { first } else { second }, false))
    }

    /// A `let` or a value name at the head; an object input's field after
    /// it.
    fn value_head(&self, r: &Read) -> Option<What> {
        let (s, decl) = self.value_decl(r.at, &r.name0)?;
        let k = r.k;
        if k == 0 {
            return Some(What::Name(s, false));
        }
        if let Symbol::Value(scope, input) = &s {
            let field: Vec<String> = (2..=k)
                .step_by(2)
                .map_while(|i| r.name_at(i).filter(|_| r.dot(i - 1)))
                .collect();
            let field = field.join(".");
            if field.split('.').count() == k / 2
                && input_fields(&decl.node).iter().any(|(f, _)| *f == field)
            {
                let f = Symbol::Field(scope.clone(), input.clone(), field);
                return Some(What::Name(f, false));
            }
        }
        Some(What::Path)
    }

    /// A refinement of an object input's field reads the field, `count:
    /// int check count >= 1`.
    fn refined_field(&self, r: &Read) -> Option<What> {
        let attr =
            r.c.ancestors()
                .find(|a| a.kind() == SyntaxKind::REFINEMENT)
                .and_then(|x| x.parent())
                .filter(|a| a.kind() == SyntaxKind::ATTR_DECL)?;
        let input = attr.ancestors().find(|a| a.kind() == SyntaxKind::INPUT)?;
        let i = declared_name(&input)?;
        let path = attr
            .children()
            .find(|x| x.kind() == SyntaxKind::BLOCK_PATH)?;
        let last = idents(&path).pop().filter(|l| l.text() == r.name0)?;
        let s = self.declaring_at(&input).and_then(|x| self.describe(x));
        let field = Symbol::Field(s, i.text().to_string(), field_path(&attr, &path, &last));
        Some(r.head_or_path(field))
    }

    /// `super.x`: what the scope around the component reads `x` as, one
    /// scope per `super` (R-186).
    fn super_read(&self, r: &Read) -> Option<What> {
        if r.name0 != "super" {
            return None;
        }
        let supers = (0..)
            .take_while(|i| {
                r.name_at(2 * i).as_deref() == Some("super") && (*i == 0 || r.dot(2 * i - 1))
            })
            .count();
        let x = r.name_at(2 * supers).filter(|_| r.dot(2 * supers - 1));
        Some(match x.and_then(|x| self.item_from(r.at, supers, &x)) {
            Some(s) if r.k == 2 * supers => What::Name(s, false),
            Some(_) if r.k > 2 * supers => What::Path,
            _ => What::Other,
        })
    }

    /// `world.T[e]`.
    fn world_read(&self, r: &Read) -> Option<What> {
        (r.name0 == "world").then(|| match r.k {
            0 => What::Other,
            k => self.typed(&r.ps, 2, k).unwrap_or(What::Path),
        })
    }

    /// A resource in scope: its reference or what a dot reads; a copy,
    /// `n.k`; a used module's item or a stack's deployment, `m.x`,
    /// `s[k=v].out`; a component's copies, `c[e].k`. A module's item or a
    /// copy's output reads the module or the copy though a resource has
    /// the name (R-76), but in a module's or a component's body its own
    /// resource wins over what its user's scope brings in (R-101).
    fn scope_read(&self, r: &Read) -> Option<What> {
        let (name0, at, k) = (&r.name0, r.at, r.k);
        let instance = self.copy_at(at, name0);
        let used = self.use_at(at, name0);
        let rs = self.resources_at(at, name0);
        let own = matches!(rs.first(), Some(Symbol::Resource(Some(_), _, _)));
        let reads_other = !(own && !self.binds_here(at, name0))
            && r.dot(1)
            && r.name_at(2).is_some_and(|x| {
                let of = match (instance, used) {
                    (Some((_, c)), _) => {
                        self.tree.by_path(c).map(|scope| Body { scope, file: None })
                    }
                    (_, Some(m)) => self.module_body(m),
                    _ => None,
                };
                of.is_some_and(|b| self.has_item(b, &x))
            });
        if !rs.is_empty() && !r.index(1) && !reads_other {
            return Some(if k == 0 {
                self.pick(r.c, rs)
            } else {
                What::Path
            });
        }
        if r.dot(1)
            && let Some((written, c)) = instance
        {
            let body = self.tree.by_path(c).map(|scope| Body { scope, file: None });
            return Some(match k {
                0 => What::Name(Symbol::Instance(written.to_string(), name0.clone()), false),
                2 => self.item(body?, &r.name_at(2)?, r.call),
                _ => What::Path,
            });
        }
        if let Some(path) = used {
            let j = if r.index(1) { 3 } else { 2 };
            if k == 0 {
                return Some(What::Name(Symbol::Module(name0.clone()), false));
            }
            if k == j
                && r.dot(j - 1)
                && let Some(x) = r.name_at(j)
            {
                return Some(match self.module_body(path) {
                    Some(b) => self.item(b, &x, r.call),
                    None => What::Path,
                });
            }
            let c = r.name_at(j);
            let inner = c.and_then(|c| self.tree.by_path(&format!("{path}.{c}")));
            return Some(self.component_read(r, j, inner).unwrap_or(What::Path));
        }
        if r.index(1) {
            let copied = |k: &DeclKind| matches!(k, DeclKind::Copied(_));
            let c = self
                .reach(at, name0, copied)
                .and_then(|(_, d)| match &d.kind {
                    DeclKind::Copied(p) => self.tree.by_path(p),
                    _ => None,
                })
                .or_else(|| {
                    self.tree
                        .component_named(name0)
                        .and_then(|p| self.tree.by_path(p))
                });
            if c.is_some() {
                let after = if k >= 1 { What::Path } else { What::Other };
                return Some(self.component_read(r, 0, c).unwrap_or(after));
            }
        }
        None
    }

    /// Whether the body `at` is in (its component's, else the program's
    /// or its module's) binds `name` itself by a `use` or a copy: then
    /// the body's own resource of the name does not win a read over it
    /// (R-101).
    fn binds_here(&self, at: &SyntaxNode, name: &str) -> bool {
        self.scope_at(at).is_some_and(|s| {
            let body = self.tree.declaring(self.tree.body(s));
            self.tree.names[body].bound(name).next().is_some()
        })
    }

    /// The path of a file, then a component of it: `net.c[e].k`.
    fn file_read(&self, r: &Read) -> Option<What> {
        let name0 = &r.name0;
        if !self
            .files
            .keys()
            .any(|f| f == name0 || f.starts_with(&format!("{name0}.")))
        {
            return None;
        }
        let mut prefix = String::new();
        for (i, p) in r.ps.iter().enumerate() {
            let Part::Name(x) = p else {
                if matches!(p, Part::Index) {
                    break;
                }
                continue;
            };
            if !prefix.is_empty() {
                prefix.push('.');
            }
            prefix.push_str(x.text());
            if self.files.contains_key(&prefix) {
                if i == r.k {
                    return Some(What::Name(Symbol::File(prefix), false));
                }
            } else if let Some(c) = self.tree.by_path(&prefix).or_else(|| {
                self.tree
                    .component_named(x.text())
                    .and_then(|p| self.tree.by_path(p))
            }) {
                return Some(self.component_read(r, i, Some(c)).unwrap_or(What::Path));
            } else if i == r.k {
                return Some(What::Other);
            }
        }
        None
    }

    /// A relation's `p[..]`; a type, `T[e]`, `T.n`.
    fn relation_or_type(&self, r: &Read) -> Option<What> {
        if r.index(1) && self.tree.is_relation(&r.name0) {
            return Some(r.head_or_path(self.predicate(r.at, &r.name0)));
        }
        self.typed(&r.ps, 0, r.k)
    }

    /// A chain read from the `T` starting at part `start`: the longest
    /// dotted run of names that is a known type, or a dotted name in a
    /// type namespace; then a resource of that type `.n`, or a path.
    fn typed(&self, ps: &[Part], start: usize, k: usize) -> Option<What> {
        let mut typ = String::new();
        let mut best: Option<(usize, String)> = None;
        let mut end = start;
        for (i, p) in ps.iter().enumerate().skip(start) {
            match p {
                Part::Name(x) => {
                    typ.push_str(x.text());
                    end = i;
                    if self.types.contains(&typ) {
                        best = Some((i, typ.clone()));
                    }
                }
                Part::Dot => typ.push('.'),
                Part::Index => break,
            }
        }
        // A dotted name in a type namespace (`k8s.x` where a `k8s.` type
        // is known) is that type's name.
        let best = best.or_else(|| {
            let ns = typ.split('.').next()?;
            (typ.contains('.') && self.types.iter().any(|t| t.starts_with(&format!("{ns}."))))
                .then(|| (end, typ.clone()))
        });
        let (end, typ) = best?;
        if k <= end {
            return Some(What::Type(typ));
        }
        if let (Some(Part::Dot), Some(Part::Name(n))) = (ps.get(end + 1), ps.get(end + 2)) {
            let mut rs: Vec<Symbol> = Vec::new();
            for (s, x, d) in self.tree.every() {
                if x == n.text() && d.kind == DeclKind::Resource(typ.clone()) {
                    let sym = Symbol::Resource(self.describe(s), typ.clone(), x.to_string());
                    if !rs.contains(&sym) {
                        rs.push(sym);
                    }
                }
            }
            if !rs.is_empty() {
                return Some(match k.cmp(&(end + 2)) {
                    std::cmp::Ordering::Equal => self.pick_first(rs),
                    _ => What::Path,
                });
            }
        }
        Some(What::Path)
    }

    fn pick_first(&self, rs: Vec<Symbol>) -> What {
        match <[Symbol; 1]>::try_from(rs) {
            Ok([one]) => What::Name(one, false),
            Err(rs) => What::Names(rs),
        }
    }

    /// A component's copies read at part `at` (`c[e].k`), `body` the
    /// component's: the component, then its output after the index.
    fn component_read(&self, r: &Read, at: usize, body: Option<ScopeId>) -> Option<What> {
        let Some(Part::Name(c)) = r.ps.get(at) else {
            return None;
        };
        let body = body?;
        if r.k == at {
            return Some(What::Name(Symbol::Module(c.text().to_string()), false));
        }
        let o = at + 3;
        if !r.index(at + 1) || !r.dot(at + 2) {
            return None;
        }
        match r.ps.get(o) {
            Some(Part::Name(x)) if r.k == o => {
                let b = Body {
                    scope: body,
                    file: None,
                };
                Some(self.item(b, x.text(), false))
            }
            _ => Some(What::Path),
        }
    }

    /// Whether the body `b` declares an item `x` its user reads.
    fn has_item(&self, b: Body, x: &str) -> bool {
        self.in_body(b, x).iter().any(|d| {
            defines_relation(&d.kind)
                || d.kind.is_value()
                || matches!(
                    d.kind,
                    DeclKind::Output { .. } | DeclKind::LetFn { .. } | DeclKind::Resource(_)
                )
        })
    }

    /// The item `x` of the body `b` (a component's or a module file's),
    /// read from outside it: a relation it defines, an output, a `let`, a
    /// value, a resource, a component of a module.
    fn item(&self, b: Body, x: &str, call: bool) -> What {
        let decls = self.in_body(b, x);
        let is = |pick: &dyn Fn(&DeclKind) -> bool| decls.iter().find(|d| pick(&d.kind));
        let scope = self.describe(b.scope);
        if call || is(&defines_relation).is_some() {
            return What::Name(Symbol::Predicate(scope, x.to_string()), false);
        }
        let found = is(&|k| matches!(k, DeclKind::Output { .. }))
            .or_else(|| is(&|k| matches!(k, DeclKind::Let | DeclKind::LetFn { .. })))
            .or_else(|| is(&|k| *k == DeclKind::Input));
        if let Some(sym) = found.and_then(|d| self.symbol(b.scope, x, d)) {
            return What::Name(sym, false);
        }
        let rs = self.resources_in(b, x);
        if !rs.is_empty() {
            return self.pick_first(rs);
        }
        if decls
            .iter()
            .any(|d| matches!(d.kind, DeclKind::Component(_)))
        {
            return What::Name(Symbol::Module(x.to_string()), false);
        }
        What::Path
    }

    /// Every name of a file, and what each is: its tree's, and those of
    /// its strings' interpolation holes.
    pub fn names<'p>(&self, f: &'p Parsed) -> Vec<Named<'p>> {
        let mut out = Vec::new();
        for t in f
            .tree
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
        {
            match t.kind() {
                SyntaxKind::IDENT => out.push(Named {
                    file: f,
                    range: t.text_range(),
                    what: self.classify(&t),
                    token: t,
                }),
                SyntaxKind::STRING => {
                    let Some(host) = t.parent() else {
                        continue;
                    };
                    for (start, tree) in holes(&t) {
                        for x in tree
                            .descendants_with_tokens()
                            .filter_map(|e| e.into_token())
                            .filter(|x| x.kind() == SyntaxKind::IDENT)
                        {
                            out.push(Named {
                                file: f,
                                range: x.text_range() + start,
                                what: self.classify_in(&x, Some(&host)),
                                token: x,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Every name of the parsed files that denotes `sym`, by file.
    pub fn occurrences<'p>(&self, files: &'p [Parsed], sym: &Symbol) -> Vec<Named<'p>> {
        files
            .iter()
            .flat_map(|f| self.names(f))
            .filter(|n| match &n.what {
                What::Name(s, _) => s == sym,
                What::Names(ss) => ss.contains(sym),
                _ => false,
            })
            .collect()
    }

    /// Every pun of the parsed files that fills the input `sym` (`env`
    /// of `use baseline { env }` fills baseline's `env`).
    pub fn filling<'p>(&self, files: &'p [Parsed], sym: &Symbol) -> Vec<Named<'p>> {
        files
            .iter()
            .flat_map(|f| self.names(f))
            .filter(|n| self.fills(&n.token).as_ref() == Some(sym))
            .collect()
    }

    /// Where what `w` names is declared: a relation's `decl` (else its
    /// first rule), an output's typed declaration, a module's file, a
    /// function's signature line; a type's `type` block.
    pub fn definition<'p>(&self, files: &'p [Parsed], w: &What) -> Vec<Site<'p>> {
        match w {
            What::Name(sym, _) => self.declarations(files, sym),
            What::Names(syms) => syms
                .iter()
                .flat_map(|s| self.declarations(files, s))
                .collect(),
            What::Type(typ) => files
                .iter()
                .flat_map(|f| {
                    f.tree
                        .descendants()
                        .filter(|n| n.kind() == SyntaxKind::TYPE_DECL)
                        .filter(|n| dotted_after_keyword(n) == *typ)
                        .filter_map(|n| {
                            let ts = own_tokens(&n);
                            let (first, last) =
                                (ts.get(1)?, ts.get(1 + 2 * typ.matches('.').count())?);
                            Some(Site::Text(f, first.text_range().cover(last.text_range())))
                        })
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn declarations<'p>(&self, files: &'p [Parsed], sym: &Symbol) -> Vec<Site<'p>> {
        match sym {
            Symbol::File(p) => {
                return self
                    .files
                    .get(p)
                    .cloned()
                    .map(Site::File)
                    .into_iter()
                    .collect();
            }
            Symbol::Function(_) | Symbol::Package(_) => return std_site(sym).into_iter().collect(),
            _ => {}
        }
        let decls: Vec<(&Parsed, SyntaxToken)> = self
            .occurrences(files, sym)
            .into_iter()
            .filter(Named::is_declaration)
            .map(|n| (n.file, n.token))
            .collect();
        let kind = |t: &SyntaxToken| t.parent().map(|p| p.kind());
        let preferred: Vec<&(&Parsed, SyntaxToken)> = match sym {
            // A relation's declaration, else its first rule.
            Symbol::Predicate(..) => {
                let declared: Vec<_> = decls
                    .iter()
                    .filter(|(_, t)| {
                        matches!(
                            kind(t),
                            Some(
                                SyntaxKind::DECL | SyntaxKind::EXTERN | SyntaxKind::INPUT_RELATION
                            )
                        )
                    })
                    .collect();
                if declared.is_empty() {
                    decls.iter().take(1).collect()
                } else {
                    declared
                }
            }
            // An output declared with its type, else where it is defined.
            Symbol::Output(..) => {
                let typed: Vec<_> = decls
                    .iter()
                    .filter(|(_, t)| {
                        t.parent().is_some_and(|p| {
                            p.children().any(|c| c.kind() == SyntaxKind::TYPE_EXPR)
                        })
                    })
                    .collect();
                if typed.is_empty() {
                    decls.iter().take(1).collect()
                } else {
                    typed
                }
            }
            // A component, else the file a `use` binds the name to.
            Symbol::Module(m) => {
                let components: Vec<_> = decls
                    .iter()
                    .filter(|(_, t)| kind(t) == Some(SyntaxKind::COMPONENT))
                    .collect();
                if components.is_empty()
                    && let Some(f) = self.used_by(m)
                {
                    return vec![Site::File(f.clone())];
                }
                components
            }
            _ => decls.iter().collect(),
        };
        preferred
            .into_iter()
            .map(|(f, t)| Site::Text(f, t.text_range()))
            .collect()
    }

    /// The file a `use` binds to the name `m`, when it is the project's.
    fn used_by(&self, m: &str) -> Option<&PathBuf> {
        self.tree.every().find_map(|(_, n, d)| match &d.kind {
            DeclKind::Use { module, .. } if n == m => self.files.get(module.as_str()),
            _ => None,
        })
    }

    /// The token at byte `at` of `path` (a name of an interpolation hole
    /// when `at` is in one), and what it is.
    pub fn at<'p>(&self, files: &'p [Parsed], path: &Path, at: usize) -> Option<Named<'p>> {
        let f = files.iter().find(|f| f.path == path)?;
        let t = token_at(&f.tree, at)?;
        if t.kind() == SyntaxKind::STRING
            && let Some(host) = t.parent()
        {
            for (start, tree) in holes(&t) {
                let Some(rel) = at.checked_sub(usize::from(start)) else {
                    continue;
                };
                if rel > usize::from(tree.text_range().end()) {
                    continue;
                }
                let Some(x) = token_at(&tree, rel).filter(|x| x.kind() == SyntaxKind::IDENT) else {
                    continue;
                };
                return Some(Named {
                    file: f,
                    range: x.text_range() + start,
                    what: self.classify_in(&x, Some(&host)),
                    token: x,
                });
            }
        }
        Some(Named {
            file: f,
            range: t.text_range(),
            what: self.classify(&t),
            token: t,
        })
    }
}

/// What a project's `use`s and copies name, read off its files before
/// the scope tree is built, as the loader reads a program's: which files
/// are modules (R-65), and which `use`s import a provider (R-112).
struct Modules {
    /// The paths `use`s and copies name.
    named: BTreeSet<String>,
    /// A `use`'s path of one segment that names no file and is not std.
    providers: BTreeSet<String>,
}

impl Modules {
    fn of(files: &[Parsed], paths: &BTreeMap<String, PathBuf>) -> Modules {
        let mut uses: BTreeMap<String, String> = BTreeMap::new();
        let mut components: BTreeSet<String> = BTreeSet::new();
        for n in files.iter().flat_map(|f| f.tree.descendants()) {
            match n.kind() {
                SyntaxKind::USE => {
                    let (path, name) = crate::syntax::resolve::use_parts(&n);
                    uses.insert(name, path);
                }
                SyntaxKind::COMPONENT => {
                    components.extend(declared_name(&n).map(|x| x.text().to_string()));
                }
                _ => {}
            }
        }
        // A copy of a component: a resource whose type's last segment is
        // a component, before it a file, a `use`'s name or nothing.
        let copy = |path: &str| match path.rsplit_once('.') {
            None => components.contains(path),
            Some((prefix, last)) => {
                let head = prefix.split('.').next().unwrap_or(prefix);
                components.contains(last)
                    && (paths.contains_key(prefix)
                        || uses.contains_key(head)
                        || components.contains(head))
            }
        };
        let mut named: BTreeSet<String> = BTreeSet::new();
        for n in files.iter().flat_map(|f| f.tree.descendants()) {
            if n.kind() == SyntaxKind::RESOURCE {
                let (path, _) = crate::syntax::resolve::copy_parts(&n);
                if copy(&path) {
                    named.insert(path);
                }
            }
        }
        let providers: BTreeSet<String> = uses
            .values()
            .filter(|p| !p.contains('.') && p.as_str() != "std" && !paths.contains_key(*p))
            .cloned()
            .collect();
        named.extend(uses.into_values().filter(|p| !providers.contains(p)));
        Modules { named, providers }
    }

    /// Whether the file at dotted `path` is a module: a `use` or a copy
    /// names it, or an item of it.
    fn is_module(&self, path: &str) -> bool {
        self.named
            .iter()
            .any(|p| p == path || p.strip_prefix(path).is_some_and(|r| r.starts_with('.')))
    }
}

/// Whether `name` is a relation of dform's own, which no program
/// declares: the compiler's, the engine's or a provider's (`deformation`,
/// `drift`), an aggregate.
pub fn is_builtin_relation(name: &str) -> bool {
    crate::loader::is_core_name(name) || crate::reference::reference(name, true).is_some()
}

/// The signature line of a function or a package in `std/*.df`.
fn std_site(sym: &Symbol) -> Option<Site<'static>> {
    let source = |file: &str| {
        crate::functions::SOURCES
            .iter()
            .find(|(f, _)| *f == file)
            .copied()
    };
    match sym {
        Symbol::Function(name) => {
            let f = crate::functions::get(name)?;
            let (file, text) = source(&f.file)?;
            Some(Site::Std(file, text, f.line))
        }
        Symbol::Package(p) => {
            let f = crate::functions::registry()
                .functions()
                .find(|f| f.package == *p)?;
            let (file, text) = source(&f.file)?;
            let line = text
                .lines()
                .position(|l| l.trim() == format!("package {p}"))?;
            Some(Site::Std(file, text, line + 1))
        }
        _ => None,
    }
}

/// The token under byte `at`, preferring a name to what abuts it.
fn token_at(root: &SyntaxNode, at: usize) -> Option<SyntaxToken> {
    let at = rowan::TextSize::from(u32::try_from(at).ok()?);
    if at > root.text_range().end() {
        return None;
    }
    let mut ts = root.token_at_offset(at);
    let (l, r) = (ts.next(), ts.next());
    match (l, r) {
        (Some(_), Some(r)) if r.kind() == SyntaxKind::IDENT => Some(r),
        (Some(l), _) => Some(l),
        _ => None,
    }
}

/// Whether an `output` names a value (`output k: T`, `output k = t`), not
/// a relation the module exports (`output p`, R-55).
fn is_value_output(n: &SyntaxNode) -> bool {
    n.children().any(|c| c.kind() == SyntaxKind::TYPE_EXPR)
        || n.children_with_tokens().any(|e| e.kind() == SyntaxKind::EQ)
}

/// The `IDENT` naming a declaration node (`component NAME`).
pub fn declared_name(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT)
}

/// The predicate a rule or fact node states: its head call's name, or a
/// value rule's.
pub fn head_name(node: &SyntaxNode) -> Option<SyntaxToken> {
    match node.kind() {
        SyntaxKind::RULE | SyntaxKind::FACT => {
            let call = node.children().find(|c| c.kind() == SyntaxKind::CALL)?;
            let chain = call.children().find(|c| c.kind() == SyntaxKind::CHAIN)?;
            let mut ts = own_tokens(&chain).into_iter();
            let first = ts.next().filter(|t| t.kind() == SyntaxKind::IDENT)?;
            ts.next().is_none().then_some(first)
        }
        SyntaxKind::LET => declared_name(node),
        _ => None,
    }
}

/// The path of a `use` or `instance` up to and including the token `t`,
/// when `t` is one of its segments.
fn path_prefix(parent: &SyntaxNode, t: &SyntaxToken) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    let mut dot = true;
    for x in own_tokens(parent).into_iter().skip(1) {
        match x.kind() {
            SyntaxKind::DOT if !dot => dot = true,
            k if k.is_word() && dot => {
                out.push(x.text().to_string());
                dot = false;
                if &x == t {
                    return Some(out.join("."));
                }
            }
            _ => return None,
        }
    }
    None
}

/// An object input's fields, each with its path: `count` of `input nodes
/// { count: int }`, `a.b` of a nested object.
fn input_fields(input: &SyntaxNode) -> Vec<(String, SyntaxToken)> {
    input
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::ATTR_DECL)
        .filter_map(|a| {
            let path = a.children().find(|c| c.kind() == SyntaxKind::BLOCK_PATH)?;
            let last = idents(&path).pop()?;
            Some((field_path(&a, &path, &last), last))
        })
        .collect()
}

/// The path of a field of an object input up to `t`: the enclosing
/// fields' paths, then its own.
fn field_path(attr: &SyntaxNode, path: &SyntaxNode, t: &SyntaxToken) -> String {
    let mut segs: Vec<String> = Vec::new();
    for a in attr
        .ancestors()
        .skip(1)
        .take_while(|a| a.kind() != SyntaxKind::INPUT)
        .filter(|a| a.kind() == SyntaxKind::ATTR_DECL)
    {
        if let Some(p) = a.children().find(|c| c.kind() == SyntaxKind::BLOCK_PATH) {
            segs.insert(
                0,
                idents(&p)
                    .iter()
                    .map(|x| x.text())
                    .collect::<Vec<_>>()
                    .join("."),
            );
        }
    }
    let own: Vec<String> = idents(path)
        .into_iter()
        .scan(false, |done, x| {
            if *done {
                return None;
            }
            *done = &x == t;
            Some(x.text().to_string())
        })
        .collect();
    segs.push(own.join("."));
    segs.join(".")
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

/// A node's dotted run of names from its first: `net.vpc` of `net.vpc[e]`.
fn dotted_names(n: &SyntaxNode) -> String {
    let mut out = String::new();
    for t in n
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
    {
        match t.kind() {
            k if k.is_word() || k == SyntaxKind::DOT => out.push_str(t.text()),
            _ => break,
        }
    }
    out
}

/// The dotted name right after a node's keyword: `net.vpc` of `type
/// net.vpc {`.
fn dotted_after_keyword(n: &SyntaxNode) -> String {
    let mut out = String::new();
    for t in own_tokens(n).into_iter().skip(1) {
        match t.kind() {
            k if k.is_word() || k == SyntaxKind::DOT => out.push_str(t.text()),
            _ => break,
        }
    }
    out
}

/// The name a `decl`, `extern` or relation `input` declares, when it is
/// one name (`decl p(a, b)`; not a dotted extern).
fn relation_name(n: &SyntaxNode) -> Option<SyntaxToken> {
    let ts = own_tokens(n);
    if ts.iter().any(|t| t.kind() == SyntaxKind::TYPE_KW) {
        return None;
    }
    let mut ids = ts.iter().skip_while(|t| !t.kind().is_keyword()).skip(1);
    let first = ids.next()?;
    let next = ts
        .iter()
        .skip_while(|t| *t != first)
        .nth(1)
        .map(SyntaxToken::kind);
    (first.kind() == SyntaxKind::IDENT && next != Some(SyntaxKind::DOT)).then(|| first.clone())
}

/// A resource header: its type, its name's token, and whether that name
/// is static: a bare name always is, the literal name (R-76); a string
/// is the clause's.
pub struct Header {
    pub typ: String,
    pub name: SyntaxToken,
    pub is_static: bool,
    /// The type's tokens.
    pub type_tokens: Vec<SyntaxToken>,
}

pub fn header(n: &SyntaxNode) -> Option<Header> {
    // Up to a value body's `=` (R-126).
    let ts: Vec<SyntaxToken> = own_tokens(n)
        .into_iter()
        .take_while(|t| t.kind() != SyntaxKind::EQ)
        .filter(|t| t.kind() != SyntaxKind::RANK)
        .collect();
    let (name, rest) = ts.split_last()?;
    if !matches!(name.kind(), SyntaxKind::IDENT | SyntaxKind::STRING) {
        return None;
    }
    let type_tokens: Vec<SyntaxToken> = rest.iter().skip(1).cloned().collect();
    let typ = type_tokens.iter().map(|t| t.text()).collect::<String>();
    Some(Header {
        typ,
        name: name.clone(),
        is_static: name.kind() == SyntaxKind::IDENT,
        type_tokens,
    })
}

/// A chain's parts, in order.
#[derive(Debug)]
enum Part {
    Name(SyntaxToken),
    Dot,
    Index,
}

fn parts(chain: &SyntaxNode) -> Vec<Part> {
    chain
        .children_with_tokens()
        .filter_map(|e| match e {
            rowan::NodeOrToken::Node(n) => (n.kind() == SyntaxKind::INDEX).then_some(Part::Index),
            // A keyword after a dot is a name (`aws.instance`).
            rowan::NodeOrToken::Token(t) => match t.kind() {
                SyntaxKind::DOT => Some(Part::Dot),
                k if k.is_word() => Some(Part::Name(t)),
                _ => None,
            },
        })
        .collect()
}

/// A name of a chain being read: the chain, its parts, which of them the
/// name is (`k`), its head's name, whether the chain is called, and where
/// its names are looked up (`at`).
struct Read<'a> {
    c: &'a SyntaxNode,
    ps: Vec<Part>,
    k: usize,
    name0: String,
    call: bool,
    at: &'a SyntaxNode,
}

impl<'a> Read<'a> {
    /// The name `t` of the chain `c`, read at `at`; `None` when `t` is no
    /// name of it or the chain starts with none.
    fn of(c: &'a SyntaxNode, t: &SyntaxToken, at: &'a SyntaxNode) -> Option<Read<'a>> {
        let ps = parts(c);
        let k = ps
            .iter()
            .position(|p| matches!(p, Part::Name(x) if x == t))?;
        let Some(Part::Name(head)) = ps.first() else {
            return None;
        };
        Some(Read {
            c,
            name0: head.text().to_string(),
            call: c.parent().is_some_and(|p| p.kind() == SyntaxKind::CALL),
            ps,
            k,
            at,
        })
    }

    fn name_at(&self, i: usize) -> Option<String> {
        match self.ps.get(i) {
            Some(Part::Name(x)) => Some(x.text().to_string()),
            _ => None,
        }
    }

    fn dot(&self, i: usize) -> bool {
        matches!(self.ps.get(i), Some(Part::Dot))
    }

    fn index(&self, i: usize) -> bool {
        matches!(self.ps.get(i), Some(Part::Index))
    }

    /// `s` when the name is the chain's head; a path after it.
    fn head_or_path(&self, s: Symbol) -> What {
        match self.k {
            0 => What::Name(s, false),
            _ => What::Path,
        }
    }
}

/// Whether `t` is the whole of a block entry with no value (a pun).
pub fn is_pun(path: &SyntaxNode, t: &SyntaxToken) -> bool {
    path.kind() == SyntaxKind::BLOCK_PATH
        && idents(path).len() == 1
        && idents(path).first() == Some(t)
        && path.parent().is_some_and(|a| {
            a.kind() == SyntaxKind::ASSIGN
                && !a
                    .children()
                    .any(|c| c.kind() != SyntaxKind::BLOCK_PATH && c.kind() != SyntaxKind::RANK)
        })
}

/// Whether a chain reads component `m` by a dynamic index: `m[e]`, or
/// `net.m[e]` by its path.
pub fn indexed(files: &[Parsed], m: &str) -> bool {
    files.iter().any(|f| {
        f.tree
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::CHAIN)
            .any(|c| {
                parts(&c)
                    .windows(2)
                    .any(|w| matches!(w, [Part::Name(x), Part::Index] if x.text() == m))
            })
    })
}

/// Every address written as a string at the top of a program (H-16):
/// the key of `T["a"]` and the left of `"a" in T`, with its type and value.
/// Inside a component `T[e]` is relative to the instance, so those are
/// left.
pub fn addresses(files: &[Parsed]) -> Vec<(&Parsed, SyntaxToken, String, String)> {
    let mut out = Vec::new();
    let literal = |t: &SyntaxToken| {
        (t.kind() == SyntaxKind::STRING && !t.text().contains("${"))
            .then(|| crate::syntax::resolve::unescape(t.text()).ok())
            .flatten()
    };
    let type_name = |c: &SyntaxNode| -> Option<String> {
        let mut name = String::new();
        for e in c.children_with_tokens() {
            match e {
                rowan::NodeOrToken::Token(t) if t.kind().is_trivia() => {}
                rowan::NodeOrToken::Token(t)
                    if matches!(t.kind(), SyntaxKind::IDENT | SyntaxKind::DOT) =>
                {
                    name.push_str(t.text())
                }
                _ => break,
            }
        }
        (!name.is_empty()).then_some(name)
    };
    for f in files {
        for n in f.tree.descendants() {
            if n.ancestors().any(|a| a.kind() == SyntaxKind::COMPONENT) {
                continue;
            }
            match n.kind() {
                SyntaxKind::CHAIN => {
                    let Some(ix) = n.children().find(|c| c.kind() == SyntaxKind::INDEX) else {
                        continue;
                    };
                    let toks: Vec<SyntaxToken> = ix
                        .descendants_with_tokens()
                        .filter_map(|e| e.into_token())
                        .filter(|t| {
                            !t.kind().is_trivia()
                                && !matches!(
                                    t.kind(),
                                    SyntaxKind::L_BRACKET | SyntaxKind::R_BRACKET
                                )
                        })
                        .collect();
                    if let ([t], Some(typ)) = (toks.as_slice(), type_name(&n))
                        && let Some(v) = literal(t)
                    {
                        out.push((f, t.clone(), typ, v));
                    }
                }
                SyntaxKind::LIT_IN | SyntaxKind::LIT_NOT_IN => {
                    let mut kids = n.children();
                    let (Some(lhs), Some(rhs)) = (kids.next(), kids.next()) else {
                        continue;
                    };
                    let toks: Vec<SyntaxToken> = lhs
                        .descendants_with_tokens()
                        .filter_map(|e| e.into_token())
                        .filter(|t| !t.kind().is_trivia())
                        .collect();
                    let rhs = rhs
                        .descendants()
                        .find(|c| c.kind() == SyntaxKind::CHAIN)
                        .or_else(|| (rhs.kind() == SyntaxKind::CHAIN).then(|| rhs.clone()));
                    if let ([t], Some(typ)) = (toks.as_slice(), rhs.as_ref().and_then(type_name))
                        && let Some(v) = literal(t)
                    {
                        out.push((f, t.clone(), typ, v));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Every string literal that is exactly `value` (no holes), by file.
pub fn strings<'p>(files: &'p [Parsed], value: &str) -> Vec<(&'p Parsed, SyntaxToken)> {
    let quoted = format!("{value:?}");
    files
        .iter()
        .flat_map(|f| {
            f.tree
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .filter(|t| t.kind() == SyntaxKind::STRING && t.text() == quoted)
                .map(move |t| (f, t))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
input env: string = "staging"
let cfg = { a = env }
component network {
  input vpc_net: inet
  # its interface: an input, a resource and an output
  resource net.vpc vpc {
    cidr = vpc_net
    tags = { env = env }
  }
  output vpc: net.vpc = vpc
  q(x) where zone_index[x] = 1, vpc.cidr == x
}
resource network main { vpc_net = cfg.a }
resource compute.vm bastion { private_ip = 1 }
p(a) where a = net.vpc["main.vpc"].cidr, c = main.vpc, b = network[a].vpc, bastion.cidr == 1, bastion in compute.vm
zone_index("a", 0)
"#;

    /// The line (from 0) of a byte of a text.
    fn line(text: &str, at: usize) -> u32 {
        text[..at].matches('\n').count() as u32
    }

    fn names(src: &str, sym: &Symbol) -> Vec<(u32, bool)> {
        let files = vec![Parsed::new(PathBuf::from("/x.df"), src.into())];
        let d = Decls::of_files(Path::new("/"), &files);
        d.occurrences(&files, sym)
            .into_iter()
            .map(|n| {
                (
                    line(&n.file.text, n.range.start().into()),
                    n.is_declaration(),
                )
            })
            .collect()
    }

    #[test]
    fn a_resource_is_found_by_its_name() {
        // Its address from outside, `net.vpc["main.vpc"]`, is a string.
        assert_eq!(
            names(
                SRC,
                &Symbol::Resource(
                    Some("component network".into()),
                    "net.vpc".into(),
                    "vpc".into()
                )
            ),
            vec![(6, true), (10, false), (11, false)]
        );
        assert_eq!(
            names(
                SRC,
                &Symbol::Resource(None, "compute.vm".into(), "bastion".into())
            ),
            vec![(14, true), (15, false), (15, false)]
        );
    }

    #[test]
    fn values_modules_instances_and_predicates() {
        // The module's input, from its body and from its instance block.
        assert_eq!(
            names(
                SRC,
                &Symbol::Value(Some("component network".into()), "vpc_net".into())
            ),
            vec![(4, true), (7, false), (13, false)]
        );
        // The program's input, read inside the module and by the let.
        assert_eq!(
            names(SRC, &Symbol::Value(None, "env".into())),
            vec![(1, true), (2, false), (8, false)]
        );
        assert_eq!(
            names(SRC, &Symbol::Let(None, "cfg".into())),
            vec![(2, true), (13, false)]
        );
        assert_eq!(
            names(SRC, &Symbol::Module("network".into())),
            vec![(3, true), (13, false), (15, false)]
        );
        assert_eq!(
            names(SRC, &Symbol::Instance("network".into(), "main".into())),
            vec![(13, true), (15, false)]
        );
        assert_eq!(
            names(SRC, &Symbol::Predicate(None, "zone_index".into())),
            vec![(11, false), (16, true)]
        );
    }

    const PRIVATE: &str = r#"
component a {
  helper(1)
  q(x) where helper(x), shared(x)
}
component b {
  helper(2)
  r(x) where helper(x)
}
helper(3)
shared(3)
s(x) where helper(x), shared(x)
"#;

    #[test]
    fn a_module_private_relation_is_its_modules_own() {
        let a = Some("component a".to_string());
        let b = Some("component b".to_string());
        assert_eq!(
            names(PRIVATE, &Symbol::Predicate(a, "helper".into())),
            vec![(2, true), (3, false)]
        );
        assert_eq!(
            names(PRIVATE, &Symbol::Predicate(b, "helper".into())),
            vec![(6, true), (7, false)]
        );
        assert_eq!(
            names(PRIVATE, &Symbol::Predicate(None, "helper".into())),
            vec![(9, true), (11, false)]
        );
        // A relation a module reads but does not define is the program's.
        assert_eq!(
            names(PRIVATE, &Symbol::Predicate(None, "shared".into())),
            vec![(3, false), (10, true), (11, false)]
        );
    }

    /// A module's own resource wins a read in its body over its user's
    /// `use` of the module's name (R-101), as the resolver reads it.
    #[test]
    fn super_reads_the_scope_around_the_component() {
        let src = r#"
input tag: string = "m"
component outer {
  input tag: string = "o"
  let x = "x"
  component inner {
    resource net.vpc v { cidr = "${tag}-${super.tag}-${super.super.tag}-${x}" }
  }
}
"#;
        let files = vec![Parsed::new(PathBuf::from("/b.df"), src.into())];
        let d = Decls::of_files(Path::new("/"), &files);
        let lines = |sym: &Symbol| -> Vec<u32> {
            d.occurrences(&files, sym)
                .into_iter()
                .map(|n| line(&n.file.text, n.range.start().into()))
                .collect()
        };
        // A bare `tag` and `super.tag` in `inner` are outer's; two
        // `super`s the stack's, as is the bare read past both components
        // (`x` is outer's let).
        let outer = Some("component outer".to_string());
        assert_eq!(
            lines(&Symbol::Value(outer.clone(), "tag".into())),
            vec![3, 6, 6]
        );
        assert_eq!(lines(&Symbol::Value(None, "tag".into())), vec![1, 6]);
        assert_eq!(lines(&Symbol::Let(outer, "x".into())), vec![4, 6]);
    }

    #[test]
    fn a_modules_own_resource_wins_over_its_users_use() {
        let file = |path: &str, src: &str| Parsed::new(PathBuf::from(path), src.into());
        let files = vec![
            file(
                "/p/traefik.df",
                "\nresource net.vpc traefik { cidr = \"x\" }\nresource net.subnet web { cidr = traefik.web }\n",
            ),
            file(
                "/p/stacks/s.df",
                "\nuse traefik\np(x) where x = traefik.web.cidr\n",
            ),
        ];
        let d = Decls::of_files(Path::new("/p"), &files);
        let sym = Symbol::Resource(
            Some("module traefik".into()),
            "net.vpc".into(),
            "traefik".into(),
        );
        let got: Vec<(String, u32)> = d
            .occurrences(&files, &sym)
            .into_iter()
            .map(|n| {
                (
                    n.file.path.display().to_string(),
                    line(&n.file.text, n.range.start().into()),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![("/p/traefik.df".into(), 1), ("/p/traefik.df".into(), 2)]
        );
    }

    /// A module's file reads its own names and never its user's (R-205):
    /// `env` in baseline.df is its input, not the stack's key; the stack's
    /// `env` given in the `use` is the stack's.
    #[test]
    fn a_module_file_reads_no_name_of_its_user() {
        let file = |path: &str, src: &str| Parsed::new(PathBuf::from(path), src.into());
        let files = vec![
            file(
                "/p/baseline.df",
                "input env: string
warn \"w\" where env == \"prod\", region == \"eu\"\n",
            ),
            file(
                "/p/stacks/s.df",
                "key env: string = \"lab\"\nlet region = \"eu\"\nuse baseline { env = env }\n",
            ),
        ];
        let d = Decls::of_files(Path::new("/p"), &files);
        let what = |f: usize, needle: &str, nth: usize| {
            let at = files[f].text.match_indices(needle).nth(nth).unwrap().0;
            d.at(&files, &files[f].path, at).map(|a| a.what)
        };
        assert_eq!(
            what(0, "env ==", 0),
            Some(What::Name(
                Symbol::Value(Some("module baseline".into()), "env".into()),
                false
            ))
        );
        // The stack's `region` is no name in the module.
        assert!(
            !matches!(
                what(0, "region", 0),
                Some(What::Name(Symbol::Let(None, _), _))
            ),
            "{:?}",
            what(0, "region", 0)
        );
        assert_eq!(
            what(1, "env }", 0),
            Some(What::Name(Symbol::Value(None, "env".into()), false))
        );
    }

    /// Two module files that define a relation of the same name: each
    /// module's is its own (R-65), read from the stack as `m.p(..)`.
    #[test]
    fn a_module_files_relation_is_its_own() {
        let file = |path: &str, src: &str| Parsed::new(PathBuf::from(path), src.into());
        let files = vec![
            file("/p/a.df", "\nhelper(1)\nq(x) where helper(x)\n"),
            file("/p/b.df", "\nhelper(2)\nr(x) where helper(x)\n"),
            file(
                "/p/stacks/s.df",
                "\nuse a\nuse b\ns(x) where a.helper(x)\nt(x) where b.helper(x)\n",
            ),
        ];
        let d = Decls::of_files(Path::new("/p"), &files);
        let at = |m: &str| -> Vec<(String, u32, bool)> {
            d.occurrences(
                &files,
                &Symbol::Predicate(Some(format!("module {m}")), "helper".into()),
            )
            .into_iter()
            .map(|n| {
                (
                    n.file.path.display().to_string(),
                    line(&n.file.text, n.range.start().into()),
                    n.is_declaration(),
                )
            })
            .collect()
        };
        assert_eq!(
            at("a"),
            vec![
                ("/p/a.df".into(), 1, true),
                ("/p/a.df".into(), 2, false),
                ("/p/stacks/s.df".into(), 3, false)
            ]
        );
        assert_eq!(
            at("b"),
            vec![
                ("/p/b.df".into(), 1, true),
                ("/p/b.df".into(), 2, false),
                ("/p/stacks/s.df".into(), 4, false)
            ]
        );
    }

    /// What a name a used module, a resource and a copy share reads: the
    /// module's item, the copy's output (R-76); a name in a string's hole
    /// is looked up where the string is.
    #[test]
    fn module_items_outputs_and_holes() {
        let file = |path: &str, src: &str| Parsed::new(PathBuf::from(path), src.into());
        let files = vec![
            file("/p/config.df", "let base_domain = \"example.org\"\n"),
            file(
                "/p/db.df",
                "component pg {\n  input name: string\n  output conn = name\n}\n",
            ),
            file(
                "/p/stacks/s.df",
                "use config\nresource k8s.secret config {}\nresource db.pg app { name = \"a\" }\n\
                 resource k8s.secret app {}\n\
                 let x = config.base_domain\nlet y = app.conn\nlet z = \"${config.base_domain}\"\n\
                 let w = \"${str.upper(\"a-${config.base_domain}\")}\"\n",
            ),
        ];
        let d = Decls::of_files(Path::new("/p"), &files);
        let s = &files[2];
        let what = |needle: &str, ahead: usize, nth: usize| {
            let at = s.text.match_indices(needle).nth(nth).unwrap().0 + ahead;
            d.at(&files, &s.path, at).unwrap().what
        };
        let item = |sym: Symbol| What::Name(sym, false);
        assert_eq!(
            what("config.base_domain", 7, 0),
            item(Symbol::Let(
                Some("module config".into()),
                "base_domain".into()
            ))
        );
        assert_eq!(
            what("config.base_domain", 7, 1),
            item(Symbol::Let(
                Some("module config".into()),
                "base_domain".into()
            ))
        );
        assert_eq!(
            what("app.conn", 4, 0),
            item(Symbol::Output(
                Some("component db.pg".into()),
                "conn".into()
            ))
        );
        assert_eq!(
            what("config {}", 0, 0),
            What::Name(
                Symbol::Resource(None, "k8s.secret".into(), "config".into()),
                true
            )
        );
        // A hole in a string in a hole (R-175).
        assert_eq!(
            what("config.base_domain", 7, 2),
            item(Symbol::Let(
                Some("module config".into()),
                "base_domain".into()
            ))
        );
        // The let's references: its declaration, the read and the holes'.
        let sym = Symbol::Let(Some("module config".into()), "base_domain".into());
        assert_eq!(d.occurrences(&files, &sym).len(), 4);
    }

    /// A bare read takes the innermost scope's declaration whether it is
    /// a `let` or an input, as the front end does: a component's input
    /// over the program's `let` of the name.
    #[test]
    fn a_components_input_wins_over_an_outer_let() {
        let src = "let schema = \"s\"\ncomponent app {\n  input schema: string\n  let x = schema\n  let y = \"${schema}\"\n}\nlet z = schema\n";
        let app = Some("component app".to_string());
        assert_eq!(
            names(src, &Symbol::Value(app, "schema".into())),
            vec![(2, true), (3, false), (4, false)]
        );
        assert_eq!(
            names(src, &Symbol::Let(None, "schema".into())),
            vec![(0, true), (6, false)]
        );
    }

    /// A let's parameter is declared in its head and read in its body,
    /// a hole of a string included; a name of the program the let does
    /// not take is the program's (R-187).
    #[test]
    fn a_lets_parameter_is_declared_in_its_head() {
        let src = "input n: int = 1\nlet cidr(n, m: int = 2) = \"10.${n}.${m}.0/24\" where n > 0\nlet x = { n }\nlet y = n\n";
        let sym = |p: &str| Symbol::Param(None, "cidr".into(), p.into());
        assert_eq!(
            names(src, &sym("n")),
            vec![(1, true), (1, false), (1, false)]
        );
        assert_eq!(names(src, &sym("m")), vec![(1, true), (1, false)]);
        assert_eq!(
            names(src, &Symbol::Value(None, "n".into())),
            vec![(0, true), (2, false), (3, false)]
        );
        let files = vec![Parsed::new(PathBuf::from("/a.df"), src.into())];
        let d = Decls::of_files(Path::new("/"), &files);
        assert!(d.taken(&sym("n"), "m"));
        assert!(!d.taken(&sym("n"), "k"));
    }

    /// A stack another stack uses is the program's in its own file, and
    /// what the user reads of it (`platform.subnet`) is that file's own
    /// declaration (R-211 step 12: the tree's answer, not a module's).
    #[test]
    fn a_stack_another_uses_is_the_programs_in_its_own_file() {
        let file = |path: &str, src: &str| Parsed::new(PathBuf::from(path), src.into());
        let files = vec![
            file(
                "/p/stacks/platform.df",
                "input region: string = \"eu\"\noutput subnet = region\n",
            ),
            file(
                "/p/stacks/apps.df",
                "use stacks.platform as platform\nlet region = \"us\"\nlet s = platform.subnet\nlet t = region\n",
            ),
        ];
        let d = Decls::of_files(Path::new("/p"), &files);
        let what = |f: usize, needle: &str, ahead: usize| {
            let at = files[f].text.find(needle).unwrap() + ahead;
            d.at(&files, &files[f].path, at).map(|a| a.what)
        };
        assert_eq!(
            what(0, "region\n", 0),
            Some(What::Name(Symbol::Value(None, "region".into()), false))
        );
        // Each stack reads its own file's `region`.
        assert_eq!(
            what(1, "t = region", 4),
            Some(What::Name(Symbol::Let(None, "region".into()), false))
        );
        assert_eq!(
            what(1, "platform.subnet", 9),
            Some(What::Name(Symbol::Output(None, "subnet".into()), false))
        );
    }
}

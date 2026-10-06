//! What a name at a place denotes, read off the syntax trees of a
//! project's files in the resolver's order (docs/grammar.md "Names")
//! without evaluating them: the declaration it is, and every place that
//! denotes the same. The language server's definition, references, rename
//! and hover read it; so may any tool that names a declaration by a place.
//!
//! Every file is a module by its path from the root (R-65): what a file a
//! `use` or an `instance` names declares at its top is that module's
//! (`module a.b`), read by its user as `m.x`; what a component declares
//! is the component's (`component c`), read through its copies as `n.k`
//! or `c[e].k`. A name a body does not declare reads outward: its
//! component's, its file's, then the program's.

use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The block a name is declared in, innermost: `component network`, or
/// `module a.b` for a module file's top; `None` for the program's.
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
    /// `type NAME = TYPE`.
    Alias(String),
    /// A component, or the name a `use` binds (R-65).
    Module(String),
    /// `instance c n`: the component's path and the instance's name.
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
/// is (`parser::parse_term`) with the byte of the file it starts at.
pub fn holes(t: &SyntaxToken) -> Vec<(rowan::TextSize, SyntaxNode)> {
    let Some(pieces) = crate::syntax::resolve::pieces(t.text()) else {
        return Vec::new();
    };
    pieces
        .into_iter()
        .filter_map(|p| match p {
            crate::syntax::resolve::Piece::Hole(src, at) => {
                let lead = src.len() - src.trim_start().len();
                let start = usize::from(t.text_range().start()) + at + lead;
                let tree = crate::syntax::parser::parse_term(src.trim()).syntax();
                Some((rowan::TextSize::from(u32::try_from(start).ok()?), tree))
            }
            _ => None,
        })
        .collect()
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

/// The declarations of every file of a project, which the order of
/// resolution consults.
#[derive(Debug, Default)]
pub struct Decls {
    lets: BTreeSet<(Scope, String)>,
    values: BTreeSet<(Scope, String)>,
    /// An object input's fields, by its scope, name and field path.
    fields: BTreeSet<(Scope, String, String)>,
    outputs: BTreeSet<(Scope, String)>,
    /// By scope and name: each static resource's types.
    resources: BTreeMap<(Scope, String), Vec<String>>,
    components: BTreeSet<String>,
    /// Components and the names `use`s and `instance`s bind.
    modules: BTreeSet<String>,
    /// Every relation's name, wherever it is defined.
    predicates: BTreeSet<String>,
    /// The relations each scope defines.
    defined: BTreeSet<(Scope, String)>,
    aliases: BTreeSet<String>,
    /// Resource headers', `type` blocks' and the schema's types.
    types: BTreeSet<String>,
    instances: BTreeSet<(String, String)>,
    /// Each file's dotted path from the root, by its tree's root.
    roots: Vec<(SyntaxNode, String)>,
    /// Each file by its dotted path.
    files: BTreeMap<String, PathBuf>,
    /// The paths `use`s and `instance`s name.
    named: BTreeSet<String>,
    /// The paths of the stacks (`stacks/s.df`, R-65): one a `use` names is
    /// read for its deployments' outputs, its resources keep their names.
    stacks: BTreeSet<String>,
    /// The name a `use` binds -> the module path it names.
    uses: BTreeMap<String, String>,
    /// The providers `use`s import (R-112): a path of one segment that
    /// names no file of the project.
    providers: BTreeSet<String>,
    /// (type, attribute path) -> the type its `ref(T)` takes (R-74).
    refs: BTreeMap<(String, String), String>,
}

/// A file's dotted path from `root`: `stacks/platform.df` is
/// `stacks.platform`.
pub fn module_path(root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?.with_extension("");
    let segs: Vec<&str> = rel.iter().map(|s| s.to_str()).collect::<Option<_>>()?;
    (!segs.is_empty()).then(|| segs.join("."))
}

impl Decls {
    /// The declarations of a project's files under `root`, a module
    /// file's its own (R-65).
    pub fn of_files(root: &Path, files: &[Parsed]) -> Decls {
        let mut d = Decls::default();
        for f in files {
            let Some(p) = module_path(root, &f.path) else {
                continue;
            };
            if f.path.parent() == Some(root.join(crate::project::STACKS_DIR).as_path()) {
                d.stacks.insert(p.clone());
            }
            d.roots.push((f.tree.clone(), p.clone()));
            d.files.insert(p, f.path.clone());
            for n in f.tree.descendants() {
                let path = match n.kind() {
                    SyntaxKind::USE => {
                        let (path, name) = crate::syntax::resolve::use_parts(&n);
                        d.uses.insert(name, path.clone());
                        path
                    }
                    SyntaxKind::INSTANCE => crate::syntax::resolve::instance_parts(&n).0,
                    _ => continue,
                };
                d.named.insert(path);
            }
            // The components, which a resource's type may name (R-113).
            for n in f.tree.descendants() {
                if n.kind() == SyntaxKind::COMPONENT
                    && let Some(x) = declared_name(&n)
                {
                    d.components.insert(x.text().to_string());
                }
            }
        }
        // A resource of a component names its file as a `use` does.
        for f in files {
            for n in f.tree.descendants() {
                if n.kind() == SyntaxKind::RESOURCE && d.is_copy(&n) {
                    d.named.insert(crate::syntax::resolve::copy_parts(&n).0);
                }
            }
        }
        let providers: Vec<(String, String)> = d
            .uses
            .iter()
            .filter(|(_, p)| !p.contains('.') && p.as_str() != "std" && !d.files.contains_key(*p))
            .map(|(n, p)| (n.clone(), p.clone()))
            .collect();
        for (name, path) in providers {
            d.uses.remove(&name);
            d.named.remove(&path);
            d.providers.insert(path);
        }
        for f in files {
            d.declare(&f.tree);
        }
        d
    }

    /// Whether `n` makes a copy of a component: an `instance`, or a
    /// `resource` whose type's last segment is a component of the
    /// project, before it a file, a `use`'s name or nothing (R-113).
    fn is_copy(&self, n: &SyntaxNode) -> bool {
        match n.kind() {
            SyntaxKind::INSTANCE => true,
            SyntaxKind::RESOURCE => {
                let (path, _) = crate::syntax::resolve::copy_parts(n);
                match path.rsplit_once('.') {
                    None => self.components.contains(&path),
                    Some((prefix, last)) => {
                        let head = prefix.split('.').next().unwrap_or(prefix);
                        self.components.contains(last)
                            && (self.files.contains_key(prefix)
                                || self.uses.contains_key(head)
                                || self.components.contains(head))
                    }
                }
            }
            _ => false,
        }
    }

    /// Whether the body `at` is in (its component's, else its file's)
    /// binds `name` itself by a `use` or a copy: then the body's own
    /// resource of the name does not win a read over it (R-101).
    fn binds_here(&self, at: &SyntaxNode, name: &str) -> bool {
        let body = at
            .ancestors()
            .find(|a| a.kind() == SyntaxKind::COMPONENT)
            .and_then(|c| c.children().find(|x| x.kind() == SyntaxKind::STMT_BLOCK))
            .or_else(|| at.ancestors().last());
        body.into_iter()
            .flat_map(|b| b.children())
            .any(|n| match n.kind() {
                SyntaxKind::USE => crate::syntax::resolve::use_parts(&n).1 == name,
                _ if self.is_copy(&n) => crate::syntax::resolve::copy_parts(&n).1 == name,
                _ => false,
            })
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

    fn declare(&mut self, root: &SyntaxNode) {
        for n in root.descendants() {
            // A component signature's inputs and outputs (R-104) are no
            // scope's: the components that have it declare their own.
            if n.ancestors().any(|a| a.kind() == SyntaxKind::SIGNATURE) {
                continue;
            }
            let at = n.parent().unwrap_or_else(|| n.clone());
            let scope = self.scopes(&at).remove(0);
            let name = || declared_name(&n).map(|t| t.text().to_string());
            match n.kind() {
                SyntaxKind::LET => self.lets.extend(name().map(|x| (scope, x))),
                SyntaxKind::INPUT => {
                    if let Some(x) = name() {
                        for (path, _) in input_fields(&n) {
                            self.fields.insert((scope.clone(), x.clone(), path));
                        }
                        self.values.insert((scope, x));
                    }
                }
                SyntaxKind::OUTPUT_DECL if is_value_output(&n) => {
                    self.outputs.extend(name().map(|x| (scope, x)))
                }
                SyntaxKind::TYPE_ALIAS => self.aliases.extend(name()),
                SyntaxKind::COMPONENT => {
                    if let Some(x) = name() {
                        self.components.insert(x.clone());
                        self.modules.insert(x);
                    }
                }
                SyntaxKind::USE if self.provider_use(&n) => {}
                SyntaxKind::USE => {
                    self.modules.insert(crate::syntax::resolve::use_parts(&n).1);
                }
                _ if self.is_copy(&n) => {
                    let (path, name) = crate::syntax::resolve::copy_parts(&n);
                    if let Some(last) = path.rsplit('.').next() {
                        self.modules.insert(last.to_string());
                    }
                    self.instances.insert((path, name));
                }
                SyntaxKind::RESOURCE => {
                    if let Some(h) = header(&n) {
                        self.types.insert(h.typ.clone());
                        if h.is_static {
                            let ts = self
                                .resources
                                .entry((scope, h.name.text().to_string()))
                                .or_default();
                            if !ts.contains(&h.typ) {
                                ts.push(h.typ);
                            }
                        }
                    }
                }
                SyntaxKind::TYPE_DECL => {
                    let t = dotted_after_keyword(&n);
                    if !t.is_empty() {
                        self.types.insert(t);
                    }
                }
                SyntaxKind::RULE
                | SyntaxKind::FACT
                | SyntaxKind::DECL
                | SyntaxKind::EXTERN
                | SyntaxKind::INPUT_RELATION => {
                    let t = match n.kind() {
                        SyntaxKind::RULE | SyntaxKind::FACT => head_name(&n),
                        _ => relation_name(&n),
                    };
                    if let Some(t) = t {
                        self.predicates.insert(t.text().to_string());
                        let s = self.relation_scope(&n);
                        self.defined.insert((s, t.text().to_string()));
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether the file at dotted `path` is a module: a `use` or an
    /// `instance` names it, or an item of it.
    fn is_module(&self, path: &str) -> bool {
        self.named
            .iter()
            .any(|p| p == path || p.strip_prefix(path).is_some_and(|r| r.starts_with('.')))
    }

    /// The scope of the top of the file `n` is in: `module PATH` when the
    /// file is a module.
    fn file_scope(&self, n: &SyntaxNode) -> Scope {
        let root = n.ancestors().last()?;
        let (_, p) = self.roots.iter().find(|(r, _)| *r == root)?;
        self.is_module(p).then(|| format!("module {p}"))
    }

    /// The scopes a name at `n` is looked up in, innermost first: its
    /// component's, its module file's, the program's.
    pub fn scopes(&self, n: &SyntaxNode) -> Vec<Scope> {
        let mut out: Vec<Scope> = Vec::new();
        out.extend(component_scope(n).map(Some));
        out.extend(self.file_scope(n).map(Some));
        out.push(None);
        out
    }

    /// The value, let or output `name` seen from `at`: the innermost
    /// scope's that declares it.
    fn lookup(
        &self,
        set: &BTreeSet<(Scope, String)>,
        at: &SyntaxNode,
        name: &str,
        wrap: fn(Scope, String) -> Symbol,
    ) -> Option<Symbol> {
        self.scopes(at)
            .into_iter()
            .find(|s| set.contains(&(s.clone(), name.to_string())))
            .map(|s| wrap(s, name.to_string()))
    }

    fn value(&self, at: &SyntaxNode, name: &str) -> Option<Symbol> {
        self.lookup(&self.values, at, name, Symbol::Value)
    }

    fn let_(&self, at: &SyntaxNode, name: &str) -> Option<Symbol> {
        self.lookup(&self.lets, at, name, Symbol::Let)
    }

    /// The resources `name` seen from `at`: the innermost scope's that
    /// declares one, each type's.
    fn resources_at(&self, at: &SyntaxNode, name: &str) -> Vec<Symbol> {
        self.scopes(at)
            .into_iter()
            .find_map(|s| {
                let ts = self.resources.get(&(s.clone(), name.to_string()))?;
                Some(
                    ts.iter()
                        .map(|t| Symbol::Resource(s.clone(), t.clone(), name.to_string()))
                        .collect(),
                )
            })
            .unwrap_or_default()
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

    /// The scope that keeps the relations defined at `at` private: its
    /// component, else its file when the file is a module.
    fn relation_scope(&self, at: &SyntaxNode) -> Scope {
        component_scope(at).or_else(|| self.file_scope(at))
    }

    /// The scope a `use` or `instance` block's rule at `at` gives rows to:
    /// the module's or the component's.
    fn block_target(&self, at: &SyntaxNode) -> Scope {
        let owner = at
            .ancestors()
            .find(|a| a.kind() == SyntaxKind::USE || self.is_copy(a))?;
        Some(match owner.kind() {
            SyntaxKind::USE => format!("module {}", crate::syntax::resolve::use_parts(&owner).0),
            _ => {
                let (path, _) = crate::syntax::resolve::copy_parts(&owner);
                format!("component {}", last_segment(&path))
            }
        })
    }

    /// The relation `name` read or defined at `at`: the innermost scope's
    /// that defines it (in a `use` or `instance` block, the module's or the
    /// component's first), else the program's.
    fn predicate(&self, at: &SyntaxNode, name: &str) -> Symbol {
        let defines =
            |s: &Scope| s.is_some() && self.defined.contains(&(s.clone(), name.to_string()));
        let scope = if crate::loader::is_core_pred(name) {
            None
        } else {
            let mut candidates: Vec<Scope> = vec![self.block_target(at)];
            candidates.extend(self.scopes(at));
            candidates.into_iter().find(defines).flatten()
        };
        Symbol::Predicate(scope, name.to_string())
    }

    /// Whether `sym`'s kind already has a declaration named `name` where
    /// `sym` is (a rename onto it would merge the two).
    pub fn taken(&self, sym: &Symbol, name: &str) -> bool {
        let n = name.to_string();
        match sym {
            // Any relation of the name, anywhere: a private one would
            // shadow a global one, or the other way round.
            Symbol::Predicate(..) => self.predicates.contains(name),
            Symbol::Value(s, _) => self.values.contains(&(s.clone(), n)),
            Symbol::Field(s, i, p) => {
                let p = match p.rsplit_once('.') {
                    Some((init, _)) => format!("{init}.{n}"),
                    None => n,
                };
                self.fields.contains(&(s.clone(), i.clone(), p))
            }
            Symbol::Let(s, _) => self.lets.contains(&(s.clone(), n)),
            Symbol::Output(s, _) => self.outputs.contains(&(s.clone(), n)),
            Symbol::Alias(_) => self.aliases.contains(name),
            Symbol::Module(_) => self.modules.contains(name),
            Symbol::Instance(_, _) => self.instances.iter().any(|(_, i)| *i == n),
            Symbol::Resource(s, _, _) => self.resources.contains_key(&(s.clone(), n)),
            Symbol::Function(_) | Symbol::Package(_) | Symbol::File(_) => true,
        }
    }

    /// How an evaluation names the cells of a scope's inputs, lets and
    /// outputs (`attr("input", S, k, V)`): `""` for the program's or a
    /// stack's own, each copy's name for a component's, each name a `use`
    /// binds for a module file's.
    pub fn cell_scopes(&self, scope: &Scope) -> Vec<String> {
        match scope.as_deref() {
            None => vec![String::new()],
            Some(s) => match (s.strip_prefix("component "), s.strip_prefix("module ")) {
                (Some(c), _) => self
                    .instances
                    .iter()
                    .filter(|(p, _)| last_segment(p) == c)
                    .map(|(_, i)| i.clone())
                    .collect(),
                (_, Some(m)) if self.stacks.contains(m) => vec![String::new()],
                (_, Some(m)) => self
                    .uses
                    .iter()
                    .filter(|(_, p)| *p == m)
                    .map(|(n, _)| n.clone())
                    .collect(),
                _ => Vec::new(),
            },
        }
    }

    /// The file of the stack whose own scope `scope` is, when it is one
    /// another stack uses: only its evaluation has its cells.
    pub fn stack_file(&self, scope: &Scope) -> Option<&PathBuf> {
        let m = scope.as_deref()?.strip_prefix("module ")?;
        self.stacks.contains(m).then(|| self.files.get(m)).flatten()
    }

    /// The addresses a resource has, as strings name it (H-16, R-112): its
    /// name in the program's scope or a stack's; its path `n.name` in each
    /// copy `n` of its component or under each name its module file is
    /// used by.
    pub fn addresses_of(&self, sym: &Symbol) -> Vec<(String, String)> {
        let Symbol::Resource(scope, typ, name) = sym else {
            return Vec::new();
        };
        let name = crate::ir::name_segment(name).into_owned();
        let under = |n: &str| (typ.clone(), crate::ir::scoped(n, &name));
        match scope.as_deref() {
            None => vec![(typ.clone(), name.clone())],
            Some(s) => match (s.strip_prefix("component "), s.strip_prefix("module ")) {
                (Some(c), _) => self
                    .instances
                    .iter()
                    .filter(|(p, _)| last_segment(p) == c)
                    .map(|(_, i)| under(i))
                    .collect(),
                (_, Some(m)) if self.stacks.contains(m) => vec![(typ.clone(), name.clone())],
                (_, Some(m)) => self
                    .uses
                    .iter()
                    .filter(|(_, p)| *p == m)
                    .map(|(n, _)| under(n))
                    .collect(),
                _ => Vec::new(),
            },
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
        let scope = || self.scopes(&outer).remove(0);
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
            SyntaxKind::COMPONENT if is_declared() => decl(Symbol::Module(name)),
            SyntaxKind::USE if self.provider_use(&parent) => What::Provider,
            SyntaxKind::USE | SyntaxKind::INSTANCE => self.statement_path(&parent, t),
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
            // An alias, bare or read through its module (`network.subnets`,
            // R-65).
            SyntaxKind::TYPE_EXPR => {
                let toks = own_tokens(&parent);
                let dotted = toks.iter().any(|x| x.kind() == SyntaxKind::DOT);
                let last = toks.iter().rev().find(|x| x.kind() == SyntaxKind::IDENT);
                if self.aliases.contains(&name) && (!dotted || last == Some(t)) {
                    used(Some(Symbol::Alias(name)))
                } else {
                    What::Type(dotted_names(&parent))
                }
            }
            SyntaxKind::BLOCK_PATH => self.block_path(&parent, t),
            SyntaxKind::CHAIN => self.chain(&parent, t, &here),
            // `{ env }`: the field's value is the name's; `{ k: v }`, a key.
            SyntaxKind::OBJECT_FIELD
                if !own_tokens(&parent)
                    .iter()
                    .any(|x| x.kind() == SyntaxKind::COLON) =>
            {
                match self.let_(&here, &name).or_else(|| self.value(&here, &name)) {
                    Some(s) => What::Name(s, false),
                    None => match self.pick(&parent, self.resources_at(&here, &name)) {
                        // A pattern's field binds a variable of its name.
                        What::Other => What::Variable,
                        w => w,
                    },
                }
            }
            // A field of an object, a named argument, a column of a `decl`.
            SyntaxKind::OBJECT_FIELD | SyntaxKind::NAMED_ARG | SyntaxKind::BIND_ARG => What::Key,
            // What a dot reads off a call's value, `oci.parse(s).digest`.
            SyntaxKind::CALL_CHAIN => What::Path,
            _ => What::Other,
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
        // `instance network.vpc main`: the component `vpc` of network.df;
        // `instance vpc main`, a component of the program.
        if self.components.contains(t.text()) {
            return What::Name(Symbol::Module(t.text().to_string()), false);
        }
        What::Other
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
            let at = input.parent().unwrap_or_else(|| input.clone());
            let s = self.scopes(&at).remove(0);
            let field = field_path(attr, path, t);
            return What::Name(Symbol::Field(s, i.text().to_string(), field), true);
        }
        let block = entry.as_ref().and_then(|e| e.parent());
        // `k = ..` in an instance or a use block: the component's or the
        // module's input `k`; a provider's `use` block holds its settings.
        match block
            .and_then(|b| b.parent())
            .filter(|s| !self.provider_use(s))
        {
            Some(i) if (i.kind() == SyntaxKind::USE || self.is_copy(&i)) && first => {
                return What::Name(Symbol::Value(self.block_target(path), name), false);
            }
            Some(i) if i.kind() == SyntaxKind::USE || self.is_copy(&i) => {
                return What::Path;
            }
            _ => {}
        }
        // An entry that is only a name is the pun `k = k` (R-33): the name
        // is also its value, a `let`, an input or a resource (the
        // reference, R-43).
        if is_pun(path, t) {
            return match self.let_(path, &name).or_else(|| self.value(path, &name)) {
                Some(s) => What::Name(s, false),
                None => match self.pick(path, self.resources_at(path, &name)) {
                    What::Other => What::Path,
                    w => w,
                },
            };
        }
        What::Path
    }

    /// A name in a chain, by the order of resolution, its names looked up
    /// from `at`.
    fn chain(&self, c: &SyntaxNode, t: &SyntaxToken, at: &SyntaxNode) -> What {
        let ps = parts(c);
        let Some(k) = ps.iter().position(|p| matches!(p, Part::Name(x) if x == t)) else {
            return What::Other;
        };
        let name_at = |i: usize| match ps.get(i) {
            Some(Part::Name(x)) => Some(x.text().to_string()),
            _ => None,
        };
        let Some(name0) = name_at(0) else {
            return What::Other;
        };
        let call = c.parent().is_some_and(|p| p.kind() == SyntaxKind::CALL);
        let dot = |i: usize| matches!(ps.get(i), Some(Part::Dot));
        let index = |i: usize| matches!(ps.get(i), Some(Part::Index));
        let after = |from: usize| if k >= from { What::Path } else { What::Other };
        // A relation's name, `p(..)`, a head; a function's, `int(..)`.
        if call && ps.len() == 1 {
            let head = c
                .parent()
                .and_then(|call| call.parent())
                .is_some_and(|h| matches!(h.kind(), SyntaxKind::RULE | SyntaxKind::FACT));
            if !head && !self.predicates.contains(&name0) && crate::functions::callable(&name0) {
                return What::Name(Symbol::Function(name0), false);
            }
            return What::Name(self.predicate(at, &name0), head);
        }
        // `m.p(..)`, the relation `p` of the module `m` binds; `n.p(..)`,
        // the relation the copy `n` exports; `inet.subnet(..)`, a function.
        if call && ps.len() == 3 && dot(1) {
            let name2 = name_at(2).unwrap_or_default();
            if let Some(path) = self.uses.get(&name0) {
                return match k {
                    0 => What::Name(Symbol::Module(name0), false),
                    _ => What::Name(
                        Symbol::Predicate(Some(format!("module {path}")), name2),
                        false,
                    ),
                };
            }
            if let Some((p, _)) = self.instances.iter().find(|(_, i)| *i == name0) {
                return match k {
                    0 => What::Name(Symbol::Instance(p.clone(), name0), false),
                    _ => {
                        let s = Some(format!("component {}", last_segment(p)));
                        What::Name(Symbol::Predicate(s, name2), false)
                    }
                };
            }
            let full = format!("{name0}.{name2}");
            if crate::functions::callable(&full) {
                return match k {
                    0 => What::Name(Symbol::Package(name0), false),
                    _ => What::Name(Symbol::Function(full), false),
                };
            }
        }
        // 1, 2: a `let`, a value name; an object input's field after it.
        if let Some(s) = self.let_(at, &name0).or_else(|| self.value(at, &name0)) {
            if k == 0 {
                return What::Name(s, false);
            }
            if let Symbol::Value(scope, input) = &s {
                let field: Vec<String> = (2..=k)
                    .step_by(2)
                    .map_while(|i| name_at(i).filter(|_| dot(i - 1)))
                    .collect();
                let field = field.join(".");
                if field.split('.').count() == k / 2
                    && self
                        .fields
                        .contains(&(scope.clone(), input.clone(), field.clone()))
                {
                    return What::Name(Symbol::Field(scope.clone(), input.clone(), field), false);
                }
            }
            return What::Path;
        }
        // A refinement of an object input's field reads the field,
        // `count: int check count >= 1`.
        if let Some(attr) = c
            .ancestors()
            .find(|a| a.kind() == SyntaxKind::REFINEMENT)
            .and_then(|r| r.parent())
            .filter(|a| a.kind() == SyntaxKind::ATTR_DECL)
            && let Some(input) = attr.ancestors().find(|a| a.kind() == SyntaxKind::INPUT)
            && let Some(i) = declared_name(&input)
            && let Some(path) = attr.children().find(|x| x.kind() == SyntaxKind::BLOCK_PATH)
            && let Some(last) = idents(&path).pop()
            && last.text() == name0
        {
            let at = input.parent().unwrap_or_else(|| input.clone());
            let s = self.scopes(&at).remove(0);
            return match k {
                0 => What::Name(
                    Symbol::Field(s, i.text().to_string(), field_path(&attr, &path, &last)),
                    false,
                ),
                _ => What::Path,
            };
        }
        // 3: `world.T[e]`.
        if name0 == "world" {
            return match k {
                0 => What::Other,
                _ => self.typed(&ps, 2, k).unwrap_or(What::Path),
            };
        }
        // 4: a resource in scope: its reference or what a dot reads. A
        // module's item or a copy's output reads the module or the copy
        // though a resource has the name (R-76).
        let instance = self.instances.iter().find(|(_, i)| *i == name0);
        let rs = self.resources_at(at, &name0);
        // In a module's or a component's body its own resource wins over
        // what its user's scope brings in (R-101).
        let own = matches!(rs.first(), Some(Symbol::Resource(Some(_), _, _)));
        let reads_other = !(own && !self.binds_here(at, &name0))
            && dot(1)
            && name_at(2).is_some_and(|x| {
                let of = match (instance, self.uses.get(&name0)) {
                    (Some((p, _)), _) => format!("component {}", last_segment(p)),
                    (_, Some(m)) => format!("module {m}"),
                    _ => return false,
                };
                self.has_item(&of, &x)
            });
        if !rs.is_empty() && !index(1) && !reads_other {
            return if k == 0 { self.pick(c, rs) } else { What::Path };
        }
        // 5: a copy, `n.k`; a used module's item or a stack's deployment,
        // `m.x`, `s[k=v].out`; a component's copies, `c[e].k`, or by the
        // path of its file, `net.c[e].k`.
        if dot(1)
            && let Some((p, _)) = instance
        {
            return match k {
                0 => What::Name(Symbol::Instance(p.clone(), name0), false),
                2 => self.item(
                    &format!("component {}", last_segment(p)),
                    &name_at(2).unwrap(),
                    call,
                ),
                _ => What::Path,
            };
        }
        if let Some(path) = self.uses.get(&name0) {
            let j = if index(1) { 3 } else { 2 };
            if k == 0 {
                return What::Name(Symbol::Module(name0), false);
            }
            if k == j
                && dot(j - 1)
                && let Some(x) = name_at(j)
            {
                return self.item(&format!("module {path}"), &x, call);
            }
            return self.component_read(&ps, j, k).unwrap_or(What::Path);
        }
        if self.components.contains(&name0) && index(1) {
            return self.component_read(&ps, 0, k).unwrap_or_else(|| after(1));
        }
        if self
            .files
            .keys()
            .any(|f| f == &name0 || f.starts_with(&format!("{name0}.")))
        {
            // The path of a file, then a component of it.
            let mut prefix = String::new();
            for (i, p) in ps.iter().enumerate() {
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
                    if i == k {
                        return What::Name(Symbol::File(prefix), false);
                    }
                } else if self.components.contains(x.text()) {
                    return self.component_read(&ps, i, k).unwrap_or(What::Path);
                } else if i == k {
                    return What::Other;
                }
            }
        }
        // 6: a relation's `p[..]`; a type, `T[e]`, `T.n`.
        if index(1) && self.predicates.contains(&name0) {
            return if k == 0 {
                What::Name(self.predicate(at, &name0), false)
            } else {
                What::Path
            };
        }
        if let Some(w) = self.typed(&ps, 0, k) {
            return w;
        }
        // 7: a variable; a dot on it is a path.
        if k == 0 { What::Variable } else { What::Path }
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
            let rs: Vec<Symbol> = self
                .resources
                .iter()
                .filter(|((_, x), ts)| x == n.text() && ts.contains(&typ))
                .map(|((s, x), _)| Symbol::Resource(s.clone(), typ.clone(), x.clone()))
                .collect();
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

    /// A component's copies read at part `at` (`c[e].k`): the component,
    /// then its output after the index.
    fn component_read(&self, ps: &[Part], at: usize, k: usize) -> Option<What> {
        let Some(Part::Name(c)) = ps.get(at) else {
            return None;
        };
        let c = c.text().to_string();
        if !self.components.contains(&c) {
            return None;
        }
        if k == at {
            return Some(What::Name(Symbol::Module(c), false));
        }
        let o = at + 3;
        if !matches!(ps.get(at + 1), Some(Part::Index))
            || !matches!(ps.get(at + 2), Some(Part::Dot))
        {
            return None;
        }
        match ps.get(o) {
            Some(Part::Name(x)) if k == o => {
                Some(self.item(&format!("component {c}"), x.text(), false))
            }
            _ => Some(What::Path),
        }
    }

    /// Whether the scope `s` declares an item `x` its user reads.
    fn has_item(&self, s: &str, x: &str) -> bool {
        let key = (Some(s.to_string()), x.to_string());
        self.defined.contains(&key)
            || self.outputs.contains(&key)
            || self.lets.contains(&key)
            || self.values.contains(&key)
            || self.resources.contains_key(&key)
    }

    /// The item `x` of the scope `s` (a component's or a module file's),
    /// read from outside it: a relation it defines, an output, a `let`, a
    /// value, a resource, a component of a module.
    fn item(&self, s: &str, x: &str, call: bool) -> What {
        let s = Some(s.to_string());
        let key = (s.clone(), x.to_string());
        let found = if self.defined.contains(&key) || call {
            Some(Symbol::Predicate(s.clone(), x.to_string()))
        } else if self.outputs.contains(&key) {
            Some(Symbol::Output(s.clone(), x.to_string()))
        } else if self.lets.contains(&key) {
            Some(Symbol::Let(s.clone(), x.to_string()))
        } else if self.values.contains(&key) {
            Some(Symbol::Value(s.clone(), x.to_string()))
        } else if let Some(ts) = self.resources.get(&key) {
            return self.pick_first(
                ts.iter()
                    .map(|t| Symbol::Resource(s.clone(), t.clone(), x.to_string()))
                    .collect(),
            );
        } else if self.components.contains(x) {
            Some(Symbol::Module(x.to_string()))
        } else {
            None
        };
        found.map_or(What::Path, |s| What::Name(s, false))
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
                    && let Some(f) = self.uses.get(m).and_then(|p| self.files.get(p))
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

/// Whether `name` is a relation of dform's own, which no program
/// declares: the compiler's, the engine's or a provider's (`deformation`,
/// `drift`), an aggregate, a data source (`csv`, `yaml`).
pub fn is_builtin_relation(name: &str) -> bool {
    crate::loader::is_core_pred(name)
        || crate::engine::reference(name, true).is_some()
        || crate::tables::FORMATS.contains(&name)
}

/// The signature line of a function or a package in `std/*.df`.
pub fn std_site(sym: &Symbol) -> Option<Site<'static>> {
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
pub fn token_at(root: &SyntaxNode, at: usize) -> Option<SyntaxToken> {
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

/// The innermost component `node` is in (itself included).
pub fn component_scope(node: &SyntaxNode) -> Scope {
    node.ancestors().find_map(|a| {
        (a.kind() == SyntaxKind::COMPONENT)
            .then(|| Some(format!("component {}", declared_name(&a)?.text())))?
    })
}

fn last_segment(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
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
let cfg = { a: env }
component network {
  input vpc_net: inet
  # its interface: an input, a resource and an output
  resource net.vpc vpc {
    cidr = vpc_net
    tags = { env: env }
  }
  output vpc: net.vpc = vpc
  q(x) where zone_index[x] = 1, vpc.cidr == x
}
instance network main { vpc_net = inet(cfg.a) }
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
                "use config\nresource k8s.secret config {}\ninstance db.pg app { name = \"a\" }\n\
                 resource k8s.secret app {}\n\
                 let x = config.base_domain\nlet y = app.conn\nlet z = \"${config.base_domain}\"\n",
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
            item(Symbol::Output(Some("component pg".into()), "conn".into()))
        );
        assert_eq!(
            what("config {}", 0, 0),
            What::Name(
                Symbol::Resource(None, "k8s.secret".into(), "config".into()),
                true
            )
        );
        // The let's references: its declaration, the read and the hole's.
        let sym = Symbol::Let(Some("module config".into()), "base_domain".into());
        assert_eq!(d.occurrences(&files, &sym).len(), 3);
    }
}

//! Scopes as data (R-211): the tree of definitions the program's names
//! are written in, and what each declares, by name. A scope is a
//! definition, which is static: a copy of a component is a declaration in
//! its user's scope pointing at the component, never a scope of its own,
//! and every copy of one component shares that component's scope. The
//! front end builds the tree once, before it lowers a statement
//! (`syntax::resolve`'s `collect`); name resolution walks it.
//!
//! The tree: the program's scope; an entry file's top level inside it
//! (what it declares is the program's); a module's file with none around
//! it (a module never reads its user's names, R-205); a component inside
//! the scope its block is written in.

use super::node::{ItemId, Name};
use crate::syntax::SyntaxNode;
use slotmap::{SecondaryMap, SlotMap};
use std::collections::BTreeMap;

slotmap::new_key_type! {
    /// A [`Scope`].
    pub struct ScopeId;
}

#[derive(Debug, Clone)]
pub struct Scope {
    /// The enclosing scope; none for the program's and a module's file.
    pub parent: Option<ScopeId>,
    pub kind: ScopeKind,
}

impl Scope {
    pub fn new(parent: Option<ScopeId>, kind: ScopeKind) -> Scope {
        Scope { parent, kind }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeKind {
    /// The program's: what its entry files declare.
    Program,
    /// An entry file's top level (`diag` source id).
    File(u32),
    /// A module's file, by its path (`modules.net`).
    Module { path: Name },
    /// `component NAME { .. }`, by its path (`modules.net.vpc`).
    Component { path: Name },
    /// A `let` with parameters: its parameters.
    LetParams(ItemId),
}

/// The program's scopes: the tree, and its indexes by where a scope is
/// written.
#[derive(Debug, Default)]
pub struct Scopes {
    tree: SlotMap<ScopeId, Scope>,
    /// What each scope declares.
    pub names: SecondaryMap<ScopeId, Names>,
    /// The program's scope, every entry file's outermost.
    pub root: ScopeId,
    /// Each file's top level, by `diag` source id.
    files: BTreeMap<u32, ScopeId>,
    /// Each component's body, by its statement's file and offset.
    blocks: BTreeMap<(u32, u32), ScopeId>,
    /// Each module's file and each component's body, by path.
    paths: BTreeMap<Name, ScopeId>,
}

impl std::ops::Index<ScopeId> for Scopes {
    type Output = Scope;

    fn index(&self, s: ScopeId) -> &Scope {
        &self.tree[s]
    }
}

impl std::ops::IndexMut<ScopeId> for Scopes {
    fn index_mut(&mut self, s: ScopeId) -> &mut Scope {
        &mut self.tree[s]
    }
}

impl Scopes {
    /// The program's scope and nothing in it.
    pub fn new() -> Scopes {
        let mut scopes = Scopes::default();
        scopes.root = scopes.insert(None, ScopeKind::Program);
        scopes
    }

    /// A new scope, declaring nothing yet.
    fn insert(&mut self, parent: Option<ScopeId>, kind: ScopeKind) -> ScopeId {
        let s = self.tree.insert(Scope::new(parent, kind));
        self.names.insert(s, Names::default());
        s
    }

    /// The top level of `file`: an entry file's inside the program's, a
    /// module's file, at `path`, with none around it (R-205).
    pub fn file(&mut self, file: u32, module: Option<&str>) -> ScopeId {
        let scope = match module {
            Some(path) => {
                let kind = ScopeKind::Module { path: path.into() };
                let s = self.insert(None, kind);
                self.paths.insert(path.into(), s);
                s
            }
            None => self.insert(Some(self.root), ScopeKind::File(file)),
        };
        self.files.insert(file, scope);
        scope
    }

    /// The body of the component at `path` whose statement is at `offset`
    /// in `file`, written in `parent`.
    pub fn component(&mut self, file: u32, offset: u32, parent: ScopeId, path: &str) -> ScopeId {
        let kind = ScopeKind::Component { path: path.into() };
        let s = self.insert(Some(parent), kind);
        self.blocks.insert((file, offset), s);
        self.paths.insert(path.into(), s);
        s
    }

    /// The top level of `file`.
    pub fn of_file(&self, file: u32) -> Option<ScopeId> {
        self.files.get(&file).copied()
    }

    /// The file whose top level `s` is.
    pub fn file_of(&self, s: ScopeId) -> Option<u32> {
        self.files.iter().find(|(_, f)| **f == s).map(|(f, _)| *f)
    }

    /// The body of the component whose statement is at `offset` in `file`.
    pub fn of_block(&self, file: u32, offset: u32) -> Option<ScopeId> {
        self.blocks.get(&(file, offset)).copied()
    }

    /// The module's file or the component's body at `path`.
    pub fn by_path(&self, path: &str) -> Option<ScopeId> {
        self.paths.get(path).copied()
    }

    /// Every module and component, by path.
    pub fn paths(&self) -> impl Iterator<Item = (&Name, ScopeId)> {
        self.paths.iter().map(|(p, s)| (p, *s))
    }

    /// Whether any scope declares the relation `name`: a head, a `decl`,
    /// a `let` or an input relation.
    pub fn declares_relation(&self, name: &str) -> bool {
        self.names.values().any(|n| {
            n.of(name).iter().any(|d| {
                d.kind.arity().is_some() || matches!(d.kind, DeclKind::RelationInput { .. })
            })
        })
    }

    /// Every scope, in the order they were made.
    pub fn ids(&self) -> impl Iterator<Item = ScopeId> + '_ {
        self.tree.keys()
    }

    /// The module path `s` is the body of, and whether it is a
    /// component's; `None` for any other scope.
    pub fn definition(&self, s: ScopeId) -> Option<(&Name, bool)> {
        match &self.tree[s].kind {
            ScopeKind::Module { path } => Some((path, false)),
            ScopeKind::Component { path } => Some((path, true)),
            _ => None,
        }
    }

    /// Whether `s` is a component's body.
    pub fn is_component(&self, s: ScopeId) -> bool {
        matches!(self.tree[s].kind, ScopeKind::Component { .. })
    }

    /// Whether `s` is an entry file's top level.
    pub fn is_entry(&self, s: ScopeId) -> bool {
        matches!(self.tree[s].kind, ScopeKind::File(_))
    }

    /// `s` and the scopes around it, innermost first.
    pub fn chain(&self, s: ScopeId) -> Vec<ScopeId> {
        let mut out = vec![s];
        let mut at = s;
        while let Some(p) = self.tree[at].parent {
            out.push(p);
            at = p;
        }
        out
    }

    /// The scopes from `s` out to the body of the component or module it
    /// is in, that body's included: what is the body's own. At the
    /// program's top level, every scope.
    pub fn own(&self, s: ScopeId) -> Vec<ScopeId> {
        let mut out = Vec::new();
        for at in self.chain(s) {
            out.push(at);
            if self.definition(at).is_some() {
                break;
            }
        }
        out
    }

    /// The body `s` is in: a component's or a module's, else the
    /// program's.
    pub fn body(&self, s: ScopeId) -> ScopeId {
        self.own(s).last().copied().unwrap_or(self.root)
    }

    /// The scope a statement written in `s` declares into: an entry
    /// file's top level declares into the program's.
    pub fn declaring(&self, s: ScopeId) -> ScopeId {
        match self.is_entry(s) {
            true => self.root,
            false => s,
        }
    }

    /// The scope around the component body `body` (what `super` names,
    /// R-186): the program's for a component of an entry file.
    pub fn around(&self, body: ScopeId) -> ScopeId {
        let parent = self.tree[body].parent.unwrap_or(self.root);
        self.declaring(parent)
    }

    /// The module whose file `s` is in: the module itself, or a component
    /// of it. `None` in an entry file.
    pub fn module_of(&self, s: ScopeId) -> Option<&Name> {
        let file = *self.chain(s).last()?;
        match &self.tree[file].kind {
            ScopeKind::Module { path } => Some(path),
            _ => None,
        }
    }

    /// A scope as a message names it: `module backups`, `component
    /// backups.volume`, `the stack`.
    pub fn describe(&self, s: ScopeId) -> String {
        match self.definition(s) {
            Some((p, true)) => format!("component {p}"),
            Some((p, false)) => format!("module {p}"),
            None => "the stack".to_string(),
        }
    }

    /// The definition whose body `declared` is, when a read in `s`
    /// reaches it past the body `s` is in (R-186): a component reads its
    /// module's items bare, and an enclosing component's, as a Rust `fn`
    /// reads its module's; expansion makes the read the instance's the
    /// copy was taken from (`modules::lexical_pred`). The program's,
    /// `""`, read in a component: no copy around it takes the name.
    /// `None` for the body's own, and for the program's read in a
    /// module's body, which reads its user's.
    pub fn lexical(&self, s: ScopeId, declared: ScopeId) -> Option<String> {
        let own = self.own(s);
        if own.contains(&declared) {
            return None;
        }
        if declared == self.root {
            let in_component = own.last().is_some_and(|b| self.is_component(*b));
            return in_component.then(String::new);
        }
        self.definition(declared).map(|(p, _)| p.clone())
    }
}

/// What a scope declares, by name: every declaration of each, in the
/// order the front end met them (a module's `use`s and the program's
/// rules as `collect` walks the file, copies and resources once every
/// `use` is known). Iteration is by name.
#[derive(Debug, Clone, Default)]
pub struct Names {
    decls: BTreeMap<Name, Vec<Decl>>,
}

/// One declaration of a name: what it is, and the statement that
/// declares it, which the front end reads its parts from (a `let`'s rows,
/// an input's type, a `decl`'s columns).
#[derive(Debug, Clone)]
pub struct Decl {
    pub kind: DeclKind,
    pub node: SyntaxNode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclKind {
    /// `input k: T`: a value read bare.
    Input,
    /// A row of `let k = v`: a value read bare, a relation of one column.
    Let,
    /// `let f(a, b) = v` (R-187): a relation of its parameters' columns and
    /// its value's.
    LetFn { arity: usize },
    /// `resource T n` with a static name (R-76): of the type `T`.
    Resource(Name),
    /// `resource C n`, `C` a component (R-113): the component's path as
    /// written, as resolved (`None`: it names no component, and the copy
    /// is the error), and whether it binds the name (a copy named by its
    /// clause binds none, R-191).
    Copy {
        written: Name,
        component: Option<Name>,
        binds: bool,
    },
    /// `use m [as n]`: the module's path as written, then as resolved.
    Use { written: Name, module: Name },
    /// `use s [as n]` of a stack the tool deploys: its index in the
    /// program's deployed stacks.
    Stack(usize),
    /// `component n`: its path.
    Component(Name),
    /// A component copied here, by its last segment (`resource postgres
    /// db` makes `postgres[t]` readable): its path.
    Copied(Name),
    /// A rule's or a fact's head, of this many columns.
    Rule { arity: usize },
    /// `decl p(..)`, of this many columns (R-55).
    Decl { arity: usize },
    /// `input p from ..`, or `input p`, whose rows the module's user
    /// gives (R-55).
    RelationInput { given: bool },
    /// `output p` alone: a relation exported (R-55).
    RelationOutput,
    /// `output k [: T]`: `T` when it is a resource type, and as written.
    Output {
        typ: Option<Name>,
        written: Option<String>,
    },
    /// `type T = component { .. }` (R-104), in the file `file`.
    Signature { file: u32 },
    /// `type T = TYPE`: an alias, by the front end's number for it.
    Alias(usize),
}

impl DeclKind {
    /// A value read bare: an input or a `let`.
    pub fn is_value(&self) -> bool {
        matches!(self, DeclKind::Input | DeclKind::Let)
    }

    /// The columns of the relation it declares, if it declares one.
    pub fn arity(&self) -> Option<usize> {
        match self {
            DeclKind::Let => Some(1),
            DeclKind::LetFn { arity } | DeclKind::Rule { arity } | DeclKind::Decl { arity } => {
                Some(*arity)
            }
            _ => None,
        }
    }

    /// Whether the scope's own statements give the relation its rows.
    pub fn defines(&self) -> bool {
        matches!(
            self,
            DeclKind::LetFn { .. } | DeclKind::Rule { .. } | DeclKind::RelationInput { .. }
        )
    }
}

impl Names {
    /// `name` declared by `node` as `kind`.
    pub fn declare(&mut self, name: impl Into<Name>, kind: DeclKind, node: SyntaxNode) {
        let decl = Decl { kind, node };
        self.decls.entry(name.into()).or_default().push(decl);
    }

    /// Every declaration of `name`, in order.
    pub fn of(&self, name: &str) -> &[Decl] {
        self.decls.get(name).map_or(&[], Vec::as_slice)
    }

    /// Every declaration of `name`, mutably (the front end resolves a
    /// `use`'s and a copy's paths once every scope's names are known).
    pub fn of_mut(&mut self, name: &str) -> &mut [Decl] {
        self.decls.get_mut(name).map_or(&mut [], Vec::as_mut_slice)
    }

    /// Every name and its declarations, by name.
    pub fn iter(&self) -> impl Iterator<Item = (&Name, &[Decl])> {
        self.decls.iter().map(|(n, d)| (n, d.as_slice()))
    }

    /// The names some declaration of which `pick` takes, in order.
    pub fn names(&self, pick: impl Fn(&DeclKind) -> bool) -> impl Iterator<Item = &Name> {
        self.decls
            .iter()
            .filter(move |(_, ds)| ds.iter().any(|d| pick(&d.kind)))
            .map(|(n, _)| n)
    }

    fn first(&self, name: &str, pick: impl Fn(&DeclKind) -> bool) -> Option<&Decl> {
        self.of(name).iter().find(|d| pick(&d.kind))
    }

    fn last(&self, name: &str, pick: impl Fn(&DeclKind) -> bool) -> Option<&Decl> {
        self.of(name).iter().rev().find(|d| pick(&d.kind))
    }

    /// Whether `name` is a value read bare here: an input or a `let`.
    pub fn is_value(&self, name: &str) -> bool {
        self.first(name, DeclKind::is_value).is_some()
    }

    /// The last `input name` here.
    pub fn input(&self, name: &str) -> Option<&SyntaxNode> {
        self.last(name, |k| *k == DeclKind::Input).map(|d| &d.node)
    }

    /// The `let name` statements here, in order.
    pub fn lets(&self, name: &str) -> impl Iterator<Item = &SyntaxNode> {
        self.of(name)
            .iter()
            .filter(|d| d.kind == DeclKind::Let)
            .map(|d| &d.node)
    }

    /// The first `let name(..)` here.
    pub fn function(&self, name: &str) -> Option<&SyntaxNode> {
        let fun = |k: &DeclKind| matches!(k, DeclKind::LetFn { .. });
        self.first(name, fun).map(|d| &d.node)
    }

    /// The types of the resources `name` here, one per declaration.
    pub fn resources(&self, name: &str) -> Option<Vec<Name>> {
        let types: Vec<Name> = self
            .of(name)
            .iter()
            .filter_map(|d| match &d.kind {
                DeclKind::Resource(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        (!types.is_empty()).then_some(types)
    }

    /// The first copy `name` here.
    fn copy(&self, name: &str) -> Option<&Decl> {
        self.first(name, |k| matches!(k, DeclKind::Copy { .. }))
    }

    /// The component the copy `name` here is of, when it names one.
    pub fn instance(&self, name: &str) -> Option<&Name> {
        match &self.copy(name)?.kind {
            DeclKind::Copy { component, .. } => component.as_ref(),
            _ => None,
        }
    }

    /// The copies here whose component is resolved: name and component.
    pub fn instances(&self) -> impl Iterator<Item = (&Name, &Name)> {
        self.decls
            .keys()
            .filter_map(|n| self.instance(n).map(|c| (n, c)))
    }

    /// Whether the copy `name` here names no component: its statement is
    /// the error.
    pub fn unbound(&self, name: &str) -> bool {
        self.copy(name).is_some_and(|d| {
            matches!(
                d.kind,
                DeclKind::Copy {
                    component: None,
                    ..
                }
            )
        })
    }

    /// The component `name` declares here: its path.
    pub fn component(&self, name: &str) -> Option<&Name> {
        let decl = self.last(name, |k| matches!(k, DeclKind::Component(_)))?;
        match &decl.kind {
            DeclKind::Component(p) => Some(p),
            _ => None,
        }
    }

    /// The component copied here whose last segment is `name`: its path.
    pub fn copied(&self, name: &str) -> Option<&Name> {
        let decl = self.first(name, |k| matches!(k, DeclKind::Copied(_)))?;
        match &decl.kind {
            DeclKind::Copied(p) => Some(p),
            _ => None,
        }
    }

    /// The module the first `use` of `name` here binds: its path.
    pub fn module(&self, name: &str) -> Option<&Name> {
        let decl = self.first(name, |k| matches!(k, DeclKind::Use { .. }))?;
        match &decl.kind {
            DeclKind::Use { module, .. } => Some(module),
            _ => None,
        }
    }

    /// The modules `use`d here: the name each binds and its path.
    pub fn uses(&self) -> impl Iterator<Item = (&Name, &Name)> {
        self.decls
            .keys()
            .filter_map(|n| self.module(n).map(|m| (n, m)))
    }

    /// The `use`s and the copies that bind `name` here, in order: several
    /// are guarded declarations (R-104).
    pub fn bound(&self, name: &str) -> impl Iterator<Item = &Decl> {
        self.of(name).iter().filter(|d| {
            matches!(
                d.kind,
                DeclKind::Use { .. } | DeclKind::Copy { binds: true, .. }
            )
        })
    }

    /// The stack the last `use` of `name` here binds: its index.
    pub fn stack(&self, name: &str) -> Option<usize> {
        let decl = self.last(name, |k| matches!(k, DeclKind::Stack(_)))?;
        match decl.kind {
            DeclKind::Stack(i) => Some(i),
            _ => None,
        }
    }

    /// The stacks `use`d here, by index.
    pub fn stacks(&self) -> impl Iterator<Item = usize> + '_ {
        self.decls.keys().filter_map(|n| self.stack(n))
    }

    /// Whether `name` is an output here, and the resource type it holds
    /// when it holds one: the last declaration's that says one.
    pub fn output(&self, name: &str) -> Option<Option<&Name>> {
        let mut outputs = self.of(name).iter().filter_map(|d| match &d.kind {
            DeclKind::Output { typ, .. } => Some(typ.as_ref()),
            _ => None,
        });
        let first = outputs.next()?;
        Some(outputs.fold(first, |typ, t| t.or(typ)))
    }

    /// The output `name`'s type as the last declaration writing one
    /// writes it (R-104).
    pub fn output_type(&self, name: &str) -> Option<&String> {
        self.of(name).iter().rev().find_map(|d| match &d.kind {
            DeclKind::Output { written, .. } => written.as_ref(),
            _ => None,
        })
    }

    /// The last component signature `name` here: its file and statement.
    pub fn signature(&self, name: &str) -> Option<(u32, &SyntaxNode)> {
        let decl = self.last(name, |k| matches!(k, DeclKind::Signature { .. }))?;
        match decl.kind {
            DeclKind::Signature { file } => Some((file, &decl.node)),
            _ => None,
        }
    }

    /// The columns the relation `name` has here, one per arity its heads
    /// and declarations give it.
    pub fn arities(&self, name: &str) -> std::collections::BTreeSet<usize> {
        self.of(name)
            .iter()
            .filter_map(|d| d.kind.arity())
            .collect()
    }

    /// The type aliases `name` here, by number.
    pub fn aliases(&self, name: &str) -> impl Iterator<Item = usize> + '_ {
        self.of(name).iter().filter_map(|d| match d.kind {
            DeclKind::Alias(id) => Some(id),
            _ => None,
        })
    }

    /// Whether `name` is a relation here: a head, a `decl` or a `let`.
    pub fn is_relation(&self, name: &str) -> bool {
        self.first(name, |k| k.arity().is_some()).is_some()
    }

    /// The first `decl name(..)` here.
    pub fn decl(&self, name: &str) -> Option<&SyntaxNode> {
        let decl = |k: &DeclKind| matches!(k, DeclKind::Decl { .. });
        self.first(name, decl).map(|d| &d.node)
    }

    /// Every relation `decl`ared here and its first `decl`, by name.
    pub fn decls(&self) -> impl Iterator<Item = (&Name, &SyntaxNode)> {
        self.decls
            .keys()
            .filter_map(|n| self.decl(n).map(|d| (n, d)))
    }

    /// Whether the module's user gives the relation `name` here (`input
    /// p`, R-55).
    pub fn takes(&self, name: &str) -> bool {
        let given = |k: &DeclKind| matches!(k, DeclKind::RelationInput { given: true });
        self.first(name, given).is_some()
    }

    /// Whether `output name` exports the relation here (R-55).
    pub fn exports(&self, name: &str) -> bool {
        self.first(name, |k| *k == DeclKind::RelationOutput)
            .is_some()
    }

    /// Whether this scope's own statements give the relation `name` rows.
    pub fn defines(&self, name: &str) -> bool {
        self.first(name, DeclKind::defines).is_some()
    }
}

/// A read's declaration: the scope that declares the name and which of its
/// declarations (`at`, R-104).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclRef {
    pub scope: ScopeId,
    pub name: Name,
    pub at: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_component_reads_its_module_lexically_and_a_module_nothing_around_it() {
        let mut s = Scopes::new();
        let stack = s.file(0, None);
        let net = s.file(1, Some("net"));
        let vpc = s.component(1, 10, net, "net.vpc");
        let top = s.component(0, 5, stack, "web");
        assert_eq!(s.chain(vpc), vec![vpc, net]);
        assert_eq!(s.chain(net), vec![net]);
        assert_eq!(s.chain(top), vec![top, stack, s.root]);
        assert_eq!(s.own(stack), vec![stack, s.root]);
        assert_eq!(s.body(vpc), vpc);
        assert_eq!(s.lexical(vpc, net), Some("net".to_string()));
        assert_eq!(s.lexical(top, s.root), Some(String::new()));
        assert_eq!(s.lexical(net, net), None);
        assert_eq!(s.around(top), s.root);
        assert_eq!(s.around(vpc), net);
        assert_eq!(s.module_of(vpc).map(String::as_str), Some("net"));
        assert_eq!(s.module_of(top), None);
        assert_eq!(s.describe(vpc), "component net.vpc");
        assert_eq!(s.by_path("net.vpc"), Some(vpc));
        assert_eq!(s.of_block(1, 10), Some(vpc));
        assert_eq!(s.declaring(stack), s.root);
    }
}

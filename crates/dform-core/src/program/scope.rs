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
use crate::ast::Span;
use slotmap::SlotMap;
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
    /// What the scope declares, by name: several for a name declared under
    /// several guards (R-104) or a `let`'s rows.
    pub decls: BTreeMap<Name, Vec<Decl>>,
}

impl Scope {
    pub fn new(parent: Option<ScopeId>, kind: ScopeKind) -> Scope {
        Scope {
            parent,
            kind,
            decls: BTreeMap::new(),
        }
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
        let mut tree = SlotMap::with_key();
        let root = tree.insert(Scope::new(None, ScopeKind::Program));
        Scopes {
            tree,
            root,
            ..Scopes::default()
        }
    }

    /// The top level of `file`: an entry file's inside the program's, a
    /// module's file, at `path`, with none around it (R-205).
    pub fn file(&mut self, file: u32, module: Option<&str>) -> ScopeId {
        let scope = match module {
            Some(path) => {
                let kind = ScopeKind::Module { path: path.into() };
                let s = self.tree.insert(Scope::new(None, kind));
                self.paths.insert(path.into(), s);
                s
            }
            None => self
                .tree
                .insert(Scope::new(Some(self.root), ScopeKind::File(file))),
        };
        self.files.insert(file, scope);
        scope
    }

    /// The body of the component at `path` whose statement is at `offset`
    /// in `file`, written in `parent`.
    pub fn component(&mut self, file: u32, offset: u32, parent: ScopeId, path: &str) -> ScopeId {
        let kind = ScopeKind::Component { path: path.into() };
        let s = self.tree.insert(Scope::new(Some(parent), kind));
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

/// One declaration of a name.
#[derive(Debug, Clone)]
pub struct Decl {
    pub kind: DeclKind,
    pub item: ItemId,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclKind {
    Let,
    LetFn,
    Input,
    Key,
    /// A resource, of its type.
    Resource(Name),
    Copy,
    Use,
    Stack,
    Component,
    Relation {
        arity: usize,
    },
    RelationInput,
    RelationOutput,
    Output,
    Alias,
    Signature,
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

//! Scopes as data (R-211): what each definition declares, by name, and the
//! scope around it. A scope is a definition, which is static: a copy of a
//! component is a declaration in its user's scope pointing at the copy's
//! item, never a scope of its own, and every copy of one component shares
//! that component's scope. The front end builds them; nothing mutates
//! them afterwards.
//!
//! Day one (step 1) holds the program's scope and one per module and
//! component, with no declarations; the front end fills `decls` at step 5
//! and the scope pass resolves reads through them at step 6.

use super::node::{ItemId, Name};
use crate::ast::Span;
use std::collections::BTreeMap;

slotmap::new_key_type! {
    /// A [`Scope`].
    pub struct ScopeId;
}

#[derive(Debug, Clone)]
pub struct Scope {
    /// The enclosing scope; none for the program's.
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
    /// The program's: its entry files' top level.
    Program,
    /// A file's own (`diag` source id).
    File(u32),
    /// A module's file, by its path (`modules.net`).
    Module { path: Name },
    /// `component NAME { .. }`, by its path (`modules.net.vpc`).
    Component { path: Name },
    /// A `let` with parameters: its parameters.
    LetParams(ItemId),
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

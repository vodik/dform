//! The program (R-211): what the front end makes of the parse tree, as
//! nodes with spans in arenas keyed by id, its scopes as data, lowered to
//! the rules the engine runs by one function, [`lower`]. Passes are
//! functions from a program to a side table keyed by node id; none
//! rewrites a node. Output order is `roots` and each node's children,
//! never an arena's.
//!
//! The migration builds it one statement kind at a time (WORK.org R-211,
//! "War game"). The resolver pushes an item per statement as it walks,
//! in order: a statement not yet ported is an [`ItemKind::Opaque`] item
//! holding what it lowered to, a module's or a component's statements are
//! a [`ItemKind::Module`] item's; `lower` gives the resolver's output
//! back, and [`check`] compares the two.

use crate::ast::{self, Span, Stmt};
use crate::diag::Diagnostic;
use slotmap::SlotMap;

pub mod build;
pub mod check;
pub mod lower;
pub mod node;
pub mod origin;
pub mod scope;
pub mod spell;
pub mod types;

pub use build::Builder;
pub use lower::{LoweredStack, lower};
pub use node::{
    Clause, ClauseId, Expr, ExprId, Goal, GoalId, Item, ItemId, ItemKind, Pattern, PatternId, Var,
    VarId,
};
pub use origin::Origin;
pub use scope::{Scope, ScopeId, ScopeKind};
pub use spell::spell;

/// A program of one stack (or one text read on its own).
#[derive(Debug, Default)]
pub struct Program {
    pub items: SlotMap<ItemId, Item>,
    pub exprs: SlotMap<ExprId, Expr>,
    pub goals: SlotMap<GoalId, Goal>,
    pub patterns: SlotMap<PatternId, Pattern>,
    pub clauses: SlotMap<ClauseId, Clause>,
    pub vars: SlotMap<VarId, Var>,
    pub scopes: SlotMap<ScopeId, Scope>,
    /// The program's top-level items in lowering order: the only order
    /// anything iterates in.
    pub roots: Vec<ItemId>,
    /// The program's scope, every other scope's outermost.
    pub scope: ScopeId,
    /// The stack's settings, as the resolver reads them.
    pub stack: Option<ast::Config>,
    /// The helper numbers taken so far.
    pub helpers: Counters,
    /// What the build found wrong; a program with any lowers to them.
    pub diags: Vec<Diagnostic>,
}

/// How many of each numbered helper the build has taken: the one counter
/// the resolver and the builders take numbers from, so a ported `not { }`
/// or aggregate (its number stamped at build, `GoalKind::Not.helper`) and
/// an opaque statement's count as one sequence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// `__neg_N`.
    pub negs: usize,
    /// `__agg_N`.
    pub aggs: usize,
}

impl Counters {
    /// The next `__neg_N` number, taken.
    pub fn neg(&mut self) -> usize {
        self.negs += 1;
        self.negs - 1
    }

    /// The next `__agg_N` number, taken.
    pub fn agg(&mut self) -> usize {
        self.aggs += 1;
        self.aggs - 1
    }
}

/// Any node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeId {
    Item(ItemId),
    Expr(ExprId),
    Goal(GoalId),
    Pattern(PatternId),
    Clause(ClauseId),
}

impl Program {
    /// An empty program: its own scope and nothing in it.
    pub fn new() -> Program {
        let mut scopes = SlotMap::with_key();
        let scope = scopes.insert(Scope::new(None, ScopeKind::Program));
        Program {
            scopes,
            scope,
            ..Program::default()
        }
    }

    /// The item of a statement not yet ported: what the resolver lowered
    /// it to, in `scope`; none when it lowered to nothing (an alias, an
    /// error).
    pub fn opaque(&mut self, stmts: Vec<Stmt>, span: Span, scope: ScopeId) -> Option<ItemId> {
        if stmts.is_empty() {
            return None;
        }
        Some(self.items.insert(Item {
            span,
            scope,
            kind: ItemKind::Opaque(stmts),
        }))
    }

    /// The scope of a module's file or a component's body, inside `parent`.
    pub fn module_scope(&mut self, parent: ScopeId, path: &str, component: bool) -> ScopeId {
        let path = path.to_string();
        let kind = match component {
            true => ScopeKind::Component { path },
            false => ScopeKind::Module { path },
        };
        self.scopes.insert(Scope::new(Some(parent), kind))
    }

    /// The item of a module's file or a component: its items, in its
    /// scope `body` (a [`Program::module_scope`]).
    pub fn module(
        &mut self,
        path: String,
        body: ScopeId,
        items: Vec<ItemId>,
        span: Span,
    ) -> ItemId {
        let scope = self.scopes[body].parent.unwrap_or(self.scope);
        let component = matches!(self.scopes[body].kind, ScopeKind::Component { .. });
        self.items.insert(Item {
            span,
            scope,
            kind: ItemKind::Module {
                path,
                component,
                body,
                items,
                signature: None,
            },
        })
    }
}

#[cfg(test)]
impl Program {
    /// A program of `stmts`, each an opaque item of its own, a module's in
    /// a module item: what the resolver would build of them.
    pub fn of_statements(stmts: Vec<Stmt>) -> Program {
        let mut p = Program::new();
        let scope = p.scope;
        p.roots = stmts.into_iter().map(|s| p.item_of(s, scope)).collect();
        p
    }

    fn item_of(&mut self, stmt: Stmt, scope: ScopeId) -> ItemId {
        let Stmt::Module(m) = stmt else {
            let span = head_span(&stmt);
            return self.opaque(vec![stmt], span, scope).expect("a statement");
        };
        let body = self.module_scope(scope, &m.name, m.component);
        let items = m.body.into_iter().map(|s| self.item_of(s, body)).collect();
        self.module(m.name, body, items, m.span)
    }
}

/// Where a statement is: its head's span, or the statement's own.
pub fn head_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::Fact(a) => a.span,
        Stmt::Rule(r) => r.head.span,
        Stmt::Module(m) => m.span,
        Stmt::Instance(i) | Stmt::Use(i) => i.span,
        Stmt::Input(i) => i.span,
        Stmt::RelationInput(e) | Stmt::Extern(e) | Stmt::Mixed(e) | Stmt::Mode(e) => e.span,
        Stmt::Output(o) => o.span,
        Stmt::Provider(c) => c.span,
        Stmt::Resource(r) => r.span,
        Stmt::Decl(d) => d.span,
        Stmt::ExternFn(e) => e.span,
        Stmt::Pending(p) => p.span,
    }
}

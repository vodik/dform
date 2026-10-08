//! The program (R-211): what the front end makes of the parse tree, as
//! nodes with spans in arenas keyed by id, its scopes as data, lowered to
//! the rules the engine runs by one function, [`lower`]. Passes are
//! functions from a program to a side table keyed by node id; none
//! rewrites a node. Output order is `roots` and each node's children,
//! never an arena's.
//!
//! The migration builds it one statement kind at a time (WORK.org R-211,
//! "War game"). Day one every statement the resolver lowers is an
//! [`ItemKind::Opaque`] item holding what it lowered to, a module's or a
//! component's in a [`ItemKind::Module`] item; `lower` gives the
//! resolver's output back, and [`check`] compares the two.

use crate::ast::{self, Span, Stmt};
use crate::diag::Diagnostic;
use slotmap::SlotMap;

pub mod check;
pub mod lower;
pub mod node;
pub mod origin;
pub mod scope;
pub mod spell;
pub mod types;

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

/// How many of each numbered helper the build has taken: a ported `not
/// { }` or aggregate takes its number at build (`GoalKind::Not.helper`),
/// so ported and opaque statements count as one sequence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// `__neg_N`.
    pub negs: usize,
    /// `__agg_N`.
    pub aggs: usize,
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

    /// The program of what the resolver lowered, every statement opaque, a
    /// module or a component an item of its own in a scope of its own; on
    /// an error, a program of the diagnostics alone.
    pub fn opaque(lowered: Result<ast::Program, Vec<Diagnostic>>, helpers: Counters) -> Program {
        let mut p = Program {
            helpers,
            ..Program::new()
        };
        match lowered {
            Ok(ast::Program { statements, stack }) => {
                let scope = p.scope;
                let roots = statements
                    .into_iter()
                    .map(|s| p.opaque_item(s, scope))
                    .collect();
                p.roots = roots;
                p.stack = stack;
            }
            Err(diags) => p.diags = diags,
        }
        p
    }

    fn opaque_item(&mut self, stmt: Stmt, scope: ScopeId) -> ItemId {
        let Stmt::Module(m) = stmt else {
            let span = head_span(&stmt);
            return self.items.insert(Item {
                span,
                scope,
                kind: ItemKind::Opaque(Box::new(stmt)),
            });
        };
        let kind = match m.component {
            true => ScopeKind::Component {
                path: m.name.clone(),
            },
            false => ScopeKind::Module {
                path: m.name.clone(),
            },
        };
        let body = self.scopes.insert(Scope::new(Some(scope), kind));
        let items = m
            .body
            .into_iter()
            .map(|s| self.opaque_item(s, body))
            .collect();
        self.items.insert(Item {
            span: m.span,
            scope,
            kind: ItemKind::Module {
                path: m.name,
                component: m.component,
                body,
                items,
                signature: None,
            },
        })
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

//! The program (R-211): what the front end makes of the parse tree, as
//! nodes with spans in arenas keyed by id, its scopes as data, lowered to
//! the rules the engine runs by one function, [`lower`]. Passes are
//! functions from a program to a side table keyed by node id; none
//! rewrites a node. Output order is `roots` and each node's children,
//! never an arena's.
//!
//! The migration builds it one statement kind at a time (WORK.org R-211,
//! "War game"). The resolver pushes an item per statement as it walks,
//! in order, each of its kind, a module's or a component's statements a
//! [`ItemKind::Module`] item's; `lower` gives the resolver's output back,
//! and [`check`] compares the two.

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
pub use lower::{LoweredStack, lower, lower_clause, lower_expr, lower_goal, lower_item};
pub use node::{
    Clause, ClauseId, Expr, ExprId, ExprKind, Goal, GoalId, Item, ItemId, ItemKind, Pattern,
    PatternId, Var, VarId,
};
pub use origin::Origin;
pub use scope::{Scope, ScopeId, ScopeKind, Scopes};
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
    /// The scopes the program's names are written in, and what each
    /// declares.
    pub scopes: Scopes,
    /// The program's top-level items in lowering order: the only order
    /// anything iterates in.
    pub roots: Vec<ItemId>,
    /// The stack's settings, as the resolver reads them.
    pub stack: Option<ast::Config>,
    /// The helper numbers taken so far.
    pub helpers: Counters,
    /// What the build found wrong; a program with any lowers to them.
    pub diags: Vec<Diagnostic>,
    /// The helper statements the terms under a node made, which no node
    /// of the terms lowers to yet (a comprehension's `not { }` rule, a
    /// loader's extern): lowered before the node's own (step 5; gone when
    /// terms are built from the tree).
    pub terms_made: std::collections::BTreeMap<NodeId, Vec<Stmt>>,
}

/// What a read the front end makes, `p(.., V, ..)`, reads: hoisted, its
/// node binds `V`; in place, the literal holding it fills `V`'s column.
#[derive(Debug, Clone)]
pub enum Read {
    /// `k(V)`: a value, `Value`.
    Value(scope::DeclRef, scope::Reach),
    /// `attr("T", A, "p", V)`: a resource's attribute, `Field` of a
    /// `Resource`.
    Attr,
    /// `output(C, "k", V)`: a copy's output, `Output`.
    Output,
    /// `cloud_attr("T", A, "p", V)`: what the provider reports, `World`.
    World,
    /// `p(a, .., V, ..)`: a relation or an extern by its other columns,
    /// `V` its `out`th, `Lookup`.
    Lookup { out: usize },
}

/// How many of each numbered helper the build has taken: the one counter
/// the resolver and the builders take numbers from, in the order the
/// statements are walked: a `not { }` or an aggregate's number is stamped
/// at build (`GoalKind::Not.helper`), so `lower` picks none.
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
        Program {
            scopes: Scopes::new(),
            ..Program::default()
        }
    }

    /// The item `kind` at `span`, in `scope`.
    pub fn item(&mut self, span: Span, scope: ScopeId, kind: ItemKind) -> ItemId {
        self.items.insert(Item { span, scope, kind })
    }

    /// Whether `e` is a read a literal tests or binds in place (`R.p ==
    /// c`, `x = k`, `not R.ready`, `has n.k`), its value column the
    /// literal's: a read's node not in term position.
    pub fn in_place(&self, e: ExprId) -> bool {
        let e = &self.exprs[e];
        let read = match &e.kind {
            ExprKind::Value { .. }
            | ExprKind::Output { .. }
            | ExprKind::World { .. }
            | ExprKind::Lookup { .. } => true,
            ExprKind::Field { base, path } => matches!(
                (&self.exprs[*base].kind, path.as_slice()),
                (ExprKind::Resource { .. }, [node::Step::Field(..)])
            ),
            _ => false,
        };
        read && e.hoisted.is_none()
    }

    /// The item of a module's file or a component: its items, in its
    /// scope `body`.
    pub fn module(
        &mut self,
        path: String,
        body: ScopeId,
        items: Vec<ItemId>,
        span: Span,
    ) -> ItemId {
        let scope = self.scopes[body].parent.unwrap_or(self.scopes.root);
        let component = self.scopes.is_component(body);
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

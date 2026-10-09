//! The program's nodes (R-211): one family per kind of thing the program
//! writes, each in its own arena of [`super::Program`] under a key of its
//! own. A node is a span and what was written there; its type, how a
//! name in it resolved and where it is lowered from are side tables a
//! pass returns, keyed by the node's id, never fields of the node.
//!
//! Nodes are per definition: a copy, a `use` and a call site share the
//! nodes of what they copy, and `lower` applies each as an environment.
//! No pass rewrites a node.
//!
//! Day one (step 1) only [`ItemKind::Opaque`] and [`ItemKind::Module`] are
//! built; every other variant is named here with the step that first
//! builds it, so each port lands as nodes and not as a new shape.

use super::scope::{DeclRef, ScopeId};
use crate::ast::{self, FieldOp, Rank, Span, TypeExpr};
use crate::functions::Function;
use crate::ir::ops::AggKind;
use crate::value::Value;

slotmap::new_key_type! {
    /// A statement or a declaration: [`Item`].
    pub struct ItemId;
    /// A term: [`Expr`].
    pub struct ExprId;
    /// A literal of a clause: [`Goal`].
    pub struct GoalId;
    /// What a binding position matches: [`Pattern`].
    pub struct PatternId;
    /// A body, after `where` or inside `not { }`: [`Clause`].
    pub struct ClauseId;
    /// A variable of a clause: [`Var`].
    pub struct VarId;
}

/// A name as written. An interned symbol later, if it matters.
pub type Name = String;

/// A type as a statement names it: `net.vpc`, `k8s.deployment`, a
/// component's path. Resolved to its declaration by the scope pass
/// (step 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRef {
    pub name: Name,
    pub span: Span,
}

/// A relation as a statement names it: `zone`, `policy.allowed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelRef {
    pub name: Name,
    pub span: Span,
}

// --- expressions --------------------------------------------------------

/// A term.
#[derive(Debug, Clone)]
pub struct Expr {
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    /// What a file being typed has not written yet: the language server
    /// builds past an error (step 12).
    Missing,
    /// `1`, `"a"`, `true`, `10.0.0.0/8`: a literal of a known kind (step 3).
    Lit(Value),
    /// `500m`, `1Gi`: a quantity whose dimension its position decides
    /// (step 3, read at step 11).
    Quantity { text: String },
    /// `x`: a variable of the clause (step 3).
    Var(VarId),
    /// `k`, `m.k`: a `let`, an input or a key read by name (step 3).
    Value(DeclRef),
    /// `db`, `T[e]`, a variable `x in T` types: a resource (step 3).
    Resource { typ: TypeRef, addr: ExprId },
    /// `ref(R)` written out (step 3).
    RefOf(ExprId),
    /// `x.f[i]["k"].len`, `R.p.q`: a path into a value or a resource
    /// (step 3).
    Field { base: ExprId, path: Vec<Step> },
    /// `n.k`, `c[e].k`: a copy's output (step 3).
    Output { copy: ExprId, key: Name },
    /// `platform[env = e].x`: a deployment's output; a key's `bool` is a
    /// pun, `platform[env].x` (step 3).
    Deployed {
        stack: DeclRef,
        keys: Vec<(Name, ExprId, bool)>,
        out: Name,
    },
    /// `world.T[a].p`: what the provider reports (step 3).
    World {
        typ: TypeRef,
        addr: ExprId,
        path: Vec<Step>,
    },
    /// `settings.k` (step 3).
    Setting { key: Name },
    /// `p[a, b]`, `ext[a].f`: a relation or an extern read by its
    /// leading columns (step 3).
    Lookup {
        rel: RelRef,
        args: Vec<ExprId>,
        path: Vec<Step>,
    },
    /// `inet.subnet(c, 8)`, `f(x, y: 2)` (step 3).
    Call { callee: Callee, args: Vec<Arg> },
    /// `a + b`, `a * b` (step 3).
    Binary { op: BinOp, lhs: ExprId, rhs: ExprId },
    /// `-a` (step 3).
    Neg(ExprId),
    /// `"a-${i}"` (step 3).
    Interp { parts: Vec<Piece> },
    /// `{ a: 1, b, ..r }` (step 3).
    Object { parts: Vec<ObjPart> },
    /// `[a, ..xs]` (step 3).
    List { parts: Vec<ListPart> },
    /// `lo..hi`, `lo..=hi` (step 3).
    Range {
        lo: ExprId,
        hi: ExprId,
        inclusive: bool,
    },
    /// `[x.name | x in r]` (step 4).
    Comprehension { item: ExprId, clause: ClauseId },
    /// `count(x)`: its group is the enclosing clause's other variables
    /// (step 4).
    Aggregate { kind: AggKind, item: ExprId },
    /// `e as T` (step 3).
    As { value: ExprId, ty: TypeExpr },
    /// `Res::Type`: a type named as a value (step 3).
    Type(TypeRef),
}

/// One segment of a path, with the span of its name for the language
/// server's definition and rename.
#[derive(Debug, Clone)]
pub enum Step {
    /// `.f`.
    Field(Name, Span),
    /// `[0]`.
    Index(ExprId),
    /// `["k"]`, `[k]`.
    Key(ExprId),
    /// `[_]`: each element, its binding the variable.
    Each(VarId, Span),
    /// `.len`.
    Len,
}

/// What a call calls.
#[derive(Debug, Clone)]
pub enum Callee {
    /// A function of the standard library.
    Std(&'static Function),
    /// A `let` with parameters (R-187).
    Let(ItemId),
    /// An extern a provider answers.
    Extern(RelRef),
    /// A document's loader, `io.read(..)`.
    Loader(Name),
    /// A constructor read as data: a provider's or stack's setting
    /// (`local("DIR")`), a `type_refine` constraint.
    Data(Name),
}

/// An argument, `x` or `name: x`.
#[derive(Debug, Clone)]
pub struct Arg {
    pub name: Option<(Name, Span)>,
    pub value: ExprId,
}

/// A piece of an interpolated string.
#[derive(Debug, Clone)]
pub enum Piece {
    Text(String),
    Hole(ExprId),
}

/// A part of an object literal.
#[derive(Debug, Clone)]
pub enum ObjPart {
    /// `k: v`, or `k` alone (`pun`).
    Field {
        key: Name,
        key_span: Span,
        value: ExprId,
        pun: bool,
    },
    /// `..e`.
    Spread(ExprId),
}

/// A part of a list literal.
#[derive(Debug, Clone)]
pub enum ListPart {
    Elem(ExprId),
    /// `..e`.
    Spread(ExprId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

// --- patterns, clauses, goals -------------------------------------------

/// What a binding position matches (step 3).
#[derive(Debug, Clone)]
pub struct Pattern {
    pub span: Span,
    pub kind: PatternKind,
}

#[derive(Debug, Clone)]
pub enum PatternKind {
    /// `_`.
    Hole,
    /// `x`: binds it, or compares when it is bound (binding order, R-10,
    /// decides).
    Bind(VarId),
    /// A literal or a bound term.
    Expr(ExprId),
    /// `(a, b)`, `(a, ..r)`.
    Tuple {
        elems: Vec<PatternId>,
        rest: Option<VarId>,
    },
    /// `{ a, b: x, ..r }`.
    Object {
        fields: Vec<(Name, Span, PatternId)>,
        rest: Option<VarId>,
    },
}

/// A body: its literals in the order written (step 4).
#[derive(Debug, Clone)]
pub struct Clause {
    pub span: Span,
    pub goals: Vec<GoalId>,
}

/// One literal of a clause (step 4).
#[derive(Debug, Clone)]
pub struct Goal {
    pub span: Span,
    pub kind: GoalKind,
}

#[derive(Debug, Clone)]
pub enum GoalKind {
    /// `p(a, b)`, `p(a: x)`, `zone({ name })`: a relation the program
    /// wrote.
    Rel { rel: RelRef, args: RelArgs },
    /// `x in e`, `(k, v) in e`; `x not in e` is a `Not` around it.
    Member { pat: PatternId, coll: Coll },
    /// `x = e`, `(a, b) = e`, `{ a, ..r } = e`.
    Bind { pat: PatternId, value: ExprId },
    /// `a < b`, chained `1 <= x <= 35`.
    Compare {
        lhs: ExprId,
        ops: Vec<(CmpOp, ExprId)>,
    },
    /// `has x.p`.
    Has(ExprId),
    /// `x.ready`: a truth test.
    Truth(ExprId),
    /// `not B`, `not { B }`. `helper` is the `__neg_N` number taken at
    /// build, so ported and opaque statements count as one sequence.
    Not {
        clause: ClauseId,
        helper: Option<u32>,
    },
    /// `n = count(x)`: an aggregate's binding. `helper` is the `__agg_N`
    /// number taken at build.
    Fold {
        var: VarId,
        agg: ExprId,
        helper: Option<u32>,
    },
}

/// A relation's arguments: by position, or by column name.
#[derive(Debug, Clone)]
pub enum RelArgs {
    Positional(Vec<PatternId>),
    Record(Vec<(Name, PatternId)>),
}

/// What `in` ranges over.
#[derive(Debug, Clone)]
pub enum Coll {
    /// A list, an object, a range: `x in xs`.
    Expr(ExprId),
    /// The resources of a type: `x in net.vpc`.
    Type(TypeRef),
    /// A provider namespace's resources: `x in k8s`.
    Namespace(Name),
    /// A provider's type of a renamed provider: `x in ca.instance`.
    ProviderType(TypeRef),
    /// What the provider reports exists: `x in world.T`.
    World(TypeRef),
    /// An enum's members.
    Enum(TypeRef),
    /// A component's copies: `x in network`.
    Copies(DeclRef),
}

/// A variable of a clause: its source name, where it is first written,
/// and the item it belongs to. `implicit`: the compiler made it (`[_]`,
/// a fresh).
#[derive(Debug, Clone)]
pub struct Var {
    pub name: Name,
    pub first: Span,
    pub item: ItemId,
    pub implicit: bool,
}

// --- items ----------------------------------------------------------------

/// A statement, in the scope it is written in.
#[derive(Debug, Clone)]
pub struct Item {
    pub span: Span,
    pub scope: ScopeId,
    pub kind: ItemKind,
}

#[derive(Debug, Clone)]
pub enum ItemKind {
    /// A statement not yet ported: what the resolver lowered it to (the
    /// statement's own, then the helpers it made), which `lower` emits as
    /// it is. Every statement was one on day one; the migration shrinks it
    /// to nothing (step 5).
    Opaque(Vec<ast::Stmt>),
    /// A module's file (`modules.net`) or a `component NAME { .. }`: its
    /// items, in its own scope.
    Module {
        path: Name,
        component: bool,
        body: ScopeId,
        items: Vec<ItemId>,
        signature: Option<TypeExpr>,
    },
    /// `let k[: T] = v [@rank] [where B]` (step 3).
    Let {
        name: Name,
        ty: Option<TypeExpr>,
        value: ExprId,
        clause: Option<ClauseId>,
        rank: Option<Rank>,
    },
    /// `let f(a, b: T = d) = v [where B]` (R-187, step 5).
    LetFn {
        name: Name,
        params: Vec<Param>,
        value: ExprId,
        clause: Option<ClauseId>,
    },
    /// `p(a, b) [@rank] [where B]`: a fact without a clause (step 5).
    Rule {
        head: Head,
        clause: Option<ClauseId>,
        rank: Option<Rank>,
    },
    /// `deny "m" [{ .. }] [where B]`, `warn ..` (step 5).
    Check {
        kind: CheckKind,
        message: ExprId,
        detail: Option<ExprId>,
        clause: Option<ClauseId>,
    },
    /// `resource T n [@rank] { .. } [where B]`, `resource T n = v` (step 5).
    Resource {
        typ: TypeRef,
        name: Header,
        rank: Option<Rank>,
        body: ResourceBody,
        clause: Option<ClauseId>,
    },
    /// `set R.p = v`, `set k += v`, a line of `set { .. }` (`group`, the
    /// block's item) (step 5).
    Contribute {
        target: Target,
        op: FieldOp,
        value: ExprId,
        rank: Option<Rank>,
        clause: Option<ClauseId>,
        group: Option<ItemId>,
    },
    /// `set from doc [@rank] [where B]` (R-38, step 5).
    SetFrom {
        doc: ExprId,
        rank: Option<Rank>,
        clause: Option<ClauseId>,
    },
    /// `use m [as n] { .. }`, `resource C n { .. }`, a project module's
    /// deployment: a copy of a definition (step 5).
    Copy {
        kind: CopyKind,
        def: DeclRef,
        name: Header,
        inputs: Vec<(Name, Span, ExprId)>,
        rows: Vec<ItemId>,
        clause: Option<ClauseId>,
        via: Option<DeclRef>,
    },
    /// `input k: T [= d] [check B] [where G]`, `key k: T`, `input k { .. }`
    /// (step 5).
    Input {
        name: Name,
        key: bool,
        ty: TypeExpr,
        default: Option<ExprId>,
        refinement: Option<ClauseId>,
        guard: Option<ClauseId>,
        fields: Vec<ItemId>,
    },
    /// `input p [from src [where B]]`: a relation's rows (R-55, step 5).
    RelationInput {
        rel: RelRef,
        source: Option<ExprId>,
        clause: Option<ClauseId>,
    },
    /// `output k[: T] [= v] [where B]` (step 5).
    Output {
        name: Name,
        ty: Option<TypeExpr>,
        value: Option<ExprId>,
        clause: Option<ClauseId>,
    },
    /// `output p`: a relation exported, `true` per reference column
    /// (step 5).
    OutputRelation { rel: RelRef, ref_columns: Vec<bool> },
    /// `provider p [as n] { k = v }` (step 5).
    Provider {
        name: Name,
        of: Option<Name>,
        settings: Vec<(Name, Span, ExprId)>,
    },
    /// `decl p(a: T, ..) [mixed]` (step 5).
    Decl {
        rel: RelRef,
        columns: Vec<(Name, Option<TypeExpr>)>,
        mixed: bool,
    },
    /// `extern p(+a: T, -b)` (step 5).
    Extern { name: Name, args: Vec<ast::BindArg> },
    /// `type T { p: T flag* }`: a type block (step 5).
    TypeBlock {
        name: Name,
        attrs: Vec<ast::AttrDecl>,
    },
    /// A doc comment's pairs (step 5).
    Doc {
        kind: &'static str,
        name: Name,
        pairs: Vec<(String, String)>,
    },
}

/// A parameter of a `let` with parameters.
#[derive(Debug, Clone)]
pub struct Param {
    pub var: VarId,
    pub ty: Option<TypeExpr>,
    pub default: Option<ExprId>,
}

/// A rule's head; an aggregate in it is an [`ExprKind::Aggregate`]
/// argument.
#[derive(Debug, Clone)]
pub struct Head {
    pub rel: RelRef,
    pub args: RelArgs,
}

/// A block's name: `n`, `"n"`, `"a-${i}"` (bound last).
#[derive(Debug, Clone)]
pub enum Header {
    Bare(Name),
    Literal(String),
    Interp(ExprId),
}

/// A resource's body: a block of entries, or a value (R-126).
#[derive(Debug, Clone)]
pub enum ResourceBody {
    Block(Vec<Entry>),
    Value(ExprId),
}

/// `p.q = v [@rank]`, `p += v`: one entry of a block.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: Vec<Step>,
    pub op: FieldOp,
    pub value: ExprId,
    pub rank: Option<Rank>,
    pub span: Span,
}

/// What a `set` writes.
#[derive(Debug, Clone)]
pub enum Target {
    /// `set R.p`, `set c.p` (an element, R-69), `set T[_].l[_].p` (R-162).
    Attr { res: ExprId, path: Vec<Step> },
    /// `set k`, `set m.k`, `set n.k`.
    Input { decl: DeclRef, path: Vec<Name> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyKind {
    Use,
    Component,
    Deployment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    Deny,
    Warn,
}

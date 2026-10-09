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
    /// `_` where a term stands: a relation's column, a pattern's element,
    /// matching anything and holding nothing (`placeholders` refuses it
    /// where a value is read).
    Hole,
    /// `value`, after the reads the resolver hoisted out of it, in the
    /// order it hoisted them: a value's `k(V)`, an attribute's `attr(T, A,
    /// "p", V)`, an index's `member(V, i, W)`. Step 3 builds a term from
    /// what the resolver lowered it to, so a read is the goals it lowered
    /// to; each becomes its read's own node (`Value`, `Field`, `Lookup`,
    /// `Output`, ..) as the builders move to the tree (steps 4-5).
    Hoisted { reads: Vec<GoalId>, value: ExprId },
    /// The value a read gives in its `column`, where a literal tests or
    /// binds it in the read itself: `R.p == c`, `x = k`, `not R.ready`,
    /// `has n.k` (step 4). `goal` is the read the resolver made (a
    /// relation's goal, its `column` a hole) until reads are built from
    /// the tree; the literal holding it fills the column.
    Read { goal: GoalId, column: usize },
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
    /// `.f`: a key, as a stored path writes its segment (quoted when it
    /// holds a `.`, R-77).
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
    /// (`local("DIR")`), a `type_refine` constraint; any name no function
    /// declares.
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
    /// `"${k}": v`: a key computed when the object is built.
    Computed { key: ExprId, value: ExprId },
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

impl BinOp {
    /// The function it lowers to: `add`, `sub`, ..
    pub fn function(self) -> &'static str {
        match self {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::Div => "div",
            BinOp::Mod => "mod",
        }
    }
}

/// The operator a lowered function is.
impl TryFrom<&str> for BinOp {
    type Error = ();

    fn try_from(f: &str) -> Result<BinOp, ()> {
        Ok(match f {
            "add" => BinOp::Add,
            "sub" => BinOp::Sub,
            "mul" => BinOp::Mul,
            "div" => BinOp::Div,
            "mod" => BinOp::Mod,
            _ => return Err(()),
        })
    }
}

/// The function an aggregate is written as: `count`, `collect_set`, ..
pub fn aggregate_name(k: AggKind) -> &'static str {
    match k {
        AggKind::Set => "collect_set",
        AggKind::List => "collect_list",
        AggKind::Count => "count",
        AggKind::Sum => "sum",
        AggKind::Min => "min",
        AggKind::Max => "max",
        AggKind::Any => "any",
        AggKind::All => "all",
    }
}

/// The aggregate a function is, if it is one.
pub fn aggregate_kind(name: &str) -> Option<AggKind> {
    use AggKind::*;
    [Set, List, Count, Sum, Min, Max, Any, All]
        .into_iter()
        .find(|k| aggregate_name(*k) == name)
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
    /// `x = e`, `(a, b) = e`, `{ a, ..r } = e`; `p = e[i]` an element
    /// (`value` a `Field` of one `Index` step); `x = R.p` the read itself
    /// (`value` an [`ExprKind::Read`]).
    Bind { pat: PatternId, value: ExprId },
    /// `a < b`, chained `1 <= x <= 35`; `R.p == c` the read itself (`lhs`
    /// an [`ExprKind::Read`]).
    Compare {
        lhs: ExprId,
        ops: Vec<(CmpOp, ExprId)>,
    },
    /// `has r`, `has r.p`, `has x.f` (step 4).
    Has(Has),
    /// `x.ready`: a truth test, `x = true`, or the read itself with
    /// `true` in its value column (an [`ExprKind::Read`]).
    Truth(ExprId),
    /// `not B`, `not { B }`. Without a helper the clause is one goal and
    /// its last literal is negated (`not p(x)`, `x not in e`); with one
    /// the clause is the body of `__neg_N(ȳ)` and the goal is `not
    /// __neg_N(ȳ)`, N taken at build so ported and opaque statements
    /// count as one sequence.
    Not {
        clause: ClauseId,
        helper: Option<NegHelper>,
    },
    /// `n = count(x)`: an aggregate's binding. `helper` is the `__agg_N`
    /// number, taken where its rule's body is folded over it.
    Fold {
        var: VarId,
        agg: ExprId,
        helper: Option<u32>,
    },
    /// The goals one written literal lowered to, after the reads it
    /// hoisted (each the goal it lowered to, as in an
    /// [`ExprKind::Hoisted`] term) and before the field reads an object
    /// pattern makes after its binding (step 4). A chained comparison
    /// whose middle term the two sides read as different quantities is a
    /// goal per pair.
    Hoisted {
        reads: Vec<GoalId>,
        goals: Vec<GoalId>,
        after: Vec<GoalId>,
    },
    /// `goal`, a `has` of a resource's attribute path (or `not` of one),
    /// marked as that test: the compiler keeps it, or puts the schema's
    /// answer in its place (R-106, `partition::answer_has`).
    Marked { mark: HasMark, goal: GoalId },
}

/// What `has` tests.
#[derive(Debug, Clone)]
pub enum Has {
    /// `has r`: the resource exists, `__identity(T, A)` (R-152).
    Resource { typ: ExprId, addr: ExprId },
    /// `has R.p`, `has k`: the read itself, `_` in its value column (an
    /// [`ExprKind::Read`]).
    Read(ExprId),
    /// `has x.f`, `has R.p.q`: the walk to it has a value, bound to a
    /// variable nothing else reads (`resolve::HAS_VAR`).
    Walk { var: VarId, value: ExprId },
}

/// The resource attribute path a `has` tests: `__has(T, A, "PATH", N)`
/// before the N literals it covers.
#[derive(Debug, Clone)]
pub struct HasMark {
    pub typ: ExprId,
    pub addr: ExprId,
    pub path: String,
}

/// The rule `__neg_N(args) :- P, B` a `not` no single literal says
/// lowers through: N taken at build, `args` the variables B shares with
/// what is bound before it, P the positive literals before it in its
/// clause except those that read `folded`, the statement's aggregate
/// values (folded after the helper, not inside it).
#[derive(Debug, Clone)]
pub struct NegHelper {
    pub number: u32,
    pub args: Vec<VarId>,
    pub folded: Vec<VarId>,
}

/// A relation's arguments: by position, or by column name.
#[derive(Debug, Clone)]
pub enum RelArgs {
    Positional(Vec<PatternId>),
    Record(Vec<(Name, PatternId)>),
}

/// What `in` ranges over: each the lowered form of a written membership,
/// with the facts it states beside it (step 4).
#[derive(Debug, Clone)]
pub enum Coll {
    /// A list, an object, a range: `x in xs`, `member(xs, x)`; `(k, v) in
    /// o`, `member(o, k, v)`.
    Expr(ExprId),
    /// The resources of a type: `x in net.vpc`, `want("net.vpc", x)`. The
    /// type is a term: a variable for `x in resource`, or the one a typed
    /// variable already holds.
    Type(ExprId),
    /// A provider namespace's resources (R-49): the fact `__namespace(ns,
    /// t)` per type of it, the test `__namespace(ns, Type)`, then
    /// `want(Type, x)` unless `x` is bound already.
    Namespace {
        ns: Name,
        types: Vec<Name>,
        typ: ExprId,
        enumerate: bool,
    },
    /// A provider's type a `use .. as` also names (R-115): the fact
    /// `__provider_type(typ, n)` per name, the test `__provider_type(typ,
    /// Type)`, then `want(Type, x)` unless `x` is bound already.
    ProviderType {
        typ: Name,
        names: Vec<Name>,
        var: ExprId,
        enumerate: bool,
    },
    /// What the provider reports exists: `x in world.T`,
    /// `cloud_exists("T", x)`.
    World(Name),
    /// An enum type's values (R-70): the fact `__enum(name, values)` at
    /// the type's declaration `def`, and `member(list, x)` over the list
    /// a hoisted `__enum(name, list)` reads.
    Enum {
        name: Name,
        values: Vec<String>,
        def: Span,
        list: ExprId,
    },
    /// A component's copies (R-67): `instance_of("component", scope, x)`.
    Copies { component: Name, scope: ExprId },
    /// The values a path with `[_]` steps reaches (R-162): `x = e`, the
    /// steps enumerating.
    Each(ExprId),
    /// `r in T` of a reference column's `r` (R-42): the type the column
    /// read tested, `Type = "T"`.
    TypeOf(ExprId),
}

/// A variable of a clause: its source name, the name `lower` writes,
/// where it is first written, and the item it belongs to (none for a term
/// built on its own, step 3's check). `implicit`: the compiler made it
/// (`[_]`, a fresh).
#[derive(Debug, Clone)]
pub struct Var {
    pub name: Name,
    /// Picked at build among the statement's names (`X`, `Item1`), as a
    /// helper number is, so the rules `lower` writes are the resolver's.
    pub lowered: Name,
    pub first: Span,
    pub item: Option<ItemId>,
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
    /// `let k[: T] = v [@rank] [where B]` (steps 3, 5). `declares`: the
    /// first typed row, whose type is the cell's (a resource type's is the
    /// reference's own).
    Let {
        name: Name,
        ty: Option<TypeExpr>,
        value: ExprId,
        clause: Option<ClauseId>,
        rank: Option<Rank>,
        declares: bool,
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
    /// `set R.p = v`, `set k += v`, `set { R.p = v .. }` [@rank] [where
    /// B] (step 5): each line a [`Write`], the lines of a block under its
    /// one clause.
    Set {
        writes: Vec<Write>,
        clause: Option<ClauseId>,
    },
    /// `set from doc [@rank] [where B]` (R-38, step 5): each leaf of the
    /// document, `path` and `value`, to the input at its path; `source`
    /// the document's rows.
    SetFrom {
        source: Source,
        path: VarId,
        value: VarId,
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
        /// Declared several times, each under a clause (R-104): which
        /// declaration this is, and at the first where each is.
        rows: Option<(usize, Vec<Span>)>,
    },
    /// `input p`, a module's relation its user gives (R-55), or `input p
    /// from src [where B]`, a stack's relation the rows of a document
    /// (R-39), each the variables `columns` (step 5); `arity` its
    /// declaration's columns.
    RelationInput {
        rel: RelRef,
        arity: usize,
        source: Option<Source>,
        columns: Vec<VarId>,
        clause: Option<ClauseId>,
    },
    /// `output k[: T] [= v] [where B]` (step 5). `ty`: the row that
    /// declares the output in its scope, the type written (`any` when
    /// none, `addr` for a resource type).
    Output {
        name: Name,
        ty: Option<TypeExpr>,
        value: Option<ExprId>,
        clause: Option<ClauseId>,
    },
    /// `output p`: a relation exported, `true` per reference column
    /// (step 5).
    OutputRelation { rel: RelRef, ref_columns: Vec<bool> },
    /// `use p [as n] { k = v .. } [where B]`: a provider's configuration
    /// (R-8, R-115, step 5), its settings in the order written. `starts`:
    /// this declaration starts the provider (the first of its name), with
    /// its `source` settings; declared several times,
    /// each under a clause (R-104), `declared` which this is and `denies`
    /// the sites the first's denies name. Its clause binds no aggregate
    /// (the front end refuses one).
    Provider {
        name: Name,
        of: Option<Name>,
        starts: bool,
        settings: Vec<Setting>,
        clause: Option<ClauseId>,
        declared: Option<usize>,
        denies: Vec<Span>,
    },
    /// `decl p(a: T, ..) [mixed]` (step 5). `fed`: no rule of its scope
    /// defines it, so its rows come from outside (a provider, given
    /// facts); a `mixed` one has both.
    Decl {
        rel: RelRef,
        columns: Vec<(Name, Option<TypeExpr>)>,
        mixed: bool,
        fed: bool,
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

/// One setting of a provider's `use` block.
#[derive(Debug, Clone)]
pub enum Setting {
    /// `source = v`: a constant, how the provider is started.
    Source {
        key: Name,
        span: Span,
        value: ExprId,
    },
    /// `k = v`: a key of the configuration the provider is given.
    Value {
        key: Name,
        span: Span,
        value: ExprId,
    },
    /// `expect_account = v`: what the account the provider reports is
    /// checked against, under the clause again (its own gathering).
    Account {
        value: ExprId,
        clause: Option<ClauseId>,
        span: Span,
    },
}

/// A parameter of a `let` with parameters: its variable (its type and
/// default are read where it is called, until they are built from the
/// tree).
#[derive(Debug, Clone)]
pub struct Param {
    pub var: VarId,
    pub ty: Option<TypeExpr>,
    pub default: Option<ExprId>,
}

/// A rule's head; an aggregate in it is an [`ExprKind::Aggregate`]
/// argument. `reads`: the reads its arguments hoisted, in order, each
/// the goal it lowered to (as an [`ExprKind::Hoisted`] term's), written
/// after the clause until reads are built from the tree.
#[derive(Debug, Clone)]
pub struct Head {
    pub rel: RelRef,
    pub args: RelArgs,
    pub reads: Vec<GoalId>,
}

/// A block's name: `n` (the name, R-76), `"n"` (one segment of the
/// address, quoted when it holds a dot, R-112), `"a-${i}"` (bound last,
/// from the clause: the reads its holes hoisted and its segment's
/// binding, its value the variable bound).
#[derive(Debug, Clone)]
pub enum Header {
    Bare(Name),
    Literal(String),
    Interp(ExprId),
}

/// A resource's body: a block of entries, or a value (R-126), its
/// entries the object's keys when it is one, else one at the root.
#[derive(Debug, Clone)]
pub enum ResourceBody {
    Block(Vec<Entry>),
    Value(ExprId),
}

/// `p.q = v [@rank]`, `p += v`, a pun `p`: one entry of a block, `path`
/// its stored path (`containers["api"].image`, R-158) as the resolver
/// reads it, until paths are built from the tree.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: String,
    pub op: FieldOp,
    pub value: ExprId,
    pub rank: Option<Rank>,
    pub span: Span,
}

/// One line of a `set`: `target (=|+=) value [@rank]`, after the reads
/// its target hoisted (each the goal it lowered to, as an
/// [`ExprKind::Hoisted`] term's); `folds`, the `__agg_N` each of its
/// clause's aggregates folds through for this line, when they fold
/// through helpers (a block's lines each fold on their own).
#[derive(Debug, Clone)]
pub struct Write {
    pub target: Target,
    pub op: FieldOp,
    pub value: ExprId,
    pub rank: Option<Rank>,
    pub reads: Vec<GoalId>,
    pub folds: Vec<u32>,
    pub span: Span,
}

/// What a `set` writes, as the cell it lowers to.
#[derive(Debug, Clone)]
pub enum Target {
    /// `set R.p`, `set T[_].l[_].p` (R-162), `set m.k` and `set n.k` (a
    /// used module's or a copy's input, `typ` the input's cell): the
    /// cell `(typ, addr, path)`.
    Attr {
        typ: ExprId,
        addr: ExprId,
        path: String,
    },
    /// `set c.p where c in R.l`, `set R.l[k].p`: the element of the keyed
    /// list `list` whose key is `key`, the fields below it `rest` (R-35,
    /// R-69).
    Element {
        typ: ExprId,
        addr: ExprId,
        list: String,
        key: ExprId,
        rest: Vec<Name>,
    },
    /// `set k`, `set k.f`: the stack's input (a field of an object one,
    /// R-54) by its path.
    Input { path: String },
}

/// The rows of a document a statement reads (`set from`, `input p from`,
/// R-39): the externs that read it, declared before the statement, and
/// the reads, each the goal it lowered to (the document's term bound,
/// then the table's extern), until reads are built from the tree.
#[derive(Debug, Clone)]
pub struct Source {
    pub externs: Vec<ast::Stmt>,
    pub reads: Vec<GoalId>,
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

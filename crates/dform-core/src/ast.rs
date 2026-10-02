pub use crate::lattice::Rank;
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone)]
pub struct Program {
    pub statements: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Term {
    Val(Value),
    Var(String),
    Wildcard,
    Func { name: String, args: Vec<Term> },
    List(Vec<Term>),
    Obj(BTreeMap<String, Term>),
    // List comprehension: [ item | body... ]
    ListComp { item: Box<Term>, body: Vec<Lit> },
}

impl Term {
    pub fn is_var(&self) -> bool {
        matches!(self, Term::Var(_))
    }
}

impl PartialEq for Term {
    fn eq(&self, other: &Self) -> bool {
        use Term::*;
        match (self, other) {
            (Val(a), Val(b)) => a == b,
            (Var(a), Var(b)) => a == b,
            (Wildcard, Wildcard) => true,
            (Func { name: an, args: aa }, Func { name: bn, args: ba }) => an == bn && aa == ba,
            (List(a), List(b)) => a == b,
            (Obj(a), Obj(b)) => a == b,
            (ListComp { item: ai, body: ab }, ListComp { item: bi, body: bb }) => {
                ai == bi && format!("{:?}", ab) == format!("{:?}", bb)
            }
            _ => false,
        }
    }
}

impl Eq for Term {}

impl PartialOrd for Term {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Term {
    fn cmp(&self, other: &Self) -> Ordering {
        use Term::*;
        let tag = |t: &Term| match t {
            Val(_) => 0,
            Var(_) => 1,
            Wildcard => 2,
            Func { .. } => 3,
            List(_) => 4,
            Obj(_) => 5,
            ListComp { .. } => 6,
        };
        let ta = tag(self);
        let tb = tag(other);
        if ta != tb {
            return ta.cmp(&tb);
        }
        match (self, other) {
            (Val(a), Val(b)) => a.cmp(b),
            (Var(a), Var(b)) => a.cmp(b),
            (Wildcard, Wildcard) => Ordering::Equal,
            (Func { name: an, args: aa }, Func { name: bn, args: ba }) => {
                an.cmp(bn).then_with(|| aa.cmp(ba))
            }
            (List(a), List(b)) => a.cmp(b),
            (Obj(a), Obj(b)) => a.cmp(b),
            (ListComp { item: ai, body: ab }, ListComp { item: bi, body: bb }) => ai
                .cmp(bi)
                .then_with(|| format!("{:?}", ab).cmp(&format!("{:?}", bb))),
            _ => Ordering::Equal,
        }
    }
}

impl Hash for Term {
    fn hash<H: Hasher>(&self, state: &mut H) {
        use Term::*;
        std::mem::discriminant(self).hash(state);
        match self {
            Val(v) => v.hash(state),
            Var(v) => v.hash(state),
            Wildcard => {}
            Func { name, args } => {
                name.hash(state);
                args.hash(state);
            }
            List(xs) => xs.hash(state),
            Obj(m) => m.hash(state),
            ListComp { item, body } => {
                item.hash(state);
                format!("{:?}", body).hash(state);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Fact(Atom),
    Rule(RuleStmt),
    Module(Module),
    Instance(Instance),
    /// `input k: T [= default] [where refinement]`: a module's or the
    /// stack's typed input.
    Input(InputDecl),
    /// `input p(cols) from facts("path")`: `p` is fed from outside
    /// the program and re-read when its source changes (`watch`).
    InputRelation(InputRelation),
    /// `input p` in a module or a component: the relation `p`'s rows are
    /// its user's to give, in the `use` or `instance` block (R-55).
    RelationInput(Extern),
    /// An output's declaration (`k: T`) or its value (`k = term`).
    Output(OutputDecl),
    /// The stack's settings, `[stacks.NAME]` over `[defaults]` in
    /// dform.toml, as the loader reads them: never written in a program
    /// (`stack`).
    Stack(Config),
    /// `provider name { ... }`: a provider the program uses (`stack`).
    Provider(Config),
    /// `use PATH [as NAME] [{ k = v }] [where B]`: a module imported into
    /// the scope (R-65), its inputs the block's: one copy of the file, as
    /// an `instance` is one of a component.
    Use(Instance),
    Resource(Resource),
    Settings(Settings),
    Decl(Decl),
    Extern(Extern),
    /// `decl p(..) mixed`: `p/N` may have both ground facts and rules (E
    /// §2.6); without it, a predicate that has both is a compile error.
    Mixed(Extern),
    /// `extern p(+in, -out, ...) [persist]`: a predicate a provider answers
    /// on demand, once its `+` arguments are ground (`externs`).
    ExternFn(ExternFn),
    /// A statement the grammar has and the evaluator does not yet: lowering
    /// rejects it naming the ticket that brings it.
    Pending(Pending),
}

/// Where a statement came from: a byte range in a source file registered
/// with `diag::add_source`. Comparison and hashing ignore it, so two facts
/// written in two places are still one fact.
#[derive(Clone, Copy, Default)]
pub struct Span {
    /// `diag` source id; 0 for code the compiler wrote.
    pub file: u32,
    pub start: u32,
    pub end: u32,
    /// `diag` origin id: the policy pack or module instance a statement
    /// was lowered out of; 0 for the program's own.
    pub origin: u32,
}

impl Span {
    pub fn is_none(&self) -> bool {
        self.file == 0
    }

    /// The two spans are the same place: the same bytes of the same
    /// source. (`==` holds of any two spans, so that facts compare by
    /// content.)
    pub fn same_place(&self, other: &Span) -> bool {
        (self.file, self.start, self.end) == (other.file, other.start, other.end)
    }

    /// This span, lowered out of `origin` unless it already was.
    pub fn within(self, origin: u32) -> Span {
        if self.origin == 0 {
            Span { origin, ..self }
        } else {
            self
        }
    }
}

impl PartialEq for Span {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Span {}

impl PartialOrd for Span {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Span {
    fn cmp(&self, _: &Self) -> Ordering {
        Ordering::Equal
    }
}

impl Hash for Span {
    fn hash<H: Hasher>(&self, _: &mut H) {}
}

impl std::fmt::Debug for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}..{}@{}", self.start, self.end, self.file)
    }
}

/// `decl p(..)` of a relation no rule defines: `p/N` is defined by a provider, not
/// by the program.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Extern {
    pub pred: String,
    pub arity: usize,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ExternFn {
    pub name: String,
    pub args: Vec<BindArg>,
    pub persist: bool,
    pub span: Span,
}

/// `decl p(Field: type, ...)`: the field names (the variables, snake_cased)
/// of `p`'s record form, in argument order.
#[derive(Debug, Clone)]
pub struct Decl {
    pub pred: String,
    pub fields: Vec<String>,
    pub span: Span,
}

/// A module or a component (R-65): a module is a file, named by its path
/// from the project root (`config`, `modules.net`), imported by `use`; a
/// component is a `component NAME { .. }` item of one (`modules.net.vpc`;
/// an entry file's is its own name), copied by `instance`. Either is
/// stamped under a name: its predicates are that name's (`n::p`), its
/// resources `n::x`, its values leaving it through outputs.
#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    pub component: bool,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// `instance component name { k = v ... } [where body]`, or `use module
/// [as name] { .. } [where body]`: each `k = v` is a contribution to input
/// `k` of the copy `name` (R-65), `module` the component's or module's
/// path.
#[derive(Debug, Clone)]
pub struct Instance {
    pub module: String,
    pub name: String,
    pub inputs: Vec<(String, Term, Span)>,
    /// The rows the block gives the module's relation inputs (R-55): its
    /// `p(..) [where B]` and `p from TERM` entries, lowered in the user's
    /// scope, each head the module's own name `p`.
    pub rows: Vec<Stmt>,
    /// The clause and the block's reads: what each input's contribution
    /// is derived under.
    pub body: Option<Vec<Lit>>,
    /// The clause alone, `where B`: what the copy exists under.
    pub clause: Option<Vec<Lit>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct InputDecl {
    pub name: String,
    pub ty: TypeExpr,
    pub default: Option<Term>,
    pub refinement: Vec<Lit>,
    /// `key k: T`: the target gives it, never `--set`, and its value names
    /// the deployment (R-29). An input in every other respect.
    pub key: bool,
    /// The object form's fields, `input k { f: T [= d] [check B] .. }`
    /// (R-54), each a declaration named by its field, a nested object's
    /// with fields of its own; empty for every other input.
    pub fields: Vec<InputDecl>,
    pub span: Span,
}

/// `input p(a, ..) from facts(SOURCE)`: the source is a call term,
/// `file("path")` or `git("repo", "ref", "path")`, checked by `watch`.
#[derive(Debug, Clone)]
pub struct InputRelation {
    pub pred: String,
    pub arity: usize,
    pub source: Term,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct OutputDecl {
    pub name: String,
    pub ty: Option<TypeExpr>,
    pub value: Option<Term>,
    pub span: Span,
}

/// A `stack` or `provider` statement: a name and its `key = value` block.
#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub config: Vec<(String, Term, Span)>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Resource {
    pub typ: Term,
    pub name: Term,
    /// `resource T N @default { ... }`: the rank of every field without its own.
    pub rank: Option<Rank>,
    pub fields: Vec<FieldAssign>,
    pub body: Option<Vec<Lit>>,
    /// `body[reads]`: the literals the fields' values read, after the
    /// block's own clauses (a `when` appends its guard after them).
    pub reads: std::ops::Range<usize>,
    pub span: Span,
}

#[derive(Debug, Copy, Clone)]
pub enum FieldOp {
    Assign,
    Add,
}

#[derive(Debug, Clone)]
pub struct FieldAssign {
    pub key: String,
    pub op: FieldOp,
    pub value: Term,
    /// `key = value @override`; `None` takes the block's rank.
    pub rank: Option<Rank>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub env: Term,
    /// `settings E @default { ... }`: the rank of every leaf without its own.
    pub rank: Option<Rank>,
    pub fields: Vec<FieldAssign>,
    pub body: Option<Vec<Lit>>,
    pub span: Span,
}

/// A parsed statement with no lowering yet (E §6 constructs whose semantics
/// are phase 6 tickets).
#[derive(Debug, Clone)]
pub struct Pending {
    pub kind: PendingKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum PendingKind {
    TypeDecl { name: String, attrs: Vec<AttrDecl> },
}

impl PendingKind {
    /// What the statement is, and the WORK.org ticket that gives it meaning.
    pub fn describe(&self) -> (&'static str, &'static str) {
        match self {
            PendingKind::TypeDecl { .. } => (
                "a type block",
                "phase 6 \"Refinement types, doc annotations, L15 inet\"",
            ),
        }
    }
}

/// `type := name | name(type, ...) | { field: type, ... } | "string"`.
#[derive(Debug, Clone)]
pub enum TypeExpr {
    Name(String),
    Apply(String, Vec<TypeExpr>),
    Object(Vec<(String, TypeExpr)>),
    Str(String),
}

/// `+name: type` (input) or `-name: type` (output) of an extern.
#[derive(Debug, Clone)]
pub struct BindArg {
    pub input: bool,
    pub name: String,
    pub ty: Option<TypeExpr>,
}

/// `path: type flag* [check body]`, or a nested block of them.
#[derive(Debug, Clone)]
pub struct AttrDecl {
    pub path: String,
    pub ty: Option<TypeExpr>,
    pub flags: Vec<String>,
    pub refinement: Vec<Lit>,
    pub children: Vec<AttrDecl>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct RuleStmt {
    pub head: Atom,
    pub body: Vec<Lit>,
}

/// A predicate applied to terms. `span` is where it was written (or the
/// statement it was lowered from); two atoms equal but for their spans are
/// one fact.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Atom {
    pub pred: String,
    pub args: Vec<Term>,
    pub record: Option<BTreeMap<String, Term>>,
    pub span: Span,
}

/// As derived before spans: a span is not part of an atom's printed form.
impl std::fmt::Debug for Atom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Atom")
            .field("pred", &self.pred)
            .field("args", &self.args)
            .field("record", &self.record)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub enum Lit {
    Pos(Atom),
    Not(Atom),
    Eq(Term, Term),
    Neq(Term, Term),
    Gt(Term, Term),
    Ge(Term, Term),
    Lt(Term, Term),
    Le(Term, Term),
}

pub use crate::lattice::Rank;
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub statements: Vec<Stmt>,
    /// The stack's settings, `[stacks.NAME]` over `[defaults]` in
    /// dform.toml, as the loader reads them, with their spans: never
    /// written in a program (`stack::config`). `None` for a program that
    /// is not a stack's.
    pub stack: Option<Config>,
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

/// A string literal term, `"s"`.
pub fn str_term(s: &str) -> Term {
    Term::Val(Value::Str(s.to_string()))
}

/// A variable term, `S`.
pub fn var(s: &str) -> Term {
    Term::Var(s.to_string())
}

/// The atom `pred(args..)` written at `span`, of no record.
pub fn atom(pred: &str, args: Vec<Term>, span: Span) -> Atom {
    Atom {
        pred: pred.into(),
        args,
        record: None,
        span,
    }
}

impl Atom {
    /// The atom with each variable `env` binds replaced by its value.
    pub fn subst(&self, env: &BTreeMap<String, Value>) -> Atom {
        Atom {
            args: self.args.iter().map(|t| t.subst(env)).collect(),
            ..self.clone()
        }
    }
}

/// The name of `names` whose field `s` reads (`net.bits`: `net`), its
/// term, and the field: the shortest such name.
fn name_of_field<'a, 'n>(
    s: &'a str,
    names: &'n BTreeMap<String, Term>,
) -> Option<(&'a str, &'n Term, &'a str)> {
    s.match_indices('.').find_map(|(i, _)| {
        let name = &s[..i];
        Some((name, names.get(name)?, &s[i + 1..]))
    })
}

impl Term {
    /// The term with each variable `env` binds replaced by its value.
    pub fn subst(&self, env: &std::collections::BTreeMap<String, Value>) -> Term {
        match self {
            Term::Var(x) => match env.get(x) {
                Some(v) => Term::Val(v.clone()),
                None => self.clone(),
            },
            Term::Func { name, args } => Term::Func {
                name: name.clone(),
                args: args.iter().map(|a| a.subst(env)).collect(),
            },
            Term::List(xs) => Term::List(xs.iter().map(|a| a.subst(env)).collect()),
            Term::Obj(m) => Term::Obj(m.iter().map(|(k, a)| (k.clone(), a.subst(env))).collect()),
            t => t.clone(),
        }
    }

    /// The term with each string a refinement names (a key of `names`: a
    /// refinement's text writes the cell it refines, and the attributes
    /// beside it, by name) read as its term. A field of one, `net.bits`
    /// (R-134), is read off it: renamed with a name it is given as a
    /// string, as written where the name reads as itself, else by
    /// `__path`.
    pub fn replace_names(&self, names: &BTreeMap<String, Term>) -> Term {
        match self {
            Term::Val(Value::Str(s)) => match names.get(s) {
                Some(v) => v.clone(),
                None => match name_of_field(s, names) {
                    Some((name, v, field)) => match v {
                        Term::Val(Value::Str(n)) => Term::Val(Value::Str(format!("{n}.{field}"))),
                        Term::Var(n) if n == name => Term::Var(s.clone()),
                        v => Term::Func {
                            name: "__path".into(),
                            args: vec![v.clone(), Term::Val(Value::Str(field.to_string()))],
                        },
                    },
                    None => self.clone(),
                },
            },
            Term::Func { name, args } => Term::Func {
                name: name.clone(),
                args: args.iter().map(|a| a.replace_names(names)).collect(),
            },
            Term::List(xs) => Term::List(xs.iter().map(|a| a.replace_names(names)).collect()),
            Term::Obj(m) => Term::Obj(
                m.iter()
                    .map(|(k, a)| (k.clone(), a.replace_names(names)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    pub fn is_var(&self) -> bool {
        matches!(self, Term::Var(_))
    }

    /// `f` of each variable in `self`, left to right, at every
    /// occurrence; a list comprehension's variables are its own and are
    /// not visited.
    pub fn for_each_var<'t>(&'t self, f: &mut impl FnMut(&'t str)) {
        match self {
            Term::Var(v) => f(v),
            Term::Func { args, .. } | Term::List(args) => {
                args.iter().for_each(|a| a.for_each_var(f))
            }
            Term::Obj(m) => m.values().for_each(|a| a.for_each_var(f)),
            Term::Val(_) | Term::Wildcard | Term::ListComp { .. } => {}
        }
    }

    /// The text of a string literal term; none for any other term.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Term::Val(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    /// A constant term's value: a literal, or a list or object of them
    /// (a fact's argument, as `--set k=@FILE.df` and an `--input-file`
    /// give it); none for a term with a variable or a call in it.
    pub fn ground(&self) -> Option<Value> {
        match self {
            Term::Val(v) => Some(v.clone()),
            Term::List(xs) => xs
                .iter()
                .map(Term::ground)
                .collect::<Option<_>>()
                .map(Value::List),
            Term::Obj(m) => m
                .iter()
                .map(|(k, x)| Some((k.clone(), x.ground()?)))
                .collect::<Option<_>>()
                .map(Value::Obj),
            _ => None,
        }
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
    /// `input p` in a module or a component: the relation `p`'s rows are
    /// its user's to give, in the `use` or `instance` block (R-55).
    RelationInput(Extern),
    /// An output's declaration (`k: T`) or its value (`k = term`).
    Output(OutputDecl),
    /// `use name { ... }`: a provider the program uses (`stack`).
    Provider(Config),
    /// `use PATH [as NAME] [{ k = v }] [where B]`: a module imported into
    /// the scope (R-65), its inputs the block's: one copy of the file, as
    /// an `instance` is one of a component.
    Use(Instance),
    Resource(Resource),
    Decl(Decl),
    Extern(Extern),
    /// `decl p(..) mixed`: `p/N` may have both ground facts and rules (E
    /// §2.6); without it, a predicate that has both is a compile error.
    Mixed(Extern),
    /// `extern p(+in, -out, ...)`: a predicate a provider answers
    /// on demand, once its `+` arguments are ground (`externs`).
    ExternFn(ExternFn),
    /// `let f(a, b) = t` (R-187): the relation `f(a, b, v)` the program
    /// answers on demand, its columns but the last bound by the literal
    /// that reads it, as a provider's table's `+` columns are (`demand`).
    Mode(Extern),
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
    pub span: Span,
}

/// `decl p(Field: type, ...)`: the field names (the variables, snake_cased)
/// of `p`'s record form, in argument order.
#[derive(Debug, Clone)]
pub struct Decl {
    pub pred: String,
    pub fields: Vec<String>,
    /// Each column's type as written, `None` where it gives none.
    pub types: Vec<Option<TypeExpr>>,
    pub span: Span,
}

/// A module or a component (R-65): a module is a file, named by its path
/// from the project root (`config`, `modules.net`), imported by `use`; a
/// component is a `component NAME { .. }` item of one (`modules.net.vpc`;
/// an entry file's is its own name), copied by `instance`. Either is
/// stamped under a name: its predicates are that name's (`n::p`), its
/// resources `n/x`, its values leaving it through outputs.
#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    pub component: bool,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// `resource component name { k = v ... } [where body]`, or `use module
/// [as name] { .. } [where body]`: each `k = v` is a contribution to input
/// `k` of the copy `name` (R-65), `module` the component's or module's
/// path.
#[derive(Debug, Clone)]
pub struct Instance {
    pub module: String,
    pub name: String,
    /// A copy named by its clause (R-191), `resource C "a-${i}" { .. }
    /// where B`: the variable the clause binds to each copy's name.
    /// `name` is then the header as written, the scope the component is
    /// expanded under before each row takes its own.
    pub named: Option<Term>,
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
    /// A copy of a module's component named through a `use` of it
    /// (`a.volume` under `use backups as a`): the instance of the module
    /// whose items the component's body reads bare (R-186).
    pub via: Option<Via>,
    pub span: Span,
}

/// The module instance a copy's component was named through (R-186):
/// the module's path, the name its `use` binds, and that instance's
/// scope as the copy's user reads it (`"a"`, or `__scope("a")` for a
/// `use` outside the user's body).
#[derive(Debug, Clone)]
pub struct Via {
    pub module: String,
    pub name: String,
    pub scope: Term,
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
    /// `input k: T where B`, a dependent input (R-104): it is declared, and
    /// read, only where `B` holds. Empty: everywhere.
    pub guard: Vec<Lit>,
    /// The object form's fields, `input k { f: T [= d] [check B] .. }`
    /// (R-54), each a declaration named by its field, a nested object's
    /// with fields of its own; empty for every other input.
    pub fields: Vec<InputDecl>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct OutputDecl {
    pub name: String,
    pub ty: Option<TypeExpr>,
    pub value: Option<Term>,
    /// `output p`: the relation `p` exported (R-55), one entry per column,
    /// true for a column its `decl` types as a resource (`s: net.subnet`),
    /// which holds the copy's resource and is exported as its address.
    pub relation: Option<Vec<bool>>,
    pub span: Span,
}

/// A `stack` or provider's `use`: a name and its `key = value` block.
#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    /// The provider a `use P as NAME` renames (R-115): `ovh` of `use ovh
    /// as ca`, whose types the program writes `ca.instance`. `None` when
    /// the name is the provider's own, and for a stack.
    pub of: Option<String>,
    pub config: Vec<(String, Term, Span)>,
    pub span: Span,
}

impl Config {
    /// The provider a provider's `use` starts: `ovh` of `use ovh as ca`
    /// and of `use ovh`.
    pub fn provider(&self) -> &str {
        self.of.as_deref().unwrap_or(&self.name)
    }
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
    /// What the compiler wrote the rule for, when it is no statement of
    /// the program. Later passes and the engine read this, never the head's
    /// name: a module's copy is `m::__neg_0`, a call site's `S::__neg_0`
    /// (R-212). A rule rewritten from another keeps it.
    pub helper: Option<Helper>,
}

impl RuleStmt {
    /// A rule of the program, or one the compiler writes in its place.
    pub fn new(head: Atom, body: Vec<Lit>) -> Self {
        RuleStmt {
            head,
            body,
            helper: None,
        }
    }

    /// A rule the compiler writes beside the program's, read by them.
    pub fn helper(kind: Helper, head: Atom, body: Vec<Lit>) -> Self {
        RuleStmt {
            head,
            body,
            helper: Some(kind),
        }
    }
}

/// The rules the compiler writes that no statement is: each is read by
/// the rule it was written for, which is stuck when it is, so the engine
/// reports that rule and never the helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Helper {
    /// `__neg_N(ȳ)`: the body of a `not { .. }` over the variables ȳ it
    /// shares with its rule. Its pattern is matched as it is, nulls and
    /// all (R-193).
    Negation,
    /// `__agg_N`: an aggregate's group and the term it folds.
    Aggregate,
    /// `__lc_N`: a list comprehension's elements.
    Comprehension,
    /// `__field_read_N`: a relation's row read by its fields.
    FieldRead,
    /// `__has_known_N`: a `not has` of a computed value, holding once the
    /// value is known.
    Known,
    /// `__ref_dep`: the order a contribution that reads a reference
    /// follows, stuck exactly when the contribution is.
    RefDep,
    /// The compiler's checks and records: `__not_planned`,
    /// `__unanswered`, `__rows`, `__declared`, `__refine_*`.
    Check,
}

impl Helper {
    /// The relations the helpers of this kind among `rules` define, in
    /// whatever scope their names were given.
    pub fn heads(self, rules: &[RuleStmt]) -> BTreeSet<String> {
        rules
            .iter()
            .filter(|r| r.helper == Some(self))
            .map(|r| r.head.pred.clone())
            .collect()
    }
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

impl Lit {
    /// The literal with each variable `env` binds replaced by its value.
    pub fn subst(&self, env: &std::collections::BTreeMap<String, Value>) -> Lit {
        self.clone().map_terms(|t| t.subst(env))
    }

    /// The literal with each name of a refinement read as its term
    /// ([`Term::replace_names`]).
    pub fn replace_names(&self, names: &BTreeMap<String, Term>) -> Lit {
        self.clone().map_terms(|t| t.replace_names(names))
    }

    /// `self` with its atom passed through `atom`, or each side of its
    /// comparison through `term`.
    pub fn map(self, atom: impl FnOnce(Atom) -> Atom, mut term: impl FnMut(Term) -> Term) -> Lit {
        match self {
            Lit::Pos(a) => Lit::Pos(atom(a)),
            Lit::Not(a) => Lit::Not(atom(a)),
            Lit::Eq(x, y) => Lit::Eq(term(x), term(y)),
            Lit::Neq(x, y) => Lit::Neq(term(x), term(y)),
            Lit::Gt(x, y) => Lit::Gt(term(x), term(y)),
            Lit::Ge(x, y) => Lit::Ge(term(x), term(y)),
            Lit::Lt(x, y) => Lit::Lt(term(x), term(y)),
            Lit::Le(x, y) => Lit::Le(term(x), term(y)),
        }
    }

    /// `self` with every term it reads (an atom's arguments, a
    /// comparison's sides) passed through `term`.
    pub fn map_terms(self, term: impl FnMut(Term) -> Term) -> Lit {
        fn args(mut a: Atom, term: impl FnMut(Term) -> Term) -> Atom {
            a.args = a.args.into_iter().map(term).collect();
            a
        }
        match self {
            Lit::Pos(a) => Lit::Pos(args(a, term)),
            Lit::Not(a) => Lit::Not(args(a, term)),
            l => l.map(|a| a, term),
        }
    }
    /// The terms a literal reads: an atom's arguments, a comparison's two
    /// sides.
    pub fn terms(&self) -> impl Iterator<Item = &Term> {
        let (args, sides): (&[Term], Option<[&Term; 2]>) = match self {
            Lit::Pos(a) | Lit::Not(a) => (&a.args, None),
            Lit::Eq(x, y)
            | Lit::Neq(x, y)
            | Lit::Gt(x, y)
            | Lit::Ge(x, y)
            | Lit::Lt(x, y)
            | Lit::Le(x, y) => (&[], Some([x, y])),
        };
        args.iter().chain(sides.into_iter().flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_term_is_ground_when_no_variable_or_call_is_in_it() {
        let s = |x: &str| Term::Val(Value::Str(x.into()));
        let obj = Term::Obj(BTreeMap::from([(
            "a".to_string(),
            Term::List(vec![s("x")]),
        )]));
        assert_eq!(
            obj.ground(),
            Some(Value::Obj(BTreeMap::from([(
                "a".to_string(),
                Value::List(vec![Value::Str("x".into())])
            )])))
        );
        let var = Term::List(vec![s("x"), Term::Var("X".into())]);
        assert_eq!(var.ground(), None);
    }

    /// A literal's terms are its atom's arguments or its two sides; their
    /// variables are visited at each occurrence, a comprehension's not.
    #[test]
    fn a_literals_variables_are_its_terms_variables() {
        let f = Term::Func {
            name: "f".into(),
            args: vec![
                var("A"),
                Term::Obj(BTreeMap::from([("k".to_string(), var("B"))])),
                Term::ListComp {
                    item: Box::new(var("C")),
                    body: vec![],
                },
            ],
        };
        let lits = [
            Lit::Pos(atom("p", vec![var("A"), Term::Wildcard], Span::default())),
            Lit::Eq(var("A"), f),
        ];
        let mut seen = Vec::new();
        for l in &lits {
            l.terms()
                .for_each(|t| t.for_each_var(&mut |v| seen.push(v)));
        }
        assert_eq!(seen, ["A", "A", "A", "B"]);
    }

    /// `map` passes the atom to its first function and a comparison's
    /// sides to its second; `map_terms` passes every term through one.
    #[test]
    fn a_literal_maps_its_atom_or_its_sides() {
        let up = |t: Term| match t {
            Term::Var(v) => Term::Var(v.to_uppercase()),
            t => t,
        };
        let pos = Lit::Not(atom("p", vec![var("a")], Span::default()));
        let renamed = pos.clone().map(
            |a| Atom {
                pred: "q".into(),
                ..a
            },
            up,
        );
        assert!(matches!(&renamed, Lit::Not(a) if a.pred == "q" && a.args == [var("a")]));
        assert!(matches!(pos.map_terms(up), Lit::Not(a) if a.pred == "p" && a.args == [var("A")]));
        assert!(matches!(
            Lit::Le(var("x"), var("y")).map_terms(up),
            Lit::Le(Term::Var(x), Term::Var(y)) if x == "X" && y == "Y"
        ));
    }
}

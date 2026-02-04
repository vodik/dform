use crate::value::Value;
use std::collections::BTreeMap;
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone)]
pub struct Program {
    pub statements: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Term {
    Val(Value),
    Var(String),
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
            Func { .. } => 2,
            List(_) => 3,
            Obj(_) => 4,
            ListComp { .. } => 5,
        };
        let ta = tag(self);
        let tb = tag(other);
        if ta != tb {
            return ta.cmp(&tb);
        }
        match (self, other) {
            (Val(a), Val(b)) => a.cmp(b),
            (Var(a), Var(b)) => a.cmp(b),
            (Func { name: an, args: aa }, Func { name: bn, args: ba }) => {
                an.cmp(bn).then_with(|| aa.cmp(ba))
            }
            (List(a), List(b)) => a.cmp(b),
            (Obj(a), Obj(b)) => a.cmp(b),
            (ListComp { item: ai, body: ab }, ListComp { item: bi, body: bb }) => {
                ai.cmp(bi).then_with(|| format!("{:?}", ab).cmp(&format!("{:?}", bb)))
            }
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
    Constraint(Constraint),
    Component(Component),
    When(When),
    Resource(Resource),
    Import(Import),
    Unique(Unique),
    Settings(Settings),
}

#[derive(Debug, Clone)]
pub struct Component {
    pub comp: String,
    pub inst: String,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct When {
    pub guard: Lit,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct Resource {
    pub typ: Term,
    pub name: Term,
    pub fields: Vec<(String, Term)>,
    pub body: Option<Vec<Lit>>,
}

#[derive(Debug, Clone)]
pub struct Import {
    pub path: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Unique {
    pub pred: String,
    pub key_arity: usize,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub env: Term,
    pub fields: Vec<(String, Term)>,
}

#[derive(Debug, Clone)]
pub struct RuleStmt {
    pub head: Atom,
    pub body: Vec<Lit>,
}

#[derive(Debug, Clone)]
pub struct Constraint {
    pub message: String,
    pub body: Vec<Lit>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Atom {
    pub pred: String,
    pub args: Vec<Term>,
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

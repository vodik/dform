use crate::value::Term;

#[derive(Debug, Clone)]
pub struct Program {
    pub statements: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Fact(Atom),
    Rule(RuleStmt),
    Constraint(Constraint),
    Component(Component),
}

#[derive(Debug, Clone)]
pub struct Component {
    pub comp: String,
    pub inst: String,
    pub body: Vec<Stmt>,
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

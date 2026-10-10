//! A component's body reads the items of the body around it bare
//! (R-186): its module's lets, inputs, resources and relations, and an
//! enclosing component's, as a Rust `fn` reads its module's. The scope
//! is lexical and by instance: the resolver marks such a read with the
//! definition it names ([`lexical_pred`], [`lexical_term`]), and
//! expansion makes it the read of that definition's instance the copy
//! was taken from: the instance it is expanded inside (a copy made in
//! the module's own file), or the one a `use` binds (`a.volume` under
//! `use backups as a` reads `a`'s, never another `use`'s).

use crate::ast::{Atom, FieldAssign, Lit, Resource, RuleStmt, Span, Stmt, Term, Via, str_term};
use crate::value::Value;

/// The mark of a read of an enclosing definition's item: `__lexical(Body,
/// name)` as an address, `__lexical:Body:name` as a relation, Body the
/// definition's path.
pub const LEXICAL: &str = "__lexical";

/// `__lexical_of(User, Name, Scope)`: the copy `Name` of the scope `User`
/// reads the items of the instance at `Scope` bare, what `why NAME` and
/// the hover answer from.
pub const LEXICAL_OF: &str = "__lexical_of";

/// The relation a component's body reads an enclosing definition's
/// value or relation `name` by.
pub fn lexical_pred(body: &str, name: &str) -> String {
    format!("{LEXICAL}:{body}:{name}")
}

/// The address a component's body reads an enclosing definition's
/// resource `name` by.
pub fn lexical_term(body: &str, name: &str) -> Term {
    Term::Func {
        name: LEXICAL.into(),
        args: vec![str_term(body), str_term(name)],
    }
}

/// The name a [`lexical_pred`] reads, for a variable named after it.
pub fn lexical_name(pred: &str) -> Option<&str> {
    pred_parts(pred).map(|(_, n)| n)
}

/// The definition and the name a [`lexical_pred`] reads.
fn pred_parts(pred: &str) -> Option<(&str, &str)> {
    pred.strip_prefix(LEXICAL)?
        .strip_prefix(':')?
        .rsplit_once(':')
}

/// The definition and the name a [`lexical_term`] reads.
fn term_parts(t: &Term) -> Option<(&str, &str)> {
    match t {
        Term::Func { name, args } if name == LEXICAL => match args.as_slice() {
            [Term::Val(Value::Str(b)), Term::Val(Value::Str(n))] => Some((b, n)),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `t` is a mark, which a copy's scoping leaves as it is.
pub(super) fn is_mark(t: &Term) -> bool {
    matches!(t, Term::Func { name, .. } if name == LEXICAL)
}

/// The reads of the definition `body`'s items made that definition's
/// instance's: `via`'s, or with none the instance being expanded, whose
/// own reads they then are.
pub(super) struct Lexical<'a> {
    pub body: &'a str,
    pub via: Option<&'a Via>,
}

impl Lexical<'_> {
    pub fn stmt(&self, s: Stmt) -> Stmt {
        let lits = |ls: Vec<Lit>| ls.into_iter().map(|l| self.lit(l)).collect();
        match s {
            Stmt::Fact(a) => Stmt::Fact(self.atom(a)),
            Stmt::Rule(r) => Stmt::Rule(RuleStmt {
                head: self.atom(r.head),
                body: lits(r.body),
                ..r
            }),
            Stmt::Resource(r) => Stmt::Resource(Resource {
                fields: r
                    .fields
                    .into_iter()
                    .map(|f| FieldAssign {
                        value: self.term(f.value),
                        ..f
                    })
                    .collect(),
                body: r.body.map(lits),
                ..r
            }),
            other => other,
        }
    }

    fn lit(&self, l: Lit) -> Lit {
        l.map(|a| self.atom(a), |t| self.term(t))
    }

    fn atom(&self, mut a: Atom) -> Atom {
        if let Some((body, name)) = pred_parts(&a.pred)
            && body == self.body
        {
            a.pred = match self.via {
                Some(v) => format!("{}::{name}", v.name),
                None => name.to_string(),
            };
        }
        a.args = a.args.into_iter().map(|t| self.term(t)).collect();
        a
    }

    fn term(&self, t: Term) -> Term {
        if let Some((body, name)) = term_parts(&t)
            && body == self.body
        {
            return match self.via {
                Some(v) => Term::Func {
                    name: crate::address::SCOPED.into(),
                    args: vec![v.scope.clone(), str_term(name)],
                },
                None => str_term(name),
            };
        }
        match t {
            Term::Func { name, args } => Term::Func {
                name,
                args: args.into_iter().map(|t| self.term(t)).collect(),
            },
            Term::List(xs) => Term::List(xs.into_iter().map(|t| self.term(t)).collect()),
            Term::Obj(m) => Term::Obj(m.into_iter().map(|(k, v)| (k, self.term(v))).collect()),
            Term::ListComp { item, body } => Term::ListComp {
                item: Box::new(self.term(*item)),
                body: body.into_iter().map(|l| self.lit(l)).collect(),
            },
            other => other,
        }
    }
}

/// The first read in `stmts` of an item of a definition `open` does not
/// hold: its definition, its name and where it is read.
pub(super) fn unreached(
    stmts: &[Stmt],
    open: &dyn Fn(&str) -> bool,
) -> Option<(String, String, Span)> {
    let find = Find { open };
    stmts.iter().find_map(|s| match s {
        Stmt::Fact(a) => find.atom(a),
        Stmt::Rule(r) => find
            .atom(&r.head)
            .or_else(|| r.body.iter().find_map(|l| find.lit(l, r.head.span))),
        Stmt::Resource(r) => r
            .fields
            .iter()
            .find_map(|f| find.term(&f.value).map(|(b, n)| (b, n, f.span)))
            .or_else(|| r.body.iter().flatten().find_map(|l| find.lit(l, r.span))),
        _ => None,
    })
}

/// [`unreached`]'s walk.
struct Find<'a> {
    open: &'a dyn Fn(&str) -> bool,
}

impl Find<'_> {
    fn atom(&self, a: &Atom) -> Option<(String, String, Span)> {
        if let Some((b, n)) = pred_parts(&a.pred)
            && !(self.open)(b)
        {
            return Some((b.to_string(), n.to_string(), a.span));
        }
        a.args
            .iter()
            .find_map(|t| self.term(t))
            .map(|(b, n)| (b, n, a.span))
    }

    fn lit(&self, l: &Lit, span: Span) -> Option<(String, String, Span)> {
        match l {
            Lit::Pos(a) | Lit::Not(a) => self.atom(a),
            Lit::Eq(x, y)
            | Lit::Neq(x, y)
            | Lit::Gt(x, y)
            | Lit::Ge(x, y)
            | Lit::Lt(x, y)
            | Lit::Le(x, y) => self
                .term(x)
                .or_else(|| self.term(y))
                .map(|(b, n)| (b, n, span)),
        }
    }

    fn term(&self, t: &Term) -> Option<(String, String)> {
        if let Some((b, n)) = term_parts(t)
            && !(self.open)(b)
        {
            return Some((b.to_string(), n.to_string()));
        }
        match t {
            Term::Func { args, .. } | Term::List(args) => args.iter().find_map(|t| self.term(t)),
            Term::Obj(m) => m.values().find_map(|t| self.term(t)),
            Term::ListComp { item, body } => self.term(item).or_else(|| {
                body.iter()
                    .find_map(|l| self.lit(l, Span::default()))
                    .map(|(b, n, _)| (b, n))
            }),
            _ => None,
        }
    }
}

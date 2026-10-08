//! A copy named by its clause (R-191): `resource node "agent-${i}" { .. }
//! where i in 0..agents` makes one copy of `node` per row of the clause,
//! named by the header's term as a provider type's resource is. The
//! component is expanded once, under the scope its header writes
//! (`agent-${i}`), and then given a column: every relation private to it
//! takes the copy's name first, and every scope it writes (`agent-${i}`,
//! `agent-${i}.inner`, `format("agent-${i}.%s", ..)` for a copy inside
//! it named by its own clause) is that name's. A copy with a static name
//! is the case where the name is a constant and the column is folded
//! away: one mechanism.

use crate::ast::{Atom, Lit, Resource, RuleStmt, Stmt, Term};
use crate::value::Value;

/// The scope a header with holes writes, and the term that names each
/// copy in its place: the clause's variable on the user's side (the
/// inputs, the rows, the gate), the gate's on the copy's.
pub(super) struct Named<'a> {
    pub scope: &'a str,
    pub copy: Term,
}

impl Named<'_> {
    /// What `s` says below the scope: `""` for the scope itself, `.x` for
    /// a scope or an address under it; `None` for any other text.
    fn under<'s>(&self, s: &'s str) -> Option<&'s str> {
        let rest = s.strip_prefix(self.scope)?;
        (rest.is_empty() || rest.starts_with('.')).then_some(rest)
    }

    /// Whether `pred` is private to the scope (`agent-${i}::p`, or
    /// `agent-${i}.inner::p` of a copy inside it).
    fn owns(&self, pred: &str) -> bool {
        pred.split_once("::")
            .is_some_and(|(scope, _)| self.under(scope).is_some())
    }

    /// `rest` under the copy's name: the name itself, or `format("%s.x",
    /// name, args..)`.
    fn scope_term(&self, rest: &str, args: Vec<Term>) -> Term {
        if rest.is_empty() && args.is_empty() {
            return self.copy.clone();
        }
        let mut all = vec![
            Term::Val(Value::Str(format!("%s{rest}"))),
            self.copy.clone(),
        ];
        all.extend(args);
        Term::Func {
            name: crate::ir::FORMAT.into(),
            args: all,
        }
    }

    pub fn stmt(&self, s: Stmt) -> Stmt {
        let lits = |ls: Vec<Lit>| ls.into_iter().map(|l| self.lit(l)).collect();
        match s {
            Stmt::Fact(a) => Stmt::Fact(self.atom(a)),
            Stmt::Rule(r) => Stmt::Rule(RuleStmt {
                head: self.atom(r.head),
                body: lits(r.body),
            }),
            Stmt::Resource(r) => Stmt::Resource(Resource {
                typ: self.term(r.typ),
                name: self.term(r.name),
                fields: r
                    .fields
                    .into_iter()
                    .map(|f| crate::ast::FieldAssign {
                        value: self.term(f.value),
                        ..f
                    })
                    .collect(),
                body: r.body.map(lits),
                ..r
            }),
            Stmt::Extern(mut e) if self.owns(&e.pred) => {
                e.arity += 1;
                Stmt::Extern(e)
            }
            Stmt::Mixed(mut e) if self.owns(&e.pred) => {
                e.arity += 1;
                Stmt::Mixed(e)
            }
            other => other,
        }
    }

    fn lit(&self, l: Lit) -> Lit {
        l.map(|a| self.atom(a), |t| self.term(t))
    }

    fn atom(&self, mut a: Atom) -> Atom {
        let own = self.owns(&a.pred);
        a.args = a.args.into_iter().map(|t| self.term(t)).collect();
        if own {
            a.args.insert(0, self.copy.clone());
        }
        a
    }

    fn term(&self, t: Term) -> Term {
        match t {
            Term::Func { ref name, .. }
                if name == super::ABSOLUTE || super::lexical::is_mark(&t) =>
            {
                t
            }
            Term::Val(Value::Str(s)) => match self.under(&s) {
                Some(rest) => self.scope_term(rest, Vec::new()),
                None => Term::Val(Value::Str(s)),
            },
            // A template a copy inside this one scoped, `format(
            // "agent-${i}.%s", N)`: `format("%s.%s", copy, N)`.
            Term::Func { name, mut args } if name == crate::ir::FORMAT => {
                let rest = match args.first() {
                    Some(Term::Val(Value::Str(tmpl))) => self.under(tmpl).map(str::to_string),
                    _ => None,
                };
                if rest.is_some() {
                    args.remove(0);
                }
                let args: Vec<Term> = args.into_iter().map(|t| self.term(t)).collect();
                match rest {
                    Some(rest) => self.scope_term(&rest, args),
                    None => Term::Func { name, args },
                }
            }
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

/// Whether a scope's segment is a header with holes, a copy named by its
/// clause: each such segment of a private relation's scope is a column
/// before the relation's own.
fn by_clause(segment: &str) -> bool {
    segment.contains("${")
}

/// A private relation's scope with each segment named by a clause read
/// off the fact's first columns, `agent-${i}.inner` with `"agent-0"` as
/// `agent-0.inner`, and the columns that are the relation's own.
pub fn runtime_scope<'a>(scope: &str, args: &'a [Term]) -> (String, &'a [Term]) {
    let mut used = 0;
    let segments: Vec<String> = crate::ir::path_segments(scope)
        .into_iter()
        .map(|seg| match (by_clause(seg), args.get(used)) {
            (true, Some(Term::Val(Value::Str(name)))) => {
                used += 1;
                name.clone()
            }
            _ => seg.to_string(),
        })
        .collect();
    (segments.join("."), &args[used..])
}

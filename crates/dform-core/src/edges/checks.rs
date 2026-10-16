//! What the columns' types say is wrong of the program's uses (R-34):
//! arithmetic on what is no number (`n + 1`, `n` a string), a comparison
//! of two types that are never equal, `x in o` over an object (R-58), a
//! field read of a value whose type has none (R-185), a reference compared
//! with a string. Each is collected by the walk and judged once the
//! columns are settled; `x = r.p` through a reference takes, before that,
//! the type the schema gives the attribute.

use super::columns::Settle;
use super::lattice::{self, shown_term, shown_var};
use super::shape::*;
use super::*;

/// One side of a comparison or an arithmetic operation: the term as
/// written, its value's node when it is a variable, the node of the
/// reference whose attribute it reads (`r.p`), and its site when it is a
/// literal the readings read.
#[derive(Clone)]
pub(super) struct Side {
    pub(super) term: Term,
    pub(super) node: Option<usize>,
    pub(super) base: Option<usize>,
    pub(super) site: Option<usize>,
}

/// A comparison or an arithmetic operation, checked once the columns are
/// settled.
pub(super) struct Check {
    pub(super) op: &'static str,
    pub(super) sides: [Side; 2],
    pub(super) span: Span,
}

/// `x in l` over `l`, a variable's node (or an object literal, `None`),
/// checked once the program's objects and lists are known.
pub(super) struct Member {
    pub(super) list: Option<(usize, String)>,
    pub(super) item: Term,
    pub(super) span: Span,
}

/// `x.p` on the variable `var` (its node `node`), checked once the
/// columns are settled: a field is read of an object (R-185).
pub(super) struct Field {
    pub(super) node: usize,
    pub(super) var: String,
    pub(super) path: String,
    pub(super) span: Span,
}

/// `x = r.p` with `r` (the variable `var`, its node `node`) a reference:
/// `x` (its node `into`) takes the type the schema gives the attribute of
/// the resource `r` names, once `r`'s type is known.
pub(super) struct Through {
    pub(super) node: usize,
    pub(super) var: String,
    pub(super) path: String,
    pub(super) into: usize,
    pub(super) span: Span,
}

/// What the walk found to check.
#[derive(Default)]
pub(super) struct Checks {
    pub(super) checks: Vec<Check>,
    pub(super) members: Vec<Member>,
    pub(super) fields: Vec<Field>,
    pub(super) throughs: Vec<Through>,
    /// What each value holds of the two collections: an object (where one
    /// is given to it), a list.
    pub(super) holds: Vec<(usize, Option<Span>, bool)>,
    /// Said as the walk finds it: a reference compared with a string.
    pub(super) said: Vec<Diagnostic>,
}

impl Checks {
    /// What collection `t`, given to the node `n` at `span`, is: an
    /// object or a list literal, or a call whose result is declared one.
    pub(super) fn shape(&mut self, n: usize, t: &Term, span: Span) {
        let (object, list) = match t {
            Term::Obj(_) | Term::Val(Value::Obj(_)) => (true, false),
            Term::List(_) | Term::Val(Value::List(_)) | Term::ListComp { .. } => (false, true),
            Term::Func { name, .. } => match crate::functions::get(name).map(|f| f.ret.trim()) {
                Some(ret) if ret.starts_with('{') => (true, false),
                Some(ret) if ret.starts_with("list") || ret.starts_with("set") => (false, true),
                _ => return,
            },
            _ => return,
        };
        self.holds.push((n, object.then_some(span), list));
    }
}

impl Graph<'_> {
    /// Each `x = r.p` with `r` a reference: `x` takes the type the schema
    /// gives the attribute of the resource `r` names.
    pub(super) fn throughs(&mut self) {
        for t in std::mem::take(&mut self.checks.throughs) {
            let r = self.find(t.node);
            let ty = self.edges[r]
                .iter()
                .find_map(|e| match (e.role, e.shape.ty(self.schema)) {
                    (Role::Reads, _) => None,
                    (_, Some(Ty::Ref(u))) => self.through(&u, &t.path),
                    _ => None,
                });
            if let Some(ty) = ty {
                let what = format!("{}.{}", shown_var(&t.var), t.path);
                let role = Role::Types { prints: false };
                self.edge(t.into, Shape::Ty(ty), what, t.span, role);
            }
        }
    }

    /// The type the schema gives `path` read through a reference to a
    /// `typ` (one type), hop by hop where the path passes another
    /// reference: `cidr` of a `net.vpc`, `vpc.cidr` of a `net.subnet`.
    fn through(&self, typ: &str, path: &str) -> Option<Ty> {
        if typ.contains('|') {
            return None;
        }
        if let Some(a) = self.schema.attr(typ, path) {
            return Some(Ty::parse(&a.ty));
        }
        let keys = crate::address::path_keys(path);
        (1..keys.len()).rev().find_map(|i| {
            let head = keys[..i].join(".");
            match Ty::parse(&self.schema.attr(typ, &head)?.ty) {
                Ty::Ref(u) => self.through(&u, &keys[i..].join(".")),
                _ => None,
            }
        })
    }
}

impl Settle<'_, '_> {
    /// What is wrong of the checks, in the order the column types say it:
    /// the comparisons and arithmetic, `x in o` over an object, what was
    /// said as the walk found it, the field reads.
    pub(super) fn judge(&mut self, checks: Checks) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        for c in &checks.checks {
            out.extend(self.check(c));
        }
        let holds = self.holds(&checks.holds);
        for m in &checks.members {
            out.extend(self.over_object(m, &holds));
        }
        out.extend(checks.said);
        for f in &checks.fields {
            out.extend(self.field_of(f));
        }
        out
    }

    /// The settled type of the value `n`.
    fn type_of(&mut self, n: usize) -> Option<Ty> {
        let r = self.g.find(n);
        self.settled.get(&r).cloned().flatten()
    }

    /// The type of a side of a comparison, as the columns settled: a
    /// variable's, a literal's kind as its site reads it, a reference's
    /// attribute's, a call's result's.
    fn side_type(&mut self, s: &Side) -> Option<Ty> {
        if let Some(n) = s.node {
            return self.type_of(n);
        }
        match self.side_term(s) {
            Term::Val(v) => lattice::kind(&v),
            // `s.vpc.cidr`: the attribute of the resource `s.vpc` names.
            Term::Func { name, args } if name == "__path" => match (args.as_slice(), s.base) {
                ([Term::Var(_), Term::Val(Value::Str(path))], Some(n)) => match self.type_of(n)? {
                    Ty::Ref(u) => self.g.through(&u, path),
                    _ => None,
                },
                _ => None,
            },
            t => lattice::term_type(&t).map(|(t, _)| t),
        }
    }

    /// A side's term, as its site reads it.
    fn side_term(&self, s: &Side) -> Term {
        match s.site {
            Some(i) => super::columns::read_as(&self.readings[i], s.term.clone()),
            None => s.term.clone(),
        }
    }

    /// The comparison or arithmetic `c`: arithmetic is on numbers,
    /// quantities and times; a comparison of what is never equal is an
    /// error.
    fn check(&mut self, c: &Check) -> Vec<Diagnostic> {
        let [a, b] = &c.sides;
        let (ta, tb) = (self.side_type(a), self.side_type(b));
        let written = format!("`{} {} {}`", shown_term(&a.term), c.op, shown_term(&b.term));
        if matches!(c.op, "+" | "-" | "*" | "/" | "%") {
            return [(&a.term, &ta), (&b.term, &tb)]
                .into_iter()
                .filter_map(|(t, ty)| lattice::not_a_number(&written, t, ty.as_ref()?, c.span))
                .collect();
        }
        let (xa, xb) = (self.side_term(a), self.side_term(b));
        lattice::compared(&written, (&xa, ta.as_ref()), (&xb, tb.as_ref()), c.span)
            .into_iter()
            .collect()
    }

    /// What each value holds, by its root: an object (where one is given
    /// to it), a list.
    fn holds(&mut self, holds: &[(usize, Option<Span>, bool)]) -> BTreeMap<usize, Held> {
        let mut out: BTreeMap<usize, Held> = BTreeMap::new();
        for &(n, object, list) in holds {
            let r = self.g.find(n);
            let mine = out.entry(r).or_default();
            *mine = (mine.0.or(object), mine.1 || list);
        }
        out
    }

    /// `x in o` (`m`) over an object: its entries are matched by a
    /// pattern, `(k, v) in o` (R-58).
    fn over_object(&mut self, m: &Member, holds: &BTreeMap<usize, Held>) -> Option<Diagnostic> {
        let o = match &m.list {
            None => "{..}".to_string(),
            Some((n, v)) => {
                let r = self.g.find(*n);
                if !matches!(holds.get(&r), Some((Some(_), false))) {
                    return None;
                }
                shown_var(v)
            }
        };
        Some(lattice::over_object(&shown_term(&m.item), &o, m.span))
    }

    /// `x.p` where `x` joins a relation's column whose type has no fields
    /// (a string, a number, a list, a reference): the error naming the
    /// column and its type, where it read nothing at run time (R-185).
    fn field_of(&mut self, f: &Field) -> Option<Diagnostic> {
        let ty = self.type_of(f.node)?;
        if !lattice::fieldless(&ty) {
            return None;
        }
        let r = self.g.find(f.node);
        let col = self.relation_column(r)?;
        lattice::no_field(&shown_var(&f.var), &f.path, &ty, &col, f.span)
    }
}

/// What a value holds of the two collections: an object (where one is
/// given to it), a list.
type Held = (Option<Span>, bool);

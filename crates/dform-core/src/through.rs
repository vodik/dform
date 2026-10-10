//! A reference is a reference everywhere (R-185, its follow-up): what the
//! schema types `ref(T)` is read through and compared as the resource it
//! names, never as an address string.
//!
//! - A read through a reference attribute reads the resource it names:
//!   `s.vpc.cidr`, lowered `attr(net.subnet, S, "vpc", Vpc),
//!   __path(Vpc, "cidr")`, is `v.cidr where v = s.vpc`: `ref(net.vpc, V,
//!   "") = Vpc, attr(net.vpc, V, "cidr", Cidr)`, one hop per reference on
//!   the path (`a.b.c.x`), so the read is an ordinary one (`why` shows it,
//!   and the read stratifies above what it reads).
//! - A variable a type binds (`v in net.vpc`: `want(net.vpc, V)`) holds the
//!   resource's address, and where it meets a reference (an attribute the
//!   schema types `ref(T)`, an element of a `list(ref(T))`, a comparison
//!   with either) it is the reference, `ref(net.vpc, V, "")`, so `s.vpc ==
//!   v` holds for the same resource.
//!
//! A reference whose type the program does not fix (an attribute of `x in
//! resource`) is not read through here: the engine's field read says so at
//! the rule.

use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::schema::Schema;
use crate::types::Ty;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// What a rule's variable holds, as far as references go.
#[derive(Clone, Debug, PartialEq)]
enum Held {
    /// A reference to a `T` (an attribute the schema types `ref(T)`).
    Ref(String),
    /// A list or set of them (`list(ref(T))`, `set(ref(T))`).
    Refs(String),
    /// The address of a `T`, as a type binds it (`want(T, V)`).
    Addr(String),
}

/// `rules`, each read through a reference attribute a hop to the
/// resource, and each address meeting a reference that reference.
pub fn through(rules: Vec<RuleStmt>, schema: &Schema) -> Vec<RuleStmt> {
    rules
        .into_iter()
        .map(|r| Rule::new(&r, schema).rewrite(r))
        .collect()
}

/// One rule's rewrite: what its variables hold, the hops made so far and
/// the names taken.
struct Rule<'a> {
    schema: &'a Schema,
    held: BTreeMap<String, Held>,
    /// The variable bound to each (reference, attribute) read.
    hops: BTreeMap<(String, String), String>,
    /// The address variable each reference variable is taken apart into.
    addrs: BTreeMap<String, String>,
    names: BTreeSet<String>,
}

impl<'a> Rule<'a> {
    fn new(r: &RuleStmt, schema: &'a Schema) -> Rule<'a> {
        let mut rule = Rule {
            schema,
            held: BTreeMap::new(),
            hops: BTreeMap::new(),
            addrs: BTreeMap::new(),
            names: BTreeSet::new(),
        };
        let mut names = BTreeSet::new();
        vars_in_atom(&r.head, &mut names);
        for l in &r.body {
            vars_in_lit(l, &mut names);
        }
        rule.names = names;
        // To a fixpoint: an element of a list of references is one.
        loop {
            let before = rule.held.len();
            for l in &r.body {
                rule.note(l);
            }
            if rule.held.len() == before {
                break;
            }
        }
        rule
    }

    /// What the positive literal `l` says its variables hold.
    fn note(&mut self, l: &Lit) {
        let Lit::Pos(a) = l else {
            return;
        };
        match (a.pred.as_str(), a.args.as_slice()) {
            ("want", [Term::Val(Value::Str(t)), Term::Var(v)]) => {
                self.hold(v, Held::Addr(t.clone()));
            }
            (
                "attr",
                [
                    Term::Val(Value::Str(t)),
                    addr,
                    Term::Val(Value::Str(p)),
                    value,
                ],
            ) => {
                if let Term::Var(v) = addr {
                    self.hold(v, Held::Addr(t.clone()));
                }
                if let (Term::Var(v), Some(h)) = (value, self.attribute(t, p)) {
                    self.hold(v, h);
                }
            }
            ("member", [Term::Var(l), .., Term::Var(v)]) => {
                if let Some(Held::Refs(t)) = self.held.get(l).cloned() {
                    self.hold(v, Held::Ref(t));
                }
            }
            _ => {}
        }
    }

    fn hold(&mut self, v: &str, h: Held) {
        self.held.entry(v.to_string()).or_insert(h);
    }

    /// What the attribute `p` of a `t` holds when the schema types it a
    /// reference or a list of them.
    fn attribute(&self, t: &str, p: &str) -> Option<Held> {
        match Ty::parse(&self.schema.attr(t, p)?.ty) {
            Ty::Ref(u) if !u.contains('|') => Some(Held::Ref(u)),
            Ty::List(inner) => match *inner {
                Ty::Ref(u) if !u.contains('|') => Some(Held::Refs(u)),
                _ => None,
            },
            _ => None,
        }
    }

    fn rewrite(mut self, mut r: RuleStmt) -> RuleStmt {
        if self.held.is_empty() {
            return r;
        }
        let mut body = Vec::with_capacity(r.body.len());
        for l in std::mem::take(&mut r.body) {
            let mut pre = Vec::new();
            let l = self.lit(l, &mut pre);
            body.extend(pre);
            body.push(l);
        }
        let mut pre = Vec::new();
        let args = std::mem::take(&mut r.head.args);
        r.head.args = args.into_iter().map(|t| self.term(t, &mut pre)).collect();
        body.extend(pre);
        r.body = body;
        r
    }

    fn lit(&mut self, l: Lit, pre: &mut Vec<Lit>) -> Lit {
        match l {
            Lit::Pos(a) => {
                // What it binds, read through now (`s in p.cluster.subnets`).
                let l = Lit::Pos(self.atom(a, pre));
                self.note(&l);
                l
            }
            Lit::Not(a) => Lit::Not(self.atom(a, pre)),
            Lit::Eq(a, b) => {
                let (a, b) = self.compared(a, b, pre);
                Lit::Eq(a, b)
            }
            Lit::Neq(a, b) => {
                let (a, b) = self.compared(a, b, pre);
                Lit::Neq(a, b)
            }
            Lit::Gt(a, b) => Lit::Gt(self.term(a, pre), self.term(b, pre)),
            Lit::Ge(a, b) => Lit::Ge(self.term(a, pre), self.term(b, pre)),
            Lit::Lt(a, b) => Lit::Lt(self.term(a, pre), self.term(b, pre)),
            Lit::Le(a, b) => Lit::Le(self.term(a, pre), self.term(b, pre)),
        }
    }

    fn atom(&mut self, mut a: Atom, pre: &mut Vec<Lit>) -> Atom {
        let args = std::mem::take(&mut a.args);
        a.args = args.into_iter().map(|t| self.term(t, pre)).collect();
        if let Some(record) = a.record.take() {
            a.record = Some(
                record
                    .into_iter()
                    .map(|(k, t)| (k, self.term(t, pre)))
                    .collect(),
            );
        }
        match (a.pred.as_str(), a.args.as_mut_slice()) {
            // `s.vpc == v`, the join `attr(net.subnet, S, "vpc", V)`: the
            // attribute holds the reference.
            ("attr", [Term::Val(Value::Str(t)), _, Term::Val(Value::Str(p)), value]) => {
                if let Some(Held::Ref(_)) = self.attribute(t, p) {
                    self.as_reference(value);
                }
            }
            // `v in [s.vpc]`, `v in s.peers`: an element of references.
            ("member", [list, .., item]) if self.references(list) => self.as_reference(item),
            _ => {}
        }
        a
    }

    /// Whether `t` holds references: a list or set of them, or a list
    /// literal of one.
    fn references(&self, t: &Term) -> bool {
        match t {
            Term::Var(v) => matches!(self.held.get(v), Some(Held::Refs(_))),
            Term::List(xs) => xs.iter().any(
                |x| matches!(x, Term::Var(v) if matches!(self.held.get(v), Some(Held::Ref(_)))),
            ),
            _ => false,
        }
    }

    /// `a == b`, `a != b`: an address compared with a reference is the
    /// reference.
    fn compared(&mut self, a: Term, b: Term, pre: &mut Vec<Lit>) -> (Term, Term) {
        let (mut a, mut b) = (self.term(a, pre), self.term(b, pre));
        let reference = |r: &Rule, t: &Term| matches!(t, Term::Var(v) if matches!(r.held.get(v), Some(Held::Ref(_))));
        if reference(self, &a) {
            self.as_reference(&mut b);
        } else if reference(self, &b) {
            self.as_reference(&mut a);
        }
        (a, b)
    }

    /// An address variable where a reference is: `ref(T, V, "")`.
    fn as_reference(&self, t: &mut Term) {
        if let Term::Var(v) = t
            && let Some(Held::Addr(typ)) = self.held.get(v)
        {
            *t = reference(typ, Term::Var(v.clone()));
        }
    }

    /// `t`, each read through a reference a hop to the resource.
    fn term(&mut self, t: Term, pre: &mut Vec<Lit>) -> Term {
        match t {
            Term::Func { name, args } => {
                let args: Vec<Term> = args.into_iter().map(|a| self.term(a, pre)).collect();
                match (name.as_str(), args.as_slice()) {
                    ("__path", [Term::Var(v), Term::Val(Value::Str(path))]) => {
                        match self.hop(v, path, pre) {
                            Some(t) => t,
                            None => Term::Func { name, args },
                        }
                    }
                    _ => Term::Func { name, args },
                }
            }
            Term::List(xs) => Term::List(xs.into_iter().map(|x| self.term(x, pre)).collect()),
            Term::Obj(m) => Term::Obj(m.into_iter().map(|(k, x)| (k, self.term(x, pre))).collect()),
            t => t,
        }
    }

    /// `__path(V, "q.rest")` with `V` a reference to a `T`: `ref(T, A, "")
    /// = V, attr(T, A, "q", W)` before the literal, and `__path(W,
    /// "rest")` (or `W`) in its place, itself read through when `q` is a
    /// reference too.
    fn hop(&mut self, v: &str, path: &str, pre: &mut Vec<Lit>) -> Option<Term> {
        let Some(Held::Ref(typ)) = self.held.get(v).cloned() else {
            return None;
        };
        let keys = crate::address::path_keys(path);
        let (first, rest) = keys.split_first()?;
        let key = (v.to_string(), first.clone());
        let w = match self.hops.get(&key) {
            Some(w) => w.clone(),
            None => {
                let addr = match self.addrs.get(v) {
                    Some(a) => a.clone(),
                    None => {
                        let a = self.fresh(&format!("{v}Ref"));
                        pre.push(Lit::Eq(reference(&typ, Term::Var(a.clone())), var(v)));
                        self.held.insert(a.clone(), Held::Addr(typ.clone()));
                        self.addrs.insert(v.to_string(), a.clone());
                        a
                    }
                };
                let w = self.fresh(&capitalised(first));
                pre.push(Lit::Pos(Atom {
                    pred: "attr".into(),
                    args: vec![
                        Term::Val(Value::Str(typ.clone())),
                        var(&addr),
                        Term::Val(Value::Str(first.clone())),
                        var(&w),
                    ],
                    record: None,
                    span: crate::ast::Span::default(),
                }));
                if let Some(h) = self.attribute(&typ, first) {
                    self.held.insert(w.clone(), h);
                }
                self.hops.insert(key, w.clone());
                w
            }
        };
        if rest.is_empty() {
            return Some(var(&w));
        }
        let rest: Vec<String> = rest
            .iter()
            .map(|k| crate::address::path_key(k).into_owned())
            .collect();
        let rest = rest.join(".");
        Some(self.hop(&w, &rest, pre).unwrap_or_else(|| Term::Func {
            name: "__path".into(),
            args: vec![var(&w), Term::Val(Value::Str(rest))],
        }))
    }

    /// A variable name no other in the rule has.
    fn fresh(&mut self, base: &str) -> String {
        let mut name = base.to_string();
        let mut n = 1;
        while self.names.contains(&name) {
            name = format!("{base}{n}");
            n += 1;
        }
        self.names.insert(name.clone());
        name
    }
}

fn var(v: &str) -> Term {
    Term::Var(v.to_string())
}

/// `ref(T, A, "")`: the resource `A` of type `T`.
fn reference(typ: &str, addr: Term) -> Term {
    Term::Func {
        name: crate::address::REF.into(),
        args: vec![
            Term::Val(Value::Str(typ.to_string())),
            addr,
            Term::Val(Value::Str(String::new())),
        ],
    }
}

/// A read's variable as the resolver names one: `cidr` is `Cidr`.
fn capitalised(key: &str) -> String {
    let mut out = String::new();
    let mut up = true;
    for c in key.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(if up {
                c.to_uppercase().collect::<Vec<_>>()
            } else {
                vec![c]
            });
            up = false;
        } else {
            up = true;
        }
    }
    if out.is_empty() { "Value".into() } else { out }
}

fn vars_in_lit(l: &Lit, out: &mut BTreeSet<String>) {
    match l {
        Lit::Pos(a) | Lit::Not(a) => vars_in_atom(a, out),
        Lit::Eq(a, b)
        | Lit::Neq(a, b)
        | Lit::Gt(a, b)
        | Lit::Ge(a, b)
        | Lit::Lt(a, b)
        | Lit::Le(a, b) => {
            vars_in_term(a, out);
            vars_in_term(b, out);
        }
    }
}

fn vars_in_atom(a: &Atom, out: &mut BTreeSet<String>) {
    a.args.iter().for_each(|t| vars_in_term(t, out));
    if let Some(r) = &a.record {
        r.values().for_each(|t| vars_in_term(t, out));
    }
}

fn vars_in_term(t: &Term, out: &mut BTreeSet<String>) {
    match t {
        Term::Var(v) => {
            out.insert(v.clone());
        }
        Term::Func { args, .. } => args.iter().for_each(|a| vars_in_term(a, out)),
        Term::List(xs) => xs.iter().for_each(|x| vars_in_term(x, out)),
        Term::Obj(m) => m.values().for_each(|x| vars_in_term(x, out)),
        Term::ListComp { item, body } => {
            vars_in_term(item, out);
            body.iter().for_each(|l| vars_in_lit(l, out));
        }
        _ => {}
    }
}

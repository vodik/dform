//! A value's type is the edge it reaches (R-192). Every position that
//! holds a value is an edge that types what flows into it: a resource's
//! attribute (the schema's type), a `set`'s target, a typed `let`, input
//! or output, a declared relation's column, a function's parameter, an
//! attribute read through a reference, the other side of a comparison.
//! A literal is read at the type of the edges its value reaches, wherever
//! it is written: in a let, a let with parameters (R-187), a relation's
//! row, an argument, an input's default or an instance's input. Inference
//! unifies every edge a value flows through: a variable joins the columns
//! it is written in, a field read (`x.cpu`) and an element (`x in l`) link
//! a value to the one they are part of, a variable given to a let, an
//! output or an input flows into it one way (R-213), and a literal's type
//! is what the edges its path reaches agree on.
//!
//! One pass over the program's nodes before it is evaluated, once the
//! providers' schemas are known ([`crate::types::read`]), each item walked
//! as the statements `lower` writes for it:
//!
//! 1. Collect: union-find nodes for each relation's columns (a let's
//!    cell is its relation's one column) and each statement's variables,
//!    the links between a value and its part at a path, the edges that
//!    give a node a type, and every literal (a site) at its node and path.
//! 2. Ask, for each site, the types the edges its node reaches give its
//!    path, through the links both ways.
//! 3. Read the literal at the one type they agree on, the reading its
//!    node's (no statement is rewritten: `lower` writes it); two types are
//!    an error naming both edges; a quantity no edge types (`100m`) in a
//!    let is an error naming the let and what would type it.
//!
//! Only the types read from a literal take part (a quantity, a time, a
//! value type read from a string: `inet`, `ip`, `uri`, `oci`, `semver`, a
//! range): a `string` position takes any of them as its text (R-133).

use crate::address::Address;
use crate::ast::{Atom, Program, Span, Stmt, Term, TypeExpr};
use crate::diag::Diagnostic;
use crate::schema::Schema;
use crate::types::{self, Ty};
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One step into a value: an object's field, a list's element, or an
/// object's entry at any key (`(k, v) in o`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Seg {
    Key(String),
    Elem,
    Any,
}

impl Seg {
    /// Whether this step reaches what `other` does: the same, or an entry
    /// at any key beside a field.
    fn meets(&self, other: &Seg) -> bool {
        match (self, other) {
            (Seg::Any, Seg::Key(_) | Seg::Any) | (Seg::Key(_), Seg::Any) => true,
            (a, b) => a == b,
        }
    }
}

/// `q` past `path`, when `path` leads to it.
fn past<'q>(q: &'q [Seg], path: &[Seg]) -> Option<&'q [Seg]> {
    (q.len() >= path.len() && q.iter().zip(path).all(|(a, b)| a.meets(b))).then(|| &q[path.len()..])
}

type Path = Vec<Seg>;

/// A path as a program writes it: `requests.cpu`, `[]` for an element.
fn shown_path(p: &[Seg]) -> String {
    let mut out = String::new();
    for s in p {
        match s {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Elem => out.push_str("[]"),
            Seg::Any => out.push_str("[k]"),
        }
    }
    out
}

/// The keys of a stored path (`spec.template.spec`).
fn keys(path: &str) -> Path {
    crate::address::path_keys(path)
        .into_iter()
        .map(Seg::Key)
        .collect()
}

/// What gives a value its type.
#[derive(Debug, Clone)]
enum Shape {
    /// A resource type's attribute at a path: the schema's type of each
    /// path under it.
    Attr(String, Path),
    /// A declared type (a let's, an input's, an output's, a column's).
    Declared(TypeExpr),
    /// A function's parameter.
    Ty(Ty),
    /// The other side of `+` or `-`, or its result (`base + 50m`): its
    /// type, a duration beside a time.
    Like(usize),
}

impl Shape {
    /// The type this gives the value's part at `q`, if it gives one.
    fn at(&self, schema: &Schema, q: &[Seg]) -> Option<Ty> {
        let ty = match self {
            Shape::Attr(typ, base) => {
                let full: Vec<&Seg> = base.iter().chain(q).collect();
                if full.contains(&&Seg::Any) {
                    return None;
                }
                let path: Vec<&str> = full
                    .iter()
                    .filter_map(|s| match s {
                        Seg::Key(k) => Some(k.as_str()),
                        _ => None,
                    })
                    .collect();
                let mut ty = Ty::parse(&schema.attr(typ, &path.join("."))?.ty);
                // The schema types a list's elements at the list's path:
                // an element past the last key is the list's element.
                let elems = full.iter().rev().take_while(|s| ***s == Seg::Elem).count();
                for _ in 0..elems {
                    ty = match ty {
                        Ty::List(inner) => *inner,
                        ty => ty,
                    };
                }
                ty
            }
            Shape::Declared(t) => declared_at(t, q)?,
            Shape::Ty(t) => ty_at(t, q)?,
            Shape::Like(_) => return None,
        };
        let ty = match ty {
            Ty::Secret(inner) => *inner,
            ty => ty,
        };
        readable(&ty).then_some(ty)
    }
}

/// The type at `q` inside the declared type `t`.
fn declared_at(t: &TypeExpr, q: &[Seg]) -> Option<Ty> {
    let Some((first, rest)) = q.split_first() else {
        return Some(types::of_expr(t));
    };
    match (t, first) {
        (TypeExpr::Apply(n, args), _) if n == "secret" => declared_at(args.first()?, q),
        (TypeExpr::Object(fs), Seg::Key(k)) => {
            declared_at(&fs.iter().find(|(f, _)| f == k)?.1, rest)
        }
        (TypeExpr::Apply(n, args), Seg::Key(_) | Seg::Any) if n == "map" => {
            declared_at(args.first()?, rest)
        }
        (TypeExpr::Apply(n, args), Seg::Elem) if n == "list" || n == "set" => {
            declared_at(args.first()?, rest)
        }
        _ => None,
    }
}

/// The type at `q` inside `t`.
fn ty_at(t: &Ty, q: &[Seg]) -> Option<Ty> {
    let Some((first, rest)) = q.split_first() else {
        return Some(t.clone());
    };
    match (t, first) {
        (Ty::Secret(inner), _) => ty_at(inner, q),
        (Ty::List(inner), Seg::Elem) | (Ty::Map(inner), Seg::Key(_) | Seg::Any) => {
            ty_at(inner, rest)
        }
        _ => None,
    }
}

/// A type a literal is read as: a quantity, a time, a value type read
/// from a string (R-31, R-66, R-134).
fn readable(ty: &Ty) -> bool {
    match ty {
        Ty::Scalar(s) => types::measured(s) || crate::value::is_value_type(s),
        _ => false,
    }
}

/// An edge: what gives the type, and where.
#[derive(Debug, Clone)]
struct Edge {
    shape: Shape,
    what: String,
    span: Span,
}

impl Edge {
    /// The edge as it types the value's part at `q`: an attribute's
    /// named by its whole path.
    fn at(&self, q: &[Seg]) -> Edge {
        let mut e = self.clone();
        if let Shape::Attr(..) = e.shape {
            for s in q {
                if let Seg::Key(k) = s {
                    e.what = format!("{}.{k}", e.what);
                }
            }
        }
        e
    }
}

/// A literal to read at its node's type at `path`.
struct Site {
    node: usize,
    path: Path,
    span: Span,
    holder: Option<Holder>,
}

/// What a literal is written in, as a message names it.
#[derive(Debug, Clone)]
enum Holder {
    /// A let's value: `let limits`.
    Let(String),
    /// Anything else, named: `input cidr of component m`.
    Named(String),
}

/// What became of a site.
#[derive(Debug, Clone)]
enum Reading {
    /// No edge types it.
    Untyped,
    /// One type, and the edge that gave it.
    At(Ty, Edge),
    /// Two edges that disagree.
    Disagree(Ty, Edge, Ty, Edge),
}

/// The scopes of a program: the names each defines, and the one around it.
#[derive(Default)]
struct Scopes {
    defines: BTreeMap<String, BTreeSet<String>>,
    parent: BTreeMap<String, String>,
}

impl Scopes {
    fn of(program: &Program) -> Scopes {
        let mut s = Scopes::default();
        s.collect("", &program.statements);
        s
    }

    fn collect(&mut self, scope: &str, stmts: &[Stmt]) {
        let defs = self.defines.entry(scope.to_string()).or_default();
        for st in stmts {
            match st {
                Stmt::Fact(a) | Stmt::Rule(crate::ast::RuleStmt { head: a, .. }) => {
                    match let_key(a) {
                        Some(k) => defs.insert(k.to_string()),
                        None => defs.insert(a.pred.clone()),
                    };
                }
                Stmt::Input(i) => {
                    defs.insert(i.name.clone());
                }
                Stmt::Output(o) => {
                    defs.insert(format!("{OUTPUT}{}", o.name));
                }
                Stmt::Decl(d) => {
                    defs.insert(d.pred.clone());
                }
                Stmt::Mode(e) | Stmt::Extern(e) | Stmt::RelationInput(e) => {
                    defs.insert(e.pred.clone());
                }
                _ => {}
            }
        }
        for st in stmts {
            if let Stmt::Module(m) = st {
                self.parent.insert(m.name.clone(), scope.to_string());
                self.collect(&m.name, &m.body);
            }
        }
    }

    /// The scope whose `pred` a read in `scope` names, and the name: an
    /// enclosing definition's marked one (R-186), else the nearest scope
    /// outward that defines it, else the program's.
    fn resolve(&self, scope: &str, pred: &str) -> (String, String) {
        if let Some(rest) = pred
            .strip_prefix(crate::modules::LEXICAL)
            .and_then(|r| r.strip_prefix(':'))
            && let Some((body, name)) = rest.rsplit_once(':')
        {
            return (body.to_string(), name.to_string());
        }
        let mut s = scope;
        loop {
            if self.defines.get(s).is_some_and(|d| d.contains(pred)) {
                return (s.to_string(), pred.to_string());
            }
            match self.parent.get(s) {
                Some(p) => s = p,
                None => return (String::new(), pred.to_string()),
            }
        }
    }
}

/// The key `k` of a `let(k, t, rank)` head.
fn let_key(a: &Atom) -> Option<&str> {
    match (a.pred.as_str(), a.args.as_slice()) {
        (crate::modules::LET, [Term::Val(Value::Str(k)), _, _]) => Some(k),
        _ => None,
    }
}

/// The links by their roots: the wholes each part is of, and the parts
/// each whole has, with the path between.
#[derive(Default)]
struct Links {
    wholes: BTreeMap<usize, Vec<(usize, Path)>>,
    parts: BTreeMap<usize, Vec<(usize, Path)>>,
    /// What each value flows into, by their roots.
    flows: BTreeMap<usize, Vec<usize>>,
}

/// The cell of an output `k`, `output:k`: no relation's name.
const OUTPUT: &str = "output:";

/// A column: (scope, relation, arity, index).
type Col = (String, String, usize, usize);

struct Graph<'a> {
    schema: &'a Schema,
    scopes: Scopes,
    parent: Vec<usize>,
    edges: Vec<Vec<Edge>>,
    /// `(whole, path, part)`: the value of `part` is `whole`'s at `path`.
    links: Vec<(usize, Path, usize)>,
    /// `(from, into)`: a variable's value given to a let, an output or an
    /// input (`let apex = env where ..`). One way (R-213): a literal in
    /// `from` reaches what `into` does, but one of `into`'s other values
    /// does not take `from`'s type, so a string let given an enum or a
    /// network in one rule keeps `""` in another a string.
    flows: Vec<(usize, usize)>,
    columns: BTreeMap<Col, usize>,
    /// The statement's variables.
    vars: BTreeMap<String, usize>,
    sites: Vec<Site>,
    /// The second pass: what to read each site as, in the order the first
    /// found them.
    readings: Option<Vec<Reading>>,
    diags: Vec<Diagnostic>,
}

/// Where a term is placed: its scope, the span of what holds it, what it
/// is written in, and whether its literals are sites (not where the
/// attribute's own reading reads them, [`crate::types::read`]).
#[derive(Clone)]
struct At {
    scope: String,
    span: Span,
    holder: Option<Holder>,
    sites: bool,
}

impl<'a> Graph<'a> {
    fn new(schema: &'a Schema, program: &Program) -> Graph<'a> {
        Graph {
            schema,
            scopes: Scopes::of(program),
            parent: Vec::new(),
            edges: Vec::new(),
            links: Vec::new(),
            flows: Vec::new(),
            columns: BTreeMap::new(),
            vars: BTreeMap::new(),
            sites: Vec::new(),
            readings: None,
            diags: Vec::new(),
        }
    }

    fn node(&mut self) -> usize {
        let n = self.parent.len();
        self.parent.push(n);
        self.edges.push(Vec::new());
        n
    }

    fn find(&mut self, mut n: usize) -> usize {
        while self.parent[n] != n {
            self.parent[n] = self.parent[self.parent[n]];
            n = self.parent[n];
        }
        n
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            let (lo, hi) = (a.min(b), a.max(b));
            self.parent[hi] = lo;
            let e = std::mem::take(&mut self.edges[hi]);
            self.edges[lo].extend(e);
        }
    }

    fn edge(&mut self, n: usize, shape: Shape, what: String, span: Span) {
        let n = self.find(n);
        self.edges[n].push(Edge { shape, what, span });
    }

    /// `part` is `whole`'s value at `path`.
    fn link(&mut self, whole: usize, path: Path, part: usize) {
        if path.is_empty() {
            self.union(whole, part);
        } else {
            self.links.push((whole, path, part));
        }
    }

    fn var(&mut self, v: &str) -> usize {
        if let Some(&n) = self.vars.get(v) {
            return n;
        }
        let n = self.node();
        self.vars.insert(v.to_string(), n);
        n
    }

    fn column(&mut self, scope: &str, pred: &str, arity: usize, i: usize) -> usize {
        let (scope, pred) = self.scopes.resolve(scope, pred);
        let c = (scope, pred, arity, i);
        if let Some(&n) = self.columns.get(&c) {
            return n;
        }
        let n = self.node();
        self.columns.insert(c, n);
        n
    }

    /// A function's arguments: each its parameter's type (a `string`
    /// parameter takes a value as its text, R-133); a coeffect's none.
    fn params(&mut self, f: &crate::functions::Function, args: &mut [Term], at: &At) {
        for (i, t) in args.iter_mut().enumerate() {
            let p = f
                .params
                .get(i)
                .or_else(|| f.variadic.then(|| f.params.last()).flatten());
            let n = self.node();
            // A coeffect's argument is the host's to read (`oci.resolve`,
            // `io.read`): its row is keyed by the value as written.
            if let Some(p) = p.filter(|_| !f.coeffect) {
                let ty = Ty::parse(&p.ty);
                if readable(&ty) {
                    let what = format!("`{}`'s argument `{}`", f.name, p.name);
                    self.edge(n, Shape::Ty(ty), what, at.span);
                }
            }
            self.place(n, Vec::new(), t, at);
        }
    }

    /// The node a term's value is: a variable's, a field read's, or a
    /// fresh one it is placed in.
    fn term_node(&mut self, t: &mut Term, at: &At) -> usize {
        match t {
            Term::Var(v) => self.var(v),
            _ => {
                let n = self.node();
                self.place(n, Vec::new(), t, at);
                n
            }
        }
    }

    /// `t` is `n`'s value at `path`.
    fn place(&mut self, n: usize, path: Path, t: &mut Term, at: &At) {
        match t {
            Term::Var(v) => {
                let x = self.var(v);
                self.link(n, path, x);
            }
            Term::Wildcard => {}
            Term::Obj(m) => {
                for (k, x) in m.iter_mut() {
                    // A keyed list's element names its key (`[key]`).
                    if k.starts_with('[') {
                        continue;
                    }
                    let mut p = path.clone();
                    p.push(Seg::Key(k.clone()));
                    self.place(n, p, x, at);
                }
            }
            Term::List(xs) => {
                let mut p = path;
                p.push(Seg::Elem);
                for x in xs {
                    self.place(n, p.clone(), x, at);
                }
            }
            Term::Val(Value::Obj(_) | Value::List(_)) => {
                let Term::Val(v) = std::mem::replace(t, Term::Wildcard) else {
                    unreachable!("matched")
                };
                let mut held = unfold(v);
                self.place(n, path, &mut held, at);
                *t = fold(held);
            }
            Term::Val(_) => self.site(n, path, t, at),
            Term::Func { name, args } => match (name.as_str(), args.as_mut_slice()) {
                // `x.p`: the part of `x` at `p`.
                ("__path", [Term::Var(v), Term::Val(Value::Str(p))]) => {
                    let (x, p) = (self.var(v), keys(p));
                    let part = self.node();
                    self.link(x, p, part);
                    self.link(n, path, part);
                }
                // A computed value read as its position's type at run
                // time ([`types::at_run_time`]): the value is the
                // position's.
                (types::AS, [inner, _]) => self.place(n, path, inner, at),
                // `{ ..a, k: v }`, `[..a, x]`: the spread source and what
                // is written beside it are each the value's (R-192).
                ("add" | "sub", [a, b]) => self.sum(n, path, a, b, at),
                (crate::functions::MERGE | crate::functions::CONCAT, args) => {
                    for a in args {
                        self.place(n, path.clone(), a, at);
                    }
                }
                (types::AMBIGUOUS, _) => self.site(n, path, t, at),
                (name, _) if name == crate::range::LOWERED => self.site(n, path, t, at),
                (name, args) => {
                    if let Some(f) = crate::functions::get(name)
                        && !f.internal
                    {
                        self.params(f, args, at);
                    } else {
                        for a in args {
                            let x = self.node();
                            self.place(x, Vec::new(), a, at);
                        }
                    }
                }
            },
            Term::ListComp { .. } => unreachable!("a literal holds no comprehension"),
        }
    }

    /// `a + b`, `a - b` at `n`'s `path`: each side takes the other's type
    /// and the result's (a duration beside a time).
    fn sum(&mut self, n: usize, path: Path, a: &mut Term, b: &mut Term, at: &At) {
        let (x, y, r) = (self.term_node(a, at), self.term_node(b, at), self.node());
        self.link(n, path, r);
        for (side, other) in [(x, y), (y, x), (x, r), (y, r)] {
            let what = "the other side".to_string();
            self.edge(side, Shape::Like(other), what, at.span);
        }
    }

    /// A literal at `n`'s `path`: found in the first pass, read in the
    /// second.
    fn site(&mut self, n: usize, path: Path, t: &mut Term, at: &At) {
        if !at.sites {
            return;
        }
        let i = self.sites.len();
        let span = ambiguous_span(t).unwrap_or(at.span);
        self.sites.push(Site {
            node: n,
            path,
            span,
            holder: at.holder.clone(),
        });
        let Some(readings) = &self.readings else {
            return;
        };
        let site = &self.sites[i];
        let held = std::mem::replace(t, Term::Wildcard);
        let (read, diag) = match readings[i].clone() {
            Reading::Untyped => match types::ambiguous(&held) {
                Some(text)
                    if matches!(site.holder, Some(Holder::Let(_))) && text.ends_with('m') =>
                {
                    (held.clone(), Some(untyped(text, site)))
                }
                _ => (held, None),
            },
            Reading::At(ty, e) => match types::literal(&ty, held.clone()) {
                Ok(x) => (x, None),
                Err(why) => (held, Some(not_its_type(&why, site, &ty, &e))),
            },
            Reading::Disagree(a, ea, b, eb) => {
                let d = disagree(&held, site, (&a, &ea), (&b, &eb));
                (held, Some(d))
            }
        };
        if let Some(d) = diag {
            // Said once: the reading's error, not the position's too.
            *t = match types::ambiguous(&read) {
                Some(text) => Term::Val(Value::Str(text.to_string())),
                None => read,
            };
            self.diags.push(d);
        } else {
            *t = read;
        }
    }

    /// The types the edges `n` reaches give its value at `q`, each with
    /// the first edge that gave it; what a sum's other side gives only
    /// where nothing else gives one (`base + 50m`, but `t + 1d` leaves the
    /// time `t` a time).
    fn types_at(&mut self, n: usize, q: &[Seg], by: &Links, depth: usize) -> Vec<(Ty, Edge)> {
        let mut strong: Vec<(Ty, Edge)> = Vec::new();
        let mut weak: Vec<(Ty, Edge)> = Vec::new();
        let add = |out: &mut Vec<(Ty, Edge)>, ty: Ty, e: Edge| {
            if !out.iter().any(|(t, _)| *t == ty) {
                out.push((ty, e));
            }
        };
        let mut seen = BTreeSet::new();
        let mut todo = vec![(self.find(n), q.to_vec())];
        while let Some((r, q)) = todo.pop() {
            if !seen.insert((r, q.clone())) || q.len() > 32 {
                continue;
            }
            for e in self.edges[r].clone() {
                match e.shape {
                    // A side of a sum is the other's type, a duration
                    // beside a time.
                    Shape::Like(m) if depth < 4 => {
                        for (ty, e) in self.types_at(m, &q, by, depth + 1) {
                            let ty = match ty {
                                Ty::Scalar(t) if t == "time" => Ty::Scalar("duration".into()),
                                ty => ty,
                            };
                            add(&mut weak, ty, e);
                        }
                    }
                    Shape::Like(_) => {}
                    _ => {
                        if let Some(ty) = e.shape.at(self.schema, &q) {
                            add(&mut strong, ty, e.at(&q));
                        }
                    }
                }
            }
            // What a value flows into, it reaches; not what else flows in.
            for into in by.flows.get(&r).into_iter().flatten() {
                todo.push((*into, q.clone()));
            }
            for (whole, path) in by.wholes.get(&r).into_iter().flatten() {
                let mut p = path.clone();
                p.extend(q.iter().cloned());
                todo.push((*whole, p));
            }
            for (part, path) in by.parts.get(&r).into_iter().flatten() {
                if let Some(rest) = past(&q, path) {
                    todo.push((*part, rest.to_vec()));
                }
            }
        }
        if strong.is_empty() { weak } else { strong }
    }

    fn readings(&mut self) -> Vec<Reading> {
        let mut by = Links::default();
        for (whole, path, part) in std::mem::take(&mut self.links) {
            let (whole, part) = (self.find(whole), self.find(part));
            by.wholes
                .entry(part)
                .or_default()
                .push((whole, path.clone()));
            by.parts.entry(whole).or_default().push((part, path));
        }
        for (from, into) in std::mem::take(&mut self.flows) {
            let (from, into) = (self.find(from), self.find(into));
            if from != into {
                by.flows.entry(from).or_default().push(into);
            }
        }
        (0..self.sites.len())
            .map(|i| {
                let (n, q) = (self.sites[i].node, self.sites[i].path.clone());
                let mut ts = self.types_at(n, &q, &by, 0).into_iter();
                match (ts.next(), ts.next()) {
                    (None, _) => Reading::Untyped,
                    (Some((t, e)), None) => Reading::At(t, e),
                    (Some((a, ea)), Some((b, eb))) => Reading::Disagree(a, ea, b, eb),
                }
            })
            .collect()
    }
}

/// An attribute as a message names it: `k8s.job["j"].spec.x`.
pub(crate) fn shown_attr(typ: &str, name: &Term, path: &str) -> String {
    match name {
        Term::Val(Value::Str(a)) => Address {
            typ: typ.to_string(),
            name: a.clone(),
        }
        .attr(path),
        _ => format!("{typ}.{path}"),
    }
}

/// A constant object or list as terms.
fn unfold(v: Value) -> Term {
    match v {
        Value::Obj(m) => Term::Obj(m.into_iter().map(|(k, v)| (k, unfold(v))).collect()),
        Value::List(xs) => Term::List(xs.into_iter().map(unfold).collect()),
        v => Term::Val(v),
    }
}

/// [`unfold`] back: still a constant.
fn fold(t: Term) -> Term {
    fn value(t: Term) -> Option<Value> {
        match t {
            Term::Val(v) => Some(v),
            Term::Obj(m) => m
                .into_iter()
                .map(|(k, t)| Some((k, value(t)?)))
                .collect::<Option<_>>()
                .map(Value::Obj),
            Term::List(xs) => xs
                .into_iter()
                .map(value)
                .collect::<Option<_>>()
                .map(Value::List),
            _ => None,
        }
    }
    let kept = t.clone();
    value(t).map_or(kept, Term::Val)
}

/// Where an ambiguous quantity literal was written.
fn ambiguous_span(t: &Term) -> Option<Span> {
    types::ambiguous(t)?;
    let Term::Func { args, .. } = t else {
        return None;
    };
    let n = |i: usize| match args.get(i) {
        Some(Term::Val(Value::Int(x))) => u32::try_from(*x).ok(),
        _ => None,
    };
    Some(Span {
        file: n(1)?,
        start: n(2)?,
        end: n(3)?,
        origin: 0,
    })
}

/// How a message names a site: `let limits` at `cpu`.
fn shown_site(site: &Site) -> String {
    let at = match site.path.is_empty() {
        true => String::new(),
        false => format!(" at `{}`", shown_path(&site.path)),
    };
    match &site.holder {
        Some(Holder::Let(k)) => format!("`let {k}`{at}"),
        Some(Holder::Named(n)) => format!("{n}{at}"),
        None => format!("this value{at}"),
    }
}

/// The literal as written.
fn shown_literal(t: &Term) -> String {
    match types::ambiguous(t) {
        Some(text) => text.to_string(),
        None => crate::spell::term(t),
    }
}

/// A site's literal that is not the type `ty` its edge `e` gives it.
fn not_its_type(why: &str, site: &Site, ty: &Ty, e: &Edge) -> Diagnostic {
    Diagnostic::error(site.span, format!("{} {why}", shown_site(site)))
        .with_label(e.span, format!("{ty} here: {}", e.what))
}

/// A site's literal two edges read as two types.
fn disagree(t: &Term, site: &Site, a: (&Ty, &Edge), b: (&Ty, &Edge)) -> Diagnostic {
    let lit = shown_literal(t);
    let within = match &site.holder {
        Some(_) => format!(" in {}", shown_site(site)),
        None => String::new(),
    };
    Diagnostic::error(
        site.span,
        format!(
            "`{lit}`{within} is {} where it reaches {} and {} where it reaches {}: one value \
             has one type",
            a.0, a.1.what, b.0, b.1.what
        ),
    )
    .with_label(a.1.span, format!("{} here", a.0))
    .with_label(b.1.span, format!("{} here", b.0))
    .with_help("give each use its own value, or write the value once per type")
}

/// An ambiguous quantity in a let that nothing it reaches types.
fn untyped(text: &str, site: &Site) -> Diagnostic {
    let k = match &site.holder {
        Some(Holder::Let(k)) => k.as_str(),
        _ => "",
    };
    let mut ty = "cpu".to_string();
    for s in site.path.iter().rev() {
        ty = match s {
            Seg::Key(f) => format!("{{ {f}: {ty} }}"),
            Seg::Elem => format!("list({ty})"),
            Seg::Any => format!("map({ty})"),
        };
    }
    Diagnostic::error(
        site.span,
        format!(
            "`{text}` in {} is millicores in a cpu position and minutes in a duration position, \
             and nothing the let reaches has a type",
            shown_site(site)
        ),
    )
    .with_help(format!(
        "give the let a type, `let {k}: {ty}`, or use it where a cpu or a duration is wanted"
    ))
}

// --- over the program's nodes ---------------------------------------------

use crate::ast::str_term;
use crate::program::node::{
    CheckKind, ClauseId, Coll, CopyKind, ExprId, ExprKind, GoalId, GoalKind, Has, Header, ItemId,
    ItemKind, ListPart, ObjPart, PatternId, PatternKind, Piece, RelArgs, ResourceBody, Setting,
    Step, Target, VarId, Write,
};
use crate::program::{self as prog, Readings};

/// A term as the walk over the nodes places it: a node, a pattern, a
/// variable, the content of an element's write, or a term `lower` writes
/// with no node of its own (a format string, a rank, the `true` a truth
/// test compares with).
#[derive(Clone)]
pub(crate) enum Part {
    Node(ExprId),
    Pat(PatternId),
    Var(VarId),
    /// An object `lower` builds of nodes (a provider's settings).
    Obj(BTreeMap<String, ExprId>),
    /// `{"[key]": k, p: {q: v}}`: an element's write (`items::element_write`),
    /// its key, the path under the element and the value.
    Elem {
        rest: Vec<String>,
        value: ExprId,
    },
    Term(Term),
}

/// The walk over a program's nodes that builds the graph: the edges, the
/// links and the sites the lowered program's statements give, statement
/// by statement in the order `lower` writes them, each site a node's or
/// a term's with no node.
struct Walk<'a, 'p> {
    g: Graph<'a>,
    p: &'p prog::Program,
    /// The second pass: each node's literal as read.
    read: Readings,
    /// The terms with no node of their own a reading changed: none can be
    /// kept, as no node holds them.
    unkeyed: usize,
}

impl<'a, 'p> Walk<'a, 'p> {
    fn new(g: Graph<'a>, p: &'p prog::Program) -> Self {
        Walk {
            g,
            p,
            read: Readings::new(),
            unkeyed: 0,
        }
    }

    fn items(&mut self, ids: &[ItemId], scope: &str) {
        for &id in ids {
            self.g.vars.clear();
            self.item(id, scope);
        }
    }

    /// The statements an item lowers to, each as the walk over the
    /// lowered program places it.
    fn item(&mut self, id: ItemId, scope: &str) {
        let p = self.p;
        let it = &p.items[id];
        let at = At {
            scope: scope.to_string(),
            span: it.span,
            holder: None,
            sites: true,
        };
        match &it.kind {
            ItemKind::Module { path, items, .. } => self.items(items, path),
            ItemKind::Let { .. } => self.let_item(id, &at),
            ItemKind::LetFn {
                name,
                params,
                value,
                clause,
            } => {
                let vars = params.iter().map(|p| p.var).collect::<Vec<_>>();
                self.let_fn(name, &vars, (*value, *clause), &at);
            }
            ItemKind::Rule { head, clause, rank } => self.rule(head, *rank, *clause, &at),
            // `p(cols) :- B, rows`.
            ItemKind::RelationInput {
                rel,
                source: Some(source),
                columns,
                clause,
                ..
            } => {
                let at = At {
                    span: rel.span,
                    ..at
                };
                let parts = columns.iter().map(|v| Part::Var(*v)).collect();
                self.relation(&rel.name, parts, &at);
                self.clause(*clause, &at);
                self.goal(source.rows, &at);
            }
            ItemKind::Check {
                kind,
                message,
                detail,
                clause,
            } => self.check(*kind, (*message, *detail), *clause, &at),
            ItemKind::Input { .. } => self.input(id, &at),
            ItemKind::Output {
                name,
                ty,
                value,
                clause,
            } => self.output((name, ty.as_ref()), *value, *clause, &at),
            ItemKind::Provider { .. } => self.provider(id, scope),
            ItemKind::Decl { rel, columns, .. } => {
                let arity = columns.len();
                for (i, (_, t)) in columns.iter().enumerate() {
                    let Some(t) = t else { continue };
                    let n = self.g.column(scope, &rel.name, arity, i);
                    let what = format!("decl {}", rel.name);
                    self.g.edge(n, Shape::Declared(t.clone()), what, it.span);
                }
            }
            ItemKind::Resource { .. } => self.resource(id, &at),
            ItemKind::Set { writes, clause } => {
                for w in writes {
                    self.g.vars.clear();
                    self.write(w, *clause, scope);
                }
            }
            ItemKind::SetFrom { .. } => self.set_from(id, &at),
            ItemKind::Copy { .. } => self.copy(id, &at),
            _ => {}
        }
    }

    /// `p(a, b) [@r] [where B]`: `p(a, b[, "r"]) :- B, reads`.
    fn rule(
        &mut self,
        head: &prog::node::Head,
        rank: Option<crate::ast::Rank>,
        clause: Option<ClauseId>,
        at: &At,
    ) {
        let at = At {
            span: head.rel.span,
            ..at.clone()
        };
        let mut parts = self.rel_parts(&head.args);
        parts.extend(rank.map(|r| Part::Term(str_term(r.name()))));
        self.relation(&head.rel.name, parts, &at);
        self.clause(clause, &at);
        let exprs = self.rel_exprs(&head.args);
        self.reads(&exprs, &at);
    }

    /// `let k[: T] = v [where B]`: `let("k", v, r) :- B, reads`, `v` given
    /// to the let's cell; its type the cell's (`decl k(k: T)`).
    fn let_item(&mut self, id: ItemId, at: &At) {
        let p = self.p;
        let ItemKind::Let {
            name,
            ty,
            value,
            clause,
            declares,
            ..
        } = &p.items[id].kind
        else {
            unreachable!("a let")
        };
        let n = self.g.column(&at.scope, name, 1, 0);
        let holder = At {
            holder: Some(Holder::Let(name.clone())),
            ..at.clone()
        };
        self.give(n, *value, &holder);
        self.clause(*clause, at);
        self.reads(&[*value], at);
        if let (true, Some(t)) = (*declares, ty) {
            let what = format!("decl {name}");
            self.g.edge(n, Shape::Declared(t.clone()), what, at.span);
        }
    }

    /// `let f(a, b) = v [where B]`: `f(A, B, v) :- demand(A, B), B, reads`,
    /// a relation's columns.
    fn let_fn(&mut self, name: &str, params: &[VarId], body: (ExprId, Option<ClauseId>), at: &At) {
        let (value, clause) = body;
        let arity = params.len() + 1;
        for (i, v) in params.iter().enumerate() {
            let n = self.g.column(&at.scope, name, arity, i);
            self.place_part(n, Vec::new(), Part::Var(*v), at);
        }
        let n = self.g.column(&at.scope, name, arity, arity - 1);
        self.place(n, Vec::new(), value, at);
        let seed = params.iter().map(|v| Part::Var(*v)).collect();
        self.atom(&crate::demand::of(name), seed, at.span, at);
        self.clause(clause, at);
        self.reads(&[value], at);
    }

    /// `deny "m" [{ .. }] [where B]`: `deny(m[, d]) :- B, reads`.
    fn check(
        &mut self,
        kind: CheckKind,
        (message, detail): (ExprId, Option<ExprId>),
        clause: Option<ClauseId>,
        at: &At,
    ) {
        let pred = match kind {
            CheckKind::Deny => "deny",
            CheckKind::Warn => "warn",
        };
        let mut exprs = vec![message];
        exprs.extend(detail);
        let parts = exprs.iter().map(|e| Part::Node(*e)).collect();
        self.relation(pred, parts, at);
        self.clause(clause, at);
        self.reads(&exprs, at);
    }

    /// `input k: T [= d]`: the input's cell, of its type, `d` one of its
    /// values; declared under clauses (R-104), `__declared(..) :- G`.
    fn input(&mut self, id: ItemId, at: &At) {
        let p = self.p;
        let ItemKind::Input {
            name,
            ty,
            default,
            guard,
            rows,
            ..
        } = &p.items[id].kind
        else {
            unreachable!("an input")
        };
        let n = self.g.column(&at.scope, name, 1, 0);
        let what = format!("input {name}");
        self.g.edge(n, Shape::Declared(ty.clone()), what, at.span);
        if let Some(d) = default {
            self.place(n, Vec::new(), *d, at);
        }
        if rows.is_some() {
            self.g.vars.clear();
            self.clause(*guard, at);
        }
    }

    /// `output k[: T] = v [where B]`: its declaration and its value are
    /// two statements; its cell is one.
    fn output(
        &mut self,
        (name, ty): (&str, Option<&TypeExpr>),
        value: Option<ExprId>,
        clause: Option<ClauseId>,
        at: &At,
    ) {
        let n = self.g.column(&at.scope, &format!("{OUTPUT}{name}"), 1, 0);
        if let Some(t) = ty {
            let what = format!("output {name}");
            self.g.edge(n, Shape::Declared(t.clone()), what, at.span);
        }
        if let Some(v) = value {
            self.g.vars.clear();
            self.give(n, v, at);
            self.clause(clause, at);
            self.reads(&[v], at);
        }
    }

    /// `set from doc [where B]`: `arg("input", "", P, V, r) :- B, rows`, a
    /// path no edge reads.
    fn set_from(&mut self, id: ItemId, at: &At) {
        let p = self.p;
        let ItemKind::SetFrom {
            source,
            path,
            value,
            rank,
            clause,
        } = &p.items[id].kind
        else {
            unreachable!("a set from a document")
        };
        let rank = rank.unwrap_or(crate::ast::Rank::Normal);
        let parts = vec![
            Part::Term(str_term(crate::modules::INPUT)),
            Part::Term(str_term("")),
            Part::Var(*path),
            Part::Var(*value),
            Part::Term(str_term(rank.name())),
        ];
        self.relation("arg", parts, at);
        self.clause(*clause, at);
        self.goal(source.rows, at);
    }

    /// `resource C name { k = v }`, `use M { k = v }`: its body, then each
    /// `k = v` the module's input `k` (`cfg.cidr = ..` a field of `cfg`).
    fn copy(&mut self, id: ItemId, at: &At) {
        let p = self.p;
        let ItemKind::Copy {
            kind,
            module,
            named,
            inputs,
            clause,
            ..
        } = &p.items[id].kind
        else {
            unreachable!("a copy")
        };
        self.clause(*clause, at);
        let mut exprs: Vec<ExprId> = named.iter().copied().collect();
        exprs.extend(inputs.iter().map(|(_, _, e)| *e));
        self.reads(&exprs, at);
        let kind = match kind {
            CopyKind::Use => "module",
            CopyKind::Component => "component",
        };
        for (k, span, e) in inputs {
            let mut path = keys(k);
            let Some(Seg::Key(input)) = (!path.is_empty()).then(|| path.remove(0)) else {
                continue;
            };
            let n = self.g.column(module, &input, 1, 0);
            let holder = Holder::Named(format!("input {k} of {kind} {module}"));
            let at = At {
                span: *span,
                holder: Some(holder),
                ..at.clone()
            };
            match path.is_empty() {
                true => self.give(n, *e, &at),
                false => self.place(n, path, *e, &at),
            }
        }
    }

    /// `resource T n { p = v .. } [where B]`: each entry is its
    /// attribute's (read by the attribute's own reading), then the body:
    /// B's literals, the entries' reads, the name's.
    fn resource(&mut self, id: ItemId, at: &At) {
        let p = self.p;
        let ItemKind::Resource {
            typ,
            name,
            body,
            clause,
            ..
        } = &p.items[id].kind
        else {
            unreachable!("a resource")
        };
        let name_t = header_term(name);
        let fields = At {
            sites: false,
            ..at.clone()
        };
        let mut reads = Vec::new();
        for (key, part, span) in entries(p, body) {
            let n = self.g.node();
            let what = shown_attr(&typ.name, &name_t, &key);
            self.g
                .edge(n, Shape::Attr(typ.name.clone(), keys(&key)), what, span);
            let at = At {
                span,
                ..fields.clone()
            };
            self.place_part(n, Vec::new(), part, &at);
        }
        match body {
            ResourceBody::Block(entries) => reads.extend(entries.iter().map(|e| e.value)),
            ResourceBody::Value(v) => reads.push(*v),
        }
        self.clause(*clause, at);
        if let Header::Interp(e) = name {
            reads.push(*e);
        }
        self.reads(&reads, at);
    }

    /// A provider's `provider_config("n", {k: v})` and its accounts, each a
    /// statement of its own.
    fn provider(&mut self, id: ItemId, scope: &str) {
        let p = self.p;
        let it = &p.items[id];
        let ItemKind::Provider {
            name,
            settings,
            clause,
            declared,
            ..
        } = &it.kind
        else {
            unreachable!("a provider")
        };
        let at = |span| At {
            scope: scope.to_string(),
            span,
            holder: None,
            sites: true,
        };
        let mut config = BTreeMap::new();
        let mut reads = Vec::new();
        for s in settings {
            if let Setting::Value { key, value, .. } = s {
                config.insert(key.clone(), *value);
                reads.push(*value);
            }
        }
        if !config.is_empty() || clause.is_some() {
            let a = at(it.span);
            let parts = vec![Part::Term(str_term(name)), Part::Obj(config)];
            self.relation("provider_config", parts, &a);
            self.clause(*clause, &a);
            self.reads(&reads, &a);
        }
        for s in settings {
            if let Setting::Account {
                value,
                clause: c,
                span,
            } = s
            {
                self.g.vars.clear();
                let a = at(*span);
                let parts = vec![Part::Term(str_term(name)), Part::Node(*value)];
                self.relation(crate::plugin::providers::EXPECT_ACCOUNT, parts, &a);
                self.clause(*c, &a);
                self.reads(&[*value], &a);
            }
        }
        if declared.is_some() {
            self.g.vars.clear();
            self.clause(*clause, &at(it.span));
        }
    }

    /// One line of a `set`: `arg(T, A, "p", v, r) :- B, reads`.
    fn write(&mut self, w: &Write, clause: Option<ClauseId>, scope: &str) {
        let at = At {
            scope: scope.to_string(),
            span: w.span,
            holder: None,
            sites: true,
        };
        let line = Line::of(w);
        let cell = line.cell.clone();
        self.line_head(w, line, clause, &at);
        self.clause(clause, &at);
        let mut exprs = cell;
        exprs.push(w.value);
        self.reads(&exprs, &at);
    }

    /// A line's head: through a variable of several types (`set x.p = v
    /// where x in resource`) `v` the attribute of each type `x` may be; at
    /// a type named, the attribute's (its literals the attribute's own
    /// reading reads); else (`+=`) a relation's columns.
    fn line_head(&mut self, w: &Write, line: Line, clause: Option<ClauseId>, at: &At) {
        let assign = matches!(w.op, crate::ast::FieldOp::Assign);
        let typ = self.part_term(&line.typ);
        let has_body = clause.is_some() || !self.reads_under(&line.cell, w.value).is_empty();
        let fields = At {
            sites: false,
            ..at.clone()
        };
        let path = &line.path;
        match (assign, &typ) {
            (true, Term::Var(_)) if has_body => {
                let n = self.g.node();
                let r = set_rule(self.p, typ, clause);
                for t in types::set_types(self.g.schema, &r, path) {
                    let what = format!("{t}.{path}");
                    self.g.edge(n, Shape::Attr(t, keys(path)), what, w.span);
                }
                self.place_part(n, Vec::new(), line.value, &fields);
            }
            (true, Term::Val(Value::Str(t))) => {
                let n = self.g.node();
                let what = shown_attr(t, &self.part_term(&line.addr), path);
                self.g
                    .edge(n, Shape::Attr(t.clone(), keys(path)), what, w.span);
                self.place_part(n, Vec::new(), line.value, &fields);
            }
            _ => {
                let pred = if assign { "arg" } else { "arg_add" };
                let rank = match line.ranked {
                    true => Some(w.rank.unwrap_or(crate::ast::Rank::Normal)),
                    false => w.rank,
                };
                let mut parts = vec![line.typ, line.addr, Part::Term(str_term(path)), line.value];
                parts.extend(rank.map(|r| Part::Term(str_term(r.name()))));
                self.relation(pred, parts, at);
            }
        }
    }

    /// The reads `cell`'s nodes and `value` hoist.
    fn reads_under(&self, cell: &[ExprId], value: ExprId) -> Vec<ExprId> {
        let mut out = Vec::new();
        for &e in cell.iter().chain([&value]) {
            self.reads_into(e, &mut out);
        }
        out
    }

    // --- clauses ----------------------------------------------------------

    fn clause(&mut self, c: Option<ClauseId>, at: &At) {
        let Some(c) = c else { return };
        let p = self.p;
        for &g in &p.clauses[c].goals {
            self.goal(g, at);
        }
    }

    /// A goal's literals: the reads its terms hoist, then its own.
    fn goal(&mut self, g: GoalId, at: &At) {
        let p = self.p;
        match &p.goals[g].kind {
            GoalKind::Group { goals, after } => {
                for &each in goals.iter().chain(after) {
                    self.goal(each, at);
                }
            }
            GoalKind::Marked { mark, goal } => {
                self.reads(&[mark.typ, mark.addr], at);
                let parts = vec![
                    Part::Node(mark.typ),
                    Part::Node(mark.addr),
                    Part::Term(str_term(&mark.path)),
                    Part::Term(Term::Wildcard),
                ];
                self.relation(crate::partition::HAS, parts, at);
                self.goal(*goal, at);
            }
            GoalKind::Not {
                clause,
                helper: None,
            } => self.clause(Some(*clause), at),
            // `not __neg_N(ȳ)`, and the helper's body.
            GoalKind::Not {
                clause,
                helper: Some(h),
            } => {
                let pred = format!("__neg_{}", h.number);
                let args = h.args.iter().map(|v| Part::Var(*v)).collect();
                self.relation(&pred, args, at);
                self.clause(Some(*clause), at);
            }
            GoalKind::Fold { var, agg, .. } => {
                self.reads(&[*agg], at);
                let x = self.var(*var);
                let y = self.term_node(Part::Node(*agg), at);
                self.g.union(x, y);
            }
            _ => {
                let exprs = self.goal_exprs(g);
                self.reads(&exprs, at);
                self.literal(g, at);
            }
        }
    }

    /// The literal a goal is, but for its reads.
    fn literal(&mut self, g: GoalId, at: &At) {
        let p = self.p;
        let goal = &p.goals[g];
        match &goal.kind {
            GoalKind::Rel { rel, args } => {
                let parts = self.rel_parts(args);
                self.atom(&rel.name, parts, rel.span, at);
            }
            GoalKind::Bind { pat, value } => {
                if p.exprs[*value].hoisted.is_none() && self.is_read(*value) {
                    self.read_lit(*value, Part::Pat(*pat), goal.span, at);
                } else {
                    let x = self.term_node(Part::Pat(*pat), at);
                    let y = self.term_node(Part::Node(*value), at);
                    self.g.union(x, y);
                }
            }
            // Both sides of a comparison are one type.
            GoalKind::Compare { lhs, ops } => {
                if p.in_place(*lhs) {
                    let span = p.exprs[*lhs].span;
                    self.read_lit(*lhs, Part::Node(ops[0].1), span, at);
                    return;
                }
                let mut prev = *lhs;
                for (_, e) in ops {
                    let x = self.term_node(Part::Node(prev), at);
                    let y = self.term_node(Part::Node(*e), at);
                    self.g.union(x, y);
                    prev = *e;
                }
            }
            GoalKind::Truth(e) => {
                let yes = Part::Term(Term::Val(Value::Bool(true)));
                if p.in_place(*e) {
                    self.read_lit(*e, yes, p.exprs[*e].span, at);
                } else {
                    let x = self.term_node(Part::Node(*e), at);
                    let y = self.term_node(yes, at);
                    self.g.union(x, y);
                }
            }
            GoalKind::Has(Has::Resource { typ, addr }) => {
                let parts = vec![Part::Node(*typ), Part::Node(*addr)];
                self.atom(crate::partition::IDENTITY, parts, goal.span, at);
            }
            GoalKind::Has(Has::Read(e)) => {
                let span = p.exprs[*e].span;
                self.read_lit(*e, Part::Term(Term::Wildcard), span, at);
            }
            GoalKind::Has(Has::Walk { var, value }) => {
                let x = self.var(*var);
                let y = self.term_node(Part::Node(*value), at);
                self.g.union(x, y);
            }
            GoalKind::Member { pat, coll } => self.member(*pat, coll, goal.span, at),
            GoalKind::Fold { .. }
            | GoalKind::Group { .. }
            | GoalKind::Marked { .. }
            | GoalKind::Not { .. } => unreachable!("walked by `goal`"),
        }
    }

    /// `x in c`.
    fn member(&mut self, pat: PatternId, coll: &Coll, span: Span, at: &At) {
        let with = |w: &mut Self, first: Vec<Part>, pred: &str| {
            let mut parts = first;
            parts.extend(w.columns(pat));
            w.atom(pred, parts, span, at);
        };
        match coll {
            Coll::Expr(e) | Coll::Enum { list: e, .. } => {
                with(self, vec![Part::Node(*e)], "member")
            }
            Coll::Type(_) => {}
            Coll::Namespace { ns, typ, .. } => {
                let parts = vec![Part::Term(str_term(ns)), Part::Node(*typ)];
                self.atom("__namespace", parts, span, at);
            }
            Coll::ProviderType { typ, var, .. } => {
                let parts = vec![Part::Term(str_term(typ)), Part::Node(*var)];
                self.atom("__provider_type", parts, span, at);
            }
            Coll::World(t) => {
                let parts = vec![Part::Term(str_term(t)), Part::Pat(pat)];
                self.atom("cloud_exists", parts, span, at);
            }
            Coll::Copies { component, scope } => {
                let parts = vec![
                    Part::Term(str_term(component)),
                    Part::Node(*scope),
                    Part::Pat(pat),
                ];
                self.atom(crate::modules::INSTANCE_OF, parts, span, at);
            }
            Coll::Each(e) | Coll::TypeOf(e) => {
                let x = self.term_node(Part::Pat(pat), at);
                let y = self.term_node(Part::Node(*e), at);
                self.g.union(x, y);
            }
        }
    }

    /// The columns a membership's pattern takes: a key and a value, or
    /// the element.
    fn columns(&self, pat: PatternId) -> Vec<Part> {
        match &self.p.patterns[pat].kind {
            PatternKind::Tuple { elems, rest: None } if elems.len() == 2 => {
                elems.iter().map(|p| Part::Pat(*p)).collect()
            }
            _ => vec![Part::Pat(pat)],
        }
    }

    /// An atom of a body: a function's arguments its parameters', an
    /// attribute's value the schema's, a membership's element its
    /// collection's, any other relation's arguments its columns.
    fn atom(&mut self, pred: &str, parts: Vec<Part>, span: Span, at: &At) {
        if let Some(f) = crate::functions::get(pred) {
            self.params(f, parts, &At { span, ..at.clone() });
            return;
        }
        match (pred, parts.as_slice()) {
            ("attr", [typ, addr, path, v]) => {
                let (Term::Val(Value::Str(typ)), Term::Val(Value::Str(path))) =
                    (self.part_term(typ), self.part_term(path))
                else {
                    return self.relation(pred, parts, at);
                };
                let n = self.g.node();
                let what = shown_attr(&typ, &self.part_term(addr), &path);
                self.g.edge(n, Shape::Attr(typ, keys(&path)), what, span);
                self.place_part(n, Vec::new(), v.clone(), at);
            }
            // `x in l`: `x` is an element of `l`.
            ("member", [list, item]) => {
                let l = self.term_node(list.clone(), at);
                let x = self.term_node(item.clone(), at);
                self.g.link(l, vec![Seg::Elem], x);
            }
            // `(k, v) in o`: `v` is `o`'s at `k`; `l[i]`, an element.
            ("member", [object, key, value]) => {
                let seg = match self.part_term(key) {
                    Term::Val(Value::Str(k)) => Seg::Key(k),
                    Term::Val(Value::Int(_)) => Seg::Elem,
                    _ => Seg::Any,
                };
                let o = self.term_node(object.clone(), at);
                let v = self.term_node(value.clone(), at);
                self.g.link(o, vec![seg], v);
            }
            ("want", _) => {}
            _ => self.relation(pred, parts, at),
        }
    }

    /// A relation's atom: each argument is its column's.
    fn relation(&mut self, pred: &str, parts: Vec<Part>, at: &At) {
        let arity = parts.len();
        for (i, part) in parts.into_iter().enumerate() {
            let n = self.g.column(&at.scope, pred, arity, i);
            self.place_part(n, Vec::new(), part, at);
        }
    }

    /// A function's arguments: each its parameter's type (a `string`
    /// parameter takes a value as its text, R-133); a coeffect's none.
    fn params(&mut self, f: &crate::functions::Function, parts: Vec<Part>, at: &At) {
        for (i, part) in parts.into_iter().enumerate() {
            let p = f
                .params
                .get(i)
                .or_else(|| f.variadic.then(|| f.params.last()).flatten());
            let n = self.g.node();
            if let Some(p) = p.filter(|_| !f.coeffect) {
                let ty = Ty::parse(&p.ty);
                if readable(&ty) {
                    let what = format!("`{}`'s argument `{}`", f.name, p.name);
                    self.g.edge(n, Shape::Ty(ty), what, at.span);
                }
            }
            self.place_part(n, Vec::new(), part, at);
        }
    }

    // --- reads ------------------------------------------------------------

    /// The literals of the reads `exprs` hoist, in the order `lower`
    /// writes them.
    fn reads(&mut self, exprs: &[ExprId], at: &At) {
        let mut ids = Vec::new();
        for &e in exprs {
            self.reads_into(e, &mut ids);
        }
        for id in ids {
            self.read(id, at);
        }
    }

    /// The reads in term position under `e`, `e`'s own last; none of a
    /// comprehension's, which are its body's.
    fn reads_into(&self, e: ExprId, out: &mut Vec<ExprId>) {
        let x = &self.p.exprs[e];
        if x.hoisted.is_none() {
            for c in self.children(e) {
                self.reads_into(c, out);
            }
            return;
        }
        let first = match &x.kind {
            ExprKind::Field { base, path } => match path.as_slice() {
                [Step::Keyed { key, typ, .. }] => vec![*base, *key, *typ],
                _ if self.is_read(e) => self.read_args(e),
                _ => self.children(e),
            },
            _ if self.is_read(e) => self.read_args(e),
            _ => self.children(e),
        };
        for c in first {
            self.reads_into(c, out);
        }
        out.push(e);
    }

    /// A hoisted read's literals, its variable in its value column.
    fn read(&mut self, e: ExprId, at: &At) {
        let p = self.p;
        let x = &p.exprs[e];
        let v = x.hoisted.expect("a read");
        if let ExprKind::Field { base, path } = &x.kind
            && let [
                Step::Keyed {
                    key,
                    typ,
                    list,
                    keys,
                    field,
                },
            ] = path.as_slice()
        {
            // `member(L, V), type_list_key(T, "l", Keys), Keys = [Key],
            // __path(V, Key) == k`.
            self.atom("member", vec![Part::Node(*base), Part::Var(v)], x.span, at);
            let parts = vec![
                Part::Node(*typ),
                Part::Term(str_term(list)),
                Part::Var(*keys),
            ];
            self.atom("type_list_key", parts, x.span, at);
            let k = self.var(*keys);
            let l = self.term_node(Part::Term(Term::List(vec![self.var_term(*field)])), at);
            self.g.union(k, l);
            let path = Term::Func {
                name: "__path".into(),
                args: vec![self.var_term(v), self.var_term(*field)],
            };
            let a = self.term_node(Part::Term(path), at);
            let b = self.term_node(Part::Node(*key), at);
            self.g.union(a, b);
            return;
        }
        if self.is_read(e) {
            self.read_lit(e, Part::Var(v), x.span, at);
        } else {
            let a = self.var(v);
            let b = match self.var_of_kind(e) {
                Some(name) => self.g.var(&name),
                None => {
                    let b = self.g.node();
                    self.kind(b, Vec::new(), e, at);
                    b
                }
            };
            self.g.union(a, b);
        }
    }

    /// Whether `e` is a read `lower` writes as an atom (`k(V)`, `attr(..)`,
    /// `member(L, i, V)`, `output(..)`, `cloud_attr(..)`, a lookup).
    fn is_read(&self, e: ExprId) -> bool {
        let p = self.p;
        match &p.exprs[e].kind {
            ExprKind::Value { .. }
            | ExprKind::Output { .. }
            | ExprKind::World { .. }
            | ExprKind::Lookup { .. }
            | ExprKind::Address { .. } => true,
            ExprKind::Field { base, path } => matches!(
                (&p.exprs[*base].kind, path.as_slice()),
                (ExprKind::Resource { .. }, [Step::Field(..)]) | (_, [Step::Index(_)])
            ),
            _ => false,
        }
    }

    /// The nodes the atom of the read `e` holds, in the order `lower`
    /// lowers them.
    fn read_args(&self, e: ExprId) -> Vec<ExprId> {
        let p = self.p;
        match &p.exprs[e].kind {
            ExprKind::Field { base, path } => match (&p.exprs[*base].kind, path.as_slice()) {
                (ExprKind::Resource { addr, .. }, [Step::Field(..)]) => vec![*addr],
                (_, [Step::Index(i)]) => vec![*base, *i],
                _ => Vec::new(),
            },
            ExprKind::Output { copy, of, .. } => {
                let mut out = vec![*copy];
                out.extend(of.as_ref().map(|(_, scope)| *scope));
                out
            }
            ExprKind::World { addr, .. } => vec![*addr],
            ExprKind::Lookup { args, .. } => args.clone(),
            ExprKind::Address { of, .. } => self.read_args(*of),
            _ => Vec::new(),
        }
    }

    /// The atom of the read `e`, `held` in its value column.
    fn read_lit(&mut self, e: ExprId, held: Part, span: Span, at: &At) {
        let p = self.p;
        match &p.exprs[e].kind {
            ExprKind::Value { decl, via } => {
                self.atom(&via.relation(&decl.name), vec![held], span, at);
            }
            ExprKind::Field { base, path } => match (&p.exprs[*base].kind, path.as_slice()) {
                (ExprKind::Resource { typ, addr }, [Step::Field(f, _)]) => {
                    let parts = vec![
                        Part::Term(str_term(&typ.name)),
                        Part::Node(*addr),
                        Part::Term(str_term(f)),
                        held,
                    ];
                    self.atom("attr", parts, span, at);
                }
                (_, [Step::Index(i)]) => {
                    let parts = vec![Part::Node(*base), Part::Node(*i), held];
                    self.atom("member", parts, span, at);
                }
                _ => unreachable!("a read"),
            },
            ExprKind::Output { copy, key, of } => {
                if let Some((of, scope)) = of {
                    let parts = vec![
                        Part::Term(str_term(of)),
                        Part::Node(*scope),
                        Part::Node(*copy),
                    ];
                    let at_scope = p.exprs[*scope].span;
                    self.atom(crate::modules::INSTANCE_OF, parts, at_scope, at);
                }
                let parts = vec![Part::Node(*copy), Part::Term(str_term(key)), held];
                self.atom("output", parts, span, at);
            }
            ExprKind::World { typ, addr, path } => {
                let parts = vec![
                    Part::Term(str_term(&typ.name)),
                    Part::Node(*addr),
                    Part::Term(str_term(&prog::lower::stored(path))),
                    held,
                ];
                self.atom("cloud_attr", parts, span, at);
            }
            ExprKind::Lookup { rel, args, out, .. } => {
                let mut parts: Vec<Part> = args.iter().map(|a| Part::Node(*a)).collect();
                parts.insert(*out, held);
                self.atom(&rel.name, parts, span, at);
            }
            // `ref(T, __scope(V), "")` in the reference's value column.
            ExprKind::Address { of, typ } => {
                let Part::Var(v) = held else {
                    unreachable!("an address is read into its variable")
                };
                let r = Term::Func {
                    name: crate::address::REF.into(),
                    args: vec![
                        str_term(&typ.name),
                        Term::Func {
                            name: crate::modules::ABSOLUTE.into(),
                            args: vec![self.var_term(v)],
                        },
                        str_term(""),
                    ],
                };
                self.read_lit(*of, Part::Term(r), p.exprs[*of].span, at);
            }
            _ => unreachable!("a read"),
        }
    }

    /// The nodes a goal's terms are, in the order `lower` lowers them.
    fn goal_exprs(&self, g: GoalId) -> Vec<ExprId> {
        let p = self.p;
        let mut out = Vec::new();
        match &p.goals[g].kind {
            GoalKind::Rel { args, .. } => out = self.rel_exprs(args),
            GoalKind::Bind { pat, value } => {
                if p.exprs[*value].hoisted.is_none() && self.is_read(*value) {
                    out = self.read_args(*value);
                    self.pat_exprs(*pat, &mut out);
                } else {
                    self.pat_exprs(*pat, &mut out);
                    out.push(*value);
                }
            }
            GoalKind::Compare { lhs, ops } => {
                if p.in_place(*lhs) {
                    out.push(ops[0].1);
                    out.extend(self.read_args(*lhs));
                } else {
                    out.push(*lhs);
                    out.extend(ops.iter().map(|(_, e)| *e));
                }
            }
            GoalKind::Truth(e) => match p.in_place(*e) {
                true => out = self.read_args(*e),
                false => out.push(*e),
            },
            GoalKind::Has(Has::Resource { typ, addr }) => out = vec![*typ, *addr],
            GoalKind::Has(Has::Read(e)) => out = self.read_args(*e),
            GoalKind::Has(Has::Walk { value, .. }) => out.push(*value),
            GoalKind::Member { pat, coll } => match coll {
                Coll::Expr(e) | Coll::Enum { list: e, .. } => {
                    out.push(*e);
                    for c in self.columns(*pat) {
                        if let Part::Pat(c) = c {
                            self.pat_exprs(c, &mut out);
                        }
                    }
                }
                Coll::Type(t) => {
                    out.push(*t);
                    self.pat_exprs(*pat, &mut out);
                }
                Coll::Namespace { typ, enumerate, .. }
                | Coll::ProviderType {
                    var: typ,
                    enumerate,
                    ..
                } => {
                    out.push(*typ);
                    if *enumerate {
                        self.pat_exprs(*pat, &mut out);
                    }
                }
                Coll::World(_) => self.pat_exprs(*pat, &mut out),
                Coll::Copies { scope, .. } => {
                    out.push(*scope);
                    self.pat_exprs(*pat, &mut out);
                }
                Coll::Each(e) | Coll::TypeOf(e) => {
                    self.pat_exprs(*pat, &mut out);
                    out.push(*e);
                }
            },
            GoalKind::Fold { agg, .. } => out.push(*agg),
            GoalKind::Group { .. } | GoalKind::Marked { .. } | GoalKind::Not { .. } => {}
        }
        out
    }

    /// The nodes a relation's arguments are.
    fn rel_exprs(&self, args: &RelArgs) -> Vec<ExprId> {
        let mut out = Vec::new();
        match args {
            RelArgs::Positional(ps) => ps.iter().for_each(|p| self.pat_exprs(*p, &mut out)),
            RelArgs::Record(fs) => fs.iter().for_each(|(_, p)| self.pat_exprs(*p, &mut out)),
        }
        out
    }

    /// A relation's arguments: positional ones its columns; a record's
    /// none.
    fn rel_parts(&self, args: &RelArgs) -> Vec<Part> {
        match args {
            RelArgs::Positional(ps) => ps.iter().map(|p| Part::Pat(*p)).collect(),
            RelArgs::Record(_) => Vec::new(),
        }
    }

    fn pat_exprs(&self, pat: PatternId, out: &mut Vec<ExprId>) {
        match &self.p.patterns[pat].kind {
            PatternKind::Expr(e) => out.push(*e),
            PatternKind::Tuple { elems, .. } => {
                elems.iter().for_each(|p| self.pat_exprs(*p, out));
            }
            PatternKind::Object { fields, .. } => {
                fields.iter().for_each(|(_, _, p)| self.pat_exprs(*p, out));
            }
            PatternKind::Hole | PatternKind::Bind(_) => {}
        }
    }

    /// The nodes `e`'s own term lowers, in order.
    fn children(&self, e: ExprId) -> Vec<ExprId> {
        let p = self.p;
        match &p.exprs[e].kind {
            ExprKind::Binary { lhs, rhs, .. } => vec![*lhs, *rhs],
            ExprKind::Neg(a) | ExprKind::Alias(a) => vec![*a],
            ExprKind::Interp { parts } => parts
                .iter()
                .filter_map(|p| match p {
                    Piece::Hole(e) => Some(*e),
                    Piece::Text(_) => None,
                })
                .collect(),
            ExprKind::Object { parts } => parts
                .iter()
                .flat_map(|p| match p {
                    ObjPart::Computed { key, value } => vec![*key, *value],
                    ObjPart::Field { value, .. } => vec![*value],
                    ObjPart::Spread(e) => vec![*e],
                })
                .collect(),
            ExprKind::List { parts } => parts
                .iter()
                .map(|p| match p {
                    ListPart::Elem(e) | ListPart::Spread(e) => *e,
                })
                .collect(),
            ExprKind::Range { lo, hi, .. } => vec![*lo, *hi],
            ExprKind::As { value, .. } => vec![*value],
            ExprKind::Field { base, .. } => vec![*base],
            ExprKind::RefOf(r) => {
                let res = match &p.exprs[*r].kind {
                    ExprKind::Field { base, .. } => *base,
                    _ => *r,
                };
                match &p.exprs[res].kind {
                    ExprKind::Resource { addr, .. } => vec![*addr],
                    _ => Vec::new(),
                }
            }
            ExprKind::Scoped { name, .. } => vec![*name],
            ExprKind::Call { args, .. } => args.iter().map(|a| a.value).collect(),
            ExprKind::Aggregate { item, .. } => vec![*item],
            _ => Vec::new(),
        }
    }

    // --- terms ------------------------------------------------------------

    /// The variable a node's term is, if it is one: a variable, or a read
    /// in term position (its variable).
    fn var_of(&self, e: ExprId) -> Option<String> {
        match self.p.exprs[e].hoisted {
            Some(v) => Some(self.p.vars[v].lowered.clone()),
            None => self.var_of_kind(e),
        }
    }

    /// [`Self::var_of`] of `e`'s own term, its hoisting aside.
    fn var_of_kind(&self, e: ExprId) -> Option<String> {
        match &self.p.exprs[e].kind {
            ExprKind::Var(v) => Some(self.p.vars[*v].lowered.clone()),
            ExprKind::Alias(a) => self.var_of(*a),
            _ => None,
        }
    }

    fn var(&mut self, v: VarId) -> usize {
        let name = self.p.vars[v].lowered.clone();
        self.g.var(&name)
    }

    fn var_term(&self, v: VarId) -> Term {
        Term::Var(self.p.vars[v].lowered.clone())
    }

    /// The term a part lowers to, its readings aside.
    fn part_term(&self, part: &Part) -> Term {
        match part {
            Part::Node(e) => prog::lower_expr(self.p, *e).0,
            Part::Var(v) => self.var_term(*v),
            Part::Term(t) => t.clone(),
            Part::Pat(_) | Part::Obj(_) | Part::Elem { .. } => Term::Wildcard,
        }
    }

    /// The node a part's value is: a variable's, or a fresh one it is
    /// placed in.
    fn term_node(&mut self, part: Part, at: &At) -> usize {
        let var = match &part {
            Part::Node(e) => self.var_of(*e),
            Part::Var(v) => Some(self.p.vars[*v].lowered.clone()),
            Part::Pat(p) => match &self.p.patterns[*p].kind {
                PatternKind::Bind(v) => Some(self.p.vars[*v].lowered.clone()),
                PatternKind::Expr(e) => self.var_of(*e),
                _ => None,
            },
            Part::Term(Term::Var(v)) => Some(v.clone()),
            _ => None,
        };
        match var {
            Some(v) => self.g.var(&v),
            None => {
                let n = self.g.node();
                self.place_part(n, Vec::new(), part, at);
                n
            }
        }
    }

    /// `e` is one of the values of the cell `n` (a let's, an output's,
    /// an input's): a variable flows into it, one way; anything else is
    /// placed in it.
    fn give(&mut self, n: usize, e: ExprId, at: &At) {
        let var = self.var_of(e).or_else(|| match &self.p.exprs[e].kind {
            ExprKind::As { value, .. } if self.p.exprs[e].hoisted.is_none() => self.var_of(*value),
            _ => None,
        });
        match var {
            Some(v) => {
                let x = self.g.var(&v);
                self.g.flows.push((x, n));
            }
            None => self.place(n, Vec::new(), e, at),
        }
    }

    fn place_part(&mut self, n: usize, path: Path, part: Part, at: &At) {
        match part {
            Part::Node(e) => self.place(n, path, e, at),
            Part::Var(v) => {
                let x = self.var(v);
                self.g.link(n, path, x);
            }
            Part::Pat(p) => self.place_pat(n, path, p, at),
            Part::Obj(m) => {
                for (k, e) in m {
                    if k.starts_with('[') {
                        continue;
                    }
                    let mut q = path.clone();
                    q.push(Seg::Key(k));
                    self.place(n, q, e, at);
                }
            }
            // `{"[key]": k, ..}`: the content under the element, a value
            // that is no object held whole under `[value]`, which no
            // path reads.
            Part::Elem { rest, value } => {
                let mut q = path;
                q.extend(rest.iter().cloned().map(Seg::Key));
                let object = matches!(
                    prog::lower_expr(self.p, value).0,
                    Term::Obj(_) | Term::Val(Value::Obj(_))
                );
                if !rest.is_empty() || object {
                    self.place(n, q, value, at);
                }
            }
            Part::Term(mut t) => {
                let before = self.g.readings.is_some().then(|| t.clone());
                self.g.place(n, path, &mut t, at);
                if before.is_some_and(|b| b != t) {
                    self.unkeyed += 1;
                }
            }
        }
    }

    fn place_pat(&mut self, n: usize, path: Path, pat: PatternId, at: &At) {
        let p = self.p;
        match &p.patterns[pat].kind {
            PatternKind::Hole => {}
            PatternKind::Bind(v) => {
                let x = self.var(*v);
                self.g.link(n, path, x);
            }
            PatternKind::Expr(e) => self.place(n, path, *e, at),
            PatternKind::Tuple { elems, .. } => {
                for el in elems {
                    let mut q = path.clone();
                    q.push(Seg::Elem);
                    self.place_pat(n, q, *el, at);
                }
            }
            PatternKind::Object { fields, .. } => {
                let fields: BTreeMap<&str, PatternId> =
                    fields.iter().map(|(k, _, p)| (k.as_str(), *p)).collect();
                for (k, f) in fields {
                    if k.starts_with('[') {
                        continue;
                    }
                    let mut q = path.clone();
                    q.push(Seg::Key(k.to_string()));
                    self.place_pat(n, q, f, at);
                }
            }
        }
    }

    /// `e` is `n`'s value at `path`.
    fn place(&mut self, n: usize, path: Path, e: ExprId, at: &At) {
        match self.p.exprs[e].hoisted {
            Some(v) => {
                let x = self.var(v);
                self.g.link(n, path, x);
            }
            None => self.kind(n, path, e, at),
        }
    }

    /// [`Self::place`] of `e`'s own term, its hoisting aside.
    fn kind(&mut self, n: usize, path: Path, e: ExprId, at: &At) {
        let p = self.p;
        let node = |e: &ExprId| Part::Node(*e);
        match &p.exprs[e].kind {
            ExprKind::Var(v) => {
                let x = self.var(*v);
                self.g.link(n, path, x);
            }
            ExprKind::Lit(_) | ExprKind::Quantity { .. } | ExprKind::Range { .. } => {
                self.leaf(n, path, e, at)
            }
            ExprKind::Binary { op, lhs, rhs } => {
                self.func(n, path, op.function(), vec![node(lhs), node(rhs)], at)
            }
            ExprKind::Neg(a) => {
                let zero = Part::Term(Term::Val(Value::Int(0)));
                self.func(n, path, "sub", vec![zero, node(a)], at)
            }
            ExprKind::Interp { parts } => self.interp(n, path, parts, at),
            ExprKind::Object { parts } => self.object(n, path, parts, at),
            ExprKind::List { parts } => {
                let mut elem = path.clone();
                elem.push(Seg::Elem);
                for part in parts {
                    match part {
                        ListPart::Elem(x) => self.place(n, elem.clone(), *x, at),
                        // `[..a, x]`: the spread source and what is
                        // written beside it are each the value's (R-192).
                        ListPart::Spread(x) => self.place(n, path.clone(), *x, at),
                    }
                }
            }
            // A computed value read as its position's type at run time:
            // the value is the position's.
            ExprKind::As { value, .. } => self.place(n, path, *value, at),
            ExprKind::Field { base, path: steps } => self.field(n, path, *base, steps, at),
            ExprKind::RefOf(_) | ExprKind::Scoped { .. } => self.named(n, path, e, at),
            ExprKind::Alias(x) => self.place(n, path, *x, at),
            // `[x | B]`: each `x` is an element.
            ExprKind::Comprehension { item, clause } => {
                self.clause(Some(*clause), at);
                self.reads(&[*item], at);
                let mut q = path;
                q.push(Seg::Elem);
                self.place(n, q, *item, at);
            }
            ExprKind::Call { callee, args } => {
                let name = match callee {
                    prog::node::Callee::Std(f) => f.name.clone(),
                    prog::node::Callee::Data(n) => n.clone(),
                    c => unreachable!("no builder calls {c:?}"),
                };
                let args = args.iter().map(|a| Part::Node(a.value)).collect();
                self.func(n, path, &name, args, at)
            }
            ExprKind::Aggregate { kind, item } => {
                let name = prog::node::aggregate_name(*kind);
                self.func(n, path, name, vec![node(item)], at)
            }
            _ => {}
        }
    }

    /// `"a-${x}"`: `str.format("a-%s", x)`.
    fn interp(&mut self, n: usize, path: Path, parts: &[Piece], at: &At) {
        let mut f = String::new();
        let mut args = vec![Part::Term(Term::Wildcard)];
        for piece in parts {
            match piece {
                Piece::Text(t) => f.push_str(t),
                Piece::Hole(e) => {
                    f.push_str("%s");
                    args.push(Part::Node(*e));
                }
            }
        }
        args[0] = Part::Term(str_term(&f));
        self.func(n, path, crate::address::FORMAT, args, at)
    }

    /// `x.f.g`: the part of `x` at `f.g`; `x.len`, `__len(x)`.
    fn field(&mut self, n: usize, path: Path, base: ExprId, steps: &[Step], at: &At) {
        if let [Step::Len] = steps {
            return self.func(n, path, crate::address::LEN, vec![Part::Node(base)], at);
        }
        let stored = prog::lower::stored(steps);
        match self.var_of(base) {
            Some(v) => {
                let x = self.g.var(&v);
                let part = self.g.node();
                self.g.link(x, keys(&stored), part);
                self.g.link(n, path, part);
            }
            None => {
                let args = vec![Part::Node(base), Part::Term(str_term(&stored))];
                self.func(n, path, "__path", args, at)
            }
        }
    }

    /// A reference given as a value (`__ref(T, A, "p")`) or a name as a
    /// read reaches it (`__scope(n)`, `__lexical(B, n)`): its arguments
    /// that are nodes as nodes, the rest as written.
    fn named(&mut self, n: usize, path: Path, e: ExprId, at: &At) {
        let Term::Func { name, args: terms } = prog::lower_expr(self.p, e).0 else {
            unreachable!("a reference or a name is a call")
        };
        let mut kids = self.children(e).into_iter();
        let mut args = Vec::new();
        for t in terms {
            match (&t, kids.len()) {
                (Term::Val(Value::Str(_)), _) | (_, 0) => args.push(Part::Term(t)),
                _ => args.push(Part::Node(kids.next().expect("a node"))),
            }
        }
        self.func(n, path, &name, args, at)
    }

    /// `{ a: 1, ..r }`: written fields an object at `path`; any spread
    /// `__merge` of the parts, each the value's; a computed key `__object`.
    fn object(&mut self, n: usize, path: Path, parts: &[ObjPart], at: &At) {
        if parts.iter().any(|p| matches!(p, ObjPart::Computed { .. })) {
            let mut args = Vec::new();
            for part in parts {
                match part {
                    ObjPart::Computed { key, value } => {
                        args.extend([Part::Node(*key), Part::Node(*value)]);
                    }
                    ObjPart::Field { key, value, .. } => {
                        args.extend([Part::Term(str_term(key)), Part::Node(*value)]);
                    }
                    ObjPart::Spread(_) => unreachable!("a computed object spreads nothing"),
                }
            }
            return self.func(n, path, crate::functions::OBJECT, args, at);
        }
        let mut run: Option<BTreeMap<String, ExprId>> = None;
        let mut out: Vec<Part> = Vec::new();
        for part in parts {
            match part {
                ObjPart::Field { key, value, .. } => {
                    run.get_or_insert_default().insert(key.clone(), *value);
                }
                ObjPart::Spread(e) => {
                    out.extend(run.take().map(Part::Obj));
                    out.push(Part::Node(*e));
                }
                ObjPart::Computed { .. } => unreachable!("handled above"),
            }
        }
        out.extend(run.map(Part::Obj));
        for part in out {
            self.place_part(n, path.clone(), part, at);
        }
    }

    /// A call `name(parts)` at `n`'s `path`, as [`Graph::place`] places
    /// the term it lowers to.
    fn func(&mut self, n: usize, path: Path, name: &str, parts: Vec<Part>, at: &At) {
        match (name, parts.as_slice()) {
            ("add" | "sub", [a, b]) => {
                let (a, b) = (a.clone(), b.clone());
                self.sum(n, path, a, b, at)
            }
            (types::AS, [inner, _]) => {
                let inner = inner.clone();
                self.place_part(n, path, inner, at)
            }
            (crate::functions::MERGE | crate::functions::CONCAT, _) => {
                for part in parts {
                    self.place_part(n, path.clone(), part, at);
                }
            }
            _ => match crate::functions::get(name) {
                Some(f) if !f.internal => self.params(f, parts, at),
                _ => {
                    for part in parts {
                        let x = self.g.node();
                        self.place_part(x, Vec::new(), part, at);
                    }
                }
            },
        }
    }

    /// `a + b`, `a - b` at `n`'s `path`: each side takes the other's type
    /// and the result's (a duration beside a time).
    fn sum(&mut self, n: usize, path: Path, a: Part, b: Part, at: &At) {
        let (x, y, r) = (self.term_node(a, at), self.term_node(b, at), self.g.node());
        self.g.link(n, path, r);
        for (side, other) in [(x, y), (y, x), (x, r), (y, r)] {
            let what = "the other side".to_string();
            self.g.edge(side, Shape::Like(other), what, at.span);
        }
    }

    /// A literal, `500m` or a range at `n`'s `path`: a site, its reading
    /// the node's.
    fn leaf(&mut self, n: usize, path: Path, e: ExprId, at: &At) {
        let mut t = prog::lower_expr(self.p, e).0;
        let before = self.g.readings.is_some().then(|| t.clone());
        self.g.place(n, path, &mut t, at);
        if before.is_some_and(|b| b != t) {
            self.read.insert(e, t);
        }
    }
}

/// A line of a `set` as `lower` writes its head `arg(T, A, "p", v, r)`:
/// its type, its address, its path, its value and the nodes of its cell.
struct Line {
    typ: Part,
    addr: Part,
    path: String,
    value: Part,
    cell: Vec<ExprId>,
    /// Whether its rank is written when none is given (an element's, an
    /// input's).
    ranked: bool,
}

impl Line {
    fn of(w: &Write) -> Line {
        match &w.target {
            Target::Attr { typ, addr, path } => Line {
                typ: Part::Node(*typ),
                addr: Part::Node(*addr),
                path: path.clone(),
                value: Part::Node(w.value),
                cell: vec![*typ, *addr],
                ranked: false,
            },
            Target::Element {
                typ,
                addr,
                list,
                key,
                rest,
            } => Line {
                typ: Part::Node(*typ),
                addr: Part::Node(*addr),
                path: list.clone(),
                value: Part::Elem {
                    rest: rest.clone(),
                    value: w.value,
                },
                cell: vec![*typ, *addr, *key],
                ranked: true,
            },
            Target::Input { path } => Line {
                typ: Part::Term(str_term(crate::modules::INPUT)),
                addr: Part::Term(str_term("")),
                path: path.clone(),
                value: Part::Node(w.value),
                cell: Vec::new(),
                ranked: true,
            },
        }
    }
}

/// A resource's name as a message reads it: written, or `None`'s stand-in
/// for an interpolated one.
pub(crate) fn header_term(name: &Header) -> Term {
    match name {
        Header::Bare(n) => str_term(n),
        Header::Literal(s) => str_term(&crate::address::name_segment(s)),
        Header::Interp(_) => Term::Wildcard,
    }
}

/// A resource's entries as `lower` writes them: its block's, or a value's
/// keys (`items::value_entries`), each its key, its value and its span.
pub(crate) fn entries(p: &prog::Program, body: &ResourceBody) -> Vec<(String, Part, Span)> {
    match body {
        ResourceBody::Block(entries) => entries
            .iter()
            .map(|e| (e.path.clone(), Part::Node(e.value), e.span))
            .collect(),
        ResourceBody::Value(v) => {
            let span = p.exprs[*v].span;
            let entry = |k: &str| crate::address::path_join("", k);
            match &p.exprs[*v].kind {
                ExprKind::Object { parts }
                    if p.exprs[*v].hoisted.is_none()
                        && parts.iter().all(|x| matches!(x, ObjPart::Field { .. })) =>
                {
                    let fields: BTreeMap<&str, ExprId> = parts
                        .iter()
                        .filter_map(|x| match x {
                            ObjPart::Field { key, value, .. } => Some((key.as_str(), *value)),
                            _ => None,
                        })
                        .collect();
                    fields
                        .into_iter()
                        .map(|(k, e)| (entry(k), Part::Node(e), span))
                        .collect()
                }
                ExprKind::Lit(Value::Obj(m)) if p.exprs[*v].hoisted.is_none() => m
                    .iter()
                    .map(|(k, x)| (entry(k), Part::Term(Term::Val(x.clone())), span))
                    .collect(),
                _ => vec![(String::new(), Part::Node(*v), span)],
            }
        }
    }
}

/// A `set` through a variable's rule as the schema's types are found
/// from it ([`types::set_types`]): its head's type, its clause's literals.
pub(crate) fn set_rule(
    p: &prog::Program,
    typ: Term,
    clause: Option<ClauseId>,
) -> crate::ast::RuleStmt {
    let body = clause.map(|c| prog::lower_clause(p, c, &[]).0);
    let head = Atom {
        pred: "arg".into(),
        args: vec![typ, Term::Wildcard, Term::Wildcard, Term::Wildcard],
        record: None,
        span: Span::default(),
    };
    crate::ast::RuleStmt::new(head, body.unwrap_or_default())
}

/// Read every literal of the program's nodes at the type of the edges it
/// reaches (R-192), as [`read`] reads the program they lower to (`unread`):
/// each node's reading, the errors, and how many terms with no node a
/// reading changed.
pub fn read_nodes(
    program: &prog::Program,
    unread: &Program,
    schema: &Schema,
) -> (Readings, Vec<Diagnostic>, usize) {
    let mut w = Walk::new(Graph::new(schema, unread), program);
    w.items(&program.roots, "");
    let readings = w.g.readings();
    let mut w = Walk::new(Graph::new(schema, unread), program);
    w.g.readings = Some(readings);
    w.items(&program.roots, "");
    (w.read, w.g.diags, w.unkeyed)
}

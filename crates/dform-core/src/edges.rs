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
//! One pass over the program before it is evaluated, once the providers'
//! schemas are known ([`crate::types::read`]):
//!
//! 1. Collect: union-find nodes for each relation's columns (a let's
//!    cell is its relation's one column) and each statement's variables,
//!    the links between a value and its part at a path, the edges that
//!    give a node a type, and every literal (a site) at its node and path.
//! 2. Ask, for each site, the types the edges its node reaches give its
//!    path, through the links both ways.
//! 3. Read the literal at the one type they agree on; two types are an
//!    error naming both edges; a quantity no edge types (`100m`) in a let
//!    is an error naming the let and what would type it.
//!
//! Only the types read from a literal take part (a quantity, a time, a
//! value type read from a string: `inet`, `ip`, `uri`, `oci`, `semver`, a
//! range): a `string` position takes any of them as its text (R-133).

use crate::address::Address;
use crate::ast::{Atom, Lit, Program, Span, Stmt, Term, TypeExpr};
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

    fn program(&mut self, stmts: &mut [Stmt], scope: &str) {
        for s in stmts {
            self.vars.clear();
            self.stmt(s, scope);
        }
    }

    fn stmt(&mut self, s: &mut Stmt, scope: &str) {
        let kind = match s {
            Stmt::Use(_) => "module",
            _ => "component",
        };
        let at = |span: Span| At {
            scope: scope.to_string(),
            span,
            holder: None,
            sites: true,
        };
        match s {
            Stmt::Fact(a) => self.head(a, &at(a.span)),
            // `set x.p = v where x in resource`: `v` is the attribute of
            // each type `x` may be.
            Stmt::Rule(r)
                if r.head.pred == "arg"
                    && r.head.args.len() >= 4
                    && matches!(r.head.args[0], Term::Var(_)) =>
            {
                let a = at(r.head.span);
                if let Term::Val(Value::Str(path)) = &r.head.args[2] {
                    let n = self.node();
                    for typ in types::set_types(self.schema, r, path) {
                        let what = format!("{typ}.{path}");
                        self.edge(n, Shape::Attr(typ, keys(path)), what, r.head.span);
                    }
                    let at = At {
                        sites: false,
                        ..a.clone()
                    };
                    self.place(n, Vec::new(), &mut r.head.args[3], &at);
                }
                self.body(&mut r.body, &a);
            }
            Stmt::Rule(r) => {
                let a = at(r.head.span);
                self.head(&mut r.head, &a);
                self.body(&mut r.body, &a);
            }
            Stmt::Module(m) => {
                let name = m.name.clone();
                self.program(&mut m.body, &name);
            }
            Stmt::Resource(r) => {
                let Term::Val(Value::Str(typ)) = &r.typ else {
                    return;
                };
                let typ = typ.clone();
                let name = r.name.clone();
                let a = At {
                    sites: false,
                    ..at(r.span)
                };
                for f in &mut r.fields {
                    let n = self.node();
                    let what = shown_attr(&typ, &name, &f.key);
                    self.edge(n, Shape::Attr(typ.clone(), keys(&f.key)), what, f.span);
                    self.place(
                        n,
                        Vec::new(),
                        &mut f.value,
                        &At {
                            span: f.span,
                            ..a.clone()
                        },
                    );
                }
                if let Some(b) = &mut r.body {
                    self.body(b, &at(r.span));
                }
            }
            Stmt::Input(i) => {
                let n = self.column(scope, &i.name, 1, 0);
                let what = format!("input {}", i.name);
                self.edge(n, Shape::Declared(i.ty.clone()), what, i.span);
                if let Some(d) = &mut i.default {
                    self.place(n, Vec::new(), d, &at(i.span));
                }
            }
            // An output's declaration and its value are two statements:
            // its cell is one.
            Stmt::Output(o) => {
                let n = self.column(scope, &format!("{OUTPUT}{}", o.name), 1, 0);
                if let Some(ty) = &o.ty {
                    let what = format!("output {}", o.name);
                    self.edge(n, Shape::Declared(ty.clone()), what, o.span);
                }
                if let Some(v) = &mut o.value {
                    self.give(n, v, &at(o.span));
                }
            }
            Stmt::Decl(d) => {
                let arity = d.fields.len();
                for (i, t) in d.types.iter().enumerate() {
                    let Some(t) = t else { continue };
                    let n = self.column(scope, &d.pred, arity, i);
                    let what = format!("decl {}", d.pred);
                    self.edge(n, Shape::Declared(t.clone()), what, d.span);
                }
            }
            // `resource C name { k = v }`, `use M { k = v }`: each `k = v`
            // is the module's input `k`.
            Stmt::Instance(u) | Stmt::Use(u) => {
                let a = at(u.span);
                if let Some(b) = &mut u.body {
                    self.body(b, &a);
                }
                let module = u.module.clone();
                for (k, t, span) in &mut u.inputs {
                    // `cfg.cidr = ..` gives a field of the input `cfg`.
                    let mut path = keys(k);
                    let Some(Seg::Key(input)) = (!path.is_empty()).then(|| path.remove(0)) else {
                        continue;
                    };
                    let n = self.column(&module, &input, 1, 0);
                    let holder = Holder::Named(format!("input {k} of {kind} {module}"));
                    let at = At {
                        span: *span,
                        holder: Some(holder),
                        ..a.clone()
                    };
                    if path.is_empty() {
                        self.give(n, t, &at);
                    } else {
                        self.place(n, path, t, &at);
                    }
                }
            }
            _ => {}
        }
    }

    /// A rule's head: a let's cell, an attribute's contribution, or a
    /// relation's row.
    fn head(&mut self, a: &mut Atom, at: &At) {
        if let Some(k) = let_key(a).map(str::to_string) {
            let n = self.column(&at.scope, &k, 1, 0);
            let at = At {
                holder: Some(Holder::Let(k)),
                ..at.clone()
            };
            self.give(n, &mut a.args[1], &at);
            return;
        }
        // `output(k, V) :- B`: the value of `output k`.
        if let ("output", [Term::Val(Value::Str(k)), _]) = (a.pred.as_str(), a.args.as_slice()) {
            let n = self.column(&at.scope, &format!("{OUTPUT}{k}"), 1, 0);
            self.give(n, &mut a.args[1], at);
            return;
        }
        // `arg(T, A, P, V, ..)`: a `set` (R-38); its literals the
        // attribute's own reading reads.
        if a.pred == "arg"
            && a.args.len() >= 4
            && let (Term::Val(Value::Str(typ)), Term::Val(Value::Str(path))) =
                (&a.args[0], &a.args[2])
        {
            let (typ, path) = (typ.clone(), path.clone());
            let n = self.node();
            let what = shown_attr(&typ, &a.args[1], &path);
            self.edge(n, Shape::Attr(typ, keys(&path)), what, a.span);
            let at = At {
                sites: false,
                ..at.clone()
            };
            self.place(n, Vec::new(), &mut a.args[3], &at);
            return;
        }
        self.relation(a, at);
    }

    /// A relation's atom: each argument is its column's.
    fn relation(&mut self, a: &mut Atom, at: &At) {
        let arity = a.args.len();
        let pred = a.pred.clone();
        for (i, t) in a.args.iter_mut().enumerate() {
            let n = self.column(&at.scope, &pred, arity, i);
            self.place(n, Vec::new(), t, at);
        }
    }

    fn body(&mut self, body: &mut [Lit], at: &At) {
        for l in body {
            self.lit(l, at);
        }
    }

    fn lit(&mut self, l: &mut Lit, at: &At) {
        match l {
            Lit::Pos(a) | Lit::Not(a) => self.atom(a, at),
            // Both sides of a comparison are one type.
            Lit::Eq(x, y)
            | Lit::Neq(x, y)
            | Lit::Gt(x, y)
            | Lit::Ge(x, y)
            | Lit::Lt(x, y)
            | Lit::Le(x, y) => {
                let a = self.term_node(x, at);
                let b = self.term_node(y, at);
                self.union(a, b);
            }
        }
    }

    fn atom(&mut self, a: &mut Atom, at: &At) {
        if let Some(f) = crate::functions::get(&a.pred) {
            let span = a.span;
            self.params(f, &mut a.args, &At { span, ..at.clone() });
            return;
        }
        match (a.pred.as_str(), a.args.as_mut_slice()) {
            // `attr(T, A, P, V)`: `V` is the attribute's.
            (
                "attr",
                [
                    Term::Val(Value::Str(typ)),
                    addr,
                    Term::Val(Value::Str(path)),
                    v,
                ],
            ) => {
                let n = self.node();
                let what = shown_attr(typ, addr, path);
                let shape = Shape::Attr(typ.clone(), keys(path));
                self.edge(n, shape, what, a.span);
                self.place(n, Vec::new(), v, at);
            }
            // `x in l`: `x` is an element of `l`.
            ("member", [list, item]) => {
                let l = self.term_node(list, at);
                let x = self.term_node(item, at);
                self.link(l, vec![Seg::Elem], x);
            }
            // `(k, v) in o`: `v` is `o`'s at `k`; `l[i]`, an element.
            ("member", [object, key, value]) => {
                let seg = match key {
                    Term::Val(Value::Str(k)) => Seg::Key(k.clone()),
                    Term::Val(Value::Int(_)) => Seg::Elem,
                    _ => Seg::Any,
                };
                let o = self.term_node(object, at);
                let v = self.term_node(value, at);
                self.link(o, vec![seg], v);
            }
            ("want", _) => {}
            _ => self.relation(a, at),
        }
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

    /// `t` is one of the values of the cell `n` (a let's, an output's,
    /// an input's): a variable flows into it, one way; anything else is
    /// placed in it.
    fn give(&mut self, n: usize, t: &mut Term, at: &At) {
        let var = match t {
            Term::Var(v) => Some(v.clone()),
            Term::Func { name, args } if name == types::AS => match args.as_slice() {
                [Term::Var(v), _] => Some(v.clone()),
                _ => None,
            },
            _ => None,
        };
        match var {
            Some(v) => {
                let x = self.var(&v);
                self.flows.push((x, n));
            }
            None => self.place(n, Vec::new(), t, at),
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
            // `[x | B]`: each `x` is an element.
            Term::ListComp { item, body } => {
                self.body(body, at);
                let mut p = path;
                p.push(Seg::Elem);
                self.place(n, p, item, at);
            }
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
fn shown_attr(typ: &str, name: &Term, path: &str) -> String {
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

/// Read every literal at the type of the edges it reaches (R-192): the
/// program's lets, rows, arguments, inputs and outputs; the errors where
/// its edges disagree or a let's ambiguous quantity reaches none.
pub fn read(program: &mut Program, schema: &Schema) -> Vec<Diagnostic> {
    let mut g = Graph::new(schema, program);
    g.program(&mut program.statements, "");
    let readings = g.readings();
    let mut g = Graph::new(schema, program);
    g.readings = Some(readings);
    g.program(&mut program.statements, "");
    g.diags
}

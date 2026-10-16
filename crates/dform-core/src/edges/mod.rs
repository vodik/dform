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
//! schemas are known ([`crate::types::read`]): over the statements `lower`
//! writes for its nodes, each literal marked with its node and each
//! relation named as its scope declares it (a [`prog::Visit`] of `lower`),
//! so the pass walks what `lower` writes, reads each literal by node, and
//! finds each relation's columns by the program's scope tree:
//!
//! 1. Collect: union-find nodes for each relation's columns (a let's
//!    cell is its relation's one column) and each statement's variables,
//!    the links between a value and its part at a path, the edges that
//!    give a node a type, and every literal (a site) at its node and path.
//! 2. Ask, for each site, the types the edges its node reaches give its
//!    path, through the links both ways.
//! 3. Settle each relation's columns (R-34, [`columns`]): the type the
//!    edges that type a column give it, its literals' kind, what flows
//!    into a cell; and judge the uses ([`checks`]). What is wrong is said
//!    first, and alone.
//! 4. Read the literal at the one type they agree on, the reading its
//!    node's (no statement is rewritten: `lower` writes it), and a
//!    relation's argument at its column's type in turn; two types are an
//!    error naming both edges; a quantity no edge types (`100m`) in a let
//!    is an error naming the let and what would type it.
//!
//! Only the types read from a literal take part in a reading (a quantity,
//! a time, a value type read from a string: `inet`, `ip`, `uri`, `oci`,
//! `semver`, a range): a `string` position takes any of them as its text
//! (R-133). A column takes every type.

mod checks;
mod columns;
pub(crate) mod lattice;
mod shape;
mod sites;
mod walk;

use crate::address::Address;
use crate::ast::{Atom, Lit, Span, Stmt, Term, TypeExpr};
use crate::diag::Diagnostic;
use crate::infer::Signatures;
use crate::schema::Schema;
use crate::types::{self, Ty};
use crate::value::Value;
use checks::Checks;
use columns::Typing;
use shape::{Edge, Path, Role, Shape};
use sites::{Holder, Reading, Site};
use std::collections::{BTreeMap, BTreeSet};

/// The cell of an output `k`, `output:k`: no relation's name.
const OUTPUT: &str = "output:";

/// A column: (the scope that declares the relation, the relation, arity,
/// index).
type Col = (ScopeId, String, usize, usize);

struct Graph<'a> {
    schema: &'a Schema,
    scopes: &'a prog::Scopes,
    parent: Vec<usize>,
    edges: Vec<Vec<Edge>>,
    /// The column each node was made for.
    col_of: Vec<Option<Col>>,
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
    /// The second pass: the type each site's column settled as, where
    /// the site is a relation's argument (R-34): read at it too.
    column_types: Vec<Option<Ty>>,
    /// The second pass: each literal's node as read, where reading
    /// changed it.
    read: Readings,
    diags: Vec<Diagnostic>,
    /// What the columns' types are settled from (R-34).
    typing: Typing,
    /// What the columns' types judge (R-34).
    checks: Checks,
}

/// Where a term is placed: its scope, the span of what holds it, what it
/// is written in, and whether its literals are sites (not where the
/// attribute's own reading reads them, [`crate::types::read`]).
#[derive(Clone)]
struct At {
    scope: ScopeId,
    span: Span,
    /// The atom the term is an argument of, else `span`: where a check
    /// says what is wrong of it (R-34).
    atom: Span,
    holder: Option<Holder>,
    sites: bool,
}

impl<'a> Graph<'a> {
    fn new(schema: &'a Schema, scopes: &'a prog::Scopes) -> Graph<'a> {
        Graph {
            schema,
            scopes,
            parent: Vec::new(),
            edges: Vec::new(),
            col_of: Vec::new(),
            links: Vec::new(),
            flows: Vec::new(),
            columns: BTreeMap::new(),
            vars: BTreeMap::new(),
            sites: Vec::new(),
            readings: None,
            column_types: Vec::new(),
            read: Readings::new(),
            diags: Vec::new(),
            typing: Typing::default(),
            checks: Checks::default(),
        }
    }

    fn node(&mut self) -> usize {
        let n = self.parent.len();
        self.parent.push(n);
        self.edges.push(Vec::new());
        self.col_of.push(None);
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

    fn edge(&mut self, n: usize, shape: Shape, what: String, span: Span, role: Role) {
        let col = self.col_of[n].clone();
        let n = self.find(n);
        self.edges[n].push(Edge {
            shape,
            what,
            span,
            role,
            col,
        });
    }

    /// `a` and `b` reach each other's types, and their columns stay
    /// apart: the sides of a comparison, a column that joins nothing (a
    /// compiler's relation, one declared `any`).
    fn reach(&mut self, a: usize, b: usize) {
        self.flows.push((a, b));
        self.flows.push((b, a));
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

    fn column(&mut self, scope: ScopeId, pred: &str, arity: usize, i: usize) -> usize {
        let (declaring, name) = self.resolve(scope, pred);
        let c = (declaring, name, arity, i);
        if let Some(&n) = self.columns.get(&c) {
            return n;
        }
        let n = self.node();
        self.col_of[n] = Some(c.clone());
        self.columns.insert(c, n);
        n
    }

    /// The relation `pred` a read in `scope` names: the scope that declares
    /// it and its name there. A used module's, `m::k` (R-113), is the
    /// module's `k`.
    fn resolve(&self, scope: ScopeId, pred: &str) -> (ScopeId, String) {
        let used = pred.split_once("::").and_then(|(m, k)| {
            let body = self.used(scope, m)?;
            Some((self.declaring(body, k), k.to_string()))
        });
        used.unwrap_or_else(|| (self.declaring(scope, pred), pred.to_string()))
    }

    /// The body of the module a `use` binds to `name`, read in `scope`.
    fn used(&self, scope: ScopeId, name: &str) -> Option<ScopeId> {
        let using = |d: &DeclKind| matches!(d, DeclKind::Use { .. });
        match &self.scopes.reaching(scope, name, using)?.1.kind {
            DeclKind::Use { module, .. } => self.scopes.by_path(module),
            _ => None,
        }
    }

    /// The scope whose `pred` a read in `scope` names: the innermost
    /// around it that declares a relation or a value of the name (an
    /// output's cell, `output:k`, an output), else the program's.
    fn declaring(&self, scope: ScopeId, pred: &str) -> ScopeId {
        let found = match pred.strip_prefix(OUTPUT) {
            Some(k) => self.scopes.reaching(scope, k, |d| {
                matches!(d, DeclKind::Output { .. } | DeclKind::RelationOutput)
            }),
            None => self.scopes.reaching(scope, pred, |d| {
                matches!(
                    d,
                    DeclKind::Input
                        | DeclKind::Let
                        | DeclKind::LetFn { .. }
                        | DeclKind::Rule { .. }
                        | DeclKind::Decl { .. }
                        | DeclKind::RelationInput { .. }
                )
            }),
        };
        found.map_or(self.scopes.root, |(s, _)| s)
    }

    /// The scope of the module or component at `path`; the program's
    /// when none is (a copy of what the program does not define).
    fn definition(&self, path: &str) -> ScopeId {
        self.scopes.by_path(path).unwrap_or(self.scopes.root)
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

// --- over what `lower` writes ---------------------------------------------

use crate::program::scope::{DeclKind, Reach, ScopeId};
use crate::program::{self as prog, ExprId, ExprKind, Readings};
use slotmap::{Key, KeyData};

/// The mark a literal's node leaves on what `lower` writes for it,
/// `__node(id, t)` ([`marked`]): the walk reads what `lower` writes and
/// gives each literal's reading to its node.
const NODE: &str = "__node";

/// What the walk reads `lower`'s output by ([`prog::Visit`]): each literal
/// marked with its node (a site the walk may read), and each relation by
/// the name its scope declares it under, whatever reaches it (an
/// enclosing definition's, R-186, too): the scope tree says which scope
/// that is ([`Graph::declaring`]).
struct Marks<'p>(&'p prog::Program);

impl prog::Visit for Marks<'_> {
    fn term(&self, id: ExprId, t: Term) -> Term {
        match self.0.exprs[id].kind {
            ExprKind::Lit(_) | ExprKind::Quantity { .. } | ExprKind::Range { .. } => Term::Func {
                name: NODE.to_string(),
                args: vec![Term::Val(Value::Int(id.data().as_ffi() as i64)), t],
            },
            _ => t,
        }
    }

    fn relation(&self, name: &str, via: &Reach) -> String {
        match via {
            Reach::Lexical(_) => name.to_string(),
            via => via.relation(name),
        }
    }
}

/// The node a mark names.
fn node_id(n: i64) -> ExprId {
    ExprId::from(KeyData::from_ffi(n as u64))
}

/// `t` without its nodes' marks: as `lower` writes it.
fn unmarked(t: &Term) -> Term {
    match t {
        Term::Func { name, args } => match (name.as_str(), args.as_slice()) {
            (NODE, [_, held]) => unmarked(held),
            _ => Term::Func {
                name: name.clone(),
                args: args.iter().map(unmarked).collect(),
            },
        },
        Term::List(xs) => Term::List(xs.iter().map(unmarked).collect()),
        Term::Obj(m) => Term::Obj(m.iter().map(|(k, x)| (k.clone(), unmarked(x))).collect()),
        Term::ListComp { item, body } => Term::ListComp {
            item: Box::new(unmarked(item)),
            body: body.iter().map(unmarked_lit).collect(),
        },
        t => t.clone(),
    }
}

fn unmarked_lit(l: &Lit) -> Lit {
    let atom = |a: &Atom| Atom {
        args: a.args.iter().map(unmarked).collect(),
        ..a.clone()
    };
    match l {
        Lit::Pos(a) => Lit::Pos(atom(a)),
        Lit::Not(a) => Lit::Not(atom(a)),
        Lit::Eq(a, b) => Lit::Eq(unmarked(a), unmarked(b)),
        Lit::Neq(a, b) => Lit::Neq(unmarked(a), unmarked(b)),
        Lit::Gt(a, b) => Lit::Gt(unmarked(a), unmarked(b)),
        Lit::Ge(a, b) => Lit::Ge(unmarked(a), unmarked(b)),
        Lit::Lt(a, b) => Lit::Lt(unmarked(a), unmarked(b)),
        Lit::Le(a, b) => Lit::Le(unmarked(a), unmarked(b)),
    }
}

/// A rule as `lower` writes it, its marks taken off.
fn unmarked_rule(r: &crate::ast::RuleStmt) -> crate::ast::RuleStmt {
    let mut r = r.clone();
    r.head.args = r.head.args.iter().map(unmarked).collect();
    r.body = r.body.iter().map(unmarked_lit).collect();
    r
}

/// What the pass makes of a program: each node's reading, every
/// relation's signature, and the errors.
pub struct Read {
    pub readings: Readings,
    pub signatures: Signatures,
    pub diags: Vec<Diagnostic>,
}

/// Read every literal of the program's nodes at the type of the edges it
/// reaches (R-192), and at the type its column settles as where it is a
/// relation's argument (R-34): the program as `lower` writes it, each
/// literal marked with its node, walked twice (the edges and the columns,
/// then the readings).
pub fn read_nodes(program: &prog::Program, schema: &Schema) -> Read {
    let Ok(lowered) = prog::lower_visited(program, &Marks(program)) else {
        return Read {
            readings: Readings::new(),
            signatures: Signatures::default(),
            diags: Vec::new(),
        };
    };
    let mut stmts = lowered.statements;
    let (scopes, root) = (&program.scopes, program.scopes.root);
    let mut g = Graph::new(schema, scopes);
    g.declarations(&stmts, root);
    g.program(&mut stmts, root);
    let readings = g.readings();
    let columns = g.columns(&readings);
    // What the columns' types say is wrong is said first, and alone: the
    // readings of a program whose columns do not settle say nothing more.
    if !columns.diags.is_empty() {
        return Read {
            readings: Readings::new(),
            signatures: columns.signatures,
            diags: columns.diags,
        };
    }
    let mut g = Graph::new(schema, scopes);
    g.declarations(&stmts, root);
    g.readings = Some(readings);
    g.column_types = columns.of_sites;
    g.program(&mut stmts, root);
    Read {
        readings: g.read,
        signatures: columns.signatures,
        diags: g.diags,
    }
}

//! Column types (R-34): every relation's columns are typed, declared by a
//! `decl` or inferred from their uses, and the uses are checked against
//! them.
//!
//! One pass over the lowered program, the shape of the secrets pass:
//!
//! 1. Collect what each column is given. Hard constraints: a `decl`'s
//!    column types, an extern's (a provider's, a table's), a typed input
//!    read into it, a function's parameter where a column's variable is
//!    an argument and its result where it is the value, a reference
//!    (`ref(T, ..)`), and with the provider's schema an attribute read
//!    into it. Literals: a non-string literal is its kind; a string
//!    literal is unknown (Postgres's unknown literal) until the column is
//!    settled. A variable in two columns makes them one (a join, or a
//!    head taking a body's column).
//! 2. Unify. Two hard constraints that disagree are an error naming both
//!    sites; a column declared `any` takes anything and joins nothing.
//! 3. A column with a hard type checks every literal against it, a string
//!    parsed as that type (R-31: `"10.0.0.0/8"` in an `inet` column); one
//!    with literals only is their kind, and literals of two kinds are an
//!    error naming both; one with string literals only is a string.
//! 4. Arithmetic on a column that is no number (`n + 1`, `n` a string),
//!    and a comparison of two types that are never equal, are errors, not
//!    a silent non-match.
//!
//! The settled signatures (`az(name: string, index: int)`) are what the
//! LSP's hover prints, and the program's literals in an `inet`, `ip`,
//! quantity or time column are read as one before it is evaluated.
//! Variables are never coerced.

use crate::ast::{Atom, Decl, ExternFn, Lit, Program, Span, Stmt, Term, TypeExpr};
use crate::diag::{Diagnostic, Diagnostics};
use crate::types::{self, Ty};
use crate::value::Value;
use anyhow::Result;
use std::collections::BTreeMap;

/// A relation's columns as declared or inferred.
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub pred: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// The `decl`'s name for it, or the variable a rule's head writes.
    pub name: Option<String>,
    /// `None`: nothing gives it a type (`any`).
    pub ty: Option<Ty>,
}

impl std::fmt::Display for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cols: Vec<String> = self
            .columns
            .iter()
            .map(|c| {
                let ty = c.ty.as_ref().map_or("any".to_string(), Ty::to_string);
                match &c.name {
                    Some(n) => format!("{n}: {ty}"),
                    None => ty,
                }
            })
            .collect();
        write!(f, "{}({})", self.pred, cols.join(", "))
    }
}

/// Every relation's signature, by name and arity.
pub type Signatures = BTreeMap<(String, usize), Signature>;

/// The `decl`s of a program, top level and in modules, before `transform`
/// lowers them away: by relation name, and whether a module declares it
/// (its relation is then the copy's, `n::p`).
#[derive(Debug, Clone, Default)]
pub struct Declared(BTreeMap<String, (Decl, bool)>);

impl Declared {
    pub fn of(program: &Program) -> Declared {
        fn walk(stmts: &[Stmt], module: bool, out: &mut BTreeMap<String, (Decl, bool)>) {
            for s in stmts {
                match s {
                    Stmt::Decl(d) => {
                        out.entry(d.pred.clone()).or_insert((d.clone(), module));
                    }
                    Stmt::Module(m) => walk(&m.body, true, out),
                    _ => {}
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&program.statements, false, &mut out);
        Declared(out)
    }

    /// The `decl` of a lowered relation: its own name's, or a module's of
    /// its last segment (`n::p`, `a.b::p`).
    fn get(&self, pred: &str) -> Option<&Decl> {
        if let Some((d, _)) = self.0.get(pred) {
            return Some(d);
        }
        let (_, last) = pred.rsplit_once("::")?;
        self.0.get(last).filter(|(_, m)| *m).map(|(d, _)| d)
    }
}

/// Relations the compiler writes and reads with values of any type: not a
/// program's relation, never typed by its uses.
fn untyped(pred: &str) -> bool {
    crate::loader::is_core_pred(pred)
        || pred.contains("__")
        || pred.starts_with("type_")
        || matches!(
            pred,
            crate::transform::SECRET_CELL
                | "instance_of"
                | "settings_row"
                | "stack_output"
                | "provider_config"
                | "provider_expect_account"
                | "let"
        )
}

/// A function's call, not a relation's atom.
fn function(pred: &str) -> Option<&'static crate::functions::Function> {
    crate::functions::get(pred)
}

/// What fixed a type, for the message.
#[derive(Debug, Clone)]
struct Hard {
    ty: Ty,
    span: Span,
    what: String,
    /// The column it was given to, when it was given to one (not to a
    /// variable).
    col: Option<Col>,
}

#[derive(Debug, Clone)]
struct Literal {
    value: Value,
    span: Span,
}

/// A column: (relation, arity, index).
type Col = (String, usize, usize);

#[derive(Default)]
struct Solver {
    parent: Vec<usize>,
    hard: Vec<Vec<Hard>>,
    lits: Vec<Vec<Literal>>,
    /// The columns each node is, for the message.
    cols: Vec<Vec<Col>>,
    /// The column a node was made for.
    col_of: Vec<Option<Col>>,
    columns: BTreeMap<Col, usize>,
    /// Columns that take anything (`decl p(x: any)`).
    open: std::collections::BTreeSet<Col>,
}

impl Solver {
    fn node(&mut self) -> usize {
        let n = self.parent.len();
        self.parent.push(n);
        self.hard.push(Vec::new());
        self.lits.push(Vec::new());
        self.cols.push(Vec::new());
        self.col_of.push(None);
        n
    }

    fn column(&mut self, c: &Col) -> usize {
        if let Some(&n) = self.columns.get(c) {
            return n;
        }
        let n = self.node();
        self.cols[n].push(c.clone());
        self.col_of[n] = Some(c.clone());
        self.columns.insert(c.clone(), n);
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
        if a == b {
            return;
        }
        let (lo, hi) = (a.min(b), a.max(b));
        self.parent[hi] = lo;
        let h = std::mem::take(&mut self.hard[hi]);
        self.hard[lo].extend(h);
        let l = std::mem::take(&mut self.lits[hi]);
        self.lits[lo].extend(l);
        let c = std::mem::take(&mut self.cols[hi]);
        self.cols[lo].extend(c);
    }

    fn hard(&mut self, n: usize, ty: Ty, span: Span, what: String) {
        if ty == Ty::Any {
            return;
        }
        let col = self.col_of[n].clone();
        let n = self.find(n);
        self.hard[n].push(Hard {
            ty,
            span,
            what,
            col,
        });
    }
}

/// A comparison or an arithmetic operation, checked once the columns are
/// settled.
struct Check {
    rule: usize,
    op: &'static str,
    a: Term,
    b: Term,
    span: Span,
}

/// One rule's variables.
#[derive(Default)]
struct Vars(BTreeMap<String, usize>);

struct Pass<'a> {
    s: Solver,
    declared: &'a Declared,
    inputs: BTreeMap<(String, String), Ty>,
    schema: Option<&'a crate::schema::Schema>,
    checks: Vec<Check>,
    /// Each rule's (statement's) variables.
    vars: Vec<Vars>,
    /// A rule head's variable names per column, for the signature.
    head_names: BTreeMap<Col, String>,
}

impl Pass<'_> {
    fn var(&mut self, rule: usize, v: &str) -> usize {
        if let Some(&n) = self.vars[rule].0.get(v) {
            return n;
        }
        let n = self.s.node();
        self.vars[rule].0.insert(v.to_string(), n);
        n
    }

    /// A term in the column `c` of a relation atom at `span`.
    fn arg(&mut self, rule: usize, c: Col, t: &Term, span: Span) {
        if self.s.open.contains(&c) {
            return;
        }
        let col = self.s.column(&c);
        match t {
            Term::Var(v) => {
                let n = self.var(rule, v);
                self.s.union(col, n);
            }
            Term::Val(v) => self.literal(col, v, span),
            t => {
                self.calls(rule, t, span);
                if let Some((ty, what)) = self.term_type(t) {
                    self.s.hard(col, ty, span, what);
                }
            }
        }
    }

    fn literal(&mut self, n: usize, v: &Value, span: Span) {
        if kind(v).is_some() || matches!(v, Value::Str(_)) {
            let n = self.s.find(n);
            self.s.lits[n].push(Literal {
                value: v.clone(),
                span,
            });
        }
    }

    /// The type a call's result has, by its signature.
    fn term_type(&self, t: &Term) -> Option<(Ty, String)> {
        let Term::Func { name, args } = t else {
            return None;
        };
        if name == "ref" && args.len() == 3 {
            return match &args[0] {
                Term::Val(Value::Str(typ)) => Some((Ty::Ref(typ.clone()), "a reference".into())),
                _ => None,
            };
        }
        if arithmetic(name).is_some() {
            return None;
        }
        let f = function(name)?;
        let ty = Ty::parse(f.ret.trim_end_matches('?'));
        (ty != Ty::Any).then(|| (ty, format!("`{name}`'s result")))
    }

    /// Every call inside `t`: a variable argument takes the parameter's
    /// type (a package function's: a conversion such as `inet(s)` takes
    /// what it converts), and an arithmetic operation is checked.
    fn calls(&mut self, rule: usize, t: &Term, span: Span) {
        match t {
            Term::Func { name, args } => {
                if let Some(op) = arithmetic(name)
                    && let [a, b] = args.as_slice()
                {
                    self.checks.push(Check {
                        rule,
                        op,
                        a: a.clone(),
                        b: b.clone(),
                        span,
                    });
                }
                self.params(rule, name, args, span);
                args.iter().for_each(|a| self.calls(rule, a, span));
            }
            Term::List(xs) => xs.iter().for_each(|a| self.calls(rule, a, span)),
            Term::Obj(m) => m.values().for_each(|a| self.calls(rule, a, span)),
            _ => {}
        }
    }

    /// A package function's variable arguments: their parameters' types.
    fn params(&mut self, rule: usize, name: &str, args: &[Term], span: Span) {
        let Some(f) = function(name) else {
            return;
        };
        if !name.contains('.') || f.internal {
            return;
        }
        for (i, a) in args.iter().enumerate() {
            let Term::Var(v) = a else { continue };
            let p = match f.params.get(i) {
                Some(p) => p,
                None if f.variadic => match f.params.last() {
                    Some(p) => p,
                    None => continue,
                },
                None => continue,
            };
            let ty = Ty::parse(&p.ty);
            if matches!(ty, Ty::Scalar(_)) {
                let n = self.var(rule, v);
                self.s.hard(
                    n,
                    ty,
                    span,
                    format!("`{name}`'s argument `{}`", p.name),
                );
            }
        }
    }

    fn atom(&mut self, rule: usize, a: &Atom) {
        if function(&a.pred).is_some() {
            self.params(rule, &a.pred, &a.args, a.span);
            a.args.iter().for_each(|t| self.calls(rule, t, a.span));
            return;
        }
        match (a.pred.as_str(), a.args.as_slice()) {
            ("attr", [Term::Val(Value::Str(typ)), scope, Term::Val(Value::Str(p)), Term::Var(v)]) => {
                let found = if typ == crate::modules::INPUT {
                    match scope {
                        Term::Val(Value::Str(s)) => self
                            .inputs
                            .get(&(s.clone(), p.clone()))
                            .map(|t| (t.clone(), format!("input {p}"))),
                        _ => None,
                    }
                } else {
                    self.schema
                        .and_then(|s| s.attr(typ, p))
                        .map(|spec| (Ty::parse(&spec.ty), format!("{typ}.{p}")))
                };
                if let Some((ty, what)) = found {
                    let n = self.var(rule, v);
                    self.s.hard(n, ty, a.span, what);
                }
                return;
            }
            ("member", [list, Term::Var(v)]) => {
                let n = self.var(rule, v);
                match list {
                    Term::Func { name, .. } if name == "int.range" => {
                        self.s
                            .hard(n, Ty::Scalar("int".into()), a.span, "a range".into());
                    }
                    Term::List(xs) => {
                        for x in xs {
                            if let Term::Val(x) = x {
                                self.literal(n, x, a.span);
                            }
                        }
                    }
                    Term::Val(Value::List(xs)) => {
                        for x in xs {
                            self.literal(n, x, a.span);
                        }
                    }
                    _ => {}
                }
                self.calls(rule, list, a.span);
                return;
            }
            _ => {}
        }
        if untyped(&a.pred) {
            a.args.iter().for_each(|t| self.calls(rule, t, a.span));
            return;
        }
        let n = a.args.len();
        for (i, t) in a.args.iter().enumerate() {
            self.arg(rule, (a.pred.clone(), n, i), t, a.span);
        }
    }

    fn lit(&mut self, rule: usize, l: &Lit, span: Span) {
        match l {
            Lit::Pos(a) | Lit::Not(a) => self.atom(rule, a),
            Lit::Eq(a, b) => {
                self.calls(rule, a, span);
                self.calls(rule, b, span);
                match (a, b) {
                    (Term::Var(x), Term::Var(y)) => {
                        let (x, y) = (self.var(rule, x), self.var(rule, y));
                        self.s.union(x, y);
                    }
                    (Term::Var(x), Term::Val(v)) | (Term::Val(v), Term::Var(x)) => {
                        let n = self.var(rule, x);
                        self.literal(n, v, span);
                    }
                    (Term::Var(x), t) | (t, Term::Var(x)) => {
                        if let Some((ty, what)) = self.term_type(t) {
                            let n = self.var(rule, x);
                            self.s.hard(n, ty, span, what);
                        }
                    }
                    _ => {}
                }
            }
            Lit::Neq(a, b) | Lit::Gt(a, b) | Lit::Ge(a, b) | Lit::Lt(a, b) | Lit::Le(a, b) => {
                self.calls(rule, a, span);
                self.calls(rule, b, span);
                let op = match l {
                    Lit::Neq(..) => "!=",
                    Lit::Gt(..) => ">",
                    Lit::Ge(..) => ">=",
                    Lit::Lt(..) => "<",
                    _ => "<=",
                };
                self.checks.push(Check {
                    rule,
                    op,
                    a: a.clone(),
                    b: b.clone(),
                    span,
                });
            }
        }
    }
}

/// `add` and its kin: the operator they are written with.
fn arithmetic(name: &str) -> Option<&'static str> {
    Some(match name {
        "add" => "+",
        "sub" => "-",
        "mul" => "*",
        "div" => "/",
        "mod" => "%",
        _ => return None,
    })
}

/// A literal's type, `None` for a string (unknown until its column is
/// settled) and for what the pass does not judge.
fn kind(v: &Value) -> Option<Ty> {
    let s = |x: &str| Some(Ty::Scalar(x.into()));
    match v {
        Value::Int(_) => s("int"),
        Value::Bool(_) => s("bool"),
        Value::IpNet { .. } => s("inet"),
        Value::Ip(_) => s("ip"),
        Value::Quantity(q) => s(q.dim().name()),
        Value::Time(_) => s("time"),
        _ => None,
    }
}

/// Whether a value of `a` can ever equal one of `b`.
fn compatible(a: &Ty, b: &Ty) -> bool {
    match (a, b) {
        (Ty::Any, _) | (_, Ty::Any) => true,
        (Ty::Secret(x), y) | (y, Ty::Secret(x)) => compatible(x, y),
        (Ty::Enum(_), Ty::Enum(_)) => true,
        (Ty::Enum(_), Ty::Scalar(s)) | (Ty::Scalar(s), Ty::Enum(_)) => s == "string",
        // A network or an address is written as text, and a function
        // reads text as one (`inet.subnet(vpc.cidr, ..)`, the cidr a
        // string attribute): a string column holding them is the narrower
        // type.
        (Ty::Scalar(x), Ty::Scalar(y)) if text_of(x, y) || text_of(y, x) => true,
        (Ty::List(x), Ty::List(y)) => compatible(x, y),
        (Ty::Ref(x), Ty::Ref(y)) => x == y,
        (Ty::Scalar(x), Ty::Scalar(y)) => x == y,
        _ => false,
    }
}

/// `narrow` is read from a string: an `inet`, an `ip`.
fn text_of(string: &str, narrow: &str) -> bool {
    string == "string" && matches!(narrow, "inet" | "ip")
}

/// The more telling of two compatible types: an enum over a string, a
/// typed list over an untyped one.
fn narrower(a: Ty, b: &Ty) -> Ty {
    match (&a, b) {
        (Ty::Any, _) => b.clone(),
        (Ty::Scalar(s), Ty::Enum(_)) if s == "string" => b.clone(),
        (Ty::Scalar(s), Ty::Scalar(n)) if text_of(s, n) => b.clone(),
        (Ty::List(x), Ty::List(_)) if **x == Ty::Any => b.clone(),
        _ => a,
    }
}

/// How a message names a column: `` `az`'s column `index` ``, or by its
/// position.
fn shown_col(c: &Col, declared: &Declared, names: &BTreeMap<Col, String>) -> String {
    let (pred, _, i) = c;
    let shown = pred.rsplit_once("::").map_or(pred.as_str(), |(_, p)| p);
    let name = declared
        .get(pred)
        .and_then(|d| d.fields.get(*i).cloned())
        .or_else(|| names.get(c).cloned());
    match name {
        Some(n) => format!("`{shown}`'s column `{n}`"),
        None => format!("`{shown}`'s column {}", i + 1),
    }
}

/// The pass's result: the signatures, and each node's settled type.
pub struct Inferred {
    pub signatures: Signatures,
    settled: BTreeMap<Col, Ty>,
}

/// Infer and check the column types of the lowered program `program`,
/// its externs `externs`, inputs `inputs` and the source program's
/// `decl`s; the schema, when given, types the attributes read into a
/// column.
pub fn infer(
    program: &Program,
    externs: &[ExternFn],
    inputs: &[crate::inputs::Declared],
    declared: &Declared,
    schema: Option<&crate::schema::Schema>,
) -> Result<Inferred> {
    let mut p = Pass {
        s: Solver::default(),
        declared,
        inputs: inputs
            .iter()
            .map(|d| {
                (
                    (d.scope.clone(), d.decl.name.clone()),
                    types::of_expr(&d.decl.ty),
                )
            })
            .collect(),
        schema,
        checks: Vec::new(),
        vars: Vec::new(),
        head_names: BTreeMap::new(),
    };
    // The declarations: every relation the program has that a `decl`
    // types, and the externs' typed columns.
    let mut arities: BTreeMap<String, std::collections::BTreeSet<usize>> = BTreeMap::new();
    for s in &program.statements {
        match s {
            Stmt::Fact(a) => {
                arities.entry(a.pred.clone()).or_default().insert(a.args.len());
            }
            Stmt::Rule(r) => {
                arities
                    .entry(r.head.pred.clone())
                    .or_default()
                    .insert(r.head.args.len());
                for l in &r.body {
                    if let Lit::Pos(a) | Lit::Not(a) = l {
                        arities.entry(a.pred.clone()).or_default().insert(a.args.len());
                    }
                }
            }
            _ => {}
        }
    }
    for (pred, ns) in &arities {
        if untyped(pred) {
            continue;
        }
        let Some(d) = declared.get(pred) else {
            continue;
        };
        if !ns.contains(&d.fields.len()) {
            continue;
        }
        for (i, ty) in d.types.iter().enumerate() {
            let c = (pred.clone(), d.fields.len(), i);
            match ty.as_ref().map(types::of_expr) {
                Some(Ty::Any) if is_any(ty.as_ref()) => {
                    p.s.open.insert(c);
                }
                Some(ty) => {
                    let n = p.s.column(&c);
                    p.s.hard(n, ty, d.span, format!("decl {}", d.pred));
                }
                None => {}
            }
        }
    }
    for e in externs {
        for (i, b) in e.args.iter().enumerate() {
            let Some(ty) = &b.ty else { continue };
            let c = (e.name.clone(), e.args.len(), i);
            let n = p.s.column(&c);
            p.s.hard(
                n,
                types::of_expr(ty),
                e.span,
                format!("extern {}'s column {}", e.name, b.name),
            );
        }
    }
    for (i, s) in program.statements.iter().enumerate() {
        p.vars.push(Vars::default());
        match s {
            Stmt::Fact(a) => p.atom(i, a),
            Stmt::Rule(r) => {
                p.atom(i, &r.head);
                for (k, t) in r.head.args.iter().enumerate() {
                    if let Term::Var(v) = t
                        && !untyped(&r.head.pred)
                    {
                        p.head_names
                            .entry((r.head.pred.clone(), r.head.args.len(), k))
                            .or_insert_with(|| shown_var(v));
                    }
                }
                for l in &r.body {
                    p.lit(i, l, r.head.span);
                }
            }
            _ => {}
        }
    }
    p.solve(&arities)
}

/// `any` written as a column's type.
fn is_any(t: Option<&TypeExpr>) -> bool {
    matches!(t, Some(TypeExpr::Name(n)) if n == "any")
}

/// A term as the program wrote it, near enough for a message.
fn shown_term(t: &Term) -> String {
    match t {
        Term::Var(v) => shown_var(v),
        t => crate::partition::fmt_term(t),
    }
}

/// A lowered variable as the program wrote it: `Zone` is `zone`; a fresh
/// one's suffix dropped.
fn shown_var(v: &str) -> String {
    let base = v.trim_end_matches(|c: char| c.is_ascii_digit() || c == '_');
    let base = if base.is_empty() { v } else { base };
    let mut out = String::new();
    for (i, ch) in base.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

impl Pass<'_> {
    fn solve(mut self, arities: &BTreeMap<String, std::collections::BTreeSet<usize>>) -> Result<Inferred> {
        let mut diags = Vec::new();
        let mut settled: BTreeMap<usize, Option<Ty>> = BTreeMap::new();
        let n = self.s.parent.len();
        for i in 0..n {
            let r = self.s.find(i);
            if settled.contains_key(&r) {
                continue;
            }
            let s = self.settle(r, &mut diags);
            settled.insert(r, s);
        }
        // Comparisons and arithmetic.
        let checks = std::mem::take(&mut self.checks);
        for c in &checks {
            let ty = |p: &mut Self, t: &Term| -> Option<Ty> {
                match t {
                    Term::Var(v) => {
                        let n = *p.vars[c.rule].0.get(v)?;
                        let r = p.s.find(n);
                        settled.get(&r).cloned().flatten()
                    }
                    Term::Val(v) => kind(v),
                    t => p.term_type(t).map(|(t, _)| t),
                }
            };
            let (ta, tb) = (ty(&mut self, &c.a), ty(&mut self, &c.b));
            let written = format!("`{} {} {}`", shown_term(&c.a), c.op, shown_term(&c.b));
            if matches!(c.op, "+" | "-" | "*" | "/" | "%") {
                for (t, ty) in [(&c.a, &ta), (&c.b, &tb)] {
                    let Some(ty) = ty else { continue };
                    let number = matches!(ty, Ty::Scalar(s) if matches!(s.as_str(),
                        "int" | "bytes" | "cpu" | "duration" | "time"));
                    if !number && !matches!(ty, Ty::Any | Ty::Secret(_)) {
                        diags.push(Diagnostic::error(
                            c.span,
                            format!(
                                "{written}: `{}` is {ty}, and arithmetic is on numbers, \
                                 quantities and times",
                                shown_term(t)
                            ),
                        ));
                    }
                }
                continue;
            }
            match (&ta, &tb, &c.a, &c.b) {
                (Some(t), _, x, Term::Val(v)) | (_, Some(t), Term::Val(v), x)
                    if matches!(v, Value::Str(_)) =>
                {
                    if let Some(why) = types::mismatch(t, &Term::Val(v.clone())) {
                        diags.push(Diagnostic::error(
                            c.span,
                            format!("{written}: `{}` {why}", shown_term(x)),
                        ));
                    }
                }
                (Some(a), Some(b), _, _) if !compatible(a, b) => {
                    diags.push(Diagnostic::error(
                        c.span,
                        format!("{written} compares {a} with {b}: they are never equal"),
                    ));
                }
                _ => {}
            }
        }
        if !diags.is_empty() {
            return Err(Diagnostics(diags).into());
        }
        // The signatures.
        let mut signatures = Signatures::new();
        let mut by_col = BTreeMap::new();
        for (pred, ns) in arities {
            if untyped(pred) || function(pred).is_some() || pred.starts_with("table.") {
                continue;
            }
            for &k in ns {
                let decl = self.declared.get(pred).filter(|d| d.fields.len() == k);
                let columns = (0..k)
                    .map(|i| {
                        let c = (pred.clone(), k, i);
                        let ty = match self.s.columns.get(&c).copied() {
                            Some(n) => {
                                let r = self.s.find(n);
                                settled.get(&r).cloned().flatten()
                            }
                            None => None,
                        };
                        let ty = if self.s.open.contains(&c) {
                            Some(Ty::Any)
                        } else {
                            ty
                        };
                        if let Some(t) = &ty {
                            by_col.insert(c.clone(), t.clone());
                        }
                        Column {
                            name: decl
                                .and_then(|d| d.fields.get(i).cloned())
                                .or_else(|| self.head_names.get(&c).cloned()),
                            ty,
                        }
                    })
                    .collect();
                signatures.insert(
                    (pred.clone(), k),
                    Signature {
                        pred: pred.clone(),
                        columns,
                    },
                );
            }
        }
        Ok(Inferred {
            signatures,
            settled: by_col,
        })
    }

    /// Settle the node `r`: its type, or the errors that say why it has
    /// none.
    fn settle(&mut self, r: usize, diags: &mut Vec<Diagnostic>) -> Option<Ty> {
        let hard = self.s.hard[r].clone();
        let lits = self.s.lits[r].clone();
        let col = self.s.cols[r]
            .iter()
            .filter(|c| !c.0.contains("__"))
            .min_by_key(|c| (self.declared.get(&c.0).is_none(), c.0.contains("::")))
            .or_else(|| self.s.cols[r].first())
            .map(|c| shown_col(c, self.declared, &self.head_names));
        let col = col.unwrap_or_else(|| "this value".into());
        let mut from: Option<Hard> = None;
        let mut bad = false;
        for h in hard {
            match &from {
                None => from = Some(h),
                Some(f) if compatible(&f.ty, &h.ty) => {
                    let ty = narrower(f.ty.clone(), &h.ty);
                    from = Some(Hard { ty, ..f.clone() });
                }
                Some(f) => {
                    bad = true;
                    let named = |h: &Hard| match &h.col {
                        Some(c) => shown_col(c, self.declared, &self.head_names),
                        None => col.clone(),
                    };
                    let (hc, fc) = (named(&h), named(f));
                    let msg = if hc == fc {
                        format!("{hc} is {} here ({}) and {} elsewhere", h.ty, h.what, f.ty)
                    } else {
                        format!(
                            "{hc} is {} here ({}), and joins {fc}, which is {}",
                            h.ty, h.what, f.ty
                        )
                    };
                    diags.push(
                        Diagnostic::error(h.span, msg)
                            .with_label(f.span, format!("{fc} is {} here ({})", f.ty, f.what))
                            .with_help("one column has one type: declare it `any` to take both"),
                    );
                    break;
                }
            }
        }
        if bad {
            return None;
        }
        if let Some(f) = from {
            for l in &lits {
                if let Some(why) = types::mismatch(&f.ty, &Term::Val(l.value.clone())) {
                    diags.push(
                        Diagnostic::error(l.span, format!("{col} {why}"))
                            .with_label(f.span, format!("{col} is {} here ({})", f.ty, f.what)),
                    );
                }
            }
            return Some(f.ty);
        }
        // Literals only: their kind; a string where nothing says otherwise.
        let mut first: Option<(&Literal, Ty)> = None;
        let mut string: Option<&Literal> = None;
        for l in &lits {
            match kind(&l.value) {
                None => {
                    string.get_or_insert(l);
                }
                Some(k) => match &first {
                    None => first = Some((l, k)),
                    Some((f, fk)) if !compatible(fk, &k) => {
                        diags.push(
                            Diagnostic::error(
                                l.span,
                                format!("{col} holds {} here and {fk} elsewhere", shown(&l.value)),
                            )
                            .with_label(f.span, format!("{} here", shown(&f.value))),
                        );
                        return None;
                    }
                    Some(_) => {}
                },
            }
        }
        match (first, string) {
            (Some((l, k)), Some(s)) => {
                diags.push(
                    Diagnostic::error(
                        l.span,
                        format!("{col} holds {} here and strings elsewhere", shown(&l.value)),
                    )
                    .with_label(s.span, format!("{} here", shown(&s.value)))
                    .with_help(format!(
                        "one column has one type: write the {k} as one everywhere, or declare \
                         the column"
                    )),
                );
                None
            }
            (Some((_, k)), None) => Some(k),
            (None, Some(_)) => Some(Ty::Scalar("string".into())),
            (None, None) => None,
        }
    }
}

fn shown(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("the string {s:?}"),
        Value::Int(i) => format!("the int {i}"),
        Value::Bool(b) => format!("the bool {b}"),
        v => format!("`{}`", crate::partition::fmt_value(v)),
    }
}

impl Inferred {
    /// A table's column with no type of its own takes the type its uses
    /// settled, so its cells are read as one (`"10.0.0.0/8"` as an inet).
    pub fn type_tables(&self, externs: &mut [ExternFn]) {
        for e in externs.iter_mut().filter(|e| e.name.starts_with("table.")) {
            let n = e.args.len();
            for (i, b) in e.args.iter_mut().enumerate() {
                if b.ty.is_some() || b.input {
                    continue;
                }
                if let Some(Ty::Scalar(s)) = self.settled.get(&(e.name.clone(), n, i)) {
                    b.ty = Some(TypeExpr::Name(s.clone()));
                }
            }
        }
    }

    /// Read the program's literals as their columns' types (R-31): a
    /// string in an `inet` column is a network, a bare integer in a
    /// `bytes` one a quantity. What `infer` checked parses.
    pub fn read(&self, program: &mut Program) {
        let read = |ty: Option<&Ty>, t: &mut Term| {
            let Some(ty) = ty else { return };
            let wanted = matches!(ty, Ty::Scalar(s) if matches!(s.as_str(),
                "inet" | "ip" | "bytes" | "cpu" | "duration" | "time"));
            if !wanted || !matches!(t, Term::Val(_)) {
                return;
            }
            let v = std::mem::replace(t, Term::Wildcard);
            *t = match types::literal(ty, v.clone()) {
                Ok(x) => x,
                Err(_) => v,
            };
        };
        let atom = |a: &mut Atom| {
            if untyped(&a.pred) || function(&a.pred).is_some() {
                return;
            }
            let n = a.args.len();
            for (i, t) in a.args.iter_mut().enumerate() {
                read(self.settled.get(&(a.pred.clone(), n, i)), t);
            }
        };
        for s in &mut program.statements {
            match s {
                Stmt::Fact(a) => atom(a),
                Stmt::Rule(r) => {
                    atom(&mut r.head);
                    for l in &mut r.body {
                        if let Lit::Pos(a) | Lit::Not(a) = l {
                            atom(a);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// A table's columns read from its first document (R-34, R-55: a
/// relation with no `decl` takes its first source's): the first row's
/// keys in the order the document writes them (a CSV's header), each typed
/// by its value where the value says (`int`, `bool`); a string, and every
/// CSV cell, is unknown until the program's uses settle it.
pub fn document_columns(
    format: &str,
    table: &str,
    text: &str,
) -> Result<Vec<(String, Option<TypeExpr>)>> {
    use anyhow::{Context, anyhow};
    let first = match format {
        "csv" => {
            let mut r = csv::Reader::from_reader(text.as_bytes());
            let header = r.headers().context("a CSV table's first line names its columns")?;
            return Ok(header.iter().map(|h| (h.to_string(), None)).collect());
        }
        "json" => serde_json::from_str::<Vec<Row>>(text)
            .context("a JSON table is a list of objects, one per row")?
            .into_iter()
            .next(),
        "yaml" => serde_yaml::from_str::<Vec<Row>>(text)
            .context("a YAML table is a list of mappings, one per row")?
            .into_iter()
            .next(),
        "toml" => toml::from_str::<BTreeMap<String, Vec<Row>>>(text)
            .with_context(|| format!("a TOML table is its rows as `[[{table}]]`"))?
            .remove(table)
            .and_then(|rows| rows.into_iter().next()),
        f => return Err(anyhow!("unknown format {f}")),
    };
    let row = first.ok_or_else(|| anyhow!("it has no rows to read the columns from"))?;
    Ok(row
        .0
        .into_iter()
        .map(|(k, t)| (k, t.0.map(|n| TypeExpr::Name(n.to_string()))))
        .collect())
}

/// A document's row: its keys in order, each with its value's type.
struct Row(Vec<(String, Cell)>);

/// A cell's type where its value says: `None` for a string (unknown) and
/// for anything else the pass does not judge.
struct Cell(Option<&'static str>);

impl<'de> serde::Deserialize<'de> for Row {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Row;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a row: a mapping of its columns")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut m: A,
            ) -> std::result::Result<Row, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, Cell>()? {
                    out.push((k, v));
                }
                Ok(Row(out))
            }
        }
        d.deserialize_map(V)
    }
}

impl<'de> serde::Deserialize<'de> for Cell {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Cell;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a value")
            }
            fn visit_bool<E>(self, _: bool) -> std::result::Result<Cell, E> {
                Ok(Cell(Some("bool")))
            }
            fn visit_i64<E>(self, _: i64) -> std::result::Result<Cell, E> {
                Ok(Cell(Some("int")))
            }
            fn visit_u64<E>(self, _: u64) -> std::result::Result<Cell, E> {
                Ok(Cell(Some("int")))
            }
            fn visit_f64<E>(self, _: f64) -> std::result::Result<Cell, E> {
                Ok(Cell(None))
            }
            fn visit_str<E>(self, _: &str) -> std::result::Result<Cell, E> {
                Ok(Cell(None))
            }
            fn visit_unit<E>(self) -> std::result::Result<Cell, E> {
                Ok(Cell(None))
            }
            fn visit_none<E>(self) -> std::result::Result<Cell, E> {
                Ok(Cell(None))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut s: A,
            ) -> std::result::Result<Cell, A::Error> {
                while s.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(Cell(None))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut m: A,
            ) -> std::result::Result<Cell, A::Error> {
                while m
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(Cell(None))
            }
        }
        d.deserialize_any(V)
    }
}

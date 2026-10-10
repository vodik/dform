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
//!    head taking a body's column), but a variable given to a let or an
//!    output flows into its cell one way (R-213): a declared type is the
//!    cell's and what flows in is checked assignable to it, an untyped
//!    cell is the wider of what flows in (an enum and a string a string).
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
use crate::spell;
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
pub struct Declared(
    BTreeMap<String, (Decl, bool)>,
    /// The outputs declared with a type, by (scope, name): the stack's own
    /// (`""`) and each copy's (`instance` or `use`), its component's.
    BTreeMap<(String, String), TypeExpr>,
);

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
        Declared(out, outputs(program))
    }

    /// The declared type of the output `name` of `scope`.
    fn output(&self, scope: &str, name: &str) -> Option<&TypeExpr> {
        self.1.get(&(scope.to_string(), name.to_string()))
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

/// The typed outputs of `program`, by (scope, name): its own top-level
/// ones (scope `""`) and each top-level copy's, its component's.
fn outputs(program: &Program) -> BTreeMap<(String, String), TypeExpr> {
    let typed = |stmts: &[Stmt]| -> Vec<(String, TypeExpr)> {
        stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Output(o) => Some((o.name.clone(), o.ty.clone()?)),
                _ => None,
            })
            .collect()
    };
    let modules = crate::modules::definitions(&program.statements);
    let mut out = BTreeMap::new();
    for (k, t) in typed(&program.statements) {
        out.insert((String::new(), k), t);
    }
    for s in &program.statements {
        let (Stmt::Instance(u) | Stmt::Use(u)) = s else {
            continue;
        };
        if let Some(m) = modules.get(u.module.as_str()) {
            for (k, t) in typed(&m.body) {
                out.insert((u.name.clone(), k), t);
            }
        }
    }
    out
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
    /// A builtin's `string` parameter: a value type is given to it as its
    /// print (R-133), so beside one it constrains nothing.
    prints: bool,
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
    /// What a node holds of the two collections: an object (where one is
    /// given to it), a list. `x in o` over objects only is an error.
    shape: Vec<(Option<Span>, bool)>,
}

impl Solver {
    fn node(&mut self) -> usize {
        let n = self.parent.len();
        self.parent.push(n);
        self.hard.push(Vec::new());
        self.lits.push(Vec::new());
        self.cols.push(Vec::new());
        self.col_of.push(None);
        self.shape.push((None, false));
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
        let (o, l) = self.shape[hi];
        let mine = &mut self.shape[lo];
        *mine = (mine.0.or(o), mine.1 || l);
    }

    /// `n` holds an object (`Some(span)`) or a list.
    fn holds(&mut self, n: usize, object: Option<Span>, list: bool) {
        let n = self.find(n);
        let mine = &mut self.shape[n];
        *mine = (mine.0.or(object), mine.1 || list);
    }

    fn hard(&mut self, n: usize, ty: Ty, span: Span, what: String) {
        self.typed(n, ty, span, what, false);
    }

    fn typed(&mut self, n: usize, ty: Ty, span: Span, what: String, prints: bool) {
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
            prints,
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

/// `x in l` (`member(L, X)`) over a variable, checked once the program's
/// objects and lists are known.
struct Member {
    rule: usize,
    list: String,
    item: Term,
    span: Span,
}

/// `x.p` on a variable (`__path(X, "p")`), checked once the columns are
/// settled: a field is read of an object (R-185).
struct Field {
    rule: usize,
    var: String,
    path: String,
    span: Span,
}

/// `x = r.p` with `r` a reference (`c = s.vpc.cidr`, `__path(Vpc,
/// "cidr")`): `x` takes the type the schema gives the attribute of the
/// resource `r` names, once `r`'s type is known.
struct Through {
    rule: usize,
    var: String,
    path: String,
    into: usize,
    span: Span,
}

/// A rule giving a let or an output its value from a variable (`let
/// apex = env where ..`): the value flows into the cell, one way. A cell
/// with a declared type keeps it, and what flows in is checked
/// assignable to it (an enum to a string); an untyped one is the wider of
/// what flows in and its literals (R-213).
#[derive(Clone)]
struct Flow {
    rule: usize,
    var: String,
    /// The cell's node.
    cell: usize,
    span: Span,
}

/// What flowed into a cell: its type, the variable as written, the rule.
struct Given {
    ty: Ty,
    var: String,
    what: String,
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
    members: Vec<Member>,
    fields: Vec<Field>,
    throughs: Vec<Through>,
    flows: Vec<Flow>,
    /// The flows into each cell, by its node once the nodes are joined.
    into: BTreeMap<usize, Vec<Flow>>,
    /// Each rule's (statement's) variables.
    vars: Vec<Vars>,
    /// A rule head's variable names per column, for the signature.
    head_names: BTreeMap<Col, String>,
    /// Errors found while collecting: a reference attribute compared with
    /// a string (`s.vpc == "main"`).
    diags: Vec<Diagnostic>,
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
            Term::Obj(_) if constant(t).is_some() => {
                self.literal(col, &constant(t).expect("a constant"), span)
            }
            t => {
                self.shape_of(col, t, span);
                self.calls(rule, t, span);
                if let Some((ty, what)) = self.term_type(t) {
                    self.s.hard(col, ty, span, what);
                }
            }
        }
    }

    /// What collection `t` is, given to node `n`: an object or a list
    /// literal, or a call whose result is declared one.
    fn shape_of(&mut self, n: usize, t: &Term, span: Span) {
        match t {
            Term::Obj(_) | Term::Val(Value::Obj(_)) => self.s.holds(n, Some(span), false),
            Term::List(_) | Term::Val(Value::List(_)) | Term::ListComp { .. } => {
                self.s.holds(n, None, true)
            }
            Term::Func { name, .. } => {
                if let Some(f) = function(name) {
                    let ret = f.ret.trim();
                    if ret.starts_with('{') {
                        self.s.holds(n, Some(span), false);
                    } else if ret.starts_with("list") || ret.starts_with("set") {
                        self.s.holds(n, None, true);
                    }
                }
            }
            _ => {}
        }
    }

    fn literal(&mut self, n: usize, v: &Value, span: Span) {
        self.shape_of(n, &Term::Val(v.clone()), span);
        // An object is judged only by a `map(T)` column (`settle`).
        if kind(v).is_some() || matches!(v, Value::Str(_) | Value::Obj(_)) {
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
        if name == crate::address::REF && args.len() == 3 {
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
    /// type (a package function's), and an arithmetic operation is
    /// checked.
    fn calls(&mut self, rule: usize, t: &Term, span: Span) {
        match t {
            Term::Func { name, args } => {
                if let ("__path", [Term::Var(v), Term::Val(Value::Str(path))]) =
                    (name.as_str(), args.as_slice())
                {
                    self.fields.push(Field {
                        rule,
                        var: v.clone(),
                        path: path.clone(),
                        span,
                    });
                }
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
                let prints = p.ty == "string";
                let what = format!("`{name}`'s argument `{}`", p.name);
                self.s.typed(n, ty, span, what, prints);
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
            // An output's or a `let`'s cell: one column, whatever reads it
            // or contributes to it, typed by the output's declaration and
            // the values given it (R-34).
            (
                "attr" | "arg",
                [
                    Term::Val(Value::Str(typ)),
                    Term::Val(Value::Str(scope)),
                    Term::Val(Value::Str(p)),
                    v,
                    ..,
                ],
            ) if typ == crate::transform::OUTPUT || typ == "let" => {
                let cell = match scope.as_str() {
                    "" => format!("{typ} {p}"),
                    s => format!("{typ} {s}.{p}"),
                };
                let c = (cell.clone(), 1, 0);
                if typ == crate::transform::OUTPUT
                    && let Some(t) = self.declared.output(scope, p)
                {
                    let n = self.s.column(&c);
                    self.s.hard(n, types::of_expr(t), a.span, cell);
                }
                // A variable or a literal; a computed value (an attribute
                // read, `ref(T, A, path)`) is typed where it is read. A
                // rule's variable flows into the cell (`Flow`).
                match v {
                    Term::Var(x) if a.pred == "arg" => {
                        let node = self.s.column(&c);
                        self.var(rule, x);
                        self.flows.push(Flow {
                            rule,
                            var: x.clone(),
                            cell: node,
                            span: a.span,
                        });
                    }
                    Term::Var(_) | Term::Val(_) => self.arg(rule, c, v, a.span),
                    t => self.calls(rule, t, a.span),
                }
                return;
            }
            (
                "attr",
                [
                    Term::Val(Value::Str(typ)),
                    scope,
                    Term::Val(Value::Str(p)),
                    Term::Var(v),
                ],
            ) => {
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
            // `s.vpc == "main"`, the join `attr(net.subnet, S, "vpc",
            // "main")`: a reference is never a string.
            (
                "attr",
                [
                    Term::Val(Value::Str(typ)),
                    addr,
                    Term::Val(Value::Str(p)),
                    Term::Val(Value::Str(text)),
                ],
            ) => {
                if let Some(Ty::Ref(want)) = self
                    .schema
                    .and_then(|s| s.attr(typ, p))
                    .map(|a| Ty::parse(&a.ty))
                {
                    let read = format!("{}.{p}", shown_term(addr));
                    let written = format!("`{read} == {text:?}`");
                    self.diags
                        .push(ref_and_string(&written, &read, &want, text, a.span));
                }
                return;
            }
            ("member", [list, item]) if !matches!(item, Term::Var(_)) => {
                self.member(rule, list, item, a.span);
                self.calls(rule, list, a.span);
                return;
            }
            ("member", [list, Term::Var(v)]) => {
                self.member(rule, list, &Term::Var(v.clone()), a.span);
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

    /// `x in l`: over an object, an error naming the pattern (R-58); over
    /// a variable, checked in `solve`.
    fn member(&mut self, rule: usize, list: &Term, item: &Term, span: Span) {
        match list {
            Term::Var(v) => self.members.push(Member {
                rule,
                list: v.clone(),
                item: item.clone(),
                span,
            }),
            Term::Obj(_) | Term::Val(Value::Obj(_)) => self.members.push(Member {
                rule,
                list: String::new(),
                item: item.clone(),
                span,
            }),
            _ => {}
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
                        let n = self.var(rule, x);
                        if let Term::Func { name, args } = t
                            && let ("__path", [Term::Var(v), Term::Val(Value::Str(path))]) =
                                (name.as_str(), args.as_slice())
                        {
                            self.throughs.push(Through {
                                rule,
                                var: v.clone(),
                                path: path.clone(),
                                into: n,
                                span,
                            });
                        }
                        self.shape_of(n, t, span);
                        if let Some((ty, what)) = self.term_type(t) {
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

/// An object literal of constants as its value (`{ x: "big" }`).
fn constant(t: &Term) -> Option<Value> {
    match t {
        Term::Val(v) => Some(v.clone()),
        Term::Obj(m) => m
            .iter()
            .map(|(k, x)| Some((k.clone(), constant(x)?)))
            .collect::<Option<_>>()
            .map(Value::Obj),
        _ => None,
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
        Value::Float(_) => s("float"),
        Value::Bool(_) => s("bool"),
        Value::IpNet { .. } => s("inet"),
        Value::Ip(_) => s("ip"),
        Value::Quantity(q) => s(q.dim().name()),
        Value::Time(_) => s("time"),
        Value::Uri(_) => s("uri"),
        Value::Oci(_) => s("oci"),
        Value::Semver(_) => s("semver"),
        Value::Range(r) => s(&format!("range({})", r.element()?)),
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
        // Numbers compare by value (R-75).
        (Ty::Scalar(x), Ty::Scalar(y)) if number(x) && number(y) => true,
        (Ty::List(x), Ty::List(y)) | (Ty::Map(x), Ty::Map(y)) => compatible(x, y),
        (Ty::Ref(x), Ty::Ref(y)) => Ty::ref_types(x).any(|t| Ty::ref_types(y).any(|u| t == u)),
        (Ty::Scalar(x), Ty::Scalar(y)) => x == y,
        _ => false,
    }
}

/// A value type with a canonical print, given as it where a string is
/// wanted: an `oci`, a `uri`, a network or an address, a time, a quantity.
fn printed(ty: &Ty) -> bool {
    matches!(ty, Ty::Scalar(s) if matches!(s.as_str(),
        "oci" | "uri" | "inet" | "ip" | "time" | "bytes" | "cpu" | "duration"
        | "semver") || crate::range::element(s).is_some())
}

/// A type whose values have no fields to read: a string, a number, a
/// bool, an address, a quantity or a time, a list, a reference. A uri, an
/// image reference, a network, a version and a range have their parts.
fn fieldless(ty: &Ty) -> bool {
    match ty {
        Ty::Secret(t) => fieldless(t),
        Ty::Scalar(s) => {
            !matches!(s.as_str(), "uri" | "oci" | "inet" | "semver")
                && crate::range::element(s).is_none()
        }
        Ty::Enum(_) | Ty::List(_) | Ty::Ref(_) => true,
        Ty::Map(_) | Ty::Any => false,
    }
}

/// `int`, `float`, `number` (either).
fn number(ty: &str) -> bool {
    matches!(ty, "int" | "float" | "number")
}

/// `narrow` is read from a string: an `inet`, an `ip`, an image
/// reference (R-133: and is its text where a string is wanted).
fn text_of(string: &str, narrow: &str) -> bool {
    string == "string"
        && (matches!(narrow, "inet" | "ip" | "oci" | "semver")
            || crate::range::element(narrow).is_some())
}

/// The more telling of two compatible types: an enum over a string, a
/// typed list over an untyped one; an int and a float are numbers.
fn narrower(a: Ty, b: &Ty) -> Ty {
    match (&a, b) {
        (Ty::Scalar(x), Ty::Scalar(y)) if x != y && number(x) && number(y) => {
            Ty::Scalar("number".into())
        }
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
        members: Vec::new(),
        fields: Vec::new(),
        throughs: Vec::new(),
        flows: Vec::new(),
        into: BTreeMap::new(),
        vars: Vec::new(),
        head_names: BTreeMap::new(),
        diags: Vec::new(),
    };
    // The declarations: every relation the program has that a `decl`
    // types, and the externs' typed columns.
    let mut arities: BTreeMap<String, std::collections::BTreeSet<usize>> = BTreeMap::new();
    for s in &program.statements {
        match s {
            Stmt::Fact(a) => {
                arities
                    .entry(a.pred.clone())
                    .or_default()
                    .insert(a.args.len());
            }
            Stmt::Rule(r) => {
                arities
                    .entry(r.head.pred.clone())
                    .or_default()
                    .insert(r.head.args.len());
                for l in &r.body {
                    if let Lit::Pos(a) | Lit::Not(a) = l {
                        arities
                            .entry(a.pred.clone())
                            .or_default()
                            .insert(a.args.len());
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
                format!("{}'s column {}", e.name, b.name),
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
                    // A reference column's variable (R-185): `ref(T, W, "")`.
                    let named = match t {
                        Term::Func { name, args } if name == crate::address::REF => args.get(1),
                        t => Some(t),
                    };
                    if let Some(Term::Var(v)) = named
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
        Term::Func { name, args } if name == "__path" => match args.as_slice() {
            [Term::Var(v), Term::Val(Value::Str(path))] => format!("{}.{path}", shown_var(v)),
            _ => spell::term(t),
        },
        t => spell::term(t),
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
    fn solve(
        mut self,
        arities: &BTreeMap<String, std::collections::BTreeSet<usize>>,
    ) -> Result<Inferred> {
        let mut diags = Vec::new();
        for t in std::mem::take(&mut self.throughs) {
            let Some(&n) = self.vars[t.rule].0.get(&t.var) else {
                continue;
            };
            let r = self.s.find(n);
            let ty = self.s.hard[r].iter().find_map(|h| match &h.ty {
                Ty::Ref(u) => self.through(u, &t.path),
                _ => None,
            });
            if let Some(ty) = ty {
                let what = format!("{}.{}", shown_var(&t.var), t.path);
                self.s.hard(t.into, ty, t.span, what);
            }
        }
        self.own_values();
        let mut settled: BTreeMap<usize, Option<Ty>> = BTreeMap::new();
        let n = self.s.parent.len();
        for i in 0..n {
            let r = self.s.find(i);
            self.settled(r, &mut settled, &mut diags);
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
                    // `s.vpc.cidr`: the attribute of the resource `s.vpc`
                    // names.
                    Term::Func { name, args } if name == "__path" => match args.as_slice() {
                        [Term::Var(v), Term::Val(Value::Str(path))] => {
                            let n = *p.vars[c.rule].0.get(v)?;
                            let r = p.s.find(n);
                            match settled.get(&r).cloned().flatten()? {
                                Ty::Ref(u) => p.through(&u, path),
                                _ => None,
                            }
                        }
                        _ => None,
                    },
                    t => p.term_type(t).map(|(t, _)| t),
                }
            };
            let (ta, tb) = (ty(&mut self, &c.a), ty(&mut self, &c.b));
            let written = format!("`{} {} {}`", shown_term(&c.a), c.op, shown_term(&c.b));
            if matches!(c.op, "+" | "-" | "*" | "/" | "%") {
                for (t, ty) in [(&c.a, &ta), (&c.b, &tb)] {
                    let Some(ty) = ty else { continue };
                    let number = matches!(ty, Ty::Scalar(s) if number(s) || matches!(s.as_str(),
                        "bytes" | "cpu" | "duration" | "time"));
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
                (Some(Ty::Ref(want)), _, x, Term::Val(Value::Str(text)))
                | (_, Some(Ty::Ref(want)), Term::Val(Value::Str(text)), x) => {
                    diags.push(ref_and_string(&written, &shown_term(x), want, text, c.span));
                }
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
        // `x in o` over an object: its entries are `(k, v) in o` (R-58).
        let members = std::mem::take(&mut self.members);
        for m in &members {
            let object = if m.list.is_empty() {
                true
            } else {
                match self.vars[m.rule].0.get(&m.list).copied() {
                    Some(n) => {
                        let r = self.s.find(n);
                        matches!(self.s.shape[r], (Some(_), false))
                    }
                    None => false,
                }
            };
            if object {
                let x = shown_term(&m.item);
                let o = match m.list.as_str() {
                    "" => "{..}".to_string(),
                    l => shown_var(l),
                };
                diags.push(
                    Diagnostic::error(
                        m.span,
                        format!(
                            "`{x} in {o}`: `{o}` is an object, and an object's entries are \
                             matched by a pattern"
                        ),
                    )
                    .with_help(format!(
                        "`(k, v) in {o}` takes each key and value, `(k, _) in {o}` each key (R-58)"
                    )),
                );
            }
        }
        diags.append(&mut self.diags);
        let fields = std::mem::take(&mut self.fields);
        for f in &fields {
            if let Some(d) = self.field_of(f, &settled) {
                diags.push(d);
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

    /// The type the schema gives `path` read through a reference to a
    /// `typ` (one type), hop by hop where the path passes another
    /// reference: `cidr` of a `net.vpc`, `vpc.cidr` of a `net.subnet`.
    fn through(&self, typ: &str, path: &str) -> Option<Ty> {
        if typ.contains('|') {
            return None;
        }
        let schema = self.schema?;
        if let Some(a) = schema.attr(typ, path) {
            return Some(Ty::parse(&a.ty));
        }
        let keys = crate::address::path_keys(path);
        (1..keys.len()).rev().find_map(|i| {
            let head = keys[..i].join(".");
            match Ty::parse(&schema.attr(typ, &head)?.ty) {
                Ty::Ref(u) => self.through(&u, &keys[i..].join(".")),
                _ => None,
            }
        })
    }

    /// `x.p` where `x` joins a relation's column whose type has no fields
    /// (a string, a number, a list, a reference): the error naming the
    /// column and its type, where it read nothing at run time (R-185).
    fn field_of(&mut self, f: &Field, settled: &BTreeMap<usize, Option<Ty>>) -> Option<Diagnostic> {
        let n = *self.vars[f.rule].0.get(&f.var)?;
        let r = self.s.find(n);
        let ty = settled.get(&r).cloned().flatten()?;
        if !fieldless(&ty) {
            return None;
        }
        let col = self.s.cols[r]
            .iter()
            .filter(|c| !untyped(&c.0) && function(&c.0).is_none())
            .min_by_key(|c| (self.declared.get(&c.0).is_none(), c.0.contains("::")))?;
        let col = shown_col(col, self.declared, &self.head_names);
        let x = shown_var(&f.var);
        let first = crate::address::path_keys(&f.path).into_iter().next()?;
        let read = format!("{x}.{}", f.path);
        let help = match &ty {
            Ty::Ref(t) => format!(
                "bind it by its type where the column is filled, `{x} in {}`: the column then \
                 holds the resource, and `{read}` reads its attribute",
                Ty::ref_types(t).next().unwrap_or(t)
            ),
            _ => format!(
                "a resource's attributes are read through a reference: bind one by its type \
                 where the column is filled (`{x} in k8s.deployment`), and `{read}` reads it"
            ),
        };
        Some(
            Diagnostic::error(
                f.span,
                format!("`{read}`: `{x}` is {ty} ({col}), which has no field `{first}`"),
            )
            .with_help(help),
        )
    }

    /// A flow from a variable nothing types, and that nothing flows into,
    /// is the cell's own value: one node with it, its literals read at
    /// the cell's type (`let c: inet = x where x = "10.0.0.0/8"`).
    fn own_values(&mut self) {
        let flows = std::mem::take(&mut self.flows);
        let mut kept: Vec<Flow> = Vec::new();
        for (i, f) in flows.iter().enumerate() {
            let n = self.vars[f.rule].0[&f.var];
            let (src, cell) = (self.s.find(n), self.s.find(f.cell));
            if src == cell {
                continue;
            }
            let given = flows[i + 1..]
                .iter()
                .chain(&kept)
                .any(|g| self.s.find(g.cell) == src);
            if self.s.hard[src].is_empty() && !given {
                self.s.union(n, f.cell);
            } else {
                kept.push(f.clone());
            }
        }
        for f in kept {
            let cell = self.s.find(f.cell);
            self.into.entry(cell).or_default().push(f);
        }
    }

    /// The type of the node `r`, settled once, after what flows into it.
    fn settled(
        &mut self,
        r: usize,
        settled: &mut BTreeMap<usize, Option<Ty>>,
        diags: &mut Vec<Diagnostic>,
    ) -> Option<Ty> {
        if let Some(t) = settled.get(&r) {
            return t.clone();
        }
        // A cycle of flows reads nothing from itself.
        settled.insert(r, None);
        let mut given = Vec::new();
        for f in self.into.get(&r).cloned().unwrap_or_default() {
            let (var, span) = (shown_var(&f.var), f.span);
            let src = self.s.find(self.vars[f.rule].0[&f.var]);
            let Some(ty) = self.settled(src, settled, diags) else {
                continue;
            };
            let what = self.s.hard[src]
                .iter()
                .find(|h| h.ty == ty)
                .or(self.s.hard[src].first())
                .map(|h| h.what.clone())
                .or_else(|| self.cell_name(src))
                .unwrap_or_else(|| "given it".into());
            given.push(Given {
                ty,
                var,
                what,
                span,
            });
        }
        let t = self.settle(r, &given, diags);
        settled.insert(r, t.clone());
        t
    }

    /// The let or the output a node is the cell of, as the program names
    /// it: `let apex`, `output ip`.
    fn cell_name(&self, r: usize) -> Option<String> {
        let output = format!("{} ", crate::transform::OUTPUT);
        self.s.cols[r]
            .iter()
            .find(|c| c.0.starts_with("let ") || c.0.starts_with(&output))
            .map(|c| c.0.clone())
    }

    /// Settle the node `r`, given `given` by the rules that flow into it:
    /// its type, or the errors that say why it has none.
    fn settle(&mut self, r: usize, given: &[Given], diags: &mut Vec<Diagnostic>) -> Option<Ty> {
        let mut hard = self.s.hard[r].clone();
        // A value type given to a builtin's `string` parameter is its print
        // there (R-133: `str.starts_with(c.image, "ghcr.io/")` over an `oci`).
        if hard.iter().any(|h| !h.prints && printed(&h.ty)) {
            hard.retain(|h| !h.prints);
        }
        let lits = self.s.lits[r].clone();
        let col = self.cell_name(r).or_else(|| {
            self.s.cols[r]
                .iter()
                .filter(|c| !c.0.contains("__"))
                .min_by_key(|c| (self.declared.get(&c.0).is_none(), c.0.contains("::")))
                .or_else(|| self.s.cols[r].first())
                .map(|c| shown_col(c, self.declared, &self.head_names))
        });
        let col = col.unwrap_or_else(|| "this value".into());
        let mut from: Option<Hard> = None;
        let mut bad = false;
        for h in hard {
            match &from {
                None => from = Some(h),
                // A column a rule per type fills holds a reference to any
                // of them (R-185): `ref(k8s.deployment | k8s.stateful_set)`.
                Some(f) if matches!((&f.ty, &h.ty), (Ty::Ref(_), Ty::Ref(_))) => {
                    let (Ty::Ref(a), Ty::Ref(b)) = (&f.ty, &h.ty) else {
                        unreachable!("matched");
                    };
                    let ty = Ty::ref_union(a, b);
                    from = Some(Hard { ty, ..f.clone() });
                }
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
            // A declared type is the cell's: what flows in is checked
            // assignable to it, and never narrows it (R-213).
            if let Some(g) = given.iter().find(|g| !compatible(&g.ty, &f.ty)) {
                let msg = if f.what.starts_with("decl ") || f.what == col {
                    format!("{col}: {} takes {}: {}", f.ty, g.var, not_a(&g.ty, &f.ty))
                } else {
                    format!(
                        "{col} is {} ({}) and takes {}: {}",
                        f.ty,
                        f.what,
                        g.var,
                        not_a(&g.ty, &f.ty)
                    )
                };
                diags.push(Diagnostic::error(f.span, msg).with_label(
                    g.span,
                    format!("{} is {} here ({})", g.var, a(&g.ty), g.what),
                ));
                return None;
            }
            let map = matches!(&f.ty, Ty::Map(_));
            for l in lits
                .iter()
                .filter(|l| map || !matches!(l.value, Value::Obj(_)))
            {
                if let Some(why) = types::mismatch(&f.ty, &Term::Val(l.value.clone())) {
                    diags.push(
                        Diagnostic::error(l.span, format!("{col} {why}"))
                            .with_label(f.span, format!("{col} is {} here ({})", f.ty, f.what)),
                    );
                }
            }
            return Some(f.ty);
        }
        if let Some(g) = given.first() {
            return self.widened(&col, g, &given[1..], &lits, diags);
        }
        // Literals only: their kind; a string where nothing says otherwise.
        let mut first: Option<(&Literal, Ty)> = None;
        let mut string: Option<&Literal> = None;
        for l in &lits {
            match kind(&l.value) {
                None if matches!(l.value, Value::Obj(_)) => {}
                None => {
                    string.get_or_insert(l);
                }
                Some(k) => match &mut first {
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
                    Some((_, fk)) => *fk = narrower(fk.clone(), &k),
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

    /// An untyped cell's type: the wider of what flows into it (`g` and
    /// `rest`) and its literals, an enum and a string a string (R-213).
    fn widened(
        &self,
        col: &str,
        g: &Given,
        rest: &[Given],
        lits: &[Literal],
        diags: &mut Vec<Diagnostic>,
    ) -> Option<Ty> {
        let mut ty = g.ty.clone();
        for h in rest {
            match wider(&ty, &h.ty) {
                Some(t) => ty = t,
                None => {
                    diags.push(
                        Diagnostic::error(
                            h.span,
                            format!(
                                "{col} takes {} here, {}, and {} elsewhere, {}: one value has \
                                 one type",
                                h.var,
                                a(&h.ty),
                                g.var,
                                a(&g.ty)
                            ),
                        )
                        .with_label(
                            g.span,
                            format!("{} is {} here ({})", g.var, a(&g.ty), g.what),
                        ),
                    );
                    return None;
                }
            }
        }
        let map = matches!(&ty, Ty::Map(_));
        for l in lits
            .iter()
            .filter(|l| map || !matches!(l.value, Value::Obj(_)))
        {
            let wide = match (&l.value, kind(&l.value)) {
                (Value::Str(_), _)
                    if types::mismatch(&ty, &Term::Val(l.value.clone())).is_none() =>
                {
                    continue;
                }
                (Value::Str(_), _) => to_string(&ty).then(|| Ty::Scalar("string".into())),
                (_, Some(k)) => wider(&ty, &k),
                _ => continue,
            };
            match wide {
                Some(t) => ty = t,
                None => {
                    diags.push(
                        Diagnostic::error(
                            l.span,
                            format!(
                                "{col} takes {} here, and {} elsewhere, {}: one value has one type",
                                shown(&l.value),
                                g.var,
                                a(&g.ty)
                            ),
                        )
                        .with_label(
                            g.span,
                            format!("{} is {} here ({})", g.var, a(&g.ty), g.what),
                        ),
                    );
                    return None;
                }
            }
        }
        Some(ty)
    }
}

/// The type two values of `a` and `b` given to one untyped cell are both
/// of: an enum and a string a string, two enums their members together,
/// an int and a float a number; `None` where they never meet.
fn wider(a: &Ty, b: &Ty) -> Option<Ty> {
    let string = || Some(Ty::Scalar("string".into()));
    match (a, b) {
        _ if a == b => Some(a.clone()),
        (Ty::Enum(x), Ty::Enum(y)) => {
            let mut m = x.clone();
            m.extend(y.iter().filter(|v| !x.contains(v)).cloned());
            Some(Ty::Enum(m))
        }
        (Ty::Scalar(s), t) | (t, Ty::Scalar(s)) if s == "string" && to_string(t) => string(),
        (Ty::Scalar(x), Ty::Scalar(y)) if number(x) && number(y) => {
            Some(Ty::Scalar("number".into()))
        }
        (Ty::Ref(x), Ty::Ref(y)) => Some(Ty::ref_union(x, y)),
        _ if compatible(a, b) => Some(narrower(a.clone(), b)),
        _ => None,
    }
}

/// A type whose values are text a string holds as they are: an enum's
/// members, a network, an address, an image reference, a version.
fn to_string(ty: &Ty) -> bool {
    match ty {
        Ty::Enum(_) => true,
        Ty::Scalar(s) => text_of("string", s),
        _ => false,
    }
}

/// `ty` with its article: `an int`, `a string`; an enum as it is written.
fn a(ty: &Ty) -> String {
    match ty {
        Ty::Scalar(s) if s.starts_with(['a', 'e', 'i', 'o']) => format!("an {ty}"),
        Ty::Scalar(_) | Ty::List(_) | Ty::Map(_) | Ty::Ref(_) | Ty::Secret(_) => format!("a {ty}"),
        _ => ty.to_string(),
    }
}

/// Why a value of `got` is not one of `want`: `an int is not a string`.
fn not_a(got: &Ty, want: &Ty) -> String {
    format!("{} is not {}", a(got), a(want))
}

/// A reference compared with a string (`written`, `read` the reference):
/// never equal, so an error, with the resource to write, or `has` where
/// the string is empty.
fn ref_and_string(written: &str, read: &str, want: &str, text: &str, span: Span) -> Diagnostic {
    let help = match (text.is_empty(), Ty::ref_types(want).next()) {
        (true, _) => format!("`has {read}` tests whether it is set"),
        (false, Some(typ)) if !want.contains('|') => format!(
            "compare with the resource: `{}`, or its name in scope",
            crate::address::Address {
                typ: typ.to_string(),
                name: text.to_string(),
            }
        ),
        _ => "compare with the resource: its name in scope, or `T[\"a\"]`".to_string(),
    };
    Diagnostic::error(
        span,
        format!(
            "{written} compares a reference, ref({want}), with the string {text:?}: a reference \
             is never a string"
        ),
    )
    .with_help(help)
}

fn shown(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("the string {s:?}"),
        Value::Int(i) => format!("the int {i}"),
        Value::Float(f) => format!("the float {f}"),
        Value::Bool(b) => format!("the bool {b}"),
        v => format!("`{}`", spell::value(v)),
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
                "inet" | "ip" | "float" | "bytes" | "cpu" | "duration" | "time"
                | "semver") || crate::range::element(s).is_some());
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
            let header = r
                .headers()
                .context("a CSV table's first line names its columns")?;
            return Ok(header.iter().map(|h| (h.to_string(), None)).collect());
        }
        "json" => serde_json::from_str::<Vec<Row>>(text)
            .context("a JSON table is a list of objects, one per row")?
            .into_iter()
            .next(),
        "yaml" => match crate::tables::yaml_stream(text)? {
            crate::tables::Stream::One(doc) => serde_yaml::from_value::<Vec<Row>>(doc),
            crate::tables::Stream::Many(docs, _) => {
                serde_yaml::from_value::<Vec<Row>>(serde_yaml::Value::Sequence(docs))
            }
        }
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
                Ok(Cell(Some("float")))
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

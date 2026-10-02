//! Externs with binding patterns (DESIGN.org "Demand-driven extern
//! predicates", E §2.6, DR-7): `extern p(+in, -out, ...) [persist].`
//!
//! An extern is a predicate its provider answers on demand: a body literal
//! `p(t1, ..., tn)` asks once its `+` arguments are ground, and the
//! answers are facts of `p` with those inputs. The literals before it in
//! the body bind the inputs; an extern under `not`, in a recursive rule, or
//! defined by a rule is a compile error.
//!
//! Evaluation is by rounds: evaluate with the answers known so far; find
//! every call the rules demand (the body before each extern literal, over
//! the facts); ask the new ones; evaluate again until no call is new. An
//! extern is not in a cycle and not negated, so an answer never retracts
//! another round's demand: the last round is the fixpoint with every answer
//! it reads. Answers are recorded in the plan file (apply asks nothing the
//! plan already asked) and, for a `persist` extern, in state, where they
//! win over asking again (a generated password stays the same).
//!
//! A program does not declare an extern: `provider NAME {}` brings the
//! provider's into scope (DESIGN.org R-8). `file`, `env` and `random` are
//! built-in fact providers ([`BUILTINS`]): their externs are the
//! compiler's own, and dform answers `file.json(+path, -value)`,
//! `file.text(+path, -value)` (paths from the program's project root,
//! `project::base_of`) and `env.var(+name, -value)` itself, with no
//! `dform.toml` source. Other externs are asked of the providers over the
//! plugin protocol (Query; the mock answers from
//! `providers/<name>/externs.df`).

use crate::ast::{Atom, BindArg, ExternFn, Lit, Program, Span, Stmt, Term, TypeExpr};
use crate::diag::{Diagnostic, Diagnostics};
use crate::engine::{self, EvalResult};
use crate::value::Value;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

/// One call: the extern and its `+` arguments, in order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Call {
    pub pred: String,
    pub inputs: Vec<Value>,
}

/// A call and its answer: every row a full tuple of the extern. A secret
/// column is a secret null ([`secret_label`]), never the value, and
/// `held` says where its provider holds each (E DR-19): what a replay
/// hands the provider that reads it inside Apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub pred: String,
    pub inputs: Vec<Value>,
    pub rows: Vec<Vec<Value>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub held: BTreeMap<String, crate::provider::Held>,
}

impl Answer {
    /// The labels of the secret nulls in its rows.
    pub fn secret_labels(&self) -> Vec<String> {
        self.rows
            .iter()
            .flatten()
            .filter_map(|v| match v {
                Value::Null {
                    label,
                    class: crate::value::NullClass::Secret,
                    ..
                } => Some(label.clone()),
                _ => None,
            })
            .collect()
    }
}

/// The label of the secret an extern's call answers in column `col` (from
/// 0): `pred/INPUTS#N`, `N` the column from 1.
pub fn secret_label(pred: &str, inputs: &[Value], col: usize) -> String {
    let ins: Vec<String> = inputs
        .iter()
        .map(|v| match v {
            Value::Str(s) => s.clone(),
            v => crate::partition::fmt_value(v),
        })
        .collect();
    crate::value::null_label(pred, &ins.join(","), &(col + 1).to_string())
}

fn vars(t: &Term, out: &mut BTreeSet<String>) {
    match t {
        Term::Var(v) => {
            out.insert(v.clone());
        }
        Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|a| vars(a, out)),
        Term::Obj(m) => m.values().for_each(|a| vars(a, out)),
        _ => {}
    }
}

/// The rules and constraints of a lowered program, as (head, body, span).
fn bodies(program: &Program) -> Vec<(Option<&Atom>, &[Lit], Span)> {
    program
        .statements
        .iter()
        .filter_map(|s| match s {
            Stmt::Rule(r) => Some((Some(&r.head), r.body.as_slice(), r.head.span)),
            _ => None,
        })
        .collect()
}

/// An atom's node in the predicate graph: its predicate, but for a cell of
/// the aggregate that is not a resource's (a `let`, an input), whose
/// contributions (`arg`) and read (`attr`) are one node per cell.
fn node(a: &Atom) -> String {
    fn s(t: &Term) -> Option<&str> {
        match t {
            Term::Val(crate::value::Value::Str(x)) => Some(x),
            _ => None,
        }
    }
    match (a.pred.as_str(), a.args.as_slice()) {
        ("arg", [t, scope, k, _, _]) | ("attr", [t, scope, k, _])
            if s(t).is_some_and(crate::transform::is_pseudo_type) =>
        {
            match (s(t), s(scope), s(k)) {
                (Some(t), Some(scope), Some(k)) => format!("({t}, {scope}, {k})"),
                _ => a.pred.clone(),
            }
        }
        _ => a.pred.clone(),
    }
}

/// The compile-time rules: an extern is not defined by a rule, not negated,
/// not in a recursive rule, called with its arity, and its `+` arguments
/// are bound by the literals before it.
pub fn check(program: &Program, fns: &[ExternFn]) -> Result<()> {
    if fns.is_empty() {
        return Ok(());
    }
    let by: BTreeMap<&str, &ExternFn> = fns.iter().map(|f| (f.name.as_str(), f)).collect();
    let mut diags = Vec::new();
    // The predicate graph: body predicate -> heads of the rules reading it.
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (head, body, _) in bodies(program) {
        let Some(h) = head else { continue };
        for l in body {
            if let Lit::Pos(a) | Lit::Not(a) = l {
                edges.entry(node(a)).or_default().insert(node(h));
            }
        }
    }
    let reaches = |from: &Atom, to: &Atom| {
        let to = node(to);
        let mut seen = BTreeSet::new();
        let mut stack = vec![node(from)];
        while let Some(p) = stack.pop() {
            if p == to {
                return true;
            }
            if let Some(next) = edges.get(&p)
                && seen.insert(p)
            {
                stack.extend(next.iter().cloned());
            }
        }
        false
    };
    for s in &program.statements {
        if let Stmt::Fact(a) = s
            && let Some(f) = by.get(a.pred.as_str())
        {
            diags.push(defined_here(a, f));
        }
    }
    for (head, body, span) in bodies(program) {
        if let Some(h) = head
            && let Some(f) = by.get(h.pred.as_str())
        {
            diags.push(defined_here(h, f));
        }
        let mut bound: BTreeSet<String> = BTreeSet::new();
        for l in body {
            match l {
                Lit::Not(a) if by.contains_key(a.pred.as_str()) => diags.push(
                    Diagnostic::error(a.span, format!("extern {} under `not`", a.pred)).with_note(
                        "an extern answers what exists; its absence is not known, so it cannot be negated",
                    ),
                ),
                Lit::Pos(a) if by.contains_key(a.pred.as_str()) => {
                    let f = by[a.pred.as_str()];
                    if a.args.len() != f.args.len() {
                        diags.push(Diagnostic::error(
                            a.span,
                            format!(
                                "extern {} takes {} arguments, not {}",
                                f.name,
                                f.args.len(),
                                a.args.len()
                            ),
                        ));
                        continue;
                    }
                    for (t, b) in a.args.iter().zip(&f.args) {
                        let mut vs = BTreeSet::new();
                        vars(t, &mut vs);
                        if b.input
                            && let Some(v) = vs.iter().find(|v| !bound.contains(*v))
                        {
                            diags.push(
                                Diagnostic::error(
                                    a.span,
                                    format!(
                                        "extern {}: +{} is not bound ({v} is unbound before it)",
                                        f.name, b.name
                                    ),
                                )
                                .with_help("bind it with a literal before the extern"),
                            );
                        }
                    }
                    if let Some(h) = head
                        && let Some(p) = body.iter().find_map(|l| match l {
                            Lit::Pos(b) | Lit::Not(b)
                                if !by.contains_key(b.pred.as_str()) && reaches(h, b) =>
                            {
                                Some(b.pred.as_str())
                            }
                            _ => None,
                        })
                    {
                        let msg = match crate::tables::describe(&f.name) {
                            Some(t) => format!("{t}: its source reads its own rows"),
                            None => format!("extern {} in a recursive rule", f.name),
                        };
                        diags.push(Diagnostic::error(a.span, msg).with_note(format!(
                            "{} depends on itself through {p}; an extern is asked once its inputs are complete",
                            h.pred
                        )));
                    }
                    a.args.iter().for_each(|t| vars(t, &mut bound));
                }
                Lit::Pos(a) => a.args.iter().for_each(|t| vars(t, &mut bound)),
                Lit::Eq(x, y) => {
                    let (mut vx, mut vy) = (BTreeSet::new(), BTreeSet::new());
                    vars(x, &mut vx);
                    vars(y, &mut vy);
                    if vy.is_subset(&bound) {
                        bound.extend(vx);
                    } else if vx.is_subset(&bound) {
                        bound.extend(vy);
                    }
                }
                _ => {}
            }
        }
        let _ = span;
    }
    if diags.is_empty() {
        Ok(())
    } else {
        Err(Diagnostics(diags).into())
    }
}

fn defined_here(a: &Atom, f: &ExternFn) -> Diagnostic {
    Diagnostic::error(
        a.span,
        format!(
            "extern {} is answered by its provider; the program may not state it",
            f.name
        ),
    )
    .with_label(f.span, "declared extern here")
}

/// Is column `b` secret-typed (`-value: secret(T)`)?
pub fn is_secret(b: &BindArg) -> bool {
    matches!(&b.ty, Some(TypeExpr::Apply(n, _)) if n == "secret")
}

/// How a call is asked of its provider: the extern and its inputs, to rows.
type Ask<'a> = dyn Fn(&ExternFn, &[Value]) -> Result<Vec<Vec<Value>>> + 'a;

/// Where the answers of one run come from, in order: the plan file's, the
/// state's persisted ones, then the provider.
pub struct Externs<'a> {
    fns: BTreeMap<String, ExternFn>,
    /// The extern literal of each rule or constraint: the body before it,
    /// and the extern.
    sites: Vec<(Vec<Lit>, Atom)>,
    known: RefCell<BTreeMap<Call, Vec<Vec<Value>>>>,
    ask: Box<Ask<'a>>,
    /// The calls the last evaluation demanded.
    demanded: RefCell<BTreeSet<Call>>,
}

impl<'a> Externs<'a> {
    pub fn new(
        lowered: &Program,
        fns: &[ExternFn],
        ask: impl Fn(&ExternFn, &[Value]) -> Result<Vec<Vec<Value>>> + 'a,
    ) -> Self {
        let names: BTreeSet<&str> = fns.iter().map(|f| f.name.as_str()).collect();
        let mut sites = Vec::new();
        for (_, body, _) in bodies(lowered) {
            for (i, l) in body.iter().enumerate() {
                if let Lit::Pos(a) = l
                    && names.contains(a.pred.as_str())
                {
                    sites.push((body[..i].to_vec(), a.clone()));
                }
            }
        }
        Externs {
            fns: fns.iter().map(|f| (f.name.clone(), f.clone())).collect(),
            sites,
            known: RefCell::new(BTreeMap::new()),
            ask: Box::new(ask),
            demanded: RefCell::new(BTreeSet::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.fns.is_empty()
    }

    /// Answers already known (from a plan file or state): not asked again.
    pub fn preload(&self, answers: impl IntoIterator<Item = Answer>) {
        let mut known = self.known.borrow_mut();
        for a in answers {
            if self.fns.contains_key(&a.pred) {
                known
                    .entry(Call {
                        pred: a.pred,
                        inputs: a.inputs,
                    })
                    .or_insert(a.rows);
            }
        }
    }

    /// The answers state persisted: those of the `persist` externs (state
    /// may hold others, a table's commit, that are never replayed).
    pub fn preload_persisted(&self, answers: impl IntoIterator<Item = Answer>) {
        let persist = |a: &Answer| self.fns.get(&a.pred).is_some_and(|f| f.persist);
        self.preload(answers.into_iter().filter(persist));
    }

    fn facts(&self) -> Vec<Atom> {
        self.known
            .borrow()
            .iter()
            .flat_map(|(c, rows)| {
                rows.iter().map(|r| Atom {
                    pred: c.pred.clone(),
                    args: r.iter().cloned().map(Term::Val).collect(),
                    record: None,
                    span: Span::default(),
                })
            })
            .collect()
    }

    /// The calls the rules demand of `facts`: each extern literal's `+`
    /// arguments under every answer to the body before it. An input that
    /// is a null is not ground yet: no call.
    fn demand(&self, facts: &BTreeSet<Atom>) -> Result<BTreeSet<Call>> {
        let mut out = BTreeSet::new();
        for (prefix, a) in &self.sites {
            let f = &self.fns[&a.pred];
            let mut body = prefix.clone();
            let mut inputs = Vec::new();
            for (k, (t, b)) in a.args.iter().zip(&f.args).enumerate() {
                if b.input {
                    let v = format!("DformExternIn{k}");
                    body.push(Lit::Eq(Term::Var(v.clone()), t.clone()));
                    inputs.push(v);
                }
            }
            for (binding, _) in engine::query(&body, facts)? {
                let vals: Option<Vec<Value>> =
                    inputs.iter().map(|v| binding.get(v).cloned()).collect();
                let Some(vals) = vals else { continue };
                if vals.iter().any(|v| matches!(v, Value::Null { .. })) {
                    continue;
                }
                out.insert(Call {
                    pred: a.pred.clone(),
                    inputs: vals,
                });
            }
        }
        Ok(out)
    }

    /// Evaluate `program` with the externs answered: rounds until no call
    /// is new.
    pub fn eval(&self, program: &Program, extra: &[Atom]) -> Result<(EvalResult, Vec<String>)> {
        self.eval_at(program, extra, None)
    }

    /// `eval`, the planner's facts in `extra` given at apply tick `tick`
    /// (`engine::eval_at`).
    pub fn eval_at(
        &self,
        program: &Program,
        extra: &[Atom],
        tick: Option<usize>,
    ) -> Result<(EvalResult, Vec<String>)> {
        let (res, violations, ()) = self.rounds(extra, |given| {
            engine::eval_at(program, given, tick).map(|(r, v)| (r, v, ()))
        })?;
        Ok((res, violations))
    }

    /// `eval`, with the last round resumable with more given facts of the
    /// predicates `later` (`engine::Resumable`: the policy pass).
    pub fn eval_resumable(
        &self,
        program: &Program,
        extra: &[Atom],
        later: &[&str],
    ) -> Result<(EvalResult, Vec<String>, engine::Resumable)> {
        self.rounds(extra, |given| engine::eval_resumable(program, given, later))
    }

    /// After a resumed evaluation over `facts`: true, and `facts`' calls are
    /// the ones demanded, when every call they demand is answered; false
    /// when a call is new (evaluate again with `eval`).
    pub fn settle(&self, facts: &BTreeSet<Atom>) -> Result<bool> {
        if self.is_empty() {
            return Ok(true);
        }
        let demand = self.demand(facts)?;
        if !demand.iter().all(|c| self.known.borrow().contains_key(c)) {
            return Ok(false);
        }
        *self.demanded.borrow_mut() = demand;
        Ok(true)
    }

    /// Rounds of `eval_round` over `extra` and the answers known, asking
    /// every new call between them, until no call is new.
    fn rounds<T>(
        &self,
        extra: &[Atom],
        mut eval_round: impl FnMut(&[Atom]) -> Result<(EvalResult, Vec<String>, T)>,
    ) -> Result<(EvalResult, Vec<String>, T)> {
        for _ in 0..64 {
            let mut given = extra.to_vec();
            given.extend(self.facts());
            let (res, violations, t) = eval_round(&given)?;
            if self.is_empty() {
                return Ok((res, violations, t));
            }
            let demand = self.demand(&res.facts)?;
            let new: Vec<Call> = {
                let known = self.known.borrow();
                demand
                    .iter()
                    .filter(|c| !known.contains_key(c))
                    .cloned()
                    .collect()
            };
            if new.is_empty() {
                *self.demanded.borrow_mut() = demand;
                return Ok((res, violations, t));
            }
            for c in new {
                let f = &self.fns[&c.pred];
                let rows =
                    (self.ask)(f, &c.inputs).with_context(|| {
                        match crate::tables::describe(&c.pred) {
                            Some(t) => format!("{t} from {}", show(&c.inputs)),
                            None => format!("extern {}({})", c.pred, show(&c.inputs)),
                        }
                    })?;
                for r in &rows {
                    if r.len() != f.args.len() {
                        bail!(
                            "extern {}: an answer has {} columns, the declaration {}",
                            c.pred,
                            r.len(),
                            f.args.len()
                        );
                    }
                }
                self.known.borrow_mut().insert(c, rows);
            }
        }
        bail!(
            "externs: calls did not settle after 64 rounds (an extern's output feeding its own input?)"
        )
    }

    /// The answers the last evaluation read, for the plan file: every call
    /// it demanded, except a call with a secret column (a secret is never
    /// written in the clear).
    pub fn recorded(&self) -> Vec<Answer> {
        self.answers(|f| !f.args.iter().any(is_secret))
    }

    /// The environment variables the last evaluation read with `env.var`,
    /// as their labels (`env.var/NAME`): what the plan file records of
    /// them, never the value.
    pub fn env_labels(&self) -> Vec<String> {
        self.demanded
            .borrow()
            .iter()
            .filter(|c| c.pred == crate::syntax::resolve::ENV_VAR)
            .filter_map(|c| match c.inputs.as_slice() {
                [Value::Str(n)] => Some(format!("{}/{n}", c.pred)),
                _ => None,
            })
            .collect()
    }

    /// The answers of `persist` externs the last evaluation read, for state.
    pub fn persisted(&self) -> Vec<Answer> {
        self.answers(|f| f.persist)
    }

    fn answers(&self, keep: impl Fn(&ExternFn) -> bool) -> Vec<Answer> {
        let known = self.known.borrow();
        self.demanded
            .borrow()
            .iter()
            .filter(|c| keep(&self.fns[&c.pred]))
            .filter_map(|c| {
                Some(Answer {
                    pred: c.pred.clone(),
                    inputs: c.inputs.clone(),
                    rows: known.get(c)?.clone(),
                    held: BTreeMap::new(),
                })
            })
            .collect()
    }
}

fn show(vs: &[Value]) -> String {
    vs.iter()
        .map(crate::partition::fmt_value)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The rows of a call: the `+` columns are the inputs, the `-` columns each
/// value of `outs`.
pub fn row(f: &ExternFn, inputs: &[Value], outs: Vec<Value>) -> Vec<Value> {
    let mut ins = inputs.iter();
    let mut outs = outs.into_iter();
    f.args
        .iter()
        .map(|b| {
            if b.input {
                ins.next().cloned()
            } else {
                outs.next()
            }
            .unwrap_or(Value::Str(String::new()))
        })
        .collect()
}

/// A built-in extern: `(name, [(input, column, type)], persist)`.
type BuiltinExtern = (
    &'static str,
    &'static [(bool, &'static str, &'static str)],
    bool,
);

/// A built-in fact provider (DESIGN.org R-8): `provider NAME {}` brings its
/// externs into scope.
pub struct Builtin {
    pub name: &'static str,
    externs: &'static [BuiltinExtern],
    /// Whether dform answers them itself; else the provider the stack
    /// configures by that name does (`random`: the secret it generates is
    /// held by the provider, which hands it over inside Apply).
    pub in_process: bool,
}

/// The built-in fact providers.
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "file",
        externs: &[
            (
                "file.json",
                &[(true, "path", "string"), (false, "value", "any")],
                false,
            ),
            (
                "file.text",
                &[(true, "path", "string"), (false, "value", "string")],
                false,
            ),
        ],
        in_process: true,
    },
    Builtin {
        name: "env",
        externs: &[(
            "env.var",
            &[(true, "name", "string"), (false, "value", "secret(string)")],
            false,
        )],
        in_process: true,
    },
    // The aws mock's data source (R-36): a table, its index a stable
    // ordinal of the names the provider defines. Declared here until a
    // provider's schema declares its externs to the compiler.
    Builtin {
        name: "aws",
        externs: &[(
            "aws.availability_zone",
            &[
                (true, "state", "string"),
                (false, "name", "string"),
                (false, "index", "int"),
            ],
            false,
        )],
        in_process: false,
    },
    Builtin {
        name: "random",
        externs: &[(
            "random.password",
            &[(true, "key", "string"), (false, "value", "secret(string)")],
            true,
        )],
        in_process: false,
    },
];

/// The built-in fact provider `name`.
pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

impl Builtin {
    /// Its externs' declarations.
    pub fn externs(&self) -> Vec<ExternFn> {
        let ty = |t: &str| match t.strip_prefix("secret(") {
            Some(inner) => TypeExpr::Apply(
                "secret".into(),
                vec![TypeExpr::Name(inner.trim_end_matches(')').into())],
            ),
            None => TypeExpr::Name(t.into()),
        };
        self.externs
            .iter()
            .map(|(name, cols, persist)| ExternFn {
                name: name.to_string(),
                args: cols
                    .iter()
                    .map(|(input, n, t)| BindArg {
                        input: *input,
                        name: n.to_string(),
                        ty: Some(ty(t)),
                    })
                    .collect(),
                persist: *persist,
                span: Span::default(),
            })
            .collect()
    }
}

/// The `file` fact provider: `file.json(+path, -value)`, `file.text(+path,
/// -value)`, a relative path from `base` (the program's project root, `project::base_of`). `None`
/// for an extern it does not answer.
pub fn file(
    f: &ExternFn,
    inputs: &[Value],
    base: &std::path::Path,
) -> Option<Result<Vec<Vec<Value>>>> {
    let kind = f.name.strip_prefix("file.")?;
    let one = |r: Result<Value>| r.map(|v| vec![row(f, inputs, vec![v])]);
    Some(match (kind, inputs) {
        ("json" | "text", [Value::Str(path)]) if f.args.len() == 2 => {
            let text =
                std::fs::read_to_string(base.join(path)).with_context(|| format!("read {path}"));
            one(text.and_then(|t| {
                if kind == "text" {
                    return Ok(Value::Str(t));
                }
                let j: serde_json::Value =
                    serde_json::from_str(&t).with_context(|| format!("parse {path} as JSON"))?;
                Ok(from_json(&j))
            }))
        }
        ("json" | "text", _) => Err(anyhow::anyhow!(
            "file.{kind} is declared `extern file.{kind}(+path, -value)`"
        )),
        _ => Err(anyhow::anyhow!(
            "the file provider answers file.json and file.text, not {}",
            f.name
        )),
    })
}

/// The built-in `env.var(+name, -value: secret(string))`: the process
/// environment's variable. Its column is a secret, so the plan file never
/// records the answer ([`Externs::recorded`]), only its label
/// ([`Externs::env_labels`]) and a keyed digest, and it is not `persist`:
/// every run reads the
/// environment again. An unset variable is an error naming it. `None` for
/// another extern.
pub fn env(f: &ExternFn, inputs: &[Value]) -> Option<Result<Vec<Vec<Value>>>> {
    if f.name != crate::syntax::resolve::ENV_VAR {
        return None;
    }
    Some(match inputs {
        [Value::Str(name)] => match std::env::var(name) {
            Ok(v) => Ok(vec![row(f, inputs, vec![Value::Str(v)])]),
            Err(std::env::VarError::NotPresent) => Err(anyhow::anyhow!(
                "env.var: {name} is not set in the environment"
            )),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(anyhow::anyhow!("env.var: {name} is not UTF-8"))
            }
        },
        _ => Err(anyhow::anyhow!(
            "env.var takes the variable's name, a string"
        )),
    })
}

/// A JSON document as a value: numbers that are not integers and `null`
/// become strings (the value model has neither).
pub fn from_json(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Str("null".into()),
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Str(n.to_string()),
        },
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(xs) => Value::List(xs.iter().map(from_json).collect()),
        serde_json::Value::Object(m) => {
            Value::Obj(m.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
        }
    }
}

/// The mock's extern answers: `providers/<name>/externs.df` beside each
/// provider's schema, else a built-in schema's
/// (`crate::schema::builtin_answers`), facts of the extern predicates.
pub fn load_answers(specs: &[String]) -> Result<Vec<Atom>> {
    let names: Vec<&str> = if specs.is_empty() {
        vec!["fake"]
    } else {
        specs.iter().map(String::as_str).collect()
    };
    let mut out = Vec::new();
    for n in names {
        let path = if n.ends_with(".df") || n.contains('/') {
            std::path::Path::new(n).with_file_name("externs.df")
        } else {
            std::path::Path::new("providers").join(n).join("externs.df")
        };
        let (origin, src) = if path.exists() {
            let src = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            (path.display().to_string(), src)
        } else if let Some(src) = crate::schema::builtin_answers(n) {
            (format!("crates/dform-mock/schemas/{n}.externs.df"), src.to_string())
        } else {
            continue;
        };
        let program = crate::parser::parse_file(&origin, &src)?;
        for s in program.statements {
            match s {
                Stmt::Fact(a) => out.push(a),
                _ => bail!(
                    "{origin}: an externs file holds the facts the mock answers with, nothing else"
                ),
            }
        }
    }
    Ok(out)
}

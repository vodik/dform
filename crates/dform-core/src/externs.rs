//! Externs with binding patterns (DESIGN.org "Demand-driven extern
//! predicates", E §2.6, DR-7): `extern p(+in, -out, ...)`.
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
//! plan already asked); nothing else keeps them. What must stay the same
//! across runs is kept by `memo.first` (R-60, [`crate::memo`]), which the
//! program writes where the value is read.
//!
//! A program does not declare an extern: `use NAME` brings the
//! provider's into scope (DESIGN.org R-8). `env` and `time` are
//! built-in fact providers ([`BUILTINS`]): their externs are the
//! compiler's own, and dform answers `env.var(+name, -value)` and
//! `time.now(-t)` itself, with no `dform.toml` source; `io.read(LOCATION)`
//! and the decodes over it are documents (`crate::tables`, R-39, R-155),
//! read over a location's transport (`crate::files`, R-153). `memo.first` is in scope with no provider's `use`. Other
//! externs are asked of the providers over the
//! plugin protocol (Query; the mock answers from
//! `providers/<name>/externs.df`).

use crate::ast::{Atom, BindArg, ExternFn, Lit, Program, Span, Stmt, Term, TypeExpr};
use crate::diag::{Diagnostic, Diagnostics};
use crate::engine::{self, EvalResult};
use crate::spell;
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

impl Answer {}

/// The label of the secret an extern's call answers in column `col` (from
/// 0): `pred/INPUTS#N`, `N` the column from 1.
pub fn secret_label(pred: &str, inputs: &[Value], col: usize) -> String {
    let ins: Vec<String> = inputs.iter().map(spell::bare).collect();
    crate::value::null_label(pred, &ins.join(","), &(col + 1).to_string())
}

/// The variables of `t`, into `out`.
fn vars(t: &Term, out: &mut BTreeSet<String>) {
    t.for_each_var(&mut |v| {
        out.insert(v.to_string());
    });
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
                    Diagnostic::error(a.span, format!("{} under `not`", called(&a.pred)))
                        .with_note(format!(
                            "{} answers what exists; its absence is not known, so it cannot be \
                             negated",
                            called(&a.pred)
                        )),
                ),
                Lit::Pos(a) if by.contains_key(a.pred.as_str()) => {
                    let f = by[a.pred.as_str()];
                    if a.args.len() != f.args.len() {
                        diags.push(Diagnostic::error(
                            a.span,
                            format!(
                                "{} takes {} arguments, not {}",
                                called(&f.name),
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
                                        "{}: +{} is not bound ({v} is unbound before it)",
                                        called(&f.name),
                                        b.name
                                    ),
                                )
                                .with_help(format!(
                                    "bind it with a literal before {}",
                                    called(&f.name)
                                )),
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
                        let (msg, asked) = match crate::tables::describe(&f.name) {
                            Some(t) => (
                                format!("{t}: its source reads its own rows"),
                                "a source is read once it is known",
                            ),
                            None => (
                                format!("{} in a recursive rule", f.name),
                                "a data source is asked once its inputs are complete",
                            ),
                        };
                        diags.push(Diagnostic::error(a.span, msg).with_note(format!(
                            "{} depends on itself through {}; {asked}",
                            program_name(&h.pred),
                            program_name(p)
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

/// A data source as a message names it (R-129: the word `extern` never
/// reaches a user): a loader's table by what it reads (`yaml document`),
/// any other by its name (`ovh.image`).
fn called(name: &str) -> String {
    crate::tables::describe(name).unwrap_or_else(|| name.to_string())
}

/// A relation as the program names it: a module's private one by its
/// module and name (`traefik.p`), not the compiler's `traefik::p`.
fn program_name(pred: &str) -> String {
    pred.replace("::", ".")
}

fn defined_here(a: &Atom, f: &ExternFn) -> Diagnostic {
    Diagnostic::error(
        a.span,
        format!(
            "{} is answered by its provider; the program may not state it",
            called(&f.name)
        ),
    )
    .with_label(f.span, "declared here")
}

/// Is column `b` secret-typed (`-value: secret(T)`)?
pub fn is_secret(b: &BindArg) -> bool {
    matches!(&b.ty, Some(TypeExpr::Apply(n, _)) if n == "secret")
}

/// How a call is asked of its provider: the extern and its inputs, to rows.
type Ask<'a> = dyn Fn(&ExternFn, &[Value]) -> Result<Vec<Vec<Value>>> + 'a;

/// Where the answers of one run come from, in order: the plan file's, then
/// the provider (or dform, for a built-in in-process extern).
pub struct Externs<'a> {
    fns: BTreeMap<String, ExternFn>,
    /// The extern literal of each rule or constraint: the body before it,
    /// and the extern.
    sites: Vec<(Vec<Lit>, Atom)>,
    known: RefCell<BTreeMap<Call, Vec<Vec<Value>>>>,
    ask: Box<Ask<'a>>,
    /// The calls the last evaluation demanded.
    demanded: RefCell<BTreeSet<Call>>,
    /// The sites whose call carries a secret (`memo.first` of a secret
    /// candidate, `secrets::secret_memos`), by index into `sites`.
    secret_sites: RefCell<BTreeSet<usize>>,
    /// The calls a secret site demanded: never recorded in the clear.
    secret_calls: RefCell<BTreeSet<Call>>,
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
            secret_sites: RefCell::new(BTreeSet::new()),
            secret_calls: RefCell::new(BTreeSet::new()),
        }
    }

    /// The extern literals `secret` (`secrets::secret_memos`): their calls
    /// carry a secret, so the plan file never records them and a
    /// `memo.first` keeps its value sealed ([`Externs::memos`]).
    pub fn mark_secret(&self, secret: &[Atom]) {
        let mut marked = self.secret_sites.borrow_mut();
        for (i, (_, a)) in self.sites.iter().enumerate() {
            if secret.contains(a) {
                marked.insert(i);
            }
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
        let secret_sites = self.secret_sites.borrow();
        for (site, (prefix, a)) in self.sites.iter().enumerate() {
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
                let call = Call {
                    pred: a.pred.clone(),
                    inputs: vals,
                };
                if secret_sites.contains(&site) {
                    self.secret_calls.borrow_mut().insert(call.clone());
                }
                out.insert(call);
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
                let rows = (self.ask)(f, &c.inputs).map_err(|e| {
                    if e.is::<crate::interrupt::Interrupted>() {
                        return e;
                    }
                    // In one shape (R-109): the call as a plan line says
                    // it (R-129 amendment), a document's as its read
                    // (`io.read("note.txt")`, R-155); why; where it is
                    // written.
                    let site = self
                        .sites
                        .iter()
                        .find(|(_, a)| a.pred == c.pred)
                        .and_then(|(_, a)| crate::diag::location(a.span))
                        .map(|(f, l, _)| format!("{f}:{l}"));
                    let what = match (&c.inputs[..], crate::tables::describe(&c.pred)) {
                        ([Value::Str(l)], Some(_)) if crate::tables::is_document(&c.pred) => {
                            call_text(&c.pred, l)
                        }
                        (_, Some(t)) => format!("{t} from {}", show(&c.inputs)),
                        (_, None) => format!("{}({})", c.pred, show(&c.inputs)),
                    };
                    crate::report::Failure {
                        what: format!("{what} failed"),
                        message: format!("{e:#}"),
                        site,
                        addr: None,
                        located: false,
                    }
                    .into()
                })?;
                for r in &rows {
                    if r.len() != f.args.len() {
                        bail!(
                            "{}: an answer has {} columns, the declaration {}",
                            called(&c.pred),
                            r.len(),
                            f.args.len()
                        );
                    }
                }
                self.known.borrow_mut().insert(c, rows);
            }
        }
        bail!(
            "data sources: calls did not settle after 64 rounds (a call's output feeding its own input?)"
        )
    }

    /// The labels of the open nulls the last evaluation's answers hold:
    /// an extern that answers a column with an open null says "not yet"
    /// (a host that does not answer yet), where a refusal is an error. An
    /// apply waits on them (R-81, [`Externs::forget_not_yet`]).
    pub fn not_yet(&self) -> BTreeSet<String> {
        let known = self.known.borrow();
        self.demanded
            .borrow()
            .iter()
            .filter_map(|c| known.get(c))
            .flatten()
            .flatten()
            .filter_map(|v| match v {
                Value::Null {
                    label,
                    class: crate::value::NullClass::Open,
                    ..
                } => Some(label.clone()),
                _ => None,
            })
            .collect()
    }

    /// Forget every answer that said "not yet" ([`Externs::not_yet`]): the
    /// next evaluation asks again.
    pub fn forget_not_yet(&self) {
        self.known.borrow_mut().retain(|_, rows| {
            !rows.iter().flatten().any(|v| {
                matches!(
                    v,
                    Value::Null {
                        class: crate::value::NullClass::Open,
                        ..
                    }
                )
            })
        });
    }

    /// The answers the last evaluation read, for the plan file: every call
    /// it demanded, except a call with a secret column or one that carries
    /// a secret (a secret is never written in the clear).
    pub fn recorded(&self) -> Vec<Answer> {
        let secret = self.secret_calls.borrow();
        self.answers(|f| !f.args.iter().any(is_secret))
            .into_iter()
            .filter(|a| {
                !secret.contains(&Call {
                    pred: a.pred.clone(),
                    inputs: a.inputs.clone(),
                })
            })
            .collect()
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

    /// The secret answers of dform's own externs the last evaluation read
    /// ([`secret_columns`], a document read into a secret `let`), each by its label
    /// ([`secret_label`]): what the plan file records the digest of. A
    /// "not yet" (a null) is none.
    pub fn secret_answers(&self) -> Vec<(String, Value)> {
        let known = self.known.borrow();
        let secret = self.secret_calls.borrow();
        let mut out = Vec::new();
        for c in self.demanded.borrow().iter() {
            let mut cols = secret_columns(&c.pred);
            // A file of given secrets' values (R-108): its secret column.
            if cols.is_empty() && crate::tables::is_sealed(&c.pred) {
                cols = self
                    .fns
                    .get(&c.pred)
                    .map(|f| f.args.iter().position(is_secret))
                    .into_iter()
                    .flatten()
                    .collect();
            }
            // A document read into a secret `let` (R-153): its value.
            if cols.is_empty() && crate::tables::is_document(&c.pred) && secret.contains(c) {
                cols = self
                    .fns
                    .get(&c.pred)
                    .map(|f| f.args.len() - 1)
                    .into_iter()
                    .collect();
            }
            for row in known.get(c).into_iter().flatten() {
                for &col in &cols {
                    match row.get(col) {
                        None | Some(Value::Null { .. }) => {}
                        Some(v) => out.push((secret_label(&c.pred, &c.inputs, col), v.clone())),
                    }
                }
            }
        }
        out
    }

    /// The version each document read into a secret `let` was answered at,
    /// by its label ([`Externs::secret_answers`]'): a source that keeps
    /// versions (a secret manager's scheme, R-172) pins where its row is
    /// to the version (`files::pinned`), which the plan file records
    /// beside the digest.
    pub fn secret_versions(&self) -> BTreeMap<String, String> {
        let known = self.known.borrow();
        let secret = self.secret_calls.borrow();
        let mut out = BTreeMap::new();
        for c in self.demanded.borrow().iter() {
            if !crate::tables::is_document(&c.pred) || !secret.contains(c) {
                continue;
            }
            let Some(col) = self.fns.get(&c.pred).map(|f| f.args.len() - 1) else {
                continue;
            };
            for row in known.get(c).into_iter().flatten() {
                if let Some(Value::Str(at)) = row.get(1)
                    && let Some(v) = crate::files::version_of(at)
                {
                    out.insert(secret_label(&c.pred, &c.inputs, col), v);
                }
            }
        }
        out
    }

    /// The `memo.first` calls the last evaluation read: each key, the value
    /// it answered, and whether it is a secret (a secret site demanded
    /// it), for state to keep ([`crate::memo::keep`]).
    pub fn memos(&self) -> Vec<(String, Value, bool)> {
        let secret = self.secret_calls.borrow();
        let mut out: Vec<(String, Value, bool)> = Vec::new();
        for a in self.answers(|f| f.name == crate::memo::FIRST) {
            let is_secret = secret.contains(&Call {
                pred: a.pred.clone(),
                inputs: a.inputs.clone(),
            });
            for r in &a.rows {
                if let [Value::Str(k), _, v] = r.as_slice() {
                    match out.iter_mut().find(|(x, _, _)| x == k) {
                        Some(m) => m.2 |= is_secret,
                        None => out.push((k.clone(), v.clone(), is_secret)),
                    }
                }
            }
        }
        out
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
    vs.iter().map(spell::value).collect::<Vec<_>>().join(", ")
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

/// A built-in extern: `(name, [(input, column, type)])`.
type BuiltinExtern = (&'static str, &'static [(bool, &'static str, &'static str)]);

/// A built-in fact provider (DESIGN.org R-8): `use NAME` brings its
/// externs into scope.
pub struct Builtin {
    pub name: &'static str,
    externs: &'static [BuiltinExtern],
    /// Whether dform answers them itself; else the provider the stack
    /// configures by that name does.
    pub in_process: bool,
    /// Whether its externs are in scope with no provider's `use`
    /// (`memo`: a relation of the language, not a provider's).
    pub always: bool,
}

/// The built-in fact providers.
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "env",
        externs: &[(
            "env.var",
            &[(true, "name", "string"), (false, "value", "secret(string)")],
        )],
        in_process: true,
        always: false,
    },
    // The current time: a fact read again every run, never a function
    // (R-60, R-62). Kept once, it is `memo.first(KEY, time.now(), T)`.
    Builtin {
        name: "time",
        externs: &[(TIME_NOW, &[(false, "t", "time")])],
        in_process: true,
        always: false,
    },
    // An image reference's tag pinned to its digest at plan (R-132): a
    // read of the registry, recorded in the plan file, never a function.
    Builtin {
        name: "oci",
        externs: &[(
            crate::files::oci::RESOLVE,
            &[(true, "reference", "oci"), (false, "resolved", "oci")],
        )],
        in_process: true,
        always: true,
    },
    Builtin {
        name: "memo",
        externs: &[(
            crate::memo::FIRST,
            &[
                (true, "key", "string"),
                (true, "candidate", "any"),
                (false, "value", "any"),
            ],
        )],
        in_process: true,
        always: true,
    },
];

/// The `time` provider's extern `time.now(-t: time)`.
pub const TIME_NOW: &str = "time.now";

/// Whether dform answers `pred` itself: an extern of an in-process
/// built-in provider.
pub fn in_process(pred: &str) -> bool {
    // A location's read that is not there yet (R-153): dform's own.
    if pred == crate::files::READ {
        return true;
    }
    pred.split_once('.')
        .and_then(|(h, _)| builtin(h))
        .is_some_and(|b| b.in_process && b.externs.iter().any(|(n, _)| *n == pred))
}

/// The secret columns (from 0) of an in-process built-in extern but
/// `env.var` (which the redactor labels by its name).
pub fn secret_columns(pred: &str) -> Vec<usize> {
    if pred == crate::syntax::resolve::ENV_VAR || !in_process(pred) {
        return Vec::new();
    }
    builtin_extern(pred)
        .map(|cols| {
            cols.iter()
                .enumerate()
                .filter(|(_, (_, _, t))| t.starts_with("secret("))
                .map(|(i, _)| i)
                .collect()
        })
        .unwrap_or_default()
}

/// The `+` columns of a row of the built-in extern `pred`: its call's
/// inputs.
pub fn inputs_of(pred: &str, row: &[Value]) -> Vec<Value> {
    builtin_extern(pred)
        .map(|cols| {
            cols.iter()
                .zip(row)
                .filter(|((input, _, _), _)| *input)
                .map(|(_, v)| v.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// An extern's call as a label names it ([`secret_label`],
/// `pred/INPUTS#N`), as the program writes it: `memo.first("k", "c")`,
/// `io.read("ssh://ubuntu@10.0.0.5/etc/k3s.yaml")`. The inputs are joined by `,`: a built-in
/// extern's that do not split into as many as it takes are said whole.
pub fn call_text(pred: &str, inputs: &str) -> String {
    // A location's read is said as the location (R-153), a document's
    // as the read a program writes (`yaml.decode(io.read("..")))`, R-155).
    if pred == crate::files::READ {
        return inputs.to_string();
    }
    if crate::tables::is_document(pred)
        && let Some(format) = pred
            .strip_prefix("table.")
            .and_then(|r| r.split('.').next())
    {
        let read = format!(
            "{}({})",
            crate::tables::READ,
            crate::ir::string_literal(inputs)
        );
        return match format {
            "text" => read,
            f => format!("{f}.decode({read})"),
        };
    }
    let n = builtin_extern(pred).map(|cols| cols.iter().filter(|(i, _, _)| *i).count());
    let parts: Vec<&str> = match (n, inputs.split(',').collect::<Vec<_>>()) {
        (Some(0), _) => Vec::new(),
        (Some(n), p) if p.len() != n => vec![inputs],
        (_, p) => p,
    };
    let args: Vec<String> = parts.iter().map(|a| crate::ir::string_literal(a)).collect();
    format!("{pred}({})", args.join(", "))
}

/// Whether a null's label `T/A#P` is an extern call's answer, not a
/// resource's attribute: `T` an extern dform answers, or `P` a column
/// number (no attribute is named by digits).
pub fn is_call_label(typ: &str, path: &str) -> bool {
    in_process(typ) || (!path.is_empty() && path.bytes().all(|b| b.is_ascii_digit()))
}

fn builtin_extern(pred: &str) -> Option<&'static [(bool, &'static str, &'static str)]> {
    let (h, _) = pred.split_once('.')?;
    builtin(h)?
        .externs
        .iter()
        .find(|(n, _)| *n == pred)
        .map(|(_, cols)| *cols)
}

/// The built-in fact provider `name`.
pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

/// A column's type as an extern declaration writes it: a name, or
/// `secret(T)`.
pub fn type_expr(t: &str) -> TypeExpr {
    match t.strip_prefix("secret(") {
        Some(inner) => TypeExpr::Apply(
            "secret".into(),
            vec![TypeExpr::Name(inner.trim_end_matches(')').into())],
        ),
        None => TypeExpr::Name(t.into()),
    }
}

impl Builtin {
    /// Its externs' declarations.
    pub fn externs(&self) -> Vec<ExternFn> {
        let ty = type_expr;
        self.externs
            .iter()
            .map(|(name, cols)| ExternFn {
                name: name.to_string(),
                args: cols
                    .iter()
                    .map(|(input, n, t)| BindArg {
                        input: *input,
                        name: n.to_string(),
                        ty: Some(ty(t)),
                    })
                    .collect(),
                span: Span::default(),
            })
            .collect()
    }
}

/// The built-in `env.var(+name, -value: secret(string))`: the process
/// environment's variable. Its column is a secret, so the plan file never
/// records the answer ([`Externs::recorded`]), only its label
/// ([`Externs::env_labels`]) and a keyed digest, and nothing keeps it:
/// every run reads the environment again. An unset variable is an error naming it. `None` for
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

/// The built-in `time.now(-t: time)`: the current time, UTC, to the
/// second, read again every run (a plan file records it, so its apply reads the plan's).
/// `DFORM_TEST_NOW` (RFC 3339) stands in for the clock in tests. `None`
/// for another extern.
pub fn time(f: &ExternFn) -> Option<Result<Vec<Vec<Value>>>> {
    if f.name != TIME_NOW {
        return None;
    }
    let now = match std::env::var("DFORM_TEST_NOW") {
        Ok(t) => t,
        Err(_) => crate::memo::now(),
    };
    Some(
        crate::time::Time::parse(&now)
            .map(|t| vec![vec![Value::Time(t)]])
            .map_err(|e| anyhow::anyhow!("time.now: {e}")),
    )
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
            (
                format!("crates/dform-mock/schemas/{n}.externs.df"),
                src.to_string(),
            )
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

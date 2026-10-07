//! Static secret labels (E DR-19, A's information-flow pass): one dataflow
//! fixpoint over predicate signatures, `public < secret`.
//!
//! Sources: a schema attribute marked `sensitive` (an `attr` read of it, a
//! `ref` to it), an input declared `secret(T)`, an extern column declared
//! `-v: secret(T)`, a function declared `-> secret(T)` (`random.password`),
//! another stack's output published as secret (its cell `(output,
//! Deployment, k)` as a run read it: `stack::Published`). A `memo.first` keeps a
//! secret when its candidate is one: that literal's value is secret, and
//! not another's (`secret_memos`). A head position is secret when a secret value reaches
//! it through its rule: a variable bound at a secret position, or built
//! from one (`format`, arithmetic, lists, objects, field access). A
//! label is the paths inside a value that are secret (R-118): a field an
//! object type declares `secret(T)` (`conn.password`) is a secret cell of
//! its own, so `conn.password` reads as a secret and `conn.host` does not.
//!
//! Then each rule is checked, each violation a compile error with a span:
//!
//! - E0301 a comparison, a builtin predicate or an inspecting function
//!   (`len`, `split`, `inet_*`, ...) over a secret: comparing leaks a bit;
//! - E0302 a negated literal over a secret: absence leaks a bit;
//! - E0303 an aggregate other than `collect_*` over a secret: `count`
//!   leaks cardinality;
//! - E0304 a secret reaching a public place: a resource attribute the
//!   schema does not mark `sensitive`, a setting, an output not declared
//!   `secret(T)`, an input not declared `secret(T)`, a `deny`/`warn`;
//! - E0305 a secret reaching a resource address (`want`, `arg`, `ref`,
//!   `scoped`): names are printed everywhere.
//!
//! `secret.declassify(V, Reason)` is the one way out: its value is public, and
//! what is inside it may be inspected (`secret.declassify(pw.len, "...")`). The
//! lowering derives `declassified(Site, Reason)` for policy to read.
//!
//! An input's own refinement (`input pw: secret(string) where ...`) is the
//! boundary where a secret may be checked, so its generated rules are
//! exempt from E0301/E0302.

use crate::ast::{Atom, Lit, Program, Span, Stmt, Term};
use crate::diag::{Diagnostic, Diagnostics};
use crate::schema::Schema;
use crate::transform::Lowered;
use crate::value::Value;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// Whether a secret flows through `name` uninspected: a function declared
/// `forwards` (`std/*.df`), or an aggregate that only collects.
fn carries(name: &str) -> bool {
    COLLECT.contains(&name) || crate::functions::get(name).is_some_and(|f| f.forwards)
}

/// `secret.declassify(V, Reason)`: `V`, public (`transform` derives
/// `declassified/2` beside the rule for policy).
pub const DECLASSIFY: &str = "secret.declassify";

/// Aggregates that only collect: their result is secret, nothing leaks.
const COLLECT: &[&str] = &["collect_set", "collect_list"];

fn s(t: &Term) -> Option<&str> {
    match t {
        Term::Val(Value::Str(x)) => Some(x),
        _ => None,
    }
}

/// Where a value is secret: the paths inside it, `""` the whole of it.
/// A `conn` whose type declares only `password: secret(string)` is
/// `{password}`, so `conn.host` is public and `conn.password` secret
/// (R-118). Empty: public.
type Label = BTreeSet<String>;

/// The secret label of each variable of a body.
type Vars = BTreeMap<String, Label>;

/// A path in a label deeper than this is cut to its first segments, so a
/// rule that nests a secret in itself (`p({a: X}) :- p(X)`) ends.
const DEPTH: usize = 8;

fn whole() -> Label {
    BTreeSet::from([String::new()])
}

/// Is `p` in `l`, or under a path that is?
fn covered(l: &Label, p: &str) -> bool {
    l.contains("") || l.contains(p) || p.match_indices('.').any(|(i, _)| l.contains(&p[..i]))
}

/// Adds `m` to `l`; whether `l` grew.
fn join(l: &mut Label, m: Label) -> bool {
    let mut grew = false;
    for p in m {
        if !covered(l, &p) {
            if p.is_empty() {
                l.clear();
            } else {
                l.retain(|q| !q.starts_with(&format!("{p}.")));
            }
            l.insert(p);
            grew = true;
        }
    }
    grew
}

/// The label of the field `f` of a value labelled `l`.
fn narrow(l: &Label, f: &str) -> Label {
    if covered(l, f) {
        return whole();
    }
    let f = format!("{f}.");
    l.iter()
        .filter_map(|p| p.strip_prefix(&f))
        .map(str::to_string)
        .collect()
}

/// The label of an object whose field `k` is labelled `l`.
fn under(l: Label, k: &str) -> Label {
    l.into_iter()
        .map(|p| {
            let p = crate::types::dotted(k, &p);
            match p.match_indices('.').nth(DEPTH - 1) {
                Some((i, _)) => p[..i].to_string(),
                None => p,
            }
        })
        .collect()
}

/// The label of the cell `p` given the secret cells `keys` of its scope: a
/// key at `p` or above it makes all of it secret, one below it that path.
fn label_at<'k>(keys: impl Iterator<Item = &'k str>, p: &str) -> Label {
    let mut out = Label::new();
    for k in keys {
        if p.is_empty() {
            join(&mut out, BTreeSet::from([k.to_string()]));
        } else if k == p || p.starts_with(&format!("{k}.")) {
            return whole();
        } else if let Some(rest) = k.strip_prefix(&format!("{p}.")) {
            join(&mut out, BTreeSet::from([rest.to_string()]));
        }
    }
    out
}

struct Pass<'a> {
    schema: &'a Schema,
    /// Secret positions: (predicate, column) -> where in its values.
    secret: BTreeMap<(String, usize), Label>,
    /// Pseudo-type cells declared secret: (type, scope, key); the key
    /// `conn.password` for a field of an object type.
    cells: BTreeSet<(String, String, String)>,
    /// Other stacks' secret outputs: (deployment, key).
    outputs: &'a BTreeSet<(String, String)>,
}

impl Pass<'_> {
    /// The label of `attr(T, A, P, _)`.
    fn attr_label(&self, typ: &Term, addr: &Term, path: &Term) -> Label {
        let all = |b: bool| if b { whole() } else { Label::new() };
        match (s(typ), s(path)) {
            (Some(t), Some(p)) => {
                let p = p.trim_start_matches('.');
                if t == crate::transform::OUTPUT {
                    let l = self.read_output_label(addr, p);
                    if !l.is_empty() {
                        return l;
                    }
                }
                if crate::transform::is_pseudo_type(t) {
                    return s(addr).map_or_else(Label::new, |a| {
                        let keys = self
                            .cells
                            .iter()
                            .filter(|(ct, ca, _)| ct == t && ca == a)
                            .map(|(_, _, k)| k.as_str());
                        label_at(keys, p)
                    });
                }
                // A read of an object holding a sensitive leaf is secret too.
                all(self.schema.is_sensitive(t, p)
                    || self.schema.facts.iter().any(|f| {
                        f.pred == "type_attr"
                            && s(&f.args[0]) == Some(t)
                            && s(&f.args[1]).is_some_and(|q| q.starts_with(&format!("{p}.")))
                            && self.flag(f, "sensitive")
                    }))
            }
            // A path the program computes: secret if any it could be is.
            (Some(t), None) => all(self.schema.facts.iter().any(|f| {
                f.pred == "type_attr" && s(&f.args[0]) == Some(t) && self.flag(f, "sensitive")
            })),
            // The resource itself, of a type the program does not fix
            // (`r in resource`, `r in k8s`): its address, never a secret.
            (None, Some("")) => Label::new(),
            (None, _) => all(self
                .schema
                .facts
                .iter()
                .any(|f| f.pred == "type_attr" && self.flag(f, "sensitive"))),
        }
    }

    /// The label of the output `k` of the deployment `addr` a run read
    /// (R-73). A deployment the program names by a computed name
    /// (`platform[env=e]`, a `format`) is secret where any it could be is.
    fn read_output_label(&self, addr: &Term, k: &str) -> Label {
        match addr {
            Term::Val(Value::Str(d)) => label_at(
                self.outputs
                    .iter()
                    .filter(|(x, _)| x == d)
                    .map(|(_, y)| y.as_str()),
                k,
            ),
            Term::Func { name, .. } if name == crate::ir::FORMAT => {
                label_at(self.outputs.iter().map(|(_, y)| y.as_str()), k)
            }
            _ => Label::new(),
        }
    }

    fn flag(&self, f: &Atom, flag: &str) -> bool {
        matches!(f.args.get(3), Some(Term::Val(Value::List(fs))) if fs.contains(&Value::Str(flag.into())))
            || matches!(f.args.get(3), Some(Term::List(fs)) if fs.iter().any(|t| s(t) == Some(flag)))
    }

    /// Where `t` is secret given the labels of the variables `vars`.
    fn term_label(&self, t: &Term, vars: &Vars) -> Label {
        let all = |b: bool| if b { whole() } else { Label::new() };
        match t {
            Term::Var(v) => vars.get(v).cloned().unwrap_or_default(),
            // Its label lowered to public.
            Term::Func { name, .. } if name == DECLASSIFY => Label::new(),
            // A function whose value is a secret (`-> secret(T)`).
            Term::Func { name, .. } if returns_secret(name) => whole(),
            Term::Func { name, args } if name == crate::ir::REF && args.len() == 3 => {
                all(!self.attr_label(&args[0], &args[1], &args[2]).is_empty()
                    || args.iter().any(|a| self.term_secret(a, vars)))
            }
            // A field of a value: the part of its label under the field.
            Term::Func { name, args } if name == "__path" => match args.as_slice() {
                [x, Term::Val(Value::Str(f))] => narrow(&self.term_label(x, vars), f),
                _ => all(args.iter().any(|a| self.term_secret(a, vars))),
            },
            Term::Func { args, .. } | Term::List(args) => {
                all(args.iter().any(|a| self.term_secret(a, vars)))
            }
            Term::Obj(m) => {
                let mut out = Label::new();
                for (k, v) in m {
                    join(&mut out, under(self.term_label(v, vars), k));
                }
                out
            }
            _ => Label::new(),
        }
    }

    /// Is any of `t` secret given the labels of the variables `vars`?
    fn term_secret(&self, t: &Term, vars: &Vars) -> bool {
        !self.term_label(t, vars).is_empty()
    }

    /// The labels of a body's variables, to a fixpoint (an equality may
    /// come before what binds its other side).
    fn body_vars(&self, body: &[Lit]) -> Vars {
        let mut vars = Vars::new();
        loop {
            let mut grew = false;
            for l in body {
                match l {
                    // A memo's value is as secret as its candidate.
                    Lit::Pos(a) if a.pred == crate::memo::FIRST && a.args.len() == 3 => {
                        let l = self.term_label(&a.args[1], &vars);
                        grew |= bind(&a.args[2], &l, &mut vars);
                    }
                    Lit::Pos(a) => {
                        for (i, t) in a.args.iter().enumerate() {
                            let mut l = self.position_label(a, i);
                            join(&mut l, self.term_label(t, &vars));
                            grew |= bind(t, &l, &mut vars);
                        }
                    }
                    Lit::Eq(x, y) => {
                        let lx = self.term_label(x, &vars);
                        let ly = self.term_label(y, &vars);
                        grew |= bind(y, &lx, &mut vars);
                        grew |= bind(x, &ly, &mut vars);
                    }
                    _ => {}
                }
            }
            if !grew {
                return vars;
            }
        }
    }

    /// The first path of a contribution `arg(T, A, P, V)` where a secret
    /// meets a public place: each path `V`'s label holds, under `P`, must
    /// be sensitive, or declared `secret(T)`, or under one that is.
    fn public_leaf(
        &self,
        typ: &Term,
        addr: &Term,
        path: &Term,
        value: &Term,
        vars: &Vars,
    ) -> Option<String> {
        let label = self.term_label(value, vars);
        if label.is_empty() {
            return None;
        }
        let Some(p) = s(path) else {
            return self
                .attr_label(typ, addr, path)
                .is_empty()
                .then(|| "?".to_string());
        };
        let p = p.trim_start_matches('.');
        label
            .iter()
            .map(|q| crate::types::dotted(p, q))
            .find(|f| !self.declared(typ, addr, f))
    }

    /// Is the path `p` of `T[A]` a declared place for a secret: sensitive
    /// in the schema, or a secret cell, or under one?
    fn declared(&self, typ: &Term, addr: &Term, p: &str) -> bool {
        match s(typ) {
            Some(t) if !crate::transform::is_pseudo_type(t) => self.schema.is_sensitive(t, p),
            _ => self.attr_label(typ, addr, &Term::Val(Value::Str(p.into()))) == whole(),
        }
    }

    fn position_label(&self, a: &Atom, i: usize) -> Label {
        match (a.pred.as_str(), a.args.len()) {
            ("attr" | "world_attr", 4) if i == 3 => {
                self.attr_label(&a.args[0], &a.args[1], &a.args[2])
            }
            _ => self
                .secret
                .get(&(a.pred.clone(), i))
                .cloned()
                .unwrap_or_default(),
        }
    }

    fn position_secret(&self, a: &Atom, i: usize) -> bool {
        !self.position_label(a, i).is_empty()
    }
}

/// Gives the variables of the pattern `t` the label `l` (an object
/// pattern's fields theirs, any other term's variables all of it);
/// whether any grew.
fn bind(t: &Term, l: &Label, out: &mut Vars) -> bool {
    if l.is_empty() {
        return false;
    }
    match t {
        Term::Var(v) => join(out.entry(v.clone()).or_default(), l.clone()),
        Term::Obj(m) => m
            .iter()
            .fold(false, |grew, (k, x)| bind(x, &narrow(l, k), out) | grew),
        Term::Func { args, .. } | Term::List(args) => args
            .iter()
            .fold(false, |grew, x| bind(x, &whole(), out) | grew),
        _ => false,
    }
}

/// Every rule and constraint: (head, body, span, is an input refinement).
fn rules(program: &Program) -> Vec<(Option<&Atom>, &[Lit], Span)> {
    program
        .statements
        .iter()
        .filter_map(|st| match st {
            Stmt::Rule(r) => Some((Some(&r.head), r.body.as_slice(), r.head.span)),
            Stmt::Fact(a) => Some((Some(a), &[][..], a.span)),
            _ => None,
        })
        .collect()
}

fn is_refinement(head: Option<&Atom>) -> bool {
    head.is_some_and(|h| {
        h.pred.rsplit("::").next().unwrap_or(&h.pred).starts_with("__refine_")
            || (h.pred == "deny"
                && matches!(h.args.first(), Some(Term::Val(Value::Str(m))) if m.contains("fails its refinement")))
    })
}

/// Whether `name` is a function declared `-> secret(T)`.
fn returns_secret(name: &str) -> bool {
    crate::functions::get(name).is_some_and(|f| f.ret.starts_with("secret("))
}

/// The secret positions of a lowered program: the fixpoint over predicate
/// signatures, from the sources to every head a secret reaches.
fn fixpoint<'a>(
    lowered: &Lowered,
    schema: &'a Schema,
    outputs: &'a BTreeSet<(String, String)>,
) -> Pass<'a> {
    let mut pass = Pass {
        schema,
        secret: BTreeMap::new(),
        cells: BTreeSet::new(),
        outputs,
    };
    for d in &lowered.inputs {
        for (q, _) in crate::types::secret_fields(&d.decl.ty) {
            pass.cells.insert((
                crate::modules::INPUT.into(),
                d.scope.clone(),
                crate::types::dotted(&d.decl.name, &q),
            ));
        }
    }
    for (scope, k) in &lowered.secret_outputs {
        pass.cells
            .insert((crate::transform::OUTPUT.into(), scope.clone(), k.clone()));
    }
    // A `let` declared secret (`modules::lets`).
    for st in &lowered.program.statements {
        if let Stmt::Fact(a) = st
            && a.pred == crate::transform::SECRET_CELL
            && let [t, scope, k] = a.args.as_slice()
            && s(t) == Some(crate::modules::LET)
            && let (Some(scope), Some(k)) = (s(scope), s(k))
        {
            pass.cells
                .insert((crate::modules::LET.into(), scope.into(), k.into()));
        }
    }
    for f in &lowered.extern_fns {
        for (i, b) in f.args.iter().enumerate() {
            if crate::externs::is_secret(b) {
                pass.secret.insert((f.name.clone(), i), whole());
            }
        }
    }
    let rs = rules(&lowered.program);
    // The fixpoint over predicate signatures.
    loop {
        let mut grew = false;
        for (head, body, _) in &rs {
            let Some(h) = head else { continue };
            let vars = pass.body_vars(body);
            for (i, t) in h.args.iter().enumerate() {
                let l = pass.term_label(t, &vars);
                if !l.is_empty() {
                    grew |= join(pass.secret.entry((h.pred.clone(), i)).or_default(), l);
                }
            }
            // A `let` holding a secret is a secret cell (R-3), each path
            // of it that is.
            if let ("arg", [t, scope, k, v, _]) = (h.pred.as_str(), h.args.as_slice())
                && s(t) == Some(crate::modules::LET)
                && let (Some(scope), Some(k)) = (s(scope), s(k))
            {
                for q in pass.term_label(v, &vars) {
                    grew |= pass.cells.insert((
                        crate::modules::LET.to_string(),
                        scope.to_string(),
                        crate::types::dotted(k, &q),
                    ));
                }
            }
        }
        if !grew {
            break;
        }
    }
    pass
}

/// The providers whose `expect_account` a secret reaches (an `env_var`, a
/// secret input): a refusal names that account by its label, never its
/// value (`Providers::check_accounts`).
/// `outputs`: other stacks' secret outputs the run read, (deployment, key).
pub fn secret_expected_accounts(
    lowered: &Lowered,
    schema: &Schema,
    outputs: &BTreeSet<(String, String)>,
) -> BTreeSet<String> {
    let pass = fixpoint(lowered, schema, outputs);
    rules(&lowered.program)
        .into_iter()
        .filter_map(|(head, body, _)| {
            let h = head.filter(|h| h.pred == crate::plugin::providers::EXPECT_ACCOUNT)?;
            let vars = pass.body_vars(body);
            match h.args.as_slice() {
                [name, account] if pass.term_secret(account, &vars) => s(name).map(str::to_string),
                _ => None,
            }
        })
        .collect()
}

/// The settings a secret reaches, by provider (`use k8s { kubeconfig
/// = k3s.kubeconfig }`: `k8s` -> `kubeconfig`): the provider takes their
/// value at Configure, in memory; dform prints each as `(sensitive)` and
/// keeps it nowhere (R-45).
pub fn secret_settings(
    lowered: &Lowered,
    schema: &Schema,
    outputs: &BTreeSet<(String, String)>,
) -> BTreeMap<String, BTreeSet<String>> {
    let pass = fixpoint(lowered, schema, outputs);
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (head, body, _) in rules(&lowered.program) {
        let Some(h) = head.filter(|h| h.pred == "provider_config") else {
            continue;
        };
        let vars = pass.body_vars(body);
        if let [name, Term::Obj(settings)] = h.args.as_slice()
            && let Some(name) = s(name)
        {
            for (k, v) in settings {
                if pass.term_secret(v, &vars) {
                    out.entry(name.to_string()).or_default().insert(k.clone());
                }
            }
        }
    }
    out
}

/// The `memo.first` literals whose candidate is a secret: their value is
/// kept sealed (`memo`), and the plan file records none of their calls.
pub fn secret_memos(
    lowered: &Lowered,
    schema: &Schema,
    outputs: &BTreeSet<(String, String)>,
) -> Vec<Atom> {
    let pass = fixpoint(lowered, schema, outputs);
    let mut out = Vec::new();
    for (_, body, _) in rules(&lowered.program) {
        let vars = pass.body_vars(body);
        for l in body {
            if let Lit::Pos(a) = l
                && a.pred == crate::memo::FIRST
                && a.args.len() == 3
                && pass.term_secret(&a.args[1], &vars)
            {
                out.push(a.clone());
            }
        }
    }
    out
}

/// The reads of a document (`table.FORMAT.document`, a loader's call)
/// whose value a rule writes into a secret cell (`let raw:
/// secret(string) = io.read("ssh://..")`, R-153): the plan file records no
/// such read, only its digest (`Externs::secret_answers`), and the apply
/// reads it again.
pub fn secret_reads(
    lowered: &Lowered,
    schema: &Schema,
    outputs: &BTreeSet<(String, String)>,
) -> Vec<Atom> {
    let pass = fixpoint(lowered, schema, outputs);
    let mut out = Vec::new();
    for (head, body, _) in rules(&lowered.program) {
        let Some(h) = head else { continue };
        let ("arg", [t, scope, k, ..]) = (h.pred.as_str(), h.args.as_slice()) else {
            continue;
        };
        let (Some(t), Some(scope), Some(k)) = (s(t), s(scope), s(k)) else {
            continue;
        };
        let secret = pass.cells.iter().any(|(ct, cs, ck)| {
            ct == t && cs == scope && (ck == k || ck.starts_with(&format!("{k}.")))
        });
        if !secret {
            continue;
        }
        for l in body {
            if let Lit::Pos(a) = l
                && crate::tables::is_document(&a.pred)
            {
                out.push(a.clone());
            }
        }
    }
    out
}

/// `__secret_column(Pred, Col, Path)`: the column `Col` of `Pred` holds a
/// secret at `Path` (`""` all of it), as the pass found (R-128).
pub const SECRET_COLUMN: &str = "__secret_column";

/// `__secret_path(T, P, Sub)`: what a rule writes at `T`'s path `P` holds
/// a secret at `Sub` below it (`""` all of it): a secret reaches it
/// through the rule (`data = { yaml: "key: ${signing}" }`; R-128).
pub const SECRET_PATH: &str = "__secret_path";

/// The predicates whose values are attribute cells: the redactor reads
/// their secrets by cell (the schema's `sensitive`, `secret_cell`) and
/// by what is written there (`__secret_path`), never by column, since
/// one column holds every resource's values.
const CELL_PREDS: [&str; 5] = ["arg", "attr", "world_attr", "cloud_attr", "cloud_computed"];

/// What the pass knows that printing needs (R-128), as facts the
/// `query::Redactor` reads beside the program's: every secret cell
/// (`secret_cell`, a `let` holding a secret and a secret field of an
/// input included), every secret column of a relation the program
/// derives (`__secret_column`), and every attribute path a secret
/// is written to (`__secret_path`). Output redacts a value by these,
/// never by its text.
pub fn taint(
    lowered: &Lowered,
    schema: &Schema,
    outputs: &BTreeSet<(String, String)>,
) -> Vec<Atom> {
    let pass = fixpoint(lowered, schema, outputs);
    let fact = |pred: &str, args: Vec<Value>| Atom {
        pred: pred.into(),
        args: args.into_iter().map(Term::Val).collect(),
        record: None,
        span: Span::default(),
    };
    let mut out: Vec<Atom> = pass
        .cells
        .iter()
        .map(|(t, sc, k)| {
            fact(
                crate::transform::SECRET_CELL,
                vec![
                    Value::Str(t.clone()),
                    Value::Str(sc.clone()),
                    Value::Str(k.clone()),
                ],
            )
        })
        .collect();
    for ((pred, col), label) in &pass.secret {
        if CELL_PREDS.contains(&pred.as_str()) {
            continue;
        }
        for p in label {
            out.push(fact(
                SECRET_COLUMN,
                vec![
                    Value::Str(pred.clone()),
                    Value::Int(*col as i64),
                    Value::Str(p.clone()),
                ],
            ));
        }
    }
    for (head, body, _) in rules(&lowered.program) {
        let Some(h) = head.filter(|h| h.pred == "arg") else {
            continue;
        };
        let [typ, _, path, value, _] = h.args.as_slice() else {
            continue;
        };
        let (Some(t), Some(p)) = (s(typ), s(path)) else {
            continue;
        };
        let p = p.trim_start_matches('.');
        for q in pass.term_label(value, &pass.body_vars(body)) {
            out.push(fact(
                SECRET_PATH,
                vec![Value::Str(t.into()), Value::Str(p.into()), Value::Str(q)],
            ));
        }
    }
    out
}

/// The pass over a lowered program against the provider schema; `outputs`
/// are other stacks' secret outputs the run read, (deployment, key).
pub fn check(
    lowered: &Lowered,
    schema: &Schema,
    outputs: &BTreeSet<(String, String)>,
) -> Result<()> {
    let pass = fixpoint(lowered, schema, outputs);
    let rs = rules(&lowered.program);

    let mut diags = Vec::new();
    for (head, body, span) in &rs {
        let vars = pass.body_vars(body);
        let refinement = is_refinement(*head);
        let secret = |t: &Term| pass.term_secret(t, &vars);
        for l in body.iter() {
            match l {
                Lit::Neq(x, y) | Lit::Gt(x, y) | Lit::Ge(x, y) | Lit::Lt(x, y) | Lit::Le(x, y)
                    if !refinement && (secret(x) || secret(y)) =>
                {
                    diags.push(e0301(*span, "a comparison"));
                }
                Lit::Eq(x, y) if !refinement => {
                    // `X = f(Secret)`: a function that inspects it.
                    for t in [x, y] {
                        if let Some(f) = inspecting(t, &|t| secret(t)) {
                            diags.push(e0301(*span, &crate::functions::shown_call(&f)));
                        }
                    }
                    // A test between two terms, neither a fresh variable.
                    if !matches!(x, Term::Var(_))
                        && !matches!(y, Term::Var(_))
                        && (secret(x) || secret(y))
                    {
                        diags.push(e0301(*span, "an equality test"));
                    }
                }
                Lit::Pos(a) if !refinement && is_builtin_pred(&a.pred) => {
                    if a.args.iter().any(&secret) {
                        diags.push(e0301(a.span, &format!("{}/{}", a.pred, a.args.len())));
                    }
                }
                Lit::Not(a) if !refinement => {
                    let bound_secret = a.args.iter().enumerate().any(|(i, t)| {
                        secret(t) || (pass.position_secret(a, i) && !matches!(t, Term::Wildcard))
                    });
                    if bound_secret {
                        diags.push(Diagnostic::error(
                            a.span,
                            format!(
                                "E0302: `not {}(...)` over a secret: its absence leaks a bit",
                                a.pred
                            ),
                        ));
                    }
                }
                _ => {}
            }
        }
        let Some(h) = head else { continue };
        // E0303: an aggregate that is not a collect.
        for t in &h.args {
            if let Term::Func { name, args } = t
                && matches!(
                    name.as_str(),
                    "count" | "sum" | "min" | "max" | "any" | "all"
                )
                && args.iter().any(&secret)
            {
                diags.push(Diagnostic::error(
                    h.span,
                    format!("E0303: {name}() over a secret leaks it; only collect_* may aggregate a secret"),
                ));
            }
            if let Some(f) = inspecting(t, &|t| secret(t))
                && !COLLECT.contains(&f.as_str())
            {
                diags.push(e0301(h.span, &crate::functions::shown_call(&f)));
            }
        }
        // E0305: a name.
        let named = |t: &Term| secret(t) || names_secret(t, &|t| secret(t));
        let addr = crate::zset::address_arg(h);
        if addr.is_some_and(named) || h.args.iter().any(|t| names_secret(t, &|t| secret(t))) {
            diags.push(Diagnostic::error(
                h.span,
                "E0305: a secret reaches a resource address; names are printed everywhere",
            ));
        }
        // E0304: a public place.
        match (h.pred.as_str(), h.args.len()) {
            ("arg", 5)
                if let Some(leak) =
                    pass.public_leaf(&h.args[0], &h.args[1], &h.args[2], &h.args[3], &vars) =>
            {
                // The type the program declares at the place, if any.
                let its = |ty: Option<&crate::ast::TypeExpr>| {
                    ty.map(|t| format!(": its type is {}", crate::inputs::type_text(t)))
                        .unwrap_or_default()
                };
                let scope = s(&h.args[1]).unwrap_or_default();
                let place = match (s(&h.args[0]), Some(leak.as_str())) {
                    (Some(crate::transform::OUTPUT), Some(p)) => {
                        let (k, rest) = p.split_once('.').unwrap_or((p, ""));
                        let ty = lowered
                            .output_types
                            .get(&(scope.to_string(), k.to_string()))
                            .and_then(|t| crate::types::field(t, rest));
                        format!("output {p}, not declared secret(T){}", its(ty))
                    }
                    (Some(crate::modules::INPUT), Some(p)) => {
                        let ty = lowered.inputs.iter().find_map(|d| {
                            let rest = match p.strip_prefix(d.decl.name.as_str())? {
                                "" => "",
                                r => r.strip_prefix('.')?,
                            };
                            (d.scope == scope)
                                .then(|| crate::types::field(&d.decl.ty, rest))
                                .flatten()
                        });
                        format!("input {p}, not declared secret(T){}", its(ty))
                    }
                    (Some(t), Some("?")) => format!("{t} at a path the program computes"),
                    (Some(t), Some(p)) => format!("{t} .{p}, not marked sensitive in the schema"),
                    _ => "an attribute path the program computes".to_string(),
                };
                diags.push(Diagnostic::error(
                    h.span,
                    format!("E0304: a secret reaches {place}"),
                ));
            }
            ("deny" | "warn", _) if !refinement && h.args.iter().any(secret) => {
                diags.push(Diagnostic::error(
                    h.span,
                    format!(
                        "E0304: a secret reaches a {} message or context, which is printed",
                        h.pred
                    ),
                ));
            }
            _ => {}
        }
    }
    if diags.is_empty() {
        Ok(())
    } else {
        diags.dedup_by(|a, b| a.render(false) == b.render(false));
        Err(Diagnostics(diags).into())
    }
}

fn e0301(span: Span, what: &str) -> Diagnostic {
    Diagnostic::error(
        span,
        format!("E0301: {what} over a secret: inspecting a secret leaks it"),
    )
    .with_help("check it at the input: `input k: secret(T) where ...`, or leave it to the provider")
}

fn is_builtin_pred(p: &str) -> bool {
    matches!(p, "member" | "enumerate") || crate::functions::is_predicate(p)
}

/// The first function in `t` that inspects a secret argument.
fn inspecting(t: &Term, secret: &dyn Fn(&Term) -> bool) -> Option<String> {
    match t {
        // What is declassified may be inspected: the rule says so.
        Term::Func { name, .. } if name == DECLASSIFY => None,
        Term::Func { name, args } => {
            if !carries(name) && args.iter().any(secret) {
                return Some(name.clone());
            }
            args.iter().find_map(|a| inspecting(a, secret))
        }
        Term::List(xs) => xs.iter().find_map(|a| inspecting(a, secret)),
        Term::Obj(m) => m.values().find_map(|a| inspecting(a, secret)),
        _ => None,
    }
}

/// Does `t` name a resource with a secret (`ref(T, Secret, P)`,
/// `scoped(S, Secret)`)?
fn names_secret(t: &Term, secret: &dyn Fn(&Term) -> bool) -> bool {
    match t {
        Term::Func { name, .. } if name == DECLASSIFY => false,
        Term::Func { name, args } => {
            (name == crate::ir::REF && args.len() == 3 && secret(&args[1]))
                || (name == crate::ir::SCOPED && args.iter().any(secret))
                || args.iter().any(|a| names_secret(a, secret))
        }
        Term::List(xs) => xs.iter().any(|a| names_secret(a, secret)),
        Term::Obj(m) => m.values().any(|a| names_secret(a, secret)),
        _ => false,
    }
}

// The held store (R-60): what dform itself keeps of a secret. A
// `memo.first` of a secret candidate is sealed with a key derived from the
// stack's key (`state.key`) before it goes into state, and opened in
// memory by the run that reads it; `random.*` derives from a master
// secret by HKDF. The seal is XChaCha20-Poly1305 (the `chacha20poly1305`
// crate) under a key HKDF-SHA256 (the `hkdf` crate) derives from the
// stack's, with a random 24-byte nonce per value and the label as the
// associated data.

/// HKDF-SHA256 (RFC 5869): `len` bytes (at most 255 blocks) of key
/// material for `info` from the input key material `ikm`.
pub fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), ikm)
        .expand(info, &mut out)
        .expect("hkdf: at most 255 blocks");
    out
}

/// The bytes the stack's key derives for `what` (`Key::derive`): a key
/// that says nothing of the stack's, for one use.
pub fn derived(k: &crate::zset::file::Key, what: &str) -> [u8; 32] {
    k.derive(what).bytes()
}

/// The held store's cipher: its key HKDF of the stack's.
fn cipher(k: &crate::zset::file::Key) -> chacha20poly1305::XChaCha20Poly1305 {
    use chacha20poly1305::KeyInit;
    let key = hkdf(b"dform held store", &k.bytes(), b"xchacha20poly1305", 32);
    chacha20poly1305::XChaCha20Poly1305::new_from_slice(&key).expect("a 32-byte key")
}

/// The length of a seal's nonce, and of its tag.
const NONCE: usize = 24;
const TAG: usize = 16;

/// `plain` sealed under the stack key `k` for `label` (what it is bound
/// to: another label's seal does not open as this one): base64 of a
/// random nonce and the ciphertext with its tag.
pub fn seal(k: &crate::zset::file::Key, label: &str, plain: &[u8]) -> Result<String> {
    use base64::Engine;
    use chacha20poly1305::aead::{Aead, Payload};
    use std::io::Read;
    let mut nonce = [0u8; NONCE];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut nonce))
        .map_err(|e| anyhow::anyhow!("read /dev/urandom for a seal's nonce: {e}"))?;
    let ct = cipher(k)
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: plain,
                aad: label.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("{label}: seal the held value"))?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(base64::engine::general_purpose::STANDARD.encode(out))
}

/// What [`seal`] sealed for `label` under `k`; an error when the seal is
/// not one (another key, another label, altered).
pub fn open(k: &crate::zset::file::Key, label: &str, sealed: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    use chacha20poly1305::aead::{Aead, Payload};
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(sealed)
        .map_err(|e| anyhow::anyhow!("{label}: the held value is not base64: {e}"))?;
    if bytes.len() < NONCE + TAG {
        anyhow::bail!("{label}: the held value is too short to be a seal");
    }
    let (nonce, ct) = bytes.split_at(NONCE);
    cipher(k)
        .decrypt(
            nonce.into(),
            Payload {
                msg: ct,
                aad: label.as_bytes(),
            },
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "{label}: the held value does not open with this stack's key (state.key): \
                 another stack's key, or altered"
            )
        })
}

#[cfg(test)]
mod held_tests {
    use super::*;

    /// RFC 5869 test case 1.
    #[test]
    fn hkdf_matches_its_rfc() {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0u8..=0x0c).collect();
        let info: Vec<u8> = (0xf0u8..=0xf9).collect();
        assert_eq!(
            hex(&hkdf(&salt, &ikm, &info, 42)),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn a_seal_opens_with_its_key_and_label_only() {
        let k = crate::zset::file::Key::from_hex(&"11".repeat(32)).unwrap();
        let other = crate::zset::file::Key::from_hex(&"22".repeat(32)).unwrap();
        let s = seal(
            &k,
            "db-pw",
            b"hunter2-but-longer-than-one-block-of-32-bytes",
        )
        .unwrap();
        assert!(!s.contains("hunter2"));
        assert_eq!(
            open(&k, "db-pw", &s).unwrap(),
            b"hunter2-but-longer-than-one-block-of-32-bytes"
        );
        assert!(open(&other, "db-pw", &s).is_err());
        assert!(open(&k, "other", &s).is_err());
        // An altered byte anywhere (nonce, ciphertext, tag) does not open.
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        let bytes = b64.decode(&s).unwrap();
        for i in [0, NONCE, bytes.len() - 1] {
            let mut bad = bytes.clone();
            bad[i] ^= 1;
            assert!(open(&k, "db-pw", &b64.encode(&bad)).is_err(), "byte {i}");
        }
        assert_ne!(
            s,
            seal(
                &k,
                "db-pw",
                b"hunter2-but-longer-than-one-block-of-32-bytes"
            )
            .unwrap()
        );
    }
}

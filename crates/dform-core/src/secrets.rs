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
//! - E0301 a comparison, a join (`pw(p), known(p)`), a builtin predicate
//!   or an inspecting function (`len`, `split`, `inet_*`, ...) over a
//!   secret: comparing leaks a bit;
//! - E0302 a negated literal over a secret: absence leaks a bit;
//! - E0303 an aggregate other than `collect_*` over a secret: `count`
//!   leaks cardinality;
//! - E0304 a secret reaching a public place: a resource attribute the
//!   schema does not mark `sensitive`, a provider's setting it does not
//!   declare sensitive (when it declares its settings), an output not declared
//!   `secret(T)`, an input not declared `secret(T)`, a `deny`/`warn`;
//! - E0305 a secret reaching a resource address (`want`, `arg`, `ref`,
//!   `scoped`): names are printed everywhere;
//! - E0306 a secret reaching what a coeffect is asked with: a location
//!   (`io.read("https://x/${pw}")`), an extern's `+` column not declared
//!   `+x: secret(T)`; it is sent over the network at plan (R-167).
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

/// `secrets(Key, Kind, Generation, RotatedAt)` (R-161): each secret the
/// deployment derives (`random`) or keeps (`memo`), by its key, at its
/// generation, rotated at a time (a key never rotated: when its
/// deployment's master was first applied, `born`; now for a deployment
/// not applied yet). A policy reads its age through the clock: `deny ..
/// where secrets(k, _, _, at), now = time.now(), now - at > 90d`.
pub const SECRETS: &str = "secrets";

/// `rotated(Key, Generation, By)` (R-161): a rotation the plan carries,
/// recorded by `dform secrets rotate` and not applied yet; `By` the
/// asserted actor. What an approval policy reads.
pub const ROTATED: &str = "rotated";

/// The rows of `secrets/4` and `rotated/3` for a run of `st`: the keys
/// its `random.*` calls derived for (`functions::random::calls`), the
/// memos it keeps, and every key rotated.
pub fn rotation_facts(st: &crate::state::State) -> Vec<Atom> {
    let now = crate::memo::now();
    let born = crate::functions::random::born();
    let born = born.as_deref();
    let time = |t: &str| {
        crate::time::Time::parse(t)
            .map(Value::Time)
            .unwrap_or_else(|_| Value::Str(t.to_string()))
    };
    let s = |x: &str| Term::Val(Value::Str(x.to_string()));
    let atom = |pred: &str, args: Vec<Term>| Atom {
        pred: pred.into(),
        args,
        record: None,
        span: Default::default(),
    };
    let mut kinds: BTreeMap<String, &str> = BTreeMap::new();
    for (k, c) in crate::functions::random::calls() {
        if c.secret {
            kinds.insert(k, "random");
        }
    }
    for k in st.memo.keys() {
        kinds.insert(k.clone(), "memo");
    }
    for k in st.secrets.keys() {
        kinds.entry(k.clone()).or_insert("random");
    }
    let mut out = Vec::new();
    for (k, kind) in kinds {
        let r = st.secrets.get(&k);
        let at = match r.filter(|r| !r.rotated_at.is_empty()) {
            Some(r) => r.rotated_at.clone(),
            None if kind == "memo" => st.memo[&k].kept.clone(),
            None => born.unwrap_or(&now).to_string(),
        };
        let generation = r.map_or(1, |r| r.generation);
        out.push(atom(
            SECRETS,
            vec![
                s(&k),
                s(kind),
                Term::Val(Value::Int(generation.into())),
                Term::Val(time(&at)),
            ],
        ));
        if let Some(r) = r.filter(|r| r.pending) {
            out.push(atom(
                ROTATED,
                vec![s(&k), Term::Val(Value::Int(r.generation.into())), s(&r.by)],
            ));
        }
    }
    out
}

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
    /// What to write instead of inspecting the secret in `terms` (E0301):
    /// a check where an input that is a secret is declared, when the
    /// secret is that input's; else declassifying it, when what the
    /// inspection tells is public.
    fn inspect_fix(&self, lowered: &Lowered, body: &[Lit], vars: &Vars, terms: &[&Term]) -> String {
        let x = terms.iter().find_map(|t| secret_var(t, vars));
        // The input whose read binds a secret variable of `terms`.
        let input = body.iter().find_map(|l| {
            let Lit::Pos(a) = l else { return None };
            let k = input_of(lowered, a)?;
            let mut holds = false;
            a.args[0].for_each_var(&mut |v| {
                holds |= terms.iter().any(|t| {
                    let mut here = false;
                    t.for_each_var(&mut |w| here |= w == v);
                    here
                }) && vars.get(v).is_some_and(|l| !l.is_empty());
            });
            holds.then_some(k)
        });
        match input {
            Some(d) => input_check(d),
            None => format!(
                "carry it uninspected to an attribute marked sensitive, {}",
                declassified(&x)
            ),
        }
    }

    /// The attributes of `t` the schema marks sensitive, said for a help.
    fn sensitive_attrs(&self, t: &str) -> String {
        let attrs: Vec<&str> = self
            .schema
            .facts
            .iter()
            .filter(|f| {
                f.pred == "type_attr" && s(&f.args[0]) == Some(t) && self.flag(f, "sensitive")
            })
            .filter_map(|f| s(&f.args[1]))
            .collect();
        match attrs.as_slice() {
            [] => format!("{t} marks no attribute sensitive: write it to a type that does"),
            ps => format!("write it to one {t} marks sensitive: {}", ps.join(", ")),
        }
    }

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
            // A path of a type the program does not fix: secret where
            // any type's is, at it, under it or above it.
            (None, Some(p)) => {
                let p = p.trim_start_matches('.');
                let near = |q: &str| {
                    q == p || q.starts_with(&format!("{p}.")) || p.starts_with(&format!("{q}."))
                };
                all(self.schema.facts.iter().any(|f| {
                    f.pred == "type_attr"
                        && s(&f.args[1]).is_some_and(near)
                        && self.flag(f, "sensitive")
                }))
            }
            (None, None) => all(self
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
            // An object with computed keys: each value's label under its
            // key when the key is a constant, all of it when it is not.
            Term::Func { name, args } if name == crate::functions::OBJECT => {
                let mut out = Label::new();
                for kv in args.chunks(2) {
                    let [k, v] = kv else { continue };
                    let l = self.term_label(v, vars);
                    match k {
                        Term::Val(Value::Str(k)) => join(&mut out, under(l, k)),
                        _ if !l.is_empty() => join(&mut out, whole()),
                        _ => false,
                    };
                }
                out
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
    let mut checker = Checker::new(lowered, schema, outputs);
    for (head, body, span) in rules(&lowered.program) {
        checker.rule(head, body, span);
    }
    checker.into_result()
}

/// The checks of every rule of a lowered program, what they found so far.
struct Checker<'a> {
    lowered: &'a Lowered,
    schema: &'a Schema,
    pass: Pass<'a>,
    /// The program's externs, by name.
    fns: BTreeMap<&'a str, &'a crate::ast::ExternFn>,
    diags: Vec<Diagnostic>,
}

/// A rule being checked: what its body's variables hold of a secret.
struct Rule<'r> {
    body: &'r [Lit],
    span: Span,
    vars: Vars,
    /// A refinement's rule tests the value it refines, by design.
    refinement: bool,
}

impl<'a> Checker<'a> {
    fn new(
        lowered: &'a Lowered,
        schema: &'a Schema,
        outputs: &'a BTreeSet<(String, String)>,
    ) -> Checker<'a> {
        Checker {
            lowered,
            schema,
            pass: fixpoint(lowered, schema, outputs),
            fns: lowered
                .extern_fns
                .iter()
                .map(|f| (f.name.as_str(), f))
                .collect(),
            diags: Vec::new(),
        }
    }

    /// What the checks found: each error once.
    fn into_result(mut self) -> Result<()> {
        if self.diags.is_empty() {
            return Ok(());
        }
        self.diags
            .dedup_by(|a, b| a.render(false) == b.render(false));
        Err(Diagnostics(self.diags).into())
    }

    /// An extern's `+` column is asked with, not answered: it binds nothing.
    fn binds(&self, a: &Atom, i: usize) -> bool {
        self.fns
            .get(a.pred.as_str())
            .and_then(|f| f.args.get(i))
            .is_none_or(|b| !b.input)
    }

    /// Whether `t` holds a secret in the rule `r`.
    fn secret(&self, r: &Rule, t: &Term) -> bool {
        self.pass.term_secret(t, &r.vars)
    }

    /// What to write instead of inspecting the secret in `terms` (E0301).
    fn inspect_fix(&self, r: &Rule, terms: &[&Term]) -> String {
        self.pass.inspect_fix(self.lowered, r.body, &r.vars, terms)
    }

    /// Every check of one rule.
    fn rule(&mut self, head: Option<&Atom>, body: &[Lit], span: Span) {
        let r = Rule {
            body,
            span,
            vars: self.pass.body_vars(body),
            refinement: is_refinement(head),
        };
        self.coeffects(&r);
        if !r.refinement {
            self.joins(&r);
        }
        for (i, l) in body.iter().enumerate() {
            self.literal(&r, i, l);
        }
        let Some(h) = head else { return };
        self.aggregates(&r, h);
        self.address(&r, h);
        self.public_place(&r, h);
    }

    /// E0306: what a coeffect is asked with is sent off the machine at
    /// plan (a location's host and path, a data source's argument),
    /// unless its column is declared `+x: secret(T)` (R-167).
    fn coeffects(&mut self, r: &Rule) {
        let (body, span, vars, fns) = (r.body, &r.span, &r.vars, &self.fns);
        let secret = |t: &Term| self.secret(r, t);
        let mut diags = Vec::new();
        // E0306: what a coeffect is asked with is sent off the machine at
        // plan (a location's host and path, a data source's argument),
        // unless its column is declared `+x: secret(T)` (R-167).
        for l in body.iter() {
            let (Lit::Pos(a) | Lit::Not(a)) = l else {
                continue;
            };
            let Some(f) = fns.get(a.pred.as_str()) else {
                continue;
            };
            // What dform answers itself stays on the machine (`memo.first`
            // of a secret candidate seals it, `env.var`); a location's read
            // does not.
            let location = crate::tables::is_document(&a.pred) || a.pred == crate::files::READ;
            if crate::externs::in_process(&a.pred) && !location {
                continue;
            }
            for (t, b) in a.args.iter().zip(&f.args) {
                if b.input && !crate::externs::is_secret(b) && secret(t) {
                    let at = if a.span.is_none() { *span } else { a.span };
                    let (what, fix) = match location {
                        true => (
                            "a location".to_string(),
                            "a credential is named, never written into the location: \
                             `[io] credentials` in dform.toml gives it to the transport"
                                .to_string(),
                        ),
                        false => (
                            format!("{}'s argument `{}`", f.name, b.name),
                            format!(
                                "{} is asked with `{}` as it is: pass a public value, {}",
                                f.name,
                                b.name,
                                declassified(&secret_var(t, &vars))
                            ),
                        ),
                    };
                    diags.push(
                        Diagnostic::error(
                            at,
                            format!("E0306: a secret reaches {what}, which is sent over the network and printed"),
                        )
                        .with_help(fix),
                    );
                }
            }
        }
        self.diags.extend(diags);
    }

    /// A join on a secret: a secret variable at two positions of the body's
    /// relations (`pw(p), known(p)`) tests the secret against the other's
    /// rows, as `p == "hunter2"` would; declassified, its value is public.
    fn joins(&mut self, r: &Rule) {
        let (body, span, vars) = (r.body, &r.span, &r.vars);
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for l in body.iter() {
            let Lit::Pos(a) = l else { continue };
            if is_builtin_pred(&a.pred) || a.pred == crate::memo::FIRST {
                continue;
            }
            let mut here = Vec::new();
            for (_, t) in a.args.iter().enumerate().filter(|(j, _)| self.binds(a, *j)) {
                pattern_vars(t, &mut here);
            }
            let at = if a.span.is_none() { *span } else { a.span };
            for v in here {
                if !seen.insert(v) && vars.get(v).is_some_and(|l| !l.is_empty()) {
                    let fix = self.inspect_fix(r, &[&Term::Var(v.into())]);
                    self.diags.push(e0301(at, "a join", fix));
                }
            }
        }
    }

    /// The body's `i`th literal `l`: a comparison, an equality test, a
    /// builtin or a function that inspects a secret (E0301); a value
    /// matched at a secret position; a negation over one (E0302).
    fn literal(&mut self, r: &Rule, i: usize, l: &Lit) {
        let (lowered, body, span, vars, refinement) =
            (self.lowered, r.body, &r.span, &r.vars, r.refinement);
        let pass = &self.pass;
        let binds = |a: &Atom, j: usize| self.binds(a, j);
        let secret = |t: &Term| self.secret(r, t);
        let mut diags = Vec::new();
        match l {
            Lit::Neq(x, y) | Lit::Gt(x, y) | Lit::Ge(x, y) | Lit::Lt(x, y) | Lit::Le(x, y)
                if !refinement && (secret(x) || secret(y)) =>
            {
                let fix = pass.inspect_fix(lowered, body, &vars, &[x, y]);
                diags.push(e0301(*span, "a comparison", fix));
            }
            Lit::Eq(x, y) if !refinement => {
                // `X = f(Secret)`: a function that inspects it.
                for t in [x, y] {
                    if let Some(f) = inspecting(t, &|t| secret(t)) {
                        diags.push(call_e0301(*span, &f, t, &vars));
                    }
                }
                if secret(x) || secret(y) {
                    // A test: both sides bound by the rest of the body
                    // (`pw(p), p == "hunter2"`), not a binding of either.
                    let bound = bound_without(body, i, &binds);
                    let mut both = BTreeSet::new();
                    term_vars(x, &mut both);
                    term_vars(y, &mut both);
                    let fix = || pass.inspect_fix(lowered, body, &vars, &[x, y]);
                    if both.is_subset(&bound) {
                        diags.push(e0301(*span, "an equality test", fix()));
                    } else if definedness(x, y) {
                        // `has conn.password`: a walk into a secret bound
                        // to a name nothing reads, for whether it is there.
                        diags.push(e0301(*span, "a definedness test (`has`)", fix()));
                    }
                }
            }
            Lit::Pos(a) if !refinement && is_builtin_pred(&a.pred) => {
                if a.args.iter().any(&secret) {
                    let args: Vec<&Term> = a.args.iter().collect();
                    let fix = pass.inspect_fix(lowered, body, &vars, &args);
                    diags.push(e0301(a.span, &format!("{}/{}", a.pred, a.args.len()), fix));
                }
            }
            // A value written at a secret position is matched against
            // it (`pw == "hunter2"` lowers to `pw("hunter2")`); `_`
            // there asks whether it is set (`has pw`).
            Lit::Pos(a) if !refinement && a.pred != crate::memo::FIRST => {
                let at = if a.span.is_none() { *span } else { a.span };
                for (j, t) in a.args.iter().enumerate() {
                    let l = pass.position_label(a, j);
                    if l.is_empty() {
                        continue;
                    }
                    let fix = || match input_of(lowered, a) {
                        Some(d) => input_check(d),
                        None => format!(
                            "carry it uninspected to an attribute marked sensitive, {}",
                            declassified(&None)
                        ),
                    };
                    if matches!(t, Term::Wildcard) && l.contains("") {
                        diags.push(e0301(at, "a definedness test (`has`)", fix()));
                    } else if matches_secret(t, &l) {
                        diags.push(e0301(at, "an equality test", fix()));
                    }
                }
            }
            Lit::Not(a) if !refinement => {
                // `not has pw` too: `_` at a secret position asks
                // whether it is set.
                let bound_secret = a.args.iter().enumerate().any(|(i, t)| {
                    let l = pass.position_label(a, i);
                    secret(t) || (!l.is_empty() && (!matches!(t, Term::Wildcard) || l.contains("")))
                });
                if bound_secret {
                    let x = a.args.iter().find_map(|t| secret_var(t, &vars));
                    let fix = match (&x, input_of(lowered, a)) {
                        (_, Some(d)) => format!(
                            "whether input `{k}` is given is a bit of it: give it a \
                                 default, `input {k}: {t} = ..`, and leave the test out",
                            k = d.decl.name,
                            t = crate::inputs::type_text(&d.decl.ty),
                        ),
                        (Some(x), None) => format!(
                            "whether `{x}` is there is a bit of it: test `{p}` of a public \
                                 value, or of `secret.declassify({x}, \"why\")` if that bit \
                                 may be known",
                            p = a.pred.rsplit("::").next().unwrap_or(&a.pred),
                        ),
                        (None, None) => format!(
                            "whether a secret is there is a bit of it: test `{}` of a \
                                 public value",
                            a.pred.rsplit("::").next().unwrap_or(&a.pred),
                        ),
                    };
                    diags.push(
                        Diagnostic::error(
                            a.span,
                            format!(
                                "E0302: `not {}(...)` over a secret: its absence leaks a bit",
                                a.pred
                            ),
                        )
                        .with_help(fix),
                    );
                }
            }
            _ => {}
        }
        self.diags.extend(diags);
    }

    /// E0303: an aggregate that is not a collect; a function of the head
    /// that inspects a secret.
    fn aggregates(&mut self, r: &Rule, h: &Atom) {
        let vars = &r.vars;
        let secret = |t: &Term| self.secret(r, t);
        let mut diags = Vec::new();
        for t in &h.args {
            if let Term::Func { name, args } = t
                && matches!(
                    name.as_str(),
                    "count" | "sum" | "min" | "max" | "any" | "all"
                )
                && args.iter().any(&secret)
            {
                let x = args
                    .iter()
                    .find_map(|t| secret_var(t, &vars))
                    .unwrap_or_else(|| "x".into());
                diags.push(
                    Diagnostic::error(
                        h.span,
                        format!("E0303: {name}() over a secret leaks it; only collect_* may aggregate a secret"),
                    )
                    .with_help(format!(
                        "`collect_list({x})` gathers the secrets and is one; {name}() a public \
                         value of the same rows instead"
                    )),
                );
            }
            if let Some(f) = inspecting(t, &|t| secret(t))
                && !COLLECT.contains(&f.as_str())
            {
                diags.push(call_e0301(h.span, &f, t, &vars));
            }
        }
        self.diags.extend(diags);
    }

    /// E0305: a secret reaches a resource's name.
    fn address(&mut self, r: &Rule, h: &Atom) {
        let (body, vars) = (r.body, &r.vars);
        let secret = |t: &Term| self.secret(r, t);
        let mut diags = Vec::new();
        let named = |t: &Term| secret(t) || names_secret(t, &|t| secret(t));
        let addr = crate::zset::address_arg(h);
        if addr.is_some_and(named) || h.args.iter().any(|t| names_secret(t, &|t| secret(t))) {
            // The secret as a relation of the body gives it, by its name.
            let x = body.iter().find_map(|l| match l {
                Lit::Pos(a) => a.args.iter().find_map(|t| secret_var(t, &vars)),
                _ => None,
            });
            diags.push(
                Diagnostic::error(
                    h.span,
                    "E0305: a secret reaches a resource address; names are printed everywhere",
                )
                .with_help(format!(
                    "an address is kept in state and printed in every plan: name the resource \
                     by a public value (a key, a label){}",
                    x.map(|x| format!(", not `{x}`")).unwrap_or_default()
                )),
            );
        }
        self.diags.extend(diags);
    }

    /// E0304: a secret reaches a public place: an input, an output or an
    /// attribute not declared secret, a provider's setting not declared
    /// sensitive, a deny's message.
    fn public_place(&mut self, r: &Rule, h: &Atom) {
        let (lowered, schema, vars, refinement) =
            (self.lowered, self.schema, &r.vars, r.refinement);
        let pass = &self.pass;
        let secret = |t: &Term| self.secret(r, t);
        let mut diags = Vec::new();
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
                // A cell the program declares: say to declare it secret,
                // the whole of it or the field.
                let declare = |what: &str, p: &str, ty: Option<&crate::ast::TypeExpr>| {
                    let t = ty.map_or("T".to_string(), crate::inputs::type_text);
                    match p.split_once('.') {
                        None => format!("declare it secret: `{what} {p}: secret({t})`"),
                        Some((k, f)) => format!(
                            "declare the field secret in `{what} {k}`'s type: `{f}: secret({t})`"
                        ),
                    }
                };
                let (place, fix) = match (s(&h.args[0]), Some(leak.as_str())) {
                    (Some(crate::transform::OUTPUT), Some(p)) => {
                        let (k, rest) = p.split_once('.').unwrap_or((p, ""));
                        let ty = lowered
                            .output_types
                            .get(&(scope.to_string(), k.to_string()))
                            .and_then(|t| crate::types::field(t, rest));
                        (
                            format!("output {p}, not declared secret(T){}", its(ty)),
                            declare("output", p, ty),
                        )
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
                        (
                            format!("input {p}, not declared secret(T){}", its(ty)),
                            declare("input", p, ty),
                        )
                    }
                    (Some(t), Some("?")) => (
                        format!("{t} at a path the program computes"),
                        format!(
                            "a computed path may be any of {t}'s: name the path; {}",
                            pass.sensitive_attrs(t)
                        ),
                    ),
                    (Some(t), Some(p)) => (
                        format!("{t} .{p}, not marked sensitive in the schema"),
                        format!(
                            "{t} .{p} is printed in every plan; {}",
                            pass.sensitive_attrs(t)
                        ),
                    ),
                    _ => (
                        "an attribute path the program computes".to_string(),
                        "write it at a path the program names, one the schema marks sensitive"
                            .to_string(),
                    ),
                };
                diags.push(
                    Diagnostic::error(h.span, format!("E0304: a secret reaches {place}"))
                        .with_help(fix),
                );
            }
            // A setting the provider declares, not sensitive, or one it
            // does not declare: the provider may print or keep it.
            ("provider_config", 2)
                if let [name, Term::Obj(settings)] = h.args.as_slice()
                    && let Some(name) = s(name)
                    && let Some(declared) = schema.settings.get(name) =>
            {
                for k in settings.keys().filter(|k| secret(&settings[*k])) {
                    if declared.get(k) == Some(&true) {
                        continue;
                    }
                    let what = match declared.contains_key(k) {
                        true => "not declared sensitive",
                        false => "which it does not declare",
                    };
                    let sensitive: Vec<&str> = declared
                        .iter()
                        .filter(|(_, s)| **s)
                        .map(|(k, _)| k.as_str())
                        .collect();
                    let help = match sensitive.as_slice() {
                        [] => format!("provider {name} declares no sensitive setting"),
                        ks => format!("provider {name}'s sensitive settings: {}", ks.join(", ")),
                    };
                    diags.push(
                        Diagnostic::error(
                            h.span,
                            format!(
                                "E0304: a secret reaches provider {name}'s setting {k}, {what}"
                            ),
                        )
                        .with_help(help),
                    );
                }
            }
            ("deny" | "warn", _) if !refinement && h.args.iter().any(secret) => {
                let x = h.args.iter().find_map(|t| secret_var(t, &vars));
                diags.push(
                    Diagnostic::error(
                        h.span,
                        format!(
                            "E0304: a secret reaches a {} message or context, which is printed",
                            h.pred
                        ),
                    )
                    .with_help(match x {
                        Some(x) => format!(
                            "say what is wrong without the value: name `{x}`, never print it"
                        ),
                        None => "say what is wrong without the value: name the secret, never \
                                 print it"
                            .to_string(),
                    }),
                );
            }
            _ => {}
        }
        self.diags.extend(diags);
    }
}

/// The variables of `t`.
fn term_vars(t: &Term, out: &mut BTreeSet<String>) {
    t.for_each_var(&mut |v| {
        out.insert(v.to_string());
    });
}

/// The variables a pattern `t` binds, each occurrence (`_` none).
fn pattern_vars<'t>(t: &'t Term, out: &mut Vec<&'t str>) {
    match t {
        Term::Var(v) => out.push(v),
        Term::Func { name, .. } if name == DECLASSIFY => {}
        Term::Func { args, .. } | Term::List(args) => {
            args.iter().for_each(|a| pattern_vars(a, out))
        }
        Term::Obj(m) => m.values().for_each(|a| pattern_vars(a, out)),
        _ => {}
    }
}

/// The variables `body` binds without its literal `skip`: a positive
/// literal its own (an extern's answer columns, `binds`), an equality one
/// side's once the other's are.
fn bound_without(
    body: &[Lit],
    skip: usize,
    binds: &dyn Fn(&Atom, usize) -> bool,
) -> BTreeSet<String> {
    let mut bound = BTreeSet::new();
    loop {
        let n = bound.len();
        for (i, l) in body.iter().enumerate() {
            match l {
                _ if i == skip => {}
                Lit::Pos(a) => a
                    .args
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| binds(a, *j))
                    .for_each(|(_, t)| term_vars(t, &mut bound)),
                Lit::Eq(x, y) => {
                    for (p, q) in [(x, y), (y, x)] {
                        let mut vq = BTreeSet::new();
                        term_vars(q, &mut vq);
                        if vq.is_subset(&bound) {
                            term_vars(p, &mut bound);
                        }
                    }
                }
                _ => {}
            }
        }
        if bound.len() == n {
            return bound;
        }
    }
}

/// Whether a pattern `t` at a position labelled `l` compares a secret
/// with something: a value, a call or a list where a secret is (a
/// variable or `_` binds it; an object pattern, field by field).
fn matches_secret(t: &Term, l: &Label) -> bool {
    match t {
        Term::Var(_) | Term::Wildcard => false,
        Term::Obj(m) => m.iter().any(|(k, x)| matches_secret(x, &narrow(l, k))),
        Term::Func { name, .. } if name == DECLASSIFY => false,
        _ => !l.is_empty(),
    }
}

/// `has x.f` over a secret, as the resolver marks it: `V = __path(X,
/// "f")` with `V` its `has` variable (`resolve::HAS_VAR`).
fn definedness(x: &Term, y: &Term) -> bool {
    matches!(
        (x, y),
        (Term::Var(v), Term::Func { name, .. })
            if name == "__path" && crate::syntax::resolve::is_has_var(v)
    )
}

/// E0301 at `span`, `what` inspecting a secret, with the fix that
/// applies there ([`Pass::inspect_fix`]).
fn e0301(span: Span, what: &str, fix: String) -> Diagnostic {
    Diagnostic::error(
        span,
        format!("E0301: {what} over a secret: inspecting a secret leaks it"),
    )
    .with_help(fix)
}

/// E0301 for the call of `f` in `t` over a secret: what it returns is
/// public only if the program says so.
fn call_e0301(span: Span, f: &str, t: &Term, vars: &Vars) -> Diagnostic {
    let shown = crate::functions::shown_call(f);
    let x = secret_var(t, vars).unwrap_or_else(|| "VALUE".into());
    e0301(
        span,
        &shown,
        format!(
            "{shown} reads the secret's value: give it `secret.declassify({x}, \"why\")` if \
             what it returns may be public"
        ),
    )
}

/// The secret variable of `t` (the first whose value is secret), by the
/// name the program wrote.
fn secret_var(t: &Term, vars: &Vars) -> Option<String> {
    let mut out = None;
    t.for_each_var(&mut |v| {
        if out.is_none() && vars.get(v).is_some_and(|l| !l.is_empty()) {
            out = Some(crate::whynot::source_name(v));
        }
    });
    out
}

/// The input `a` reads, `k(V)` (`scope::k(V)` in a copy), when it is
/// one the program declares.
fn input_of<'l>(lowered: &'l Lowered, a: &Atom) -> Option<&'l crate::inputs::Declared> {
    if a.args.len() != 1 {
        return None;
    }
    lowered.inputs.iter().find(|d| match d.scope.as_str() {
        "" => a.pred == d.decl.name,
        sc => a.pred.strip_prefix(sc).and_then(|r| r.strip_prefix("::")) == Some(&d.decl.name),
    })
}

/// Check the input `d` where it is declared (E0301's fix when the secret
/// is an input's).
fn input_check(d: &crate::inputs::Declared) -> String {
    format!(
        "check it where it is declared, `input {}: {} check ..`: the check runs and prints no \
         value",
        d.decl.name,
        crate::inputs::type_text(&d.decl.ty)
    )
}

/// The way a secret leaves on purpose, said of `x`.
fn declassified(x: &Option<String>) -> String {
    let x = x.as_deref().unwrap_or("VALUE");
    format!("or `secret.declassify({x}, \"why\")` if it is public")
}

fn is_builtin_pred(p: &str) -> bool {
    matches!(p, "member" | "enumerate") || crate::functions::is_predicate(p)
}

/// The first function in `t` that inspects a secret argument.
fn inspecting(t: &Term, secret: &dyn Fn(&Term) -> bool) -> Option<String> {
    match t {
        // What is declassified may be inspected: the rule says so.
        Term::Func { name, .. } if name == DECLASSIFY => None,
        // A computed key is a name, printed wherever the object is: a
        // read of the secret. Its values are carried.
        Term::Func { name, args } if name == crate::functions::OBJECT => {
            if args.iter().step_by(2).any(secret) {
                return Some(name.clone());
            }
            args.iter().find_map(|a| inspecting(a, secret))
        }
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

/// Stand-ins (R-164): what a run that does not hold the master derives in
/// place of each secret the master derives (a `random.*` value, a sealed
/// memo's), and how it proves one unchanged without its value.
///
/// A stand-in is the value the same derivation gives under a public
/// master made from the master id, so it has the value's shape and is a
/// function of the derivation's inputs (the function, the deployment, the
/// key, every knob, the master id) alone: public. A run with the master
/// derives both and records, beside each secret leaf an apply wrote
/// (`StateEntry::derived`), the digest of the leaf with every derived
/// value in it replaced by its stand-in ([`digest`]). A run without the
/// master derives the stand-ins only: a leaf whose digest is the one state
/// recorded is unchanged, any other change of it needs the master. A
/// stand-in never leaves the process but as such a digest: an action that
/// would send one is not sent ([`carries`]).
pub mod standin {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Mutex;

    struct Registry {
        /// Each derived value's text (a stand-in's own, in a run without
        /// the master) -> its label and its stand-in's text.
        derived: BTreeMap<String, (String, String)>,
        /// The secrets of the run that are not derived (an input's, an
        /// environment variable's): a leaf holding one has no digest, as
        /// its digest would say something of it.
        sources: BTreeSet<String>,
        /// The run does not hold the master: its derived values are
        /// stand-ins.
        active: bool,
    }

    static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
        derived: BTreeMap::new(),
        sources: BTreeSet::new(),
        active: false,
    });

    fn registry() -> std::sync::MutexGuard<'static, Registry> {
        REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The run holds no master (`true`): what it derives are stand-ins.
    pub fn set_active(active: bool) {
        registry().active = active;
    }

    pub fn active() -> bool {
        registry().active
    }

    /// `value`, derived as `label`, stands in as `standin`.
    pub fn register(value: &str, label: &str, standin: &str) {
        if value.is_empty() {
            return;
        }
        registry()
            .derived
            .entry(value.to_string())
            .or_insert_with(|| (label.to_string(), standin.to_string()));
    }

    /// The run's secrets that are not derived (`query::Redactor::sources`).
    pub fn set_sources(sources: impl IntoIterator<Item = String>) {
        registry().sources = sources.into_iter().filter(|s| !s.is_empty()).collect();
    }

    /// Whether `text` is or holds a derived value (or its stand-in).
    pub fn derived(text: &str) -> bool {
        let r = registry();
        r.derived
            .iter()
            .any(|(v, (_, s))| text.contains(v.as_str()) || text.contains(s.as_str()))
    }

    /// The label of each derived value `text` holds, in a run without the
    /// master (where a value is its stand-in).
    pub fn labels(text: &str) -> Vec<String> {
        let r = registry();
        r.derived
            .iter()
            .filter(|(v, _)| text.contains(v.as_str()))
            .map(|(_, (l, _))| l.clone())
            .collect()
    }

    /// Whether `j` holds a stand-in a run without the master derived: what
    /// it must never send.
    pub fn carries(j: &serde_json::Value) -> bool {
        let r = registry();
        if !r.active {
            return false;
        }
        fn walk(j: &serde_json::Value, f: &dyn Fn(&str) -> bool) -> bool {
            match j {
                serde_json::Value::String(s) => f(s),
                serde_json::Value::Array(xs) => xs.iter().any(|x| walk(x, f)),
                serde_json::Value::Object(m) => m.values().any(|x| walk(x, f)),
                _ => false,
            }
        }
        walk(j, &|s| r.derived.keys().any(|v| s.contains(v.as_str())))
    }

    /// The digest of the leaf `j` with each derived value in it replaced
    /// by its stand-in, hex: `None` when it holds none, or holds a secret
    /// that is not derived.
    pub fn digest(j: &serde_json::Value) -> Option<String> {
        let r = registry();
        let mut hit = false;
        fn walk(j: &serde_json::Value, r: &Registry, hit: &mut bool) -> Option<serde_json::Value> {
            Some(match j {
                serde_json::Value::String(s) => {
                    let mut out = s.clone();
                    // The longest first: one derived value may hold another.
                    let mut vs: Vec<(&String, &String)> =
                        r.derived.iter().map(|(v, (_, st))| (v, st)).collect();
                    vs.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));
                    for (v, st) in vs {
                        if out.contains(v.as_str()) {
                            *hit = true;
                            out = out.replace(v.as_str(), st);
                        }
                    }
                    if r.sources.iter().any(|x| out.contains(x.as_str())) {
                        return None;
                    }
                    serde_json::Value::String(out)
                }
                serde_json::Value::Array(xs) => serde_json::Value::Array(
                    xs.iter().map(|x| walk(x, r, hit)).collect::<Option<_>>()?,
                ),
                serde_json::Value::Object(m) => serde_json::Value::Object(
                    m.iter()
                        .map(|(k, x)| Some((k.clone(), walk(x, r, hit)?)))
                        .collect::<Option<_>>()?,
                ),
                other => other.clone(),
            })
        }
        let standin = walk(j, &r, &mut hit)?;
        hit.then(|| {
            crate::approval::sha256_hex(crate::approval::canonical_json(&standin).as_bytes())
        })
    }

    /// Forget the run's registry (a test's, between deployments).
    pub fn clear() {
        let mut r = registry();
        r.derived.clear();
        r.sources.clear();
        r.active = false;
    }
}

/// A deployment's secrets, as `dform secrets list` lists them (R-161):
/// each by its key, where it comes from, its generation and age, and the
/// cells that read it, each with how a new value lands there. Never a
/// value: a secret is matched by the values the run holds, and named.
pub mod inventory {
    use crate::ast::{Atom, Term};
    use crate::value::Value;
    use std::collections::{BTreeMap, BTreeSet};

    /// Where a secret comes from.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Kind {
        /// `random.*`: derived from the master, rotated by its generation.
        Random,
        /// `memo.first`: kept in state, rotated by forgetting it.
        Memo,
        /// An input or an environment variable: the operator gives it.
        Given,
        /// A provider, another stack or a location holds it.
        Held,
        /// A secret manager keeps it, versioned (R-172): a location read
        /// into a secret cell, at the version the manager answered.
        Managed,
    }

    impl Kind {
        pub fn word(self) -> &'static str {
            match self {
                Kind::Random => "random",
                Kind::Memo => "memo",
                Kind::Given => "given",
                Kind::Held => "held",
                Kind::Managed => "managed",
            }
        }
    }

    /// How a new value lands in a cell.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Lands {
        Update,
        /// The attribute is `force_new`: the object is replaced.
        Replace,
        /// A replace `lifecycle prevent_destroy` refuses.
        Refused,
    }

    impl Lands {
        pub fn words(self) -> &'static str {
            match self {
                Lands::Update => "update",
                Lands::Replace => "forces replace",
                Lands::Refused => "refused by prevent_destroy",
            }
        }
    }

    /// A cell that reads a secret: the attribute's address and path.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    pub struct Cell {
        pub typ: String,
        pub name: String,
        pub path: String,
        pub lands: Lands,
    }

    impl std::fmt::Display for Cell {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{} {}.{}", self.typ, self.name, self.path)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Secret {
        pub key: String,
        pub kind: Kind,
        /// A `random.*` or memo key's generation (1 unless rotated).
        pub generation: u32,
        /// The master epoch it derives from (R-165), or a memo is sealed
        /// under; none for a given or held one.
        pub epoch: Option<u32>,
        /// Since when it is what it is: rotated, kept, or (a key never
        /// rotated) its master first applied; RFC 3339.
        pub since: Option<String>,
        /// A given or held secret: where it lives, as `rotate` says it.
        pub lives: Option<String>,
        /// A managed secret's version, as its manager names it.
        pub version: Option<String>,
        pub cells: Vec<Cell>,
        /// The values: matched, never printed.
        values: BTreeSet<Value>,
    }

    impl Secret {
        /// A memo state keeps that is no secret: rotated as one.
        pub fn memo(key: &str) -> Secret {
            Secret {
                key: key.to_string(),
                kind: Kind::Memo,
                generation: 1,
                epoch: None,
                since: None,
                lives: None,
                version: None,
                cells: Vec::new(),
                values: BTreeSet::new(),
            }
        }

        /// How a new value lands, at worst: none when nothing reads it.
        pub fn lands(&self) -> Option<Lands> {
            self.cells.iter().map(|c| c.lands).max()
        }
    }

    /// How long ago `since` (RFC 3339) was, as a listing says it: `3d`,
    /// `5h`, `40m`, `now`.
    pub fn age(since: &str) -> String {
        let (Ok(then), Ok(now)) = (
            since.parse::<jiff::Timestamp>(),
            crate::memo::now().parse::<jiff::Timestamp>(),
        ) else {
            return String::new();
        };
        match now.as_second() - then.as_second() {
            s if s < 60 => "now".into(),
            s if s < 3600 => format!("{}m", s / 60),
            s if s < 86400 => format!("{}h", s / 3600),
            s => format!("{}d", s / 86400),
        }
    }

    /// The paths inside `v` (`.a.b`, `[0]`) where `secret` is or is in a
    /// string; "" for `v` itself.
    fn reaches(v: &Value, secret: &Value, at: String, out: &mut Vec<String>) {
        let hit = match (v, secret) {
            (Value::Str(s), Value::Str(x)) => !x.is_empty() && s.contains(x.as_str()),
            _ => v == secret,
        };
        if hit {
            out.push(at);
            return;
        }
        match v {
            Value::Obj(m) => {
                for (k, x) in m {
                    reaches(x, secret, crate::ir::path_join(&at, k), out);
                }
            }
            Value::List(xs) => {
                for (i, x) in xs.iter().enumerate() {
                    reaches(x, secret, format!("{at}[{i}]"), out);
                }
            }
            _ => {}
        }
    }

    fn text(t: &Term) -> Option<&str> {
        match t {
            Term::Val(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    /// The secrets of a run: `facts` its evaluation's, `redact` its
    /// labels, `st` its state, `born` when its master was first applied,
    /// `current` its master's epoch.
    pub fn of(
        facts: &BTreeSet<Atom>,
        redact: &crate::query::Redactor,
        st: &crate::state::State,
        schema: &crate::schema::Schema,
        born: Option<&str>,
        current: u32,
    ) -> Vec<Secret> {
        let mut out: BTreeMap<String, Secret> = BTreeMap::new();
        fn row<'a>(out: &'a mut BTreeMap<String, Secret>, key: &str, kind: Kind) -> &'a mut Secret {
            let e = out.entry(key.to_string()).or_insert_with(|| Secret {
                key: key.to_string(),
                kind,
                generation: 1,
                epoch: None,
                since: None,
                lives: None,
                version: None,
                cells: Vec::new(),
                values: BTreeSet::new(),
            });
            // A memo of a random candidate is the memo: its value is kept.
            if (e.kind, kind) == (Kind::Random, Kind::Memo) {
                e.kind = Kind::Memo;
            }
            e
        }
        for (k, c) in crate::functions::random::calls() {
            if c.secret {
                row(&mut out, &k, Kind::Random).values.extend(c.values);
            }
        }
        let secret = |v: &Value| redact.labelled().any(|(x, _)| x == v);
        for a in facts.iter().filter(|a| a.pred == crate::memo::FIRST) {
            if let [k, _, Term::Val(v)] = a.args.as_slice()
                && let Some(k) = text(k)
                && (secret(v) || st.memo.get(k).is_some_and(|m| !m.sealed.is_empty()))
            {
                row(&mut out, k, Kind::Memo).values.insert(v.clone());
            }
        }
        for (k, m) in &st.memo {
            if !m.sealed.is_empty() {
                row(&mut out, k, Kind::Memo);
            }
        }
        for k in st.secrets.keys() {
            row(&mut out, k, Kind::Random);
        }
        // What the operator gives: a secret input (`secret_cell(input, ..)`)
        // and a secret environment variable.
        let inputs: BTreeSet<(&Value, &Value)> = facts
            .iter()
            .filter(|a| a.pred == crate::transform::SECRET_CELL)
            .filter_map(|a| match a.args.as_slice() {
                [Term::Val(t), Term::Val(s), Term::Val(k)]
                    if t.as_str() == Some(crate::modules::INPUT) =>
                {
                    Some((s, k))
                }
                _ => None,
            })
            .collect();
        for a in facts.iter().filter(|a| a.pred == "attr") {
            if let [
                Term::Val(t),
                Term::Val(scope),
                Term::Val(k),
                Term::Val(v),
                ..,
            ] = a.args.as_slice()
                && t.as_str() == Some(crate::modules::INPUT)
                && inputs.contains(&(scope, k))
                && let Some(name) = k.as_str()
            {
                let s = row(&mut out, name, Kind::Given);
                s.lives.get_or_insert_with(|| {
                    format!("the input {name}: --set, an input file or a settings block")
                });
                s.values.insert(v.clone());
            }
        }
        for a in facts
            .iter()
            .filter(|a| a.pred == crate::syntax::resolve::ENV_VAR)
        {
            if let [Term::Val(Value::Str(name)), Term::Val(v)] = a.args.as_slice() {
                let s = row(&mut out, name, Kind::Given);
                s.lives
                    .get_or_insert_with(|| format!("the environment variable {name}"));
                s.values.insert(v.clone());
            }
        }
        // What a secret manager keeps (R-172): a location read into a
        // secret cell at the version its manager answered, which pins
        // where the read's row is (`files::pinned`).
        for a in facts.iter().filter(|a| crate::tables::is_document(&a.pred)) {
            if let [Term::Val(loc), Term::Val(Value::Str(at)), Term::Val(v)] = a.args.as_slice()
                && secret(v)
                && let Some(version) = crate::files::version_of(at)
            {
                let loc = loc
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::partition::fmt_bare(loc));
                let s = row(&mut out, &loc, Kind::Managed);
                s.lives
                    .get_or_insert_with(|| format!("{loc}, its secret manager"));
                s.version = Some(version);
                s.values.insert(v.clone());
            }
        }
        let known: BTreeSet<Value> = out.values().flat_map(|s| s.values.clone()).collect();
        // The rest of the run's secrets, by their labels.
        let mut held_at: BTreeSet<(String, String, String)> = BTreeSet::new();
        for (v, label) in redact.labelled() {
            if known.contains(v) {
                continue;
            }
            let (kind, key, lives) = if let Some((t, a, p)) = crate::value::null_parts(label) {
                match t.as_str() {
                    crate::modules::LET | crate::modules::INPUT => continue,
                    t if schema
                        .attr(t, &crate::provider::norm_path(&p))
                        .is_some_and(|x| x.has("computed") || x.has("optional_computed")) =>
                    {
                        held_at.insert((t.to_string(), a.clone(), p.clone()));
                        (
                            Kind::Held,
                            label.to_string(),
                            format!("{t} {a}, its provider"),
                        )
                    }
                    t if schema.attr(t, &crate::provider::norm_path(&p)).is_some() => continue,
                    _ => (Kind::Held, label.to_string(), format!("{t} {a}")),
                }
            } else {
                (Kind::Held, label.to_string(), label.to_string())
            };
            let s = row(&mut out, &key, kind);
            s.lives.get_or_insert(lives);
            s.values.insert(v.clone());
        }
        // `lifecycle(r, "prevent_destroy")`.
        let prevent: BTreeSet<crate::ir::Address> = facts
            .iter()
            .filter(|a| a.pred == "lifecycle")
            .filter_map(|a| match a.args.as_slice() {
                [r, w] if text(w) == Some("prevent_destroy") => crate::zset::referenced(r),
                _ => None,
            })
            .collect();
        // Each resource attribute a secret reaches.
        for a in facts.iter().filter(|a| a.pred == "attr") {
            let [
                Term::Val(tv),
                Term::Val(addr),
                Term::Val(pv),
                Term::Val(v),
                ..,
            ] = a.args.as_slice()
            else {
                continue;
            };
            let (Some(t), Some(p)) = (tv.as_str(), pv.as_str()) else {
                continue;
            };
            if [crate::modules::LET, crate::modules::INPUT].contains(&t) {
                continue;
            }
            let name = crate::partition::fmt_bare(addr);
            if held_at.contains(&(t.to_string(), name.clone(), p.to_string())) {
                continue;
            }
            for s in out.values_mut() {
                for x in &s.values {
                    let mut at = Vec::new();
                    reaches(v, x, String::new(), &mut at);
                    for sub in at {
                        let path = match sub.as_str() {
                            "" => p.to_string(),
                            s if s.starts_with('[') => format!("{p}{s}"),
                            s => crate::types::dotted(p, s),
                        };
                        let addr = crate::ir::Address {
                            typ: t.to_string(),
                            name: name.clone(),
                        };
                        let lands = match schema.forces_new(t, &crate::provider::norm_path(&path)) {
                            false => Lands::Update,
                            true if prevent.contains(&addr) => Lands::Refused,
                            true => Lands::Replace,
                        };
                        let c = Cell {
                            typ: t.to_string(),
                            name: name.clone(),
                            path,
                            lands,
                        };
                        if !s.cells.contains(&c) {
                            s.cells.push(c);
                        }
                    }
                }
            }
        }
        for s in out.values_mut() {
            s.cells.sort();
            s.epoch = match s.kind {
                Kind::Random => Some(
                    st.secrets
                        .get(&s.key)
                        .and_then(|r| r.epoch)
                        .unwrap_or(current),
                ),
                Kind::Memo => st.memo.get(&s.key).map(|m| m.epoch.unwrap_or(1)),
                _ => None,
            };
            if let Some(r) = st.secrets.get(&s.key).filter(|r| !r.rotated_at.is_empty()) {
                s.generation = r.generation;
                s.since = Some(r.rotated_at.clone());
            } else {
                s.since = match s.kind {
                    Kind::Memo => st.memo.get(&s.key).map(|m| m.kept.clone()),
                    Kind::Random => born.map(str::to_string),
                    _ => None,
                };
            }
        }
        let mut v: Vec<Secret> = out.into_values().collect();
        v.sort_by(|a, b| (a.kind, &a.key).cmp(&(b.kind, &b.key)));
        v
    }
}

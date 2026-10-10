//! A small lint pass over the lowered program: warn about an
//! `input(...)` key, from `--set` or a fact, that no rule body reads,
//! about a stack key input that defaults to production, and about a body
//! of more than [`LONG_BODY`] literals in the source (R-10).
//!
//! Its F13 half is a compile error now: an instance that sets a key its
//! module does not declare as an input names the key (`modules`).
//!
//! And, over an evaluation, the collision lint of a keyed stack
//! ([`key_collisions`]): a resource whose name-like attribute's value does
//! not depend on any key input ([`from_key`]) is named the same by every
//! deployment of the stack. `dform stack rekey` lists the other side
//! ([`key_named`]): the resources whose names do depend on the key, and so
//! change when it does.

use crate::address::Address;
use crate::ast::{Atom, Lit, Program, RuleStmt, Span, Stmt, Term};
use crate::circuit::{self, Circuit, Leaf, NodeId, View};
use crate::engine::EvalResult;
use crate::schema::Schema;
use crate::spell;
use crate::transform;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// A name-like attribute of a resource in an evaluation: `path` (dotted,
/// maybe below the attribute `fact` holds) names the object in the cloud.
#[derive(Debug, Clone)]
pub struct Named {
    pub addr: Address,
    pub path: String,
    pub value: Value,
    /// The `attr(T, A, P, V)` fact the value is in.
    pub fact: circuit::Fact,
}

impl Named {
    fn text(&self) -> String {
        format!(
            "{} = {}",
            crate::report::attribute(&self.addr, &self.path),
            spell::value(&self.value)
        )
    }
}

/// Every name-like attribute (`Schema::name_like_paths`) of every resource
/// the evaluation wants, whose value is known (a null is minted by the
/// cloud, not written).
pub fn name_like(facts: &BTreeSet<Atom>, schema: &Schema) -> Vec<Named> {
    let str_of = |t: &Term| match t {
        Term::Val(Value::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let wanted: BTreeSet<(String, String)> = facts
        .iter()
        .filter(|a| a.pred == "want" && a.args.len() == 2)
        .filter_map(|a| Some((str_of(&a.args[0])?, str_of(&a.args[1])?)))
        .collect();
    let mut out = Vec::new();
    for a in facts
        .iter()
        .filter(|a| a.pred == "attr" && a.args.len() == 4)
    {
        let (Some(t), Some(n), Some(p), Term::Val(v)) = (
            str_of(&a.args[0]),
            str_of(&a.args[1]),
            str_of(&a.args[2]),
            &a.args[3],
        ) else {
            continue;
        };
        if !wanted.contains(&(t.clone(), n.clone())) {
            continue;
        }
        for path in schema.name_like_paths(&t) {
            // The attribute is the path's first segment; below it, walk
            // the object.
            let value = if path == p {
                Some(v.clone())
            } else if let Some(rest) = path.strip_prefix(&format!("{p}.")) {
                rest.split('.').try_fold(v.clone(), |v, seg| match v {
                    Value::Obj(m) => m.get(seg).cloned(),
                    _ => None,
                })
            } else {
                None
            };
            match value {
                None | Some(Value::Null { .. }) => {}
                Some(value) => out.push(Named {
                    addr: Address {
                        typ: t.clone(),
                        name: n.clone(),
                    },
                    path,
                    value,
                    fact: crate::engine::circuit_fact(a),
                }),
            }
        }
    }
    out
}

/// Does the value of the name-like attribute `n` depend on one of the
/// stack's inputs `keys` (`attr("input", "", k, _)`)? Value dependence: a
/// key input's value flows into the value term of a rule that derived it,
/// through bindings, interpolation, calls, lookups or refs. A read that
/// only gates a rule, or feeds another field, does not count.
///
/// Within a rule the flow is static: from the variables of the head's
/// term, across `=` and every positive literal sharing one (a lookup's
/// key selects its value). Between rules it follows the circuit: each such
/// literal's matched fact, at the columns the flow reaches, and each ref's
/// attribute.
fn from_key(res: &EvalResult, n: &Named, keys: &[String]) -> bool {
    let Some(start) = res.circuit.fact_id(&n.fact) else {
        return false;
    };
    let below: Vec<String> = match n.fact.args.get(2) {
        Some(Value::Str(p)) => n
            .path
            .strip_prefix(&format!("{p}."))
            .map(|rest| rest.split('.').map(str::to_string).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut flow = Flow {
        res,
        keys,
        seen: BTreeSet::new(),
    };
    flow.fact(start, &BTreeSet::from([3]), &below)
}

struct Flow<'a> {
    res: &'a EvalResult,
    keys: &'a [String],
    seen: BTreeSet<(NodeId, BTreeSet<usize>)>,
}

impl Flow<'_> {
    fn is_key(&self, f: &circuit::Fact) -> bool {
        f.pred == "attr"
            && matches!(f.args.as_slice(),
                [Value::Str(t), Value::Str(scope), Value::Str(k), _]
                    if t == crate::modules::INPUT && scope.is_empty() && self.keys.contains(k))
    }

    /// Do columns `cols` of the fact at `id` depend on a key? `below`: the
    /// path inside the value column the question is about.
    fn fact(&mut self, id: NodeId, cols: &BTreeSet<usize>, below: &[String]) -> bool {
        if !self.seen.insert((id, cols.clone())) {
            return false;
        }
        let View::Fact { fact, alts, .. } = self.res.circuit.view(id) else {
            return false;
        };
        if self.is_key(fact) && cols.contains(&3) {
            return true;
        }
        // A ref's value is its attribute's.
        let mut refs = Vec::new();
        for &c in cols {
            if let Some(v) = fact.args.get(c) {
                refs_in(v, &mut refs);
            }
        }
        for (t, a, p) in refs {
            let top = p.split('.').next().unwrap_or(&p);
            let target = self.res.facts.iter().find(|f| {
                f.pred == "attr"
                    && matches!(f.args.as_slice(),
                        [Term::Val(Value::Str(ft)), Term::Val(Value::Str(fa)), Term::Val(Value::Str(fp)), _]
                            if *ft == t && *fa == a && fp == top)
            });
            if let Some(id) =
                target.and_then(|f| self.res.circuit.fact_id(&crate::engine::circuit_fact(f)))
                && self.fact(id, &BTreeSet::from([3]), &[])
            {
                return true;
            }
        }
        alts.iter().any(|&alt| self.firing(alt, cols, below))
    }

    fn firing(&mut self, alt: NodeId, cols: &BTreeSet<usize>, below: &[String]) -> bool {
        let View::Times { children, bindings } = self.res.circuit.view(alt) else {
            return false;
        };
        let rule = children
            .iter()
            .find_map(|&c| match self.res.circuit.view(c) {
                View::Leaf(Leaf::Rule { id }) => Some(id.clone()),
                _ => None,
            });
        let Some(rule) = rule else {
            // A fact the program states, or one given: no key flows in.
            return false;
        };
        let facts: Vec<NodeId> = children
            .iter()
            .copied()
            .filter(|&c| matches!(self.res.circuit.view(c), View::Fact { .. }))
            .collect();
        if rule == crate::engine::ATTR_SIGMA {
            // attr(T, A, P, V) over its contributions arg(T, A, P, V, R).
            return facts.iter().any(|&f| self.fact(f, cols, below));
        }
        let Some(r) = rule
            .strip_prefix('r')
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| self.res.rules.get(i))
        else {
            // Another aggregate: every contribution, every column.
            return facts.iter().any(|&f| self.fact(f, cols, &[]));
        };
        let terms: Vec<&Term> = cols
            .iter()
            .filter_map(|&c| r.head.args.get(c))
            .map(|t| narrow(t, below))
            .collect();
        let sources = flows_into(r, &terms);
        let bound = |t: &Term| match t {
            Term::Val(v) => Some(v.clone()),
            Term::Var(x) => bindings
                .iter()
                .find(|(k, _)| k == x)
                .map(|(_, v)| v.clone()),
            _ => None,
        };
        for (a, cols) in sources {
            let want: Vec<Option<Value>> = a.args.iter().map(bound).collect();
            for &f in &facts {
                let View::Fact { fact, .. } = self.res.circuit.view(f) else {
                    continue;
                };
                let matches = fact.pred == a.pred
                    && fact.args.len() == want.len()
                    && fact
                        .args
                        .iter()
                        .zip(&want)
                        .all(|(v, w)| w.as_ref().is_none_or(|w| w == v));
                if matches && self.fact(f, &cols, &[]) {
                    return true;
                }
            }
        }
        false
    }
}

/// The part of `t` at `path`, as far as `t` spells it out.
fn narrow<'t>(t: &'t Term, path: &[String]) -> &'t Term {
    match (t, path.split_first()) {
        (Term::Obj(m), Some((k, rest))) => m.get(k).map_or(t, |v| narrow(v, rest)),
        _ => t,
    }
}

/// A positive body literal, and the columns of it a flow reaches.
type Source<'r> = (&'r Atom, BTreeSet<usize>);

/// What flows into `terms` in rule `r`: the positive body literals, each
/// with the columns the flow reaches.
fn flows_into<'r>(r: &'r RuleStmt, terms: &[&'r Term]) -> Vec<Source<'r>> {
    let mut vars = BTreeSet::new();
    for t in terms {
        term_vars(t, &mut vars);
    }
    loop {
        let before = vars.len();
        for l in &r.body {
            let mut here = BTreeSet::new();
            match l {
                Lit::Eq(a, b) => {
                    term_vars(a, &mut here);
                    term_vars(b, &mut here);
                }
                Lit::Pos(a) => a.args.iter().for_each(|t| term_vars(t, &mut here)),
                _ => continue,
            }
            if !here.is_disjoint(&vars) {
                vars.extend(here);
            }
        }
        if vars.len() == before {
            break;
        }
    }
    r.body
        .iter()
        .filter_map(|l| match l {
            Lit::Pos(a) => Some(a),
            _ => None,
        })
        .filter_map(|a| {
            let cols: BTreeSet<usize> = a
                .args
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    let mut here = BTreeSet::new();
                    term_vars(t, &mut here);
                    !here.is_disjoint(&vars)
                })
                .map(|(i, _)| i)
                .collect();
            (!cols.is_empty()).then_some((a, cols))
        })
        .collect()
}

fn term_vars<'t>(t: &'t Term, out: &mut BTreeSet<&'t str>) {
    t.for_each_var(&mut |v| {
        out.insert(v);
    });
}

/// The `(type, address, path)` of every ref in `v`.
fn refs_in(v: &Value, out: &mut Vec<(String, String, String)>) {
    v.for_each_scalar(&mut |x| {
        if let Value::Ref { typ, name, attr } = x {
            out.push((typ.clone(), name.clone(), attr.clone()));
        }
    });
}

/// Where the value of an `attr` fact is written: the place of the rule or
/// fact behind its first contribution.
fn owner(circuit: &Circuit, fact: &circuit::Fact) -> Option<String> {
    let mut id = circuit.fact_id(fact)?;
    // attr <- Σ <- arg <- the rule firing (or the fact) that wrote it.
    for _ in 0..8 {
        match circuit.view(id) {
            View::Fact { alts, .. } => id = *alts.first()?,
            View::Times { children, .. } => {
                for &c in children {
                    match circuit.view(c) {
                        View::Leaf(circuit::Leaf::Rule { id: r }) if !r.starts_with('Σ') => {
                            return circuit.rule_at(r).map(str::to_string);
                        }
                        // `file:line:col (arg)`: the place.
                        View::Leaf(circuit::Leaf::Base { span }) => {
                            return Some(span.split(" (").next().unwrap_or(span).to_string());
                        }
                        _ => {}
                    }
                }
                id = *children
                    .iter()
                    .find(|&&c| matches!(circuit.view(c), View::Fact { .. }))?;
            }
            View::Leaf(_) | View::Dead => return None,
        }
    }
    None
}

/// One finding of the collision lint: its message, and the `attr` facts
/// of the names it is about.
#[derive(Debug, Clone)]
pub struct Collision {
    pub text: String,
    pub facts: Vec<circuit::Fact>,
}

/// The collision lint of a keyed stack: the name-like attributes whose
/// values do not depend on any key input, one finding per provider (R-117)
/// listing each at the place it is written. `stack` is the deployment's
/// name; `provider` names the provider that serves a type, as the
/// program's provider's `use` block does when it configures one. A provider
/// whose configuration differs per deployment ([`configured_per_key`])
/// reaches a per-deployment account, and its names do not collide.
pub fn key_collisions(
    res: &EvalResult,
    schema: &Schema,
    keys: &[String],
    stack: &str,
    provider: impl Fn(&str) -> String,
) -> Vec<Collision> {
    let mut isolated: BTreeMap<String, bool> = BTreeMap::new();
    let mut groups: BTreeMap<String, Vec<(String, circuit::Fact)>> = BTreeMap::new();
    for n in name_like(&res.facts, schema) {
        if from_key(res, &n, keys) {
            continue;
        }
        let p = provider(&n.addr.typ);
        if *isolated
            .entry(p.clone())
            .or_insert_with(|| configured_per_key(&res.rules, schema, &p, keys))
        {
            continue;
        }
        let at = owner(&res.circuit, &n.fact)
            .map(|at| format!("{at}: "))
            .unwrap_or_default();
        groups
            .entry(p)
            .or_default()
            .push((format!("{at}{}", n.text()), n.fact));
    }
    let (key, keys) = (&keys[0], keys.join(", "));
    groups
        .into_iter()
        .map(|(p, names)| {
            let text = match names.as_slice() {
                [(name, _)] => format!(
                    "{name} does not depend on the stack's key ({keys}): every deployment of \
                     {stack} gives it this name, and they collide; derive it from the key \
                     (\"...${{{key}}}\"), or say `isolated = true` on the stack when each key \
                     value deploys into its own account"
                ),
                _ => format!(
                    "provider {p}: {} names do not depend on the stack's key ({keys}): every \
                     deployment of {stack} gives each the same name, and they collide: {}; \
                     derive them from the key (\"...${{{key}}}\"), configure provider {p} from \
                     the key, or say `isolated = true` on the stack when each key value deploys \
                     into its own account",
                    names.len(),
                    names
                        .iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
            };
            Collision {
                text,
                facts: names.into_iter().map(|(_, f)| f).collect(),
            }
        })
        .collect()
}

/// Whether the configuration of the provider `name` differs per
/// deployment of a keyed stack (R-117): its `provider_config(name, ..)`
/// reads, through any chain of rules, a key input, or a computed attribute
/// of a resource, which the cloud gives each deployment's own object (`k8s
/// { kubeconfig = k3s.kubeconfig }`, read back from a server the deployment
/// creates). Its resources then live in a per-deployment account.
fn configured_per_key(rules: &[RuleStmt], schema: &Schema, name: &str, keys: &[String]) -> bool {
    fn text(t: &Term) -> Option<&str> {
        match t {
            Term::Val(Value::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }
    // Two columns that may hold the same value.
    let meet = |a: &Term, b: &Term| match (text(a), text(b)) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    };
    let mut todo: Vec<usize> = rules
        .iter()
        .enumerate()
        .filter(|(_, r)| r.head.pred == "provider_config")
        .filter(|(_, r)| r.head.args.first().and_then(text) == Some(name))
        .map(|(i, _)| i)
        .collect();
    let mut seen: BTreeSet<usize> = todo.iter().copied().collect();
    while let Some(i) = todo.pop() {
        for (k, l) in rules[i].body.iter().enumerate() {
            let (Lit::Pos(a) | Lit::Not(a)) = l else {
                continue;
            };
            let cell = (a.pred == "attr" || a.pred == transform::ATTR_BASE) && a.args.len() == 4;
            if cell {
                let (t, p) = (text(&a.args[0]), text(&a.args[2]));
                if t == Some(crate::modules::INPUT)
                    && text(&a.args[1]) == Some("")
                    && p.is_some_and(|p| keys.iter().any(|k| k == p))
                {
                    return true;
                }
                let read = p.map(|p| read_paths(&rules[i], k, &a.args[3], p));
                let under = |q: &str| {
                    read.as_ref().is_none_or(|ps| {
                        ps.iter().any(|p| {
                            q == p
                                || q.strip_prefix(p.as_str())
                                    .is_some_and(|r| r.starts_with('.') || r.starts_with('['))
                                || p.strip_prefix(q).is_some_and(|r| r.starts_with('.'))
                        })
                    })
                };
                if schema
                    .computed
                    .keys()
                    .any(|(ct, cp)| t.is_none_or(|t| t == ct) && under(cp))
                {
                    return true;
                }
            }
            for (j, r) in rules.iter().enumerate() {
                let h = &r.head;
                let writes = if cell {
                    h.pred == "arg"
                        && h.args.len() == 5
                        && (0..3).all(|c| meet(&h.args[c], &a.args[c]))
                } else {
                    h.pred == a.pred && h.args.len() == a.args.len()
                };
                if writes && seen.insert(j) {
                    todo.push(j);
                }
            }
        }
    }
    false
}

/// The paths rule `r` reads of the attribute `p` its body literal `at`
/// binds to `v`: each `__path(V, "rest")` it takes of it, `p.rest`, or `p`
/// when it uses the value whole.
fn read_paths(r: &RuleStmt, at: usize, v: &Term, p: &str) -> Vec<String> {
    let Term::Var(v) = v else {
        return vec![p.to_string()];
    };
    fn walk(t: &Term, v: &str, p: &str, out: &mut Vec<String>, whole: &mut bool) {
        match t {
            Term::Var(x) if x == v => *whole = true,
            Term::Func { name, args } if name == "__path" => match args.as_slice() {
                [Term::Var(x), Term::Val(Value::Str(rest))] if x == v => {
                    out.push(format!("{p}.{rest}"))
                }
                _ => args.iter().for_each(|a| walk(a, v, p, out, whole)),
            },
            Term::Func { args, .. } | Term::List(args) => {
                args.iter().for_each(|a| walk(a, v, p, out, whole))
            }
            Term::Obj(m) => m.values().for_each(|a| walk(a, v, p, out, whole)),
            Term::Val(_) | Term::Wildcard | Term::Var(_) | Term::ListComp { .. } => {}
        }
    }
    let mut terms: Vec<&Term> = r.head.args.iter().collect();
    for (k, l) in r.body.iter().enumerate() {
        match l {
            _ if k == at => {}
            Lit::Pos(a) | Lit::Not(a) => terms.extend(&a.args),
            Lit::Eq(x, y)
            | Lit::Neq(x, y)
            | Lit::Gt(x, y)
            | Lit::Ge(x, y)
            | Lit::Lt(x, y)
            | Lit::Le(x, y) => terms.extend([x, y]),
        }
    }
    let (mut out, mut whole) = (Vec::new(), false);
    for t in terms {
        walk(t, v, p, &mut out, &mut whole);
    }
    if whole || out.is_empty() {
        out.push(p.to_string());
    }
    out
}

/// The name-like attributes that depend on a key input: what a new key
/// value renames, usually a replace. One line per attribute.
pub fn key_named(res: &EvalResult, schema: &Schema, keys: &[String]) -> Vec<String> {
    name_like(&res.facts, schema)
        .into_iter()
        .filter(|n| from_key(res, n, keys))
        .map(|n| n.text())
        .collect()
}

/// Run the lint over `program` (which is lowered internally) plus the set of
/// `--set`/`--data` keys supplied on the command line. Returns one message
/// per unread key; empty means clean.
///
/// Lowering errors are swallowed here (not this pass's job to report; the
/// real evaluation will surface them) -- an empty list is returned instead.
pub fn lint(program: &Program, cli_keys: &[String]) -> Vec<String> {
    let mut out = production_defaults(program);
    out.extend(long_bodies(program));
    let Ok(lowered) = transform::lower(program) else {
        return out;
    };
    out.extend(lint_lowered(&lowered.program, cli_keys));
    out.extend(guard_lints(program, &lowered.program));
    out
}

/// The values each `enum` or `bool` input of the stack takes: the space a
/// clause over them is decided in at compile time (R-104), as `dform test`
/// enumerates it.
pub fn enum_inputs(program: &Program) -> BTreeMap<String, Vec<Value>> {
    let mut out = BTreeMap::new();
    for s in &program.statements {
        let Stmt::Input(i) = s else { continue };
        let values = match &i.ty {
            crate::ast::TypeExpr::Name(n) if n == "bool" => {
                vec![Value::Bool(false), Value::Bool(true)]
            }
            t => match crate::types::members(t) {
                Some(ms) => ms.into_iter().map(Value::Str).collect(),
                None => continue,
            },
        };
        out.insert(i.name.clone(), values);
    }
    out
}

/// Whether a clause holds where the stack's enum inputs have the values
/// `at`, when it reads nothing else: `cloud("aws")` (`cloud == "aws"`),
/// `cloud(C), C != "gcp"`, their negations. `None`: it reads something
/// else, so only the evaluation decides it.
pub fn guard_holds(body: &[Lit], at: &BTreeMap<String, Value>) -> Option<bool> {
    let mut vars: BTreeMap<&str, &Value> = BTreeMap::new();
    let mut holds = true;
    // The readers bind first, whatever the order.
    for l in body {
        let (Lit::Pos(a) | Lit::Not(a)) = l else {
            continue;
        };
        let v = at.get(&a.pred)?;
        let [t] = a.args.as_slice() else {
            return None;
        };
        let matched = match t {
            Term::Val(x) => x == v,
            Term::Var(x) if matches!(l, Lit::Pos(_)) => match vars.insert(x, v) {
                Some(prev) => prev == v,
                None => true,
            },
            Term::Wildcard => true,
            _ => return None,
        };
        holds &= matched == matches!(l, Lit::Pos(_));
    }
    let val = |t: &Term| -> Option<Value> {
        match t {
            Term::Val(v) => Some(v.clone()),
            Term::Var(x) => vars.get(x.as_str()).map(|v| (*v).clone()),
            _ => None,
        }
    };
    for l in body {
        match l {
            Lit::Eq(a, b) => holds &= val(a)? == val(b)?,
            Lit::Neq(a, b) => holds &= val(a)? != val(b)?,
            Lit::Pos(_) | Lit::Not(_) => {}
            _ => return None,
        }
    }
    Some(holds)
}

/// Every combination of the values of `inputs`, the first slowest.
pub fn combinations(inputs: &[(&String, &Vec<Value>)]) -> Vec<BTreeMap<String, Value>> {
    let mut out = vec![BTreeMap::new()];
    for (k, vs) in inputs {
        out = out
            .into_iter()
            .flat_map(|c| {
                vs.iter().map(move |v| {
                    let mut c = c.clone();
                    c.insert((*k).clone(), v.clone());
                    c
                })
            })
            .collect();
    }
    out
}

/// `cloud == "aws", env == "dev"`.
pub fn combination_text(c: &BTreeMap<String, Value>) -> String {
    c.iter()
        .map(|(k, v)| format!("{k} == {}", spell::value(v)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The enum inputs a clause reads.
pub fn guard_reads<'a>(
    body: &[Lit],
    space: &'a BTreeMap<String, Vec<Value>>,
) -> Vec<(&'a String, &'a Vec<Value>)> {
    let preds: BTreeSet<&str> = body
        .iter()
        .filter_map(|l| match l {
            Lit::Pos(a) | Lit::Not(a) => Some(a.pred.as_str()),
            _ => None,
        })
        .collect();
    space
        .iter()
        .filter(|(k, _)| preds.contains(k.as_str()))
        .collect()
}

/// The two lints over a name declared several times, each under a clause
/// over enum inputs (R-104), decided over the enum product as `dform test`
/// enumerates it: a combination where no declaration holds, and one where
/// two do, each naming the sites. A clause that reads anything else is
/// the evaluation's to decide.
fn guard_lints(program: &Program, lowered: &Program) -> Vec<String> {
    let space = enum_inputs(program);
    let mut groups: BTreeMap<(String, String), Vec<(&RuleStmt, Span)>> = BTreeMap::new();
    for s in &lowered.statements {
        let Stmt::Rule(r) = s else { continue };
        let pred = r.head.pred.rsplit("::").next().unwrap_or(&r.head.pred);
        if pred != crate::modules::DECLARED {
            continue;
        }
        let [Term::Val(Value::Str(name)), _] = r.head.args.as_slice() else {
            continue;
        };
        groups
            .entry((r.head.pred.clone(), name.clone()))
            .or_default()
            .push((r, r.head.span));
    }
    let mut out = Vec::new();
    for ((_, name), decls) in groups.into_iter().filter(|(_, d)| d.len() > 1) {
        let mut reads: Vec<(&String, &Vec<Value>)> = Vec::new();
        for (r, _) in &decls {
            for x in guard_reads(&r.body, &space) {
                if !reads.contains(&x) {
                    reads.push(x);
                }
            }
        }
        let site = |span: Span| crate::diag::at(span).unwrap_or_default();
        let (mut none, mut both) = (None, None);
        for c in combinations(&reads) {
            let holds: Option<Vec<Span>> = decls
                .iter()
                .map(|(r, span)| Some(guard_holds(&r.body, &c)?.then_some(*span)))
                .collect::<Option<Vec<_>>>()
                .map(|v| v.into_iter().flatten().collect());
            let Some(holds) = holds else {
                break;
            };
            match holds.as_slice() {
                [] if none.is_none() => none = Some(combination_text(&c)),
                [a, b, ..] if both.is_none() => both = Some((combination_text(&c), *a, *b)),
                _ => {}
            }
        }
        let sites: Vec<String> = decls.iter().map(|(_, s)| site(*s)).collect();
        if let Some(c) = none {
            out.push(format!(
                "`{name}` has no declaration when {c} (declared at {})",
                sites.join(", ")
            ));
        }
        if let Some((c, a, b)) = both {
            out.push(format!(
                "`{name}`: the clauses of two declarations both hold when {c}: {} and {}",
                site(a),
                site(b)
            ));
        }
    }
    out
}

/// A key input whose default is `"prod"` or `"production"`: a plan or
/// apply that names no value of the key is of the production deployment.
fn production_defaults(program: &Program) -> Vec<String> {
    let mut out = Vec::new();
    for s in &program.statements {
        let Stmt::Input(i) = s else { continue };
        let Some(Term::Val(Value::Str(v))) = &i.default else {
            continue;
        };
        let production = ["prod", "production"]
            .iter()
            .any(|p| v.eq_ignore_ascii_case(p));
        if !production || !i.key {
            continue;
        }
        let at = crate::diag::at(i.span)
            .map(|a| format!("{a}: "))
            .unwrap_or_default();
        // A stack is named after its file.
        let stack = crate::diag::location(i.span)
            .map(|(f, _, _)| crate::state::stack_name(std::path::Path::new(&f)))
            .unwrap_or_else(|| "the stack".into());
        out.push(format!(
            "{at}key {k} of stack {stack} defaults to \"{v}\": a plan or apply that names no \
             {k} is of {stack}[{k}={v}]; default to another value, or give none",
            k = i.name
        ));
    }
    out
}

/// A body of more literals than this is a lint warning (R-10): name the
/// join. `fmt` breaks one past [`crate::fmt::INLINE_LITERALS`].
pub const LONG_BODY: usize = 5;

/// Every `where` body of more than [`LONG_BODY`] literals in the files the
/// program was read from: a join that long reads better as a relation
/// named for what it finds, as the tour's `network_of`.
fn long_bodies(program: &Program) -> Vec<String> {
    use crate::syntax::SyntaxKind::{BODY, COMPREHENSION, LIT_NOT_BLOCK, REFINEMENT};
    let mut files = BTreeSet::new();
    source_files(&program.statements, &mut files);
    let mut out = Vec::new();
    for file in files {
        let at = |start: u32| crate::ast::Span {
            file,
            start,
            end: start,
            origin: 0,
        };
        let Some((_, text)) = crate::diag::source_of(at(0)) else {
            continue;
        };
        let tree = crate::syntax::parser::parse(&text).syntax();
        for b in tree.descendants().filter(|n| n.kind() == BODY) {
            let parent = b.parent().map(|p| p.kind());
            if matches!(parent, Some(REFINEMENT | COMPREHENSION | LIT_NOT_BLOCK)) {
                continue;
            }
            let n = b.children().count();
            if n <= LONG_BODY {
                continue;
            }
            let loc = crate::diag::at(at(b.text_range().start().into()))
                .map(|a| format!("{a}: "))
                .unwrap_or_default();
            out.push(format!(
                "{loc}a clause of {n} literals: name the join, a relation of the literals that \
                 find one thing (as the tour's `network_of`), and read it here"
            ));
        }
    }
    out
}

/// The source files of `stmts`' spans, modules' bodies included.
fn source_files(stmts: &[Stmt], out: &mut BTreeSet<u32>) {
    for s in crate::modules::nested(stmts) {
        let span = match s {
            Stmt::Fact(a) => a.span,
            Stmt::Rule(r) => r.head.span,
            Stmt::Resource(r) => r.span,
            Stmt::Output(o) => o.span,
            Stmt::Instance(i) | Stmt::Use(i) => i.span,
            Stmt::Module(m) => m.span,
            _ => continue,
        };
        if !span.is_none() {
            out.insert(span.file);
        }
    }
}

fn lint_lowered(program: &Program, cli_keys: &[String]) -> Vec<String> {
    let mut produced: BTreeSet<String> = cli_keys.iter().cloned().collect();
    let mut read: BTreeSet<String> = BTreeSet::new();
    for stmt in &program.statements {
        match stmt {
            Stmt::Fact(a) => produced.extend(input_key(a)),
            Stmt::Rule(r) => {
                produced.extend(input_key(&r.head));
                read.extend(r.body.iter().filter_map(read_key));
            }
            _ => {}
        }
    }
    produced
        .difference(&read)
        .map(|key| format!("input {key} is set but nothing reads it"))
        .collect()
}

fn read_key(lit: &Lit) -> Option<String> {
    match lit {
        Lit::Pos(a) | Lit::Not(a) => input_key(a),
        _ => None,
    }
}

/// The key of `input(Key, Value)` when it is a literal.
fn input_key(atom: &Atom) -> Option<String> {
    match (atom.pred.as_str(), atom.args.as_slice()) {
        ("input", [Term::Val(Value::Str(k)), _]) => Some(k.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir;
    use crate::loader;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
    }

    /// Reverted-fix check: with the F13 bug back (`vpc_cidr` instead of
    /// `vpc_net`), `examples/adopt/stacks/adopt_demo.df` derives zero resources and the
    /// evaluated fact set has no `want`/`adopt` for `net.vpc`. This test
    /// fails if that bug is reintroduced.
    #[test]
    fn adopt_demo_derives_a_vpc() {
        let root = workspace_root();
        let file = root.join("examples/adopt/stacks/adopt_demo.df");
        let program = loader::load_program(&[file]).expect("load adopt_demo.df");

        let set_env_prod = Atom {
            pred: "input".to_string(),
            args: vec![
                Term::Val(Value::Str("env".to_string())),
                Term::Val(Value::Str("prod".to_string())),
            ],
            record: None,
            span: Default::default(),
        };
        let (res, warnings) =
            crate::engine::eval(&program, &[set_env_prod]).expect("eval adopt_demo.df");
        assert!(
            warnings.is_empty(),
            "unexpected engine warnings: {warnings:?}"
        );

        let wants: Vec<_> = res
            .facts
            .iter()
            .filter(|a| a.pred == "want" || a.pred == "adopt")
            .collect();
        assert!(
            !wants.is_empty(),
            "expected at least one want/adopt fact, got none -- facts: {:?}",
            res.facts
        );

        // The specific bug (F13): the VPC itself must be among the derived
        // resources, not just some unrelated want/adopt fact.
        let resources = ir::compile_resources(res.facts.iter().cloned(), &Default::default())
            .expect("compile resources");
        assert!(
            resources.iter().any(|r| r.addr.typ == "net.vpc"),
            "expected a net.vpc resource to be derived, got: {:?}",
            resources
                .iter()
                .map(|r| (&r.addr.typ, &r.addr.name))
                .collect::<Vec<_>>()
        );
    }

    /// R-10: a body of more than five literals is a warning naming where
    /// it is; five are not.
    #[test]
    fn warns_about_a_long_body() {
        let lits = |n: usize| {
            (0..n)
                .map(|i| format!("q(x, {i})"))
                .collect::<Vec<_>>()
                .join("\n  ")
        };
        let src = format!(
            "\n\ndecl q(a, b)\nq(1, 2)\np(x) where {{\n  {}\n}}\nr(x) where {{\n  {}\n}}\n",
            lits(6),
            lits(5)
        );
        let program = crate::parser::parse_file("long.df", &src).expect("parse");
        let warnings = lint(&program, &[]);
        let long: Vec<&String> = warnings.iter().filter(|w| w.contains("literals")).collect();
        assert_eq!(long.len(), 1, "{warnings:?}");
        assert!(
            long[0].starts_with("long.df:5:12: a clause of 6 literals: name the join"),
            "{}",
            long[0]
        );
    }

    #[test]
    fn warns_about_unread_cli_input() {
        let src = r#"
            env("staging")
        "#;
        let program = crate::parser::parse_program(src).expect("parse");
        let warnings = lint(&program, &["region".to_string()]);
        assert!(
            warnings.iter().any(|w| w.contains("region")),
            "expected a warning naming the unread '--set region=...' input, got: {warnings:?}"
        );
    }
}

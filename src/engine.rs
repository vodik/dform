use crate::ast::{Atom, Constraint, Lit, Program, RuleStmt, Stmt, Term};
use crate::lattice::{self, Collapsed2, Lattice, Rank, RankedContribution, Shadowed, Witnesses};
use crate::partition::{self, Node};
use crate::transform;
use crate::value::Value;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone)]
pub struct EvalResult {
    pub facts: BTreeSet<Atom>,
    pub warnings: Vec<String>,
}

/// Predicates the attribute aggregate derives; no rule may.
const AGGREGATE_OUTPUTS: [&str; 3] = ["attr", "attr_conflict", "attr_stuck"];

/// Schema facts that choose a path's lattice. They are read by the
/// aggregate, not by rules, so they must be facts.
const LATTICE_DECLS: [&str; 2] = ["type_lattice", "type_list_key"];

pub fn eval(program: &Program, extra_facts: &[Atom]) -> Result<(EvalResult, Vec<String>)> {
    let lowered = transform::lower(program)?;
    let program = lowered.program;
    let externs = lowered.externs;
    let mut facts: BTreeSet<Atom> = BTreeSet::new();
    let mut origins = Origins::default();
    for f in extra_facts {
        facts.insert(ensure_ground(f)?);
    }

    let mut rules = Vec::new();
    let mut constraints = Vec::new();
    let mut fact_atoms = Vec::new();

    for stmt in &program.statements {
        match stmt {
            Stmt::Fact(a) => {
                let g = ensure_ground(a)?;
                origins.note(&g, partition::fmt_atom(&g));
                facts.insert(g);
                fact_atoms.push(a.clone());
            }
            Stmt::Rule(r) => rules.push(r.clone()),
            Stmt::Constraint(c) => constraints.push(c.clone()),
            _ => {
                // Lowered program should contain only facts/rules/constraints.
            }
        }
    }
    for r in &rules {
        if AGGREGATE_OUTPUTS.contains(&r.head.pred.as_str()) {
            bail!("{} is derived by the attribute aggregate; contribute with arg instead: {}", r.head.pred, partition::fmt_rule(r));
        }
        if LATTICE_DECLS.contains(&r.head.pred.as_str()) {
            bail!("{} must be a fact, not a rule: {}", r.head.pred, partition::fmt_rule(r));
        }
    }

    check_defined(&rules, &constraints, &facts, &externs)?;

    // Simulation (proposal F): the recorder needs the rule list to print
    // stuck instances; constraints are appended as deny rules.
    if crate::sim::active() {
        let mut all = rules.clone();
        for c in &constraints {
            all.push(RuleStmt {
                head: Atom { pred: "deny".into(), args: vec![Term::Val(Value::Str(c.message.clone()))], record: None },
                body: c.body.clone(),
            });
        }
        crate::sim::with(|s| s.rules = all);
    }

    // Stratified evaluation over the partition graph (E §2.6, F DR-12
    // revised). Every rule runs in the stratum of its head node.
    let mut graph_rules = rules.clone();
    graph_rules.extend(constraints.iter().map(partition::constraint_rule));
    let opts = partition::Options { externs: externs.iter().map(|e| e.pred.clone()).collect() };
    let graph = partition::build_lowered(graph_rules, &fact_atoms, &crate::schema::fake(), &opts);
    let strata = match partition::stratify(&graph) {
        partition::Verdict::Stratified { strata } => strata,
        partition::Verdict::Rejected { scc, negative_edges } => {
            bail!("{}", partition::cycle_error(&graph, &scc, &negative_edges))
        }
    };
    let rule_stratum: Vec<usize> = rules
        .iter()
        .map(|r| strata.get(&partition::head_node(&r.head)).copied().unwrap_or(0))
        .collect();
    let rule_text: Vec<String> = rules.iter().map(partition::fmt_rule).collect();
    let mut attrs = AttrAggregate::new(&strata);
    let max_stratum = rule_stratum.iter().copied().max().unwrap_or(0);
    for s in 0..=max_stratum {
        // Attribute groups whose contributors all sit below this stratum
        // are complete: collapse them before any rule here reads them.
        attrs.emit_ready(s, &mut facts, &origins)?;
        let rules_s: Vec<(usize, &RuleStmt)> =
            rules.iter().enumerate().filter(|(i, _)| rule_stratum[*i] == s).collect();
        if rules_s.is_empty() {
            continue;
        }

        let mut changed = true;
        let mut iterations = 0usize;
        while changed {
            iterations += 1;
            if iterations > 200 {
                bail!("evaluation did not converge in stratum {s}");
            }
            changed = false;

            let snapshot: Vec<Atom> = facts.iter().cloned().collect();
            let mut derived: Vec<(usize, Atom)> = Vec::new();
            for (i, r) in &rules_s {
                crate::sim::set_current(*i, &r.head);
                derived.extend(eval_rule(r, &snapshot)?.into_iter().map(|a| (*i, a)));
            }
            for (i, a) in derived {
                origins.note(&a, rule_text[i].clone());
                if facts.insert(a) {
                    changed = true;
                }
            }
        }
    }
    attrs.emit_ready(usize::MAX, &mut facts, &origins)?;
    attrs.check_complete(&facts)?;

    // Constraints are checked against the final fact set.
    let snapshot: Vec<Atom> = facts.iter().cloned().collect();
    let mut violations = Vec::new();
    for (k, c) in constraints.iter().enumerate() {
        crate::sim::set_current(
            rules.len() + k,
            &Atom { pred: "deny".into(), args: vec![Term::Val(Value::Str(c.message.clone()))], record: None },
        );
        if constraint_violated(c, &snapshot)? {
            violations.push(c.message.clone());
        }
    }

    // Policy facts: deny/warn.
    let mut warnings = Vec::new();
    for a in &snapshot {
        match a.pred.as_str() {
            "warn" => warnings.push(format_policy_fact(a)?),
            "deny" => violations.push(format_policy_fact(a)?),
            _ => {}
        }
    }

    Ok((EvalResult { facts, warnings }, violations))
}

/// E §2.6: a body predicate with no definition is a compile error. Defined
/// means: a fact or a rule head, a builtin, a compiler-owned or
/// provider-injected predicate, a fact given to this run, or `extern p/N.`.
fn check_defined(
    rules: &[RuleStmt],
    constraints: &[Constraint],
    facts: &BTreeSet<Atom>,
    externs: &BTreeSet<crate::ast::Extern>,
) -> Result<()> {
    let mut defined: BTreeSet<&str> = facts.iter().map(|a| a.pred.as_str()).collect();
    defined.extend(rules.iter().map(|r| r.head.pred.as_str()));
    defined.extend(externs.iter().map(|e| e.pred.as_str()));
    let is_defined = |p: &str| {
        defined.contains(p) || is_builtin_pred(p) || matches!(p, "member" | "enumerate") || crate::loader::is_core_pred(p)
    };
    let bodies = rules
        .iter()
        .map(|r| (&r.body, partition::fmt_rule(r)))
        .chain(constraints.iter().map(|c| (&c.body, partition::fmt_rule(&partition::constraint_rule(c)))));
    let mut errors = Vec::new();
    for (body, text) in bodies {
        for lit in body {
            let (Lit::Pos(a) | Lit::Not(a)) = lit else { continue };
            if !is_defined(&a.pred) {
                errors.push(format!("undefined predicate {}/{} in rule: {text}", a.pred, a.args.len()));
            }
        }
    }
    if !errors.is_empty() {
        bail!("{}\n(declare a predicate a provider feeds with `extern p/N.`)", errors.join("\n"));
    }
    Ok(())
}

/// Where each contribution came from: the text of every rule that derived
/// it, or "fact". The AST has no spans, so rule text is the provenance.
#[derive(Default)]
struct Origins(BTreeMap<Atom, BTreeSet<String>>);

impl Origins {
    fn note(&mut self, a: &Atom, from: String) {
        if a.pred == "arg" && a.args.len() == 5 {
            self.0.entry(a.clone()).or_default().insert(from);
        }
    }
    fn of(&self, a: &Atom) -> Vec<String> {
        self.0.get(a).map(|s| s.iter().cloned().collect()).unwrap_or_default()
    }
}

/// One attribute group `(T, A, P)`: the type, the address, the normalized path.
type GroupKey = (String, Value, String);

/// One contribution to a group: the `arg/5` fact, its rank, and its value
/// after path normalization.
type Contribution = (Atom, Rank, Value);

/// The attribute aggregate of E §2.5 inside the evaluator: `attr/4`,
/// `attr_conflict/5` and `attr_stuck/4` from the `arg/5` contributions, one
/// ranked lattice cell per group, collapsed with F's shadow-aware rule
/// (DR-9 revised). A group is collapsed once, as soon as every partition
/// node that can contribute to it is in a lower stratum; the stratifier
/// puts every reader of the group above that.
struct AttrAggregate {
    arg_nodes: Vec<(Node, usize)>,
    emitted: BTreeMap<GroupKey, Vec<Atom>>,
}

impl AttrAggregate {
    fn new(strata: &BTreeMap<Node, usize>) -> Self {
        let arg_nodes = strata.iter().filter(|(n, _)| n.pred == "arg").map(|(n, s)| (n.clone(), *s)).collect();
        AttrAggregate { arg_nodes, emitted: BTreeMap::new() }
    }

    /// The first stratum at which group `(typ, path)` is complete.
    fn ready_at(&self, typ: &str, path: &str) -> usize {
        let node = Node { pred: "arg".into(), typ: Some(typ.into()), path: Some(path.into()) };
        self.arg_nodes.iter().filter(|(n, _)| n.unifies(&node)).map(|(_, s)| s + 1).max().unwrap_or(0)
    }

    fn emit_ready(&mut self, stratum: usize, facts: &mut BTreeSet<Atom>, origins: &Origins) -> Result<()> {
        let lattices = declared_lattices(facts)?;
        let mut out = Vec::new();
        for (key, contribs) in groups(facts)? {
            if self.emitted.contains_key(&key) || self.ready_at(&key.0, &key.2) > stratum {
                continue;
            }
            let lat = lattices.get(&(key.0.clone(), key.2.clone())).cloned().unwrap_or_else(|| infer_lattice(&contribs));
            out.extend(collapse_group(&key, &contribs, &lat, origins));
            self.emitted.insert(key, contribs.into_iter().map(|(a, _, _)| a).collect());
        }
        facts.extend(out);
        Ok(())
    }

    /// Guard on the stratifier: no contribution arrived after its group was
    /// collapsed.
    fn check_complete(&self, facts: &BTreeSet<Atom>) -> Result<()> {
        for (key, contribs) in groups(facts)? {
            let now: Vec<Atom> = contribs.into_iter().map(|(a, _, _)| a).collect();
            if self.emitted.get(&key) != Some(&now) {
                bail!("internal: attribute {} {} {} gained a contribution after it was collapsed", key.0, partition::fmt_value(&key.1), key.2);
            }
        }
        Ok(())
    }
}

fn parse_rank(v: &Value) -> Option<Rank> {
    match v.as_str()? {
        "default" => Some(Rank::Default),
        transform::NORMAL => Some(Rank::Normal),
        "override" => Some(Rank::Override),
        _ => None,
    }
}

fn rank_name(r: Rank) -> &'static str {
    match r {
        Rank::Default => "default",
        Rank::Normal => transform::NORMAL,
        Rank::Override => "override",
    }
}

/// Every `arg/5` contribution, grouped by `(T, A, normalized P)`.
fn groups(facts: &BTreeSet<Atom>) -> Result<BTreeMap<GroupKey, Vec<Contribution>>> {
    let mut out: BTreeMap<GroupKey, Vec<Contribution>> = BTreeMap::new();
    for a in facts.iter().filter(|a| a.pred == "arg" && a.args.len() == 5) {
        let vals: Vec<&Value> = a
            .args
            .iter()
            .map(|t| match t {
                Term::Val(v) => Ok(v),
                _ => Err(anyhow!("internal: non-ground contribution")),
            })
            .collect::<Result<_>>()?;
        let (Some(typ), Some(path)) = (vals[0].as_str(), vals[2].as_str()) else {
            bail!("contribution {} needs a string type and path", partition::fmt_atom(a));
        };
        let Some(rank) = parse_rank(vals[4]) else {
            bail!("contribution {}: rank must be default, normal or override", partition::fmt_atom(a));
        };
        let (path, value) = transform::normalize_contribution(typ, path, Term::Val(vals[3].clone()));
        let value = eval_term(&value, &HashMap::new()).ok_or_else(|| anyhow!("internal: normalize"))?;
        out.entry((typ.to_string(), vals[1].clone(), path)).or_default().push((a.clone(), rank, value));
    }
    Ok(out)
}

/// `type_lattice(T, P, flat|map|set)` and `type_list_key(T, P, Keys)` facts.
fn declared_lattices(facts: &BTreeSet<Atom>) -> Result<BTreeMap<(String, String), Lattice>> {
    let mut out = BTreeMap::new();
    for a in facts.iter().filter(|a| LATTICE_DECLS.contains(&a.pred.as_str())) {
        let [Term::Val(Value::Str(t)), Term::Val(Value::Str(p)), Term::Val(k)] = a.args.as_slice() else {
            bail!("{}/3 expects (Type, Path, ...): {}", a.pred, partition::fmt_atom(a));
        };
        let lat = match (a.pred.as_str(), k) {
            ("type_lattice", Value::Str(k)) if k == "flat" => Lattice::Flat,
            ("type_lattice", Value::Str(k)) if k == "map" => Lattice::Map(Box::new(Lattice::Flat)),
            ("type_lattice", Value::Str(k)) if k == "set" => Lattice::Set,
            ("type_list_key", Value::List(ks)) => Lattice::Keyed {
                keys: ks.iter().map(value_to_string).collect(),
                elem: Box::new(Lattice::Map(Box::new(Lattice::Flat))),
            },
            ("type_list_key", Value::Str(k)) => {
                Lattice::Keyed { keys: vec![k.clone()], elem: Box::new(Lattice::Map(Box::new(Lattice::Flat))) }
            }
            _ => bail!("{}: unknown lattice {}", partition::fmt_atom(a), partition::fmt_value(k)),
        };
        let key = (t.clone(), p.clone());
        if out.get(&key).is_some_and(|l| *l != lat) {
            bail!("path {t} {p} declares two lattices");
        }
        out.insert(key, lat);
    }
    Ok(out)
}

/// With no declaration, a path whose contributions are all objects is a
/// Map with Flat leaves; anything else is Flat (a list is one value, and
/// two authors of different lists conflict, E DR-1).
fn infer_lattice(contribs: &[Contribution]) -> Lattice {
    if contribs.iter().all(|(_, _, v)| matches!(v, Value::Obj(_))) {
        Lattice::Map(Box::new(Lattice::Flat))
    } else {
        Lattice::Flat
    }
}

fn str_val(s: &str) -> Term {
    Term::Val(Value::Str(s.to_string()))
}

fn obj(kv: Vec<(&str, Value)>) -> Value {
    Value::Obj(kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// The collapsed cell as facts: the value, or the conflict with a `deny`
/// naming every witness, or the stuck disagreement; plus a warning per
/// shadowed disagreement at a losing rank.
fn collapse_group(key: &GroupKey, contribs: &[Contribution], lat: &Lattice, origins: &Origins) -> Vec<Atom> {
    let (typ, addr, path) = key;
    let cells: Vec<RankedContribution> =
        contribs.iter().enumerate().map(|(i, (_, r, v))| (i as u32, *r, v.clone())).collect();
    let head = |pred: &str, rest: Vec<Value>| Atom {
        pred: pred.into(),
        args: [str_val(typ), Term::Val(addr.clone()), str_val(path)].into_iter().chain(rest.into_iter().map(Term::Val)).collect(),
        record: None,
    };
    let witness = |w: u32| {
        let (a, r, v) = &contribs[w as usize];
        obj(vec![
            ("rank", Value::Str(rank_name(*r).into())),
            ("value", v.clone()),
            ("from", Value::List(origins.of(a).into_iter().map(Value::Str).collect())),
        ])
    };
    let witnesses = |ws: &Witnesses| Value::List(ws.iter().map(|w| witness(*w)).collect());
    let ctx = |extra: Vec<(&str, Value)>| {
        let mut kv = vec![("type", Value::Str(typ.clone())), ("addr", addr.clone()), ("path", Value::Str(path.clone()))];
        kv.extend(extra);
        obj(kv)
    };
    let policy = |pred: &str, msg: &str, ctx: Value| Atom {
        pred: pred.into(),
        args: vec![str_val(msg), Term::Val(ctx)],
        record: None,
    };
    let mut out = Vec::new();
    let shadowed = match lattice::lub_ranked(lat, path, &cells) {
        Collapsed2::Bottom => vec![],
        Collapsed2::Val { value, shadowed, .. } => {
            out.push(head("attr", vec![value]));
            shadowed
        }
        Collapsed2::Stuck { nulls, shadowed, .. } => {
            out.push(head("attr_stuck", vec![Value::List(nulls.into_iter().map(Value::Str).collect())]));
            shadowed
        }
        Collapsed2::Conflict { a, b, reason, witnesses: ws, shadowed, .. } => {
            let first = |w: &Witnesses| w.iter().next().map(|w| witness(*w)).unwrap_or(Value::Obj(BTreeMap::new()));
            out.push(head("attr_conflict", vec![first(&a.1), first(&b.1)]));
            out.push(policy(
                "deny",
                "conflicting attribute contributions",
                ctx(vec![("reason", Value::Str(reason)), ("witnesses", witnesses(&ws))]),
            ));
            shadowed
        }
    };
    for sh in shadowed {
        let (rank, what, ws) = match sh {
            Shadowed::Stuck { rank, nulls, witnesses } => {
                (rank, format!("undecided until {}", nulls.iter().map(|n| format!("?{n}")).collect::<Vec<_>>().join(" ")), witnesses)
            }
            Shadowed::Conflict { rank, path, reason, witnesses } => (rank, format!("{reason} at {path}"), witnesses),
        };
        out.push(policy(
            "warn",
            "attr_shadowed: contributions at a losing rank disagree and are overridden",
            ctx(vec![("rank", Value::Str(rank_name(rank).into())), ("reason", Value::Str(what)), ("witnesses", witnesses(&ws))]),
        ));
    }
    out
}

fn format_policy_fact(a: &Atom) -> Result<String> {
    if a.args.is_empty() {
        bail!("policy fact must have at least a message argument");
    }
    let msg = match &a.args[0] {
        Term::Val(Value::Str(s)) => s.clone(),
        _ => bail!("policy message must be a string"),
    };
    if a.args.len() == 1 {
        return Ok(msg);
    }
    let Term::Val(ctx) = &a.args[1] else {
        bail!("policy context must be ground");
    };
    Ok(format!("{msg} ctx={}", value_to_json_string(ctx)?))
}

fn value_to_json_string(v: &Value) -> Result<String> {
    Ok(serde_json::to_string(&value_to_json(v))?)
}

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::List(xs) => serde_json::Value::Array(xs.iter().map(value_to_json).collect()),
        Value::Obj(m) => serde_json::Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect(),
        ),
        Value::Ip(n) => serde_json::Value::String(crate::value::u32_to_ipv4(*n)),
        Value::IpNet { addr, prefix } => {
            serde_json::Value::String(crate::value::ipnet_to_string(*addr, *prefix))
        }
        Value::IpRange { start, end } => serde_json::Value::String(format!(
            "{}-{}",
            crate::value::u32_to_ipv4(*start),
            crate::value::u32_to_ipv4(*end)
        )),
        Value::Ref { typ, name, attr } => {
            serde_json::Value::String(format!("ref({typ},{name},{attr})"))
        }
        Value::CloudRef { typ, name, attr } => {
            serde_json::Value::String(format!("cloud_ref({typ},{name},{attr})"))
        }
        Value::Null { label, .. } => serde_json::Value::String(format!("?{label}")),
    }
}

fn ensure_ground(a: &Atom) -> Result<Atom> {
    let mut args = Vec::with_capacity(a.args.len());
    for t in &a.args {
        let v = eval_term(t, &HashMap::new())
            .with_context(|| format!("fact is not ground: {}(...)", a.pred))?;
        args.push(Term::Val(v));
    }
    Ok(Atom {
        pred: a.pred.clone(),
        args,
        record: None,
    })
}

fn eval_rule(rule: &RuleStmt, facts: &[Atom]) -> Result<Vec<Atom>> {
    let collect = find_collect(&rule.head);
    if let Some((idx, kind)) = collect {
        return eval_rule_collect(rule, facts, idx, kind);
    }

    let mut out = Vec::new();
    let bindings = eval_body(&rule.body, facts)?;
    for b in bindings {
        // Rule 2 (E §2.7): an address argument is a content position. A head
        // whose address carries a null is stuck, not derived.
        if crate::sim::active() && matches!(rule.head.pred.as_str(), "want" | "arg" | "adopt") && rule.head.args.len() >= 2 {
            if let Some(v) = eval_term(&rule.head.args[1], &b) {
                let nulls = crate::lattice::nulls_in(&v);
                if !nulls.is_empty() {
                    crate::sim::record_stuck(&b, nulls, "resource address carries a null");
                    continue;
                }
            }
        }
        let head = instantiate_atom(&rule.head, &b)
            .with_context(|| format!("instantiate head {}", rule.head.pred))?;
        out.push(head);
    }
    Ok(out)
}

#[derive(Debug, Copy, Clone)]
enum CollectKind {
    Set,
    List,
    Count,
}

fn eval_rule_collect(
    rule: &RuleStmt,
    facts: &[Atom],
    idx: usize,
    kind: CollectKind,
) -> Result<Vec<Atom>> {
    let Term::Func { name: _, args } = &rule.head.args[idx] else {
        bail!("internal: collect idx not func");
    };
    if args.len() != 1 {
        bail!("collect*(...) must have exactly one argument");
    }
    let item_term = args[0].clone();

    if crate::sim::active() {
        crate::sim::record_agg(&rule.body);
    }
    let bindings = eval_body(&rule.body, facts)?;
    let mut groups_set: BTreeMap<Vec<Value>, BTreeSet<Value>> = BTreeMap::new();
    let mut groups_list: BTreeMap<Vec<Value>, Vec<Value>> = BTreeMap::new();
    for b in bindings {
        let mut key = Vec::new();
        let mut key_nulls = BTreeSet::new();
        for (i, t) in rule.head.args.iter().enumerate() {
            if i == idx {
                continue;
            }
            let v = eval_term(t, &b).ok_or_else(|| anyhow!("non-ground head term"))?;
            key_nulls.extend(crate::lattice::nulls_in(&v));
            key.push(v);
        }
        let item = eval_term(&item_term, &b).ok_or_else(|| anyhow!("non-ground collect item"))?;
        if crate::sim::active() {
            // Rule 2: a group key is a content position, always.
            if !key_nulls.is_empty() {
                crate::sim::record_stuck(&b, key_nulls, "aggregate group key carries a null");
                continue;
            }
            // Rule 2 read literally: the aggregated value is a content
            // position. E §7.2 says collect_set forwards nulls; the flag
            // decides which reading runs.
            let item_nulls = crate::lattice::nulls_in(&item);
            if !item_nulls.is_empty() && crate::sim::agg_is_content() {
                crate::sim::record_stuck(&b, item_nulls, "aggregated value carries a null (Rule 2 literal)");
                continue;
            }
        }
        match kind {
            CollectKind::Set => {
                groups_set.entry(key).or_default().insert(item);
            }
            CollectKind::List | CollectKind::Count => {
                groups_list.entry(key).or_default().push(item);
            }
        }
    }

    let mut out = Vec::new();

    let mut emit_group = |key: Vec<Value>, mut items: Vec<Value>| {
        // Deterministic output: Datalog doesn't define an order, so we sort.
        items.sort();

        let mut args_out = Vec::with_capacity(rule.head.args.len());
        let mut k = 0usize;
        for i in 0..rule.head.args.len() {
            if i == idx {
                match kind {
                    CollectKind::Count => args_out.push(Term::Val(Value::Int(items.len() as i64))),
                    _ => args_out.push(Term::Val(Value::List(items.clone()))),
                }
            } else {
                args_out.push(Term::Val(key[k].clone()));
                k += 1;
            }
        }
        out.push(Atom {
            pred: rule.head.pred.clone(),
            args: args_out,
            record: None,
        });
    };

    for (key, items) in groups_set {
        emit_group(key, items.into_iter().collect());
    }
    for (key, items) in groups_list {
        emit_group(key, items);
    }

    Ok(out)
}

fn find_collect(head: &Atom) -> Option<(usize, CollectKind)> {
    for (i, t) in head.args.iter().enumerate() {
        let Term::Func { name, args } = t else {
            continue;
        };
        if args.len() != 1 {
            continue;
        }
        match name.as_str() {
            // Back-compat: `collect(X)` is set-like.
            "collect" | "collect_set" => return Some((i, CollectKind::Set)),
            "collect_list" => return Some((i, CollectKind::List)),
            "count" => return Some((i, CollectKind::Count)),
            _ => {}
        }
    }
    None
}

fn constraint_violated(c: &Constraint, facts: &[Atom]) -> Result<bool> {
    let bindings = eval_body(&c.body, facts)?;
    Ok(!bindings.is_empty())
}

fn eval_body(body: &[Lit], facts: &[Atom]) -> Result<Vec<HashMap<String, Value>>> {
    let mut states: Vec<HashMap<String, Value>> = vec![HashMap::new()];
    for lit in body {
        let mut next = Vec::new();
        match lit {
            Lit::Pos(atom) => {
                if atom.pred == "member" || atom.pred == "enumerate" {
                    for s in &states {
                        eval_member_like(atom, s, &mut next)?;
                    }
                    states = next;
                    if states.is_empty() {
                        break;
                    }
                    continue;
                }
                if is_builtin_pred(&atom.pred) {
                    for s in &states {
                        if eval_builtin_pred(atom, s)? {
                            next.push(s.clone());
                        }
                    }
                    states = next;
                    if states.is_empty() {
                        break;
                    }
                    continue;
                }
                for s in &states {
                    for f in facts.iter().filter(|x| x.pred == atom.pred) {
                        if let Some(s2) = unify_atom(atom, f, s)? {
                            next.push(s2);
                        }
                    }
                }
            }
            Lit::Not(atom) => {
                for s in &states {
                    if atom.pred == "member" {
                        if atom.args.len() == 2 {
                            if eval_not_member2(atom, s)? {
                                next.push(s.clone());
                            }
                            continue;
                        }
                        if atom.args.len() == 3 {
                            if eval_not_member3(atom, s)? {
                                next.push(s.clone());
                            }
                            continue;
                        }
                        bail!("member/2 or member/3 expected");
                    }
                    if atom.pred == "enumerate" {
                        // `enumerate/3` is a generator; `not enumerate(...)` is meaningless
                        // (it would require checking existence over an implicit domain).
                        bail!("negation not supported for enumerate/3");
                    }
                    if is_builtin_pred(&atom.pred) {
                        // Negation-as-failure for builtin predicates is just boolean negation.
                        if !eval_builtin_pred(atom, s)? {
                            next.push(s.clone());
                        }
                    } else {
                        let grounded = ground_atom(atom, s)
                            .with_context(|| format!("unsafe negation: not {}(...)", atom.pred))?;
                        if crate::sim::active() {
                            // Rule 2: a negation pattern holding an open or
                            // secret null is a content position; fresh nulls
                            // are decided under UNA (by label).
                            let mut open = BTreeSet::new();
                            for t in &grounded.args {
                                if let Term::Val(v) = t {
                                    if crate::sim::has_open_or_secret(v) {
                                        open.extend(crate::lattice::nulls_in(v));
                                    }
                                }
                            }
                            if !open.is_empty() {
                                crate::sim::record_stuck(s, open, format!("negation pattern not {}(..) holds an open/secret null", atom.pred));
                                continue;
                            }
                            crate::sim::record_neg(&grounded);
                        }
                        let any = facts
                            .iter()
                            .any(|f| f.pred == grounded.pred && f.args == grounded.args);
                        if !any {
                            next.push(s.clone());
                        }
                    }
                }
            }
            Lit::Eq(a, b) => {
                for s in &states {
                    if let Some(s2) = eval_eq(a, b, s)? {
                        next.push(s2);
                    }
                }
            }
            Lit::Neq(a, b) => {
                for s in &states {
                    if let Some(s2) = eval_neq(a, b, s)? {
                        next.push(s2);
                    }
                }
            }
            Lit::Gt(a, b) | Lit::Ge(a, b) | Lit::Lt(a, b) | Lit::Le(a, b) => {
                for s in &states {
                    if eval_cmp(lit, a, b, s)? {
                        next.push(s.clone());
                    }
                }
            }
        }
        states = next;
        if states.is_empty() {
            break;
        }
    }
    Ok(states)
}

fn is_builtin_pred(pred: &str) -> bool {
    matches!(pred, "inet_overlaps" | "inet_contains" | "ip_unspecified")
}

fn eval_builtin_pred(atom: &Atom, state: &HashMap<String, Value>) -> Result<bool> {
    // Builtin predicates are functions that return Bool.
    let Some(v) = eval_func(&atom.pred, &atom.args, state) else {
        if crate::sim::active() && atom.args.iter().any(|t| eval_term(t, state).map(|v| crate::sim::has_null(&v)).unwrap_or(false)) {
            // Stuck was recorded by eval_func; the literal does not hold.
            return Ok(false);
        }
        bail!("unsafe builtin predicate {}(...)", atom.pred);
    };
    match v {
        Value::Bool(b) => Ok(b),
        other => bail!("builtin predicate {} returned non-bool: {other:?}", atom.pred),
    }
}

fn eval_member_like(
    atom: &Atom,
    state: &HashMap<String, Value>,
    out: &mut Vec<HashMap<String, Value>>,
) -> Result<()> {
    match atom.pred.as_str() {
        "member" => {
            if atom.args.len() == 2 {
                return eval_member2(atom, state, out);
            }
            if atom.args.len() == 3 {
                return eval_member3(atom, state, out);
            }
            bail!("member/2 or member/3 expected");
        }
        "enumerate" => {
            if atom.args.len() != 3 {
                bail!("enumerate/3 expected");
            }
            eval_member3(atom, state, out)
        }
        _ => bail!("internal: eval_member_like called for non-member"),
    }
}

fn eval_member2(
    atom: &Atom,
    state: &HashMap<String, Value>,
    out: &mut Vec<HashMap<String, Value>>,
) -> Result<()> {
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe member: list is not ground"))?;
    if let Value::Null { .. } = &list_v {
        // Rule 2: member over a null list is a content position.
        crate::sim::record_stuck(state, crate::lattice::nulls_in(&list_v), "member/2 over a null list");
        return Ok(());
    }
    let Value::List(items) = list_v else {
        bail!("member/2 first argument must be a list");
    };
    for item in &items {
        let mut s2 = state.clone();
        if unify_term(&atom.args[1], item, &mut s2)? {
            out.push(s2);
        }
    }
    Ok(())
}

fn eval_not_member2(atom: &Atom, state: &HashMap<String, Value>) -> Result<bool> {
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe not member: list is not ground"))?;
    if let Value::Null { .. } = &list_v {
        crate::sim::record_stuck(state, crate::lattice::nulls_in(&list_v), "not member/2 over a null list");
        return Ok(false);
    }
    let Value::List(items) = list_v else {
        bail!("member/2 first argument must be a list");
    };
    let item_v = eval_term(&atom.args[1], state)
        .ok_or_else(|| anyhow!("unsafe not member: item is not ground"))?;
    if crate::sim::active() {
        let mut unknown = BTreeSet::new();
        for x in &items {
            match crate::lattice::eq3(x, &item_v) {
                crate::lattice::Truth::True => return Ok(false),
                crate::lattice::Truth::Unknown => {
                    unknown.extend(crate::lattice::nulls_in(x));
                    unknown.extend(crate::lattice::nulls_in(&item_v));
                }
                crate::lattice::Truth::False => {}
            }
        }
        if !unknown.is_empty() {
            crate::sim::record_stuck(state, unknown, "not member/2: membership undecidable");
            return Ok(false);
        }
        return Ok(true);
    }
    Ok(!items.iter().any(|x| *x == item_v))
}

fn eval_not_member3(atom: &Atom, state: &HashMap<String, Value>) -> Result<bool> {
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe not member: list is not ground"))?;
    let Value::List(items) = list_v else {
        bail!("member/3 first argument must be a list");
    };
    let idx_v = eval_term(&atom.args[1], state)
        .ok_or_else(|| anyhow!("unsafe not member: index is not ground"))?;
    let Value::Int(i) = idx_v else {
        bail!("member/3 index must be int");
    };
    if i < 0 {
        return Ok(true);
    }
    let i = i as usize;
    if i >= items.len() {
        return Ok(true);
    }
    let item_v = eval_term(&atom.args[2], state)
        .ok_or_else(|| anyhow!("unsafe not member: item is not ground"))?;
    Ok(items[i] != item_v)
}

fn eval_member3(
    atom: &Atom,
    state: &HashMap<String, Value>,
    out: &mut Vec<HashMap<String, Value>>,
) -> Result<()> {
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe member: list is not ground"))?;
    if let Value::Null { .. } = &list_v {
        crate::sim::record_stuck(state, crate::lattice::nulls_in(&list_v), "member/3 over a null list");
        return Ok(());
    }
    let Value::List(items) = list_v else {
        bail!("member/3 first argument must be a list");
    };
    for (i, item) in items.iter().enumerate() {
        let mut s2 = state.clone();
        if !unify_term(&atom.args[1], &Value::Int(i as i64), &mut s2)? {
            continue;
        }
        if !unify_term(&atom.args[2], item, &mut s2)? {
            continue;
        }
        out.push(s2);
    }
    Ok(())
}

fn unify_atom(pattern: &Atom, fact: &Atom, state: &HashMap<String, Value>) -> Result<Option<HashMap<String, Value>>> {
    if pattern.args.len() != fact.args.len() {
        return Ok(None);
    }
    let mut out = state.clone();
    for (p, f) in pattern.args.iter().zip(&fact.args) {
        let Term::Val(fv) = f else {
            bail!("internal: non-ground fact");
        };
        if !unify_term(p, fv, &mut out)? {
            return Ok(None);
        }
    }
    Ok(Some(out))
}

/// Three-valued equality for unification when the simulation is active:
/// Unknown records a stuck instance and fails the match (Rule 2).
fn sim_eq(a: &Value, b: &Value, out: &HashMap<String, Value>) -> bool {
    if !crate::sim::active() {
        return a == b;
    }
    match crate::lattice::eq3(a, b) {
        crate::lattice::Truth::True => true,
        crate::lattice::Truth::False => false,
        crate::lattice::Truth::Unknown => {
            let mut nulls = crate::lattice::nulls_in(a);
            nulls.extend(crate::lattice::nulls_in(b));
            crate::sim::record_stuck(out, nulls, "unification against an open/secret null");
            false
        }
    }
}

fn unify_term(pat: &Term, fv: &Value, out: &mut HashMap<String, Value>) -> Result<bool> {
    match pat {
        Term::Val(v) => Ok(sim_eq(v, fv, out)),
        Term::Var(name) => {
            if let Some(bound) = out.get(name) {
                Ok(sim_eq(bound, fv, out))
            } else {
                out.insert(name.clone(), fv.clone());
                Ok(true)
            }
        }
        Term::Wildcard => Ok(true),
        Term::List(items) => {
            let Value::List(vs) = fv else {
                return Ok(false);
            };
            if items.len() != vs.len() {
                return Ok(false);
            }
            let mut tmp = out.clone();
            for (t, v) in items.iter().zip(vs) {
                if !unify_term(t, v, &mut tmp)? {
                    return Ok(false);
                }
            }
            *out = tmp;
            Ok(true)
        }
        Term::Obj(m) => {
            let Value::Obj(vm) = fv else {
                return Ok(false);
            };
            if m.len() != vm.len() {
                return Ok(false);
            }
            let mut tmp = out.clone();
            for (k, t) in m {
                let Some(v) = vm.get(k) else {
                    return Ok(false);
                };
                if !unify_term(t, v, &mut tmp)? {
                    return Ok(false);
                }
            }
            *out = tmp;
            Ok(true)
        }
        Term::Func { name, args } => {
            // Special pattern unification for scoped(Scope, LocalName).
            // This allows rules to join on component-scoped resources while still
            // binding LocalName variables.
            if name == "scoped" && args.len() == 2 {
                let Some(Value::Str(scope)) = eval_term(&args[0], out) else {
                    return Ok(false);
                };
                let Value::Str(full) = fv else {
                    return Ok(false);
                };
                let prefix = format!("{scope}::");
                let Some(suffix) = full.strip_prefix(&prefix) else {
                    return Ok(false);
                };
                return unify_term(&args[1], &Value::Str(suffix.to_string()), out);
            }

            let pv = match eval_term(pat, out) {
                Some(v) => v,
                None => return Ok(false),
            };
            Ok(sim_eq(&pv, fv, out))
        }
        Term::ListComp { .. } => {
            // Comprehensions must be lowered before evaluation.
            Ok(false)
        }
    }
}

fn ground_atom(atom: &Atom, state: &HashMap<String, Value>) -> Result<Atom> {
    let mut args = Vec::with_capacity(atom.args.len());
    for t in &atom.args {
        let v = eval_term(t, state).ok_or_else(|| anyhow!("unbound var in negation"))?;
        args.push(Term::Val(v));
    }
    Ok(Atom {
        pred: atom.pred.clone(),
        args,
        record: None,
    })
}

fn instantiate_atom(atom: &Atom, state: &HashMap<String, Value>) -> Result<Atom> {
    let mut args = Vec::with_capacity(atom.args.len());
    for t in &atom.args {
        if matches!(t, Term::Func { name, .. } if name == "collect") {
            bail!("internal: collect must be handled separately");
        }
        let v = eval_term(t, state).ok_or_else(|| anyhow!("non-ground head"))?;
        args.push(Term::Val(v));
    }
    Ok(Atom {
        pred: atom.pred.clone(),
        args,
        record: None,
    })
}

fn eval_eq(a: &Term, b: &Term, state: &HashMap<String, Value>) -> Result<Option<HashMap<String, Value>>> {
    let mut out = state.clone();
    match (eval_term(a, &out), eval_term(b, &out)) {
        (Some(av), Some(bv)) => Ok(sim_eq(&av, &bv, &out).then_some(out)),
        (Some(av), None) => {
            if bind_term(b, av, &mut out)? {
                Ok(Some(out))
            } else {
                Ok(None)
            }
        }
        (None, Some(bv)) => {
            if bind_term(a, bv, &mut out)? {
                Ok(Some(out))
            } else {
                Ok(None)
            }
        }
        (None, None) => {
            for t in [a, b] {
                if let Some((name, args)) = failed_builtin(t, &out) {
                    if crate::sim::active() && args.iter().any(crate::sim::has_null) {
                        // Rule 2: a builtin over a null is stuck (recorded).
                        return Ok(None);
                    }
                    let args: Vec<String> = args.iter().map(partition::fmt_value).collect();
                    bail!("{name}({}) is not defined for these arguments", args.join(", "));
                }
            }
            bail!("unsafe equality: both sides unbound")
        }
    }
}

/// The innermost function application in `t` whose arguments are all ground
/// but which has no value: a builtin applied to the wrong kind of value
/// (`"10" + 1`, `to_int("abc")`). `None` when the term is merely unbound.
fn failed_builtin(t: &Term, state: &HashMap<String, Value>) -> Option<(String, Vec<Value>)> {
    let Term::Func { name, args } = t else { return None };
    if let Some(inner) = args.iter().find_map(|a| failed_builtin(a, state)) {
        return Some(inner);
    }
    let vals: Option<Vec<Value>> = args.iter().map(|a| eval_term(a, state)).collect();
    let vals = vals?;
    eval_func(name, args, state).is_none().then(|| (name.clone(), vals))
}

fn eval_neq(a: &Term, b: &Term, state: &HashMap<String, Value>) -> Result<Option<HashMap<String, Value>>> {
    match (eval_term(a, state), eval_term(b, state)) {
        (Some(av), Some(bv)) => {
            if crate::sim::active() {
                return Ok(match crate::lattice::eq3(&av, &bv) {
                    crate::lattice::Truth::False => Some(state.clone()),
                    crate::lattice::Truth::True => None,
                    crate::lattice::Truth::Unknown => {
                        let mut nulls = crate::lattice::nulls_in(&av);
                        nulls.extend(crate::lattice::nulls_in(&bv));
                        crate::sim::record_stuck(state, nulls, "!= against an open/secret null");
                        None
                    }
                });
            }
            Ok((av != bv).then_some(state.clone()))
        }
        _ => bail!("unsafe !=: both sides must be ground"),
    }
}

fn eval_cmp(op_lit: &Lit, a: &Term, b: &Term, state: &HashMap<String, Value>) -> Result<bool> {
    let Some(av) = eval_term(a, state) else {
        bail!("unsafe comparison: left not ground");
    };
    let Some(bv) = eval_term(b, state) else {
        bail!("unsafe comparison: right not ground");
    };
    if crate::sim::active() && (crate::sim::has_null(&av) || crate::sim::has_null(&bv)) {
        let mut nulls = crate::lattice::nulls_in(&av);
        nulls.extend(crate::lattice::nulls_in(&bv));
        crate::sim::record_stuck(state, nulls, "ordering comparison over a null");
        return Ok(false);
    }
    let (ai, bi) = match (&av, &bv) {
        (Value::Int(x), Value::Int(y)) => (*x, *y),
        _ => bail!("comparison only supports ints"),
    };
    Ok(match op_lit {
        Lit::Gt(_, _) => ai > bi,
        Lit::Ge(_, _) => ai >= bi,
        Lit::Lt(_, _) => ai < bi,
        Lit::Le(_, _) => ai <= bi,
        _ => unreachable!(),
    })
}

fn bind_term(t: &Term, v: Value, out: &mut HashMap<String, Value>) -> Result<bool> {
    match t {
        Term::Var(name) => {
            if let Some(bound) = out.get(name) {
                Ok(bound == &v)
            } else {
                out.insert(name.clone(), v);
                Ok(true)
            }
        }
        _ => Ok(false),
    }
}

fn eval_term(term: &Term, state: &HashMap<String, Value>) -> Option<Value> {
    match term {
        Term::Val(v) => Some(v.clone()),
        Term::Var(name) => state.get(name).cloned(),
        Term::Wildcard => None,
        Term::Func { name, args } => eval_func(name, args, state),
        Term::List(xs) => {
            let mut out = Vec::new();
            for x in xs {
                out.push(eval_term(x, state)?);
            }
            Some(Value::List(out))
        }
        Term::Obj(m) => {
            let mut out = BTreeMap::new();
            for (k, v) in m {
                out.insert(k.clone(), eval_term(v, state)?);
            }
            Some(Value::Obj(out))
        }
        Term::ListComp { .. } => None,
    }
}

fn eval_func(name: &str, args: &[Term], state: &HashMap<String, Value>) -> Option<Value> {
    if crate::sim::active() {
        match name {
            // Rule 1: ref to a computed attribute IS the null; forwarded.
            "ref" if args.len() == 3 => {
                let t = eval_term(&args[0], state)?.as_str()?.to_string();
                let n = eval_term(&args[1], state)?;
                if crate::sim::has_null(&n) {
                    crate::sim::record_stuck(state, crate::lattice::nulls_in(&n), "ref address carries a null");
                    return None;
                }
                let n = value_to_string(&n);
                let a = eval_term(&args[2], state)?.as_str()?.to_string();
                if let Some(v) = crate::sim::null_for(&t, &n, &a) {
                    return Some(v);
                }
                // Not computed: a configured attribute. E rewrites this to an
                // attr join; the simulation keeps the opaque Ref.
            }
            "cloud_ref" | "gref" | "collect" | "collect_set" | "collect_list" | "count" => {}
            // Rule 2: every other builtin argument is a content position.
            _ => {
                let mut nulls = BTreeSet::new();
                for a in args {
                    if let Some(v) = eval_term(a, state) {
                        nulls.extend(crate::lattice::nulls_in(&v));
                    }
                }
                if !nulls.is_empty() {
                    let what = if name == "scoped" { "resource address carries a null".to_string() } else { format!("builtin {name}() over a null") };
                    crate::sim::record_stuck(state, nulls, what);
                    return None;
                }
            }
        }
    }
    match name {
        "add" => {
            if args.len() != 2 {
                return None;
            }
            let a = as_i64(&eval_term(&args[0], state)?)?;
            let b = as_i64(&eval_term(&args[1], state)?)?;
            Some(Value::Int(a + b))
        }
        "sub" => {
            if args.len() != 2 {
                return None;
            }
            let a = as_i64(&eval_term(&args[0], state)?)?;
            let b = as_i64(&eval_term(&args[1], state)?)?;
            Some(Value::Int(a - b))
        }
        "mul" => {
            if args.len() != 2 {
                return None;
            }
            let a = as_i64(&eval_term(&args[0], state)?)?;
            let b = as_i64(&eval_term(&args[1], state)?)?;
            Some(Value::Int(a * b))
        }
        "div" => {
            if args.len() != 2 {
                return None;
            }
            let a = as_i64(&eval_term(&args[0], state)?)?;
            let b = as_i64(&eval_term(&args[1], state)?)?;
            if b == 0 {
                return None;
            }
            Some(Value::Int(a / b))
        }
        "mod" => {
            if args.len() != 2 {
                return None;
            }
            let a = as_i64(&eval_term(&args[0], state)?)?;
            let b = as_i64(&eval_term(&args[1], state)?)?;
            if b == 0 {
                return None;
            }
            Some(Value::Int(a % b))
        }
        "ip" => {
            if args.len() != 1 {
                return None;
            }
            let s = eval_term(&args[0], state)?.as_str()?.to_string();
            let n = crate::value::ipv4_to_u32(&s)?;
            Some(Value::Ip(n))
        }
        "ip_str" => {
            if args.len() != 1 {
                return None;
            }
            match eval_term(&args[0], state)? {
                Value::Ip(n) => Some(Value::Str(crate::value::u32_to_ipv4(n))),
                Value::Str(s) => Some(Value::Str(s)),
                _ => None,
            }
        }
        "inet" => {
            if args.len() != 1 {
                return None;
            }
            let s = eval_term(&args[0], state)?.as_str()?.to_string();
            let (addr, prefix) = crate::value::parse_ipnet(&s)?;
            Some(Value::IpNet { addr, prefix })
        }
        "inet_str" => {
            if args.len() != 1 {
                return None;
            }
            match eval_term(&args[0], state)? {
                Value::IpNet { addr, prefix } => {
                    Some(Value::Str(crate::value::ipnet_to_string(addr, prefix)))
                }
                Value::Str(s) => Some(Value::Str(s)),
                _ => None,
            }
        }
        "iprange" => {
            if args.len() != 2 {
                return None;
            }
            let a = eval_term(&args[0], state)?;
            let b = eval_term(&args[1], state)?;
            let sa = as_ip_u32(&a)?;
            let sb = as_ip_u32(&b)?;
            let (start, end) = if sa <= sb { (sa, sb) } else { (sb, sa) };
            Some(Value::IpRange { start, end })
        }
        "ip_unspecified" => {
            if args.len() != 1 {
                return None;
            }
            let v = eval_term(&args[0], state)?;
            let n = as_ip_u32(&v)?;
            Some(Value::Bool(n == 0))
        }
        "inet_contains" => {
            if args.len() != 2 {
                return None;
            }
            let net = eval_term(&args[0], state)?;
            let ip = eval_term(&args[1], state)?;
            let (addr, prefix) = as_ipnet(&net)?;
            let n = as_ip_u32(&ip)?;
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
            Some(Value::Bool((n & mask) == addr))
        }
        "inet_overlaps" => {
            if args.len() != 2 {
                return None;
            }
            let a = eval_term(&args[0], state)?;
            let b = eval_term(&args[1], state)?;
            let (a0, a1) = ipnet_range(&a)?;
            let (b0, b1) = ipnet_range(&b)?;
            Some(Value::Bool(a0 <= b1 && b0 <= a1))
        }
        "inet_addr" => {
            if args.len() != 2 {
                return None;
            }
            let net = eval_term(&args[0], state)?;
            let n = eval_term(&args[1], state)?;
            let (addr, _prefix) = as_ipnet(&net)?;
            let idx = as_i64(&n)?;
            if idx < 0 {
                return None;
            }
            Some(Value::Ip(addr.wrapping_add(idx as u32)))
        }
        "inet_host" => {
            if args.len() != 2 {
                return None;
            }
            let net = eval_term(&args[0], state)?;
            let n = eval_term(&args[1], state)?;
            let (start, end) = ipnet_range(&net)?;
            // usable hosts exclude network + broadcast
            if end <= start + 1 {
                return None;
            }
            let first = start + 1;
            let last = end - 1;
            let idx = as_i64(&n)?;
            if idx < 0 {
                return None;
            }
            let ip = first + (idx as u32);
            if ip > last {
                return None;
            }
            Some(Value::Ip(ip))
        }
        "inet_subnet" => {
            if args.len() != 3 {
                return None;
            }
            let net = eval_term(&args[0], state)?;
            let newbits = eval_term(&args[1], state)?;
            let netnum = eval_term(&args[2], state)?;
            let (addr, prefix) = as_ipnet(&net)?;
            let nb = as_i64(&newbits)?;
            let nn = as_i64(&netnum)?;
            if nb < 0 || nn < 0 {
                return None;
            }
            let new_prefix = (prefix as i64) + nb;
            if new_prefix > 32 {
                return None;
            }
            let shift = 32 - (new_prefix as u32);
            let subnet_addr = addr + ((nn as u32) << shift);
            Some(Value::IpNet {
                addr: subnet_addr,
                prefix: new_prefix as u8,
            })
        }
        "scoped" => {
            if args.len() != 2 {
                return None;
            }
            let scope = value_to_string(&eval_term(&args[0], state)?);
            let name = value_to_string(&eval_term(&args[1], state)?);
            Some(Value::Str(format!("{scope}::{name}")))
        }
        "format" => {
            let mut ev = Vec::new();
            for a in args {
                ev.push(eval_term(a, state)?);
            }
            let fmt = ev.first()?.as_str()?.to_string();
            let mut out = String::new();
            let mut parts = fmt.split("%s");
            out.push_str(parts.next().unwrap_or(""));
            for (i, p) in parts.enumerate() {
                let v = ev.get(i + 1)?;
                out.push_str(&value_to_string(v));
                out.push_str(p);
            }
            Some(Value::Str(out))
        }
        "concat" => {
            let mut out = String::new();
            for a in args {
                out.push_str(&value_to_string(&eval_term(a, state)?));
            }
            Some(Value::Str(out))
        }
        "ref" => {
            if args.len() != 3 {
                return None;
            }
            let t = eval_term(&args[0], state)?.as_str()?.to_string();
            let n = eval_term(&args[1], state)?.as_str()?.to_string();
            let a = eval_term(&args[2], state)?.as_str()?.to_string();
            Some(Value::Ref {
                typ: t,
                name: n,
                attr: a,
            })
        }
        "cloud_ref" => {
            if args.len() != 3 {
                return None;
            }
            let t = eval_term(&args[0], state)?.as_str()?.to_string();
            let n = eval_term(&args[1], state)?.as_str()?.to_string();
            let a = eval_term(&args[2], state)?.as_str()?.to_string();
            Some(Value::CloudRef {
                typ: t,
                name: n,
                attr: a,
            })
        }
        "gref" => {
            if args.len() != 3 {
                return None;
            }
            let t = eval_term(&args[0], state)?.as_str()?.to_string();
            let n = eval_term(&args[1], state)?.as_str()?.to_string();
            let a = eval_term(&args[2], state)?.as_str()?.to_string();
            Some(Value::Ref {
                typ: t,
                name: n,
                attr: a,
            })
        }
        "cidrsubnet" => {
            if args.len() != 3 {
                return None;
            }
            let cidr = eval_term(&args[0], state)?.as_str()?.to_string();
            let newbits = match eval_term(&args[1], state)? {
                Value::Int(i) => i,
                _ => return None,
            };
            let netnum = match eval_term(&args[2], state)? {
                Value::Int(i) => i,
                _ => return None,
            };
            Some(Value::Str(cidrsubnet(&cidr, newbits as u32, netnum as u32)?))
        }
        // Explicit conversions (DESIGN.org "Silent string-to-int coercion").
        "to_int" => match (args, eval_term(args.first()?, state)?) {
            ([_], Value::Int(i)) => Some(Value::Int(i)),
            ([_], Value::Str(s)) => s.trim().parse().ok().map(Value::Int),
            _ => None,
        },
        "to_string" => match args {
            [a] => scalar_text(&eval_term(a, state)?).map(Value::Str),
            _ => None,
        },
        "len" => match args {
            [a] => match eval_term(a, state)? {
                Value::List(xs) => Some(Value::Int(xs.len() as i64)),
                Value::Obj(m) => Some(Value::Int(m.len() as i64)),
                Value::Str(s) => Some(Value::Int(s.chars().count() as i64)),
                _ => None,
            },
            _ => None,
        },
        "lower" | "upper" => match args {
            [a] => {
                let s = eval_term(a, state)?.as_str()?.to_string();
                Some(Value::Str(if name == "lower" { s.to_lowercase() } else { s.to_uppercase() }))
            }
            _ => None,
        },
        "split" => match args {
            [a, sep] => {
                let s = eval_term(a, state)?.as_str()?.to_string();
                let sep = eval_term(sep, state)?.as_str()?.to_string();
                if sep.is_empty() {
                    return None;
                }
                Some(Value::List(s.split(sep.as_str()).map(|x| Value::Str(x.to_string())).collect()))
            }
            _ => None,
        },
        "join" => match args {
            [l, sep] => {
                let Value::List(xs) = eval_term(l, state)? else { return None };
                let sep = eval_term(sep, state)?.as_str()?.to_string();
                let parts: Option<Vec<String>> = xs.iter().map(scalar_text).collect();
                Some(Value::Str(parts?.join(&sep)))
            }
            _ => None,
        },
        "collect" => None,
        _ => None,
    }
}

/// Arithmetic takes integers only; a string is converted with `to_int`.
fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }
}

/// The text of a scalar, for `to_string` and `join`. Lists, objects,
/// references and nulls have no text.
fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::Str(_) | Value::Int(_) | Value::Bool(_) | Value::Ip(_) | Value::IpNet { .. } | Value::IpRange { .. } => {
            Some(value_to_string(v))
        }
        _ => None,
    }
}

fn as_ip_u32(v: &Value) -> Option<u32> {
    match v {
        Value::Ip(n) => Some(*n),
        Value::Str(s) => crate::value::ipv4_to_u32(s),
        _ => None,
    }
}

fn as_ipnet(v: &Value) -> Option<(u32, u8)> {
    match v {
        Value::IpNet { addr, prefix } => Some((*addr, *prefix)),
        Value::Str(s) => crate::value::parse_ipnet(s),
        _ => None,
    }
}

fn ipnet_range(v: &Value) -> Option<(u32, u32)> {
    let (addr, prefix) = as_ipnet(v)?;
    let host_bits = 32 - (prefix as u32);
    let size = if host_bits == 32 {
        u32::MAX
    } else {
        (1u64 << host_bits) as u32
    };
    let end = addr.wrapping_add(size.wrapping_sub(1));
    Some((addr, end))
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(_) => "<list>".to_string(),
        Value::Obj(_) => "<obj>".to_string(),
        Value::Ip(n) => crate::value::u32_to_ipv4(*n),
        Value::IpNet { addr, prefix } => crate::value::ipnet_to_string(*addr, *prefix),
        Value::IpRange { start, end } => format!(
            "{}-{}",
            crate::value::u32_to_ipv4(*start),
            crate::value::u32_to_ipv4(*end)
        ),
        Value::Ref { typ, name, attr } => format!("ref({typ},{name},{attr})"),
        Value::CloudRef { typ, name, attr } => format!("cloud_ref({typ},{name},{attr})"),
        Value::Null { label, .. } => format!("?{label}"),
    }
}

fn cidrsubnet(cidr: &str, newbits: u32, netnum: u32) -> Option<String> {
    // Supports only IPv4 CIDRs like "10.0.0.0/16".
    let (ip, prefix) = cidr.split_once('/')?;
    let prefix: u32 = prefix.parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let new_prefix = prefix.checked_add(newbits)?;
    if new_prefix > 32 {
        return None;
    }
    let base = ipv4_to_u32(ip)?;
    // netnum selects the subnet within the expanded prefix.
    let shift = 32 - new_prefix;
    let subnet_base = base + (netnum << shift);
    Some(format!("{}/{}", u32_to_ipv4(subnet_base), new_prefix))
}

fn ipv4_to_u32(ip: &str) -> Option<u32> {
    crate::value::ipv4_to_u32(ip)
}

fn u32_to_ipv4(v: u32) -> String {
    crate::value::u32_to_ipv4(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> Result<(EvalResult, Vec<String>)> {
        let program = crate::parser::parse_program(src)?;
        eval(&program, &[])
    }

    fn facts_of(r: &EvalResult, pred: &str) -> Vec<String> {
        r.facts.iter().filter(|a| a.pred == pred).map(partition::fmt_atom).collect()
    }

    /// DESIGN.org "Aggregates are not stratified": a consumer of an
    /// aggregate used to see every partial result mid-fixpoint.
    #[test]
    fn aggregate_consumer_sees_one_complete_result() {
        let (r, _) = run(
            "n(1).
             n(2) :- n(1).
             n(3) :- n(2).
             all(collect_set(X)) :- n(X).
             snap(L) :- all(L).",
        )
        .unwrap();
        assert_eq!(facts_of(&r, "snap"), vec!["snap([1, 2, 3])".to_string()]);
    }

    /// A cycle through negation is a compile error naming the cycle with
    /// the text of every rule on it.
    #[test]
    fn negative_cycle_is_an_error_with_rule_text() {
        let err = run(
            "q(1).
             p(X) :- q(X), not r(X).
             r(X) :- p(X).",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("negative cycle"), "{err}");
        assert!(err.contains("p(X) :- q(X), not r(X)"), "{err}");
    }

    /// Rules run in the stratum of their head's partition node: a `want`
    /// of one type may negate, or aggregate over, `want` of another type.
    #[test]
    fn want_is_partitioned_by_type() {
        let (r, _) = run(
            "want(net.subnet, a).
             want(net.subnet, b).
             subnets(collect_set(S)) :- want(net.subnet, S).
             want(db.postgres, db) :- subnets(L), member(L, a), not want(net.subnet, c).",
        )
        .unwrap();
        assert!(facts_of(&r, "want").contains(&"want(\"db.postgres\", \"db\")".to_string()));
    }

    fn input(k: &str, v: Value) -> Atom {
        Atom { pred: "input".into(), args: vec![str_val(k), Term::Val(v)], record: None }
    }

    /// Two rules set one attribute to different values: no attr fact, an
    /// attr_conflict, and a deny naming the resource, the path and both
    /// contributing rules.
    #[test]
    fn conflicting_contributions_derive_a_deny_naming_every_witness() {
        let (r, violations) = run(
            "resource net.vpc main { cidr = \"10.0.0.0/16\" }.
             arg(net.vpc, main, cidr, \"10.1.0.0/16\") :- want(net.vpc, main).",
        )
        .unwrap();
        assert!(facts_of(&r, "attr").iter().all(|a| !a.contains("cidr")), "{:?}", facts_of(&r, "attr"));
        assert_eq!(facts_of(&r, "attr_conflict").len(), 1);
        assert_eq!(violations.len(), 1, "{violations:?}");
        let v = &violations[0];
        let (msg, ctx) = v.split_once(" ctx=").unwrap();
        assert_eq!(msg, "conflicting attribute contributions");
        let ctx: serde_json::Value = serde_json::from_str(ctx).unwrap();
        assert_eq!(ctx["type"], "net.vpc");
        assert_eq!(ctx["addr"], "main");
        assert_eq!(ctx["path"], "cidr");
        let from: Vec<String> = ctx["witnesses"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|w| w["from"].as_array().unwrap().iter().map(|f| f.as_str().unwrap().to_string()))
            .collect();
        assert_eq!(
            from,
            vec![
                "arg(\"net.vpc\", \"main\", \"cidr\", \"10.0.0.0/16\", \"normal\")".to_string(),
                "arg(\"net.vpc\", \"main\", \"cidr\", \"10.1.0.0/16\", \"normal\") :- want(\"net.vpc\", \"main\")".to_string(),
            ]
        );
    }

    /// Readers see the collapsed value, never a raw contribution: `setting`
    /// and `output` are the same aggregate on pseudo-types, `+=` is a plain
    /// contribution, and a Set path unions every author.
    #[test]
    fn setting_and_output_readers_read_the_collapsed_value() {
        let (r, violations) = run(
            "type_lattice(settings, sinks, set).
             settings prod { sinks += [\"cloudwatch\"], days = 14 }.
             setting_add(prod, sinks, [\"s3\"]).
             component network main { output(ids, [a, b]). }.
             got(S, D) :- setting(prod, sinks, S), setting(prod, days, D).
             ids(L) :- output(network.main, ids, L).
             deny(\"no audit\") :- not setting(prod, audit, true).",
        )
        .unwrap();
        assert_eq!(facts_of(&r, "got"), vec!["got([\"cloudwatch\", \"s3\"], 14)".to_string()]);
        assert_eq!(facts_of(&r, "ids"), vec!["ids([\"a\", \"b\"])".to_string()]);
        assert_eq!(violations, vec!["no audit".to_string()]);
    }

    /// E §2.5 path normalization: `tags.team` contributes `{team: V}` to
    /// `tags`, which is a Map, so it meets the block's other tags per leaf.
    #[test]
    fn dotted_path_contributes_to_its_top_level_attribute() {
        let (r, _) = run(
            "resource net.vpc main { tags = { env: dev } }.
             arg(net.vpc, main, \"tags.team\", platform) :- want(net.vpc, main).",
        )
        .unwrap();
        assert_eq!(
            facts_of(&r, "attr"),
            vec!["attr(\"net.vpc\", \"main\", \"tags\", {env: \"dev\", team: \"platform\"})".to_string()]
        );
    }

    /// Ranks in the core form: the winning rank decides; two disagreeing
    /// defaults under a normal value are a warning, not an error (F DR-9).
    #[test]
    fn highest_rank_wins_and_a_shadowed_disagreement_warns() {
        let (r, violations) = run(
            "want(net.vpc, main).
             arg(net.vpc, main, cidr, \"10.0.0.0/16\", default).
             arg(net.vpc, main, cidr, \"10.9.0.0/16\", default).
             arg(net.vpc, main, cidr, \"10.1.0.0/16\").",
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(facts_of(&r, "attr"), vec!["attr(\"net.vpc\", \"main\", \"cidr\", \"10.1.0.0/16\")".to_string()]);
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].starts_with("attr_shadowed"), "{:?}", r.warnings);
    }

    /// A null contribution (a computed attribute at plan time) is carried
    /// through the aggregate as a value.
    #[test]
    fn a_null_contribution_is_carried_through_attr() {
        let null = Value::Null { label: "net.vpc/main#id".into(), class: crate::value::NullClass::Fresh, ty: "string".into() };
        let program = crate::parser::parse_program(
            "want(net.subnet, a).
             arg(net.subnet, a, vpc_id, V) :- input(vpc, V).
             seen(V) :- arg(net.subnet, a, vpc_id, V).",
        )
        .unwrap();
        let (r, violations) = eval(&program, &[input("vpc", null.clone())]).unwrap();
        assert!(violations.is_empty());
        assert_eq!(facts_of(&r, "seen"), vec![format!("seen({})", partition::fmt_value(&null))]);
    }

    /// `unique` lowers to nothing: one value per key is the aggregate's job.
    #[test]
    fn unique_lowers_to_nothing() {
        let (r, _) = run("unique p(1). p(1, a). p(1, b).").unwrap();
        assert_eq!(facts_of(&r, "p").len(), 2);
    }

    /// DR-1 acceptance: shuffling statement order yields identical attr/4.
    #[test]
    fn statement_order_does_not_change_attr() {
        fn shuffle(stmts: &mut Vec<Stmt>, seed: &mut u64) {
            for i in (1..stmts.len()).rev() {
                *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                stmts.swap(i, (*seed >> 33) as usize % (i + 1));
            }
            for s in stmts.iter_mut() {
                match s {
                    Stmt::Component(c) => shuffle(&mut c.body, seed),
                    Stmt::ComponentDef(c) => shuffle(&mut c.body, seed),
                    Stmt::PolicyPack(p) => shuffle(&mut p.body, seed),
                    _ => {}
                }
            }
        }
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let program = crate::loader::load_program(&[root.join("dform.df")]).unwrap();
        for env in ["staging", "prod"] {
            let extra = [input("env", Value::Str(env.into()))];
            let attrs = |p: &Program| {
                let (r, _) = eval(p, &extra).unwrap();
                facts_of(&r, "attr")
            };
            let base = attrs(&program);
            assert!(base.len() > 40, "{}", base.len());
            let mut seed = 7u64;
            for _ in 0..8 {
                let mut p = program.clone();
                shuffle(&mut p.statements, &mut seed);
                assert_eq!(attrs(&p), base, "env={env}");
            }
        }
    }

    /// E §2.4 syntax: `@default` / `@override` after a value, and after a
    /// `resource` or `settings` header for every leaf without its own.
    #[test]
    fn ranks_in_blocks() {
        let (r, violations) = run(
            "resource net.vpc main @default {
               cidr = \"10.0.0.0/16\"
               tags = { env: dev, team: net }
               public = true @override
             }.
             resource net.vpc main {
               cidr = \"10.1.0.0/16\"
               tags = { team: platform }
               public = false
             }.
             env_name(dev). env_name(prod).
             settings E @default { days = 3, zones = [a] } :- env_name(E).
             settings prod { days = 14 }.
             got(E, D, Z) :- setting(E, days, D), setting(E, zones, Z).",
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(
            facts_of(&r, "attr").into_iter().filter(|a| a.contains("net.vpc")).collect::<Vec<_>>(),
            vec![
                "attr(\"net.vpc\", \"main\", \"cidr\", \"10.1.0.0/16\")".to_string(),
                "attr(\"net.vpc\", \"main\", \"public\", true)".to_string(),
                "attr(\"net.vpc\", \"main\", \"tags\", {env: \"dev\", team: \"platform\"})".to_string(),
            ]
        );
        assert_eq!(
            facts_of(&r, "got"),
            vec!["got(\"dev\", 3, [\"a\"])".to_string(), "got(\"prod\", 14, [\"a\"])".to_string()]
        );
    }

    /// dform.df's settings are a `@default` layer plus per-environment
    /// blocks (E §7.1). Every environment compiles to exactly the resources
    /// the three copied blocks it replaced did.
    #[test]
    fn dform_df_default_layer_matches_the_copied_blocks() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let src = std::fs::read_to_string(root.join("dform.df")).unwrap();
        let start = src.find("env_name(staging).").unwrap();
        let end = src.find("import \"modules/network.df\".").unwrap();
        let copied = "type_lattice(settings, audit.sinks, set).
            settings prod {
              network.main.vpc_net = inet(\"10.20.0.0/16\")
              network.peer.vpc_net = inet(\"10.21.0.0/16\")
              db = { backup_days: 14, multi_az: true }
              k8s = { private_api: true, nodepool: { min: 3, max: 10 } }
              audit.sinks += [\"cloudwatch\"]
            }.
            settings staging {
              network.main.vpc_net = inet(\"10.50.0.0/16\")
              network.peer.vpc_net = inet(\"10.60.0.0/16\")
              db = { backup_days: 3, multi_az: false }
              k8s = { private_api: false, nodepool: { min: 1, max: 3 } }
            }.
            settings dev {
              network.main.vpc_net = inet(\"10.90.0.0/16\")
            }.
            ";
        let dir = std::env::temp_dir().join(format!("dform-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("dform.df");
        let old_src = format!("{}{}{}", &src[..start], copied, &src[end..]).replace("import \"", &format!("import \"{}/", root.display()));
        std::fs::write(&old, old_src).unwrap();
        let resources = |path: &std::path::Path, env: Option<&str>| {
            let program = crate::loader::load_program(&[path.to_path_buf()]).unwrap();
            let extra: Vec<Atom> = env.map(|e| input("env", Value::Str(e.into()))).into_iter().collect();
            let (r, violations) = eval(&program, &extra).unwrap();
            let docs: Vec<String> = crate::ir::compile_resources(r.facts.iter().cloned())
                .unwrap()
                .iter()
                .map(|r| format!("{} {} {}", r.addr.typ, r.addr.name, partition::fmt_value(&r.attrs)))
                .collect();
            (docs, violations, r.warnings)
        };
        for env in [None, Some("staging"), Some("prod"), Some("dev")] {
            let new = resources(&root.join("dform.df"), env);
            assert!(!new.0.is_empty());
            assert_eq!(new, resources(&old, env), "env={env:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A ranked Set is three shelves: a policy pack's `@default` set is
    /// replaced wholesale by a normal one, and same-shelf sets union.
    #[test]
    fn a_default_set_is_replaced_not_unioned() {
        let (r, violations) = run(
            "type_lattice(net.vpc, sgs, set).
             resource net.vpc a { sgs = [base] }.
             resource net.vpc b { }.
             policy_pack p {
               arg(T, N, sgs, [default_sg, ssh], default) :- want(T, N).
               arg(T, N, sgs, [audit]) :- want(T, N), N = \"a\".
             }.
             apply_policy p.",
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(
            facts_of(&r, "attr"),
            vec![
                "attr(\"net.vpc\", \"a\", \"sgs\", [\"audit\", \"base\"])".to_string(),
                "attr(\"net.vpc\", \"b\", \"sgs\", [\"default_sg\", \"ssh\"])".to_string(),
            ]
        );
    }

    /// DESIGN.org "Unknown predicates are silently empty": a misspelled
    /// predicate is a compile error naming it and the rule.
    #[test]
    fn undefined_predicate_is_an_error() {
        let err = run(
            "env(prod).
             resource net.vpc main { cidr = \"10.0.0.0/16\" } :- envv(prod).",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("undefined predicate envv/1"), "{err}");
        assert!(err.contains("want(\"net.vpc\", \"main\") :- envv(\"prod\")"), "{err}");
    }

    /// `extern p/N.` declares a provider-fed predicate; provider-injected
    /// predicates are defined with no rows.
    #[test]
    fn extern_and_provider_predicates_are_defined() {
        let (r, _) = run(
            "extern allowed/1.
             want(net.vpc, a).
             lonely(N) :- want(net.vpc, N), not allowed(N), not cloud_exists(net.vpc, N).",
        )
        .unwrap();
        assert_eq!(facts_of(&r, "lonely"), vec!["lonely(\"a\")".to_string()]);
    }

    /// DESIGN.org "Silent string-to-int coercion": arithmetic takes
    /// integers; conversions are explicit builtins.
    #[test]
    fn coercion_is_explicit() {
        let err = run("s(\"10\"). n(X) :- s(S), X = S + 1.").unwrap_err().to_string();
        assert!(err.contains("add(\"10\", 1) is not defined"), "{err}");
        let err = run("n(X) :- X = to_int(\"abc\") + 1.").unwrap_err().to_string();
        assert!(err.contains("to_int(\"abc\") is not defined"), "{err}");
        let (r, _) = run(
            "s(\"10\").
             explicit(X) :- s(S), X = to_int(S) + 1.
             text(T) :- T = to_string(14).
             sizes(A, B, C) :- A = len([x, y]), B = len(\"héllo\"), C = len({k: 1}).
             cases(L, U) :- L = lower(\"AbC\"), U = upper(\"AbC\").
             parts(P) :- P = split(\"a,b,c\", \",\").
             joined(J) :- J = join([a, 1, true], \"-\").",
        )
        .unwrap();
        assert_eq!(facts_of(&r, "explicit"), vec!["explicit(11)".to_string()]);
        assert_eq!(facts_of(&r, "text"), vec!["text(\"14\")".to_string()]);
        assert_eq!(facts_of(&r, "sizes"), vec!["sizes(2, 5, 1)".to_string()]);
        assert_eq!(facts_of(&r, "cases"), vec!["cases(\"abc\", \"ABC\")".to_string()]);
        assert_eq!(facts_of(&r, "parts"), vec!["parts([\"a\", \"b\", \"c\"])".to_string()]);
        assert_eq!(facts_of(&r, "joined"), vec!["joined(\"a-1-true\")".to_string()]);
    }

    /// A builtin over a null is a content position: under the stuck
    /// simulation it records a stuck instance instead of deriving.
    #[test]
    fn a_builtin_over_a_null_is_stuck() {
        let program = crate::parser::parse_program(
            "want(net.vpc, a).
             id_len(N) :- want(net.vpc, A), N = len(ref(net.vpc, A, id)).",
        )
        .unwrap();
        let r = crate::sim::eval_sim(&program, &[], crate::schema::fake(), Default::default()).unwrap();
        assert!(r.facts.iter().all(|a| a.pred != "id_len"));
        assert!(r.sim.stuck.iter().any(|s| s.reason == "builtin len() over a null"), "{:?}", r.sim.stuck);
    }
}

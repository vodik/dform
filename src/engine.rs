use crate::ast::{Atom, Constraint, Lit, Program, RuleStmt, Stmt, Term};
use crate::circuit::{self, Circuit, Leaf, NodeId};
use crate::lattice::{self, Collapsed2, Lattice, Rank, RankedContribution, Shadowed, Witnesses};
use crate::lattice::{Truth, nulls_in};
use crate::partition::{self, Node};
use crate::stuck::{self, Stuck};
use crate::transform;
use crate::value::Value;
use anyhow::{Context, Result, anyhow, bail};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone)]
pub struct EvalResult {
    pub facts: BTreeSet<Atom>,
    pub warnings: Vec<String>,
    /// Rule instances that need a null's content (E §2.7 Rule 2) or read
    /// something undetermined (Rule 3); also derived as `stuck/4`.
    pub stuck: Vec<Stuck>,
    /// Provenance (E §3.1, DR-10): every fact above has a node here.
    pub circuit: Circuit,
}

/// The circuit's spelling of a ground fact.
pub fn circuit_fact(a: &Atom) -> circuit::Fact {
    circuit::Fact::new(
        &a.pred,
        a.args
            .iter()
            .map(|t| match t {
                Term::Val(v) => v.clone(),
                other => Value::Str(partition::fmt_term(other)),
            })
            .collect(),
    )
}

/// The aggregate marker Σ of the attribute aggregate (E §3.1).
const ATTR_SIGMA: &str = "Σattr";

/// The evaluator's side of the circuit: node ids by fact, so recording a
/// firing needs no copy of a tuple the circuit already has.
#[derive(Default)]
struct Prov {
    circuit: Circuit,
    ids: BTreeMap<Atom, NodeId>,
}

impl Prov {
    fn record(&mut self, a: &Atom, children: Vec<NodeId>, bindings: Vec<(String, Value)>) {
        match self.ids.get(a) {
            Some(&id) => self.circuit.fire(id, children, bindings),
            None => {
                let id = self
                    .circuit
                    .derive_with(circuit_fact(a), children, bindings);
                self.ids.insert(a.clone(), id);
            }
        }
    }

    fn given(&mut self, a: &Atom, leaf: Leaf) {
        let l = self.circuit.leaf(leaf);
        self.record(a, vec![l], vec![]);
    }

    fn rule(&mut self, id: String, text: &str) -> NodeId {
        self.circuit.name_rule(&id, text);
        self.circuit.leaf(Leaf::Rule { id })
    }

    fn absent(&mut self, a: &Atom) -> NodeId {
        self.circuit.leaf(Leaf::Absent {
            pattern: partition::fmt_atom(a),
        })
    }

    fn id(&self, a: &Atom) -> NodeId {
        self.ids[a]
    }
}

/// The leaf for a fact given to this run rather than stated by the program.
fn given_leaf(a: &Atom, externs: &BTreeSet<crate::ast::Extern>) -> Leaf {
    let text = partition::fmt_atom(a);
    match a.pred.as_str() {
        "input" | "data" => {
            let flag = if a.pred == "input" { "set" } else { "data" };
            let kv = match a.args.as_slice() {
                [Term::Val(k), Term::Val(v)] => {
                    let raw = |v: &Value| match v {
                        Value::Str(s) => s.clone(),
                        v => partition::fmt_value(v),
                    };
                    format!("{}={}", raw(k), raw(v))
                }
                _ => text,
            };
            Leaf::Input {
                source: format!("--{flag} {kv}"),
            }
        }
        p if p.starts_with("type_") => Leaf::Schema { span: text },
        p if externs.iter().any(|e| e.pred == p) => Leaf::Extern { call: text },
        _ => Leaf::World { event: text },
    }
}

/// Predicates the attribute aggregate derives; no rule may.
const AGGREGATE_OUTPUTS: [&str; 3] = ["attr", "attr_conflict", "attr_stuck"];

/// Schema facts that choose a path's lattice. They are read by the
/// aggregate, not by rules, so they must be facts.
const LATTICE_DECLS: [&str; 2] = ["type_lattice", "type_list_key"];

pub fn eval(program: &Program, extra_facts: &[Atom]) -> Result<(EvalResult, Vec<String>)> {
    // The program as it runs and the partition graph it is stratified by,
    // built in one place (`dform strata` prints the same graph).
    let compiled = partition::compile(program, extra_facts)?;
    let externs = compiled.externs;
    let mut facts: BTreeSet<Atom> = BTreeSet::new();
    let mut origins = Origins::default();
    let mut prov = Prov::default();
    for f in extra_facts {
        let g = ensure_ground(f)?;
        prov.given(&g, given_leaf(&g, &externs));
        facts.insert(g);
    }

    // Source facts are known by statement index until the AST has spans.
    let mut stmt_of: BTreeMap<Atom, usize> = BTreeMap::new();
    for (i, stmt) in compiled.statements.iter().enumerate() {
        if let Stmt::Fact(a) = stmt
            && let Ok(g) = ensure_ground(a)
        {
            stmt_of.entry(g).or_insert(i);
        }
    }
    let (rules, constraints, fact_atoms) = (compiled.rules, compiled.constraints, compiled.facts);
    for r in &rules {
        if AGGREGATE_OUTPUTS.contains(&r.head.pred.as_str()) {
            bail!(
                "{} is derived by the attribute aggregate; contribute with arg instead: {}",
                r.head.pred,
                partition::fmt_rule(r)
            );
        }
        if LATTICE_DECLS.contains(&r.head.pred.as_str()) {
            bail!(
                "{} must be a fact, not a rule: {}",
                r.head.pred,
                partition::fmt_rule(r)
            );
        }
    }

    for a in &fact_atoms {
        let g = ensure_ground(a)?;
        origins.note(&g, partition::fmt_atom(&g));
        let span = match stmt_of.get(&g) {
            Some(i) => format!("statement {i} ({})", g.pred),
            None => format!("compiler ({})", g.pred),
        };
        prov.given(&g, Leaf::Base { span });
        facts.insert(g);
    }

    check_defined(&rules, &constraints, &facts, &externs)?;

    // Stratified evaluation over the partition graph (E §2.6, F DR-12
    // revised). Every rule runs in the stratum of its head node.
    let graph = compiled.graph;
    let strata = match partition::stratify(&graph) {
        partition::Verdict::Stratified { strata } => strata,
        partition::Verdict::Rejected {
            scc,
            negative_edges,
        } => {
            bail!("{}", partition::cycle_error(&graph, &scc, &negative_edges))
        }
    };
    let rule_stratum: Vec<usize> = rules
        .iter()
        .map(|r| {
            strata
                .get(&partition::head_node(&r.head))
                .copied()
                .unwrap_or(0)
        })
        .collect();
    let rule_text: Vec<String> = rules.iter().map(partition::fmt_rule).collect();
    let rule_leaf: Vec<NodeId> = rule_text
        .iter()
        .enumerate()
        .map(|(i, t)| prov.rule(format!("r{i}"), t))
        .collect();
    let sigma = prov.rule(
        ATTR_SIGMA.into(),
        "attribute aggregate (lub_ranked, E §2.5)",
    );
    let aggregates: BTreeSet<String> = rules
        .iter()
        .filter(|r| partition::is_aggregate_head(&r.head))
        .map(|r| r.head.pred.clone())
        .chain(["attr".to_string()])
        .collect();
    let mut stucks: Vec<Stuck> = Vec::new();
    let known = RefCell::new(stuck::Known::default());
    let mut attrs = AttrAggregate::new(&strata);
    let max_stratum = rule_stratum.iter().copied().max().unwrap_or(0);
    for s in 0..=max_stratum {
        // Attribute groups whose contributors all sit below this stratum
        // are complete: collapse them before any rule here reads them.
        let ready = attrs.emit_ready(s, &mut facts, &origins, &known.borrow(), &mut prov, sigma)?;
        for st in ready {
            known.borrow_mut().add(&st);
            stucks.push(st);
        }
        let rules_s: Vec<(usize, &RuleStmt)> = rules
            .iter()
            .enumerate()
            .filter(|(i, _)| rule_stratum[*i] == s)
            .collect();
        if rules_s.is_empty() {
            continue;
        }
        let recs: Vec<Rec> = rules_s
            .iter()
            .map(|(i, r)| Rec {
                rule: *i,
                head: &r.head,
                text: &rule_text[*i],
                known: &known,
                aggregates: &aggregates,
                found: RefCell::new(Vec::new()),
            })
            .collect();

        // Naive iteration to the stratum's fixpoint. It terminates: facts
        // only grow, and a stratum derives finitely many unless a builtin
        // invents values without bound (`n(Y) :- n(X), Y = X + 1`). An
        // aggregate may share a stratum with its readers, so a stuck
        // instance found here is known to the next round (Rule 3).
        let mut seen = vec![0usize; recs.len()];
        let mut changed = true;
        while changed {
            changed = false;

            let snapshot: Vec<Atom> = facts.iter().cloned().collect();
            let mut derived: Vec<(usize, Derived)> = Vec::new();
            for ((i, r), rec) in rules_s.iter().zip(&recs) {
                derived.extend(eval_rule(r, &snapshot, rec)?.into_iter().map(|a| (*i, a)));
            }
            for (i, d) in derived {
                origins.note(&d.head, rule_text[i].clone());
                let mut children = vec![rule_leaf[i]];
                children.extend(d.used.iter().map(|k| prov.id(&snapshot[*k])));
                for a in &d.absent {
                    children.push(prov.absent(a));
                }
                prov.record(&d.head, children, d.bindings);
                if facts.insert(d.head) {
                    changed = true;
                }
            }
            for (rec, n) in recs.iter().zip(seen.iter_mut()) {
                let found = rec.found.borrow();
                for st in &found[*n..] {
                    known.borrow_mut().add(st);
                    changed = true;
                }
                *n = found.len();
            }
        }
        stucks.extend(recs.into_iter().flat_map(|r| r.found.into_inner()));
    }
    let ready = attrs.emit_ready(
        usize::MAX,
        &mut facts,
        &origins,
        &known.borrow(),
        &mut prov,
        sigma,
    )?;
    stucks.extend(ready);
    attrs.check_complete(&facts)?;

    // Constraints are checked against the final fact set.
    let snapshot: Vec<Atom> = facts.iter().cloned().collect();
    let mut violations = Vec::new();
    for (k, c) in constraints.iter().enumerate() {
        let head = Atom {
            pred: "deny".into(),
            args: vec![Term::Val(Value::Str(c.message.clone()))],
            record: None,
        };
        let text = partition::fmt_rule(&partition::constraint_rule(c));
        let rec = Rec {
            rule: rules.len() + k,
            head: &head,
            text: &text,
            known: &known,
            aggregates: &aggregates,
            found: RefCell::new(Vec::new()),
        };
        if constraint_violated(c, &snapshot, &rec)? {
            violations.push(c.message.clone());
        }
        stucks.extend(rec.found.into_inner());
    }
    // A compiler-generated companion (`__ref_dep`) is stuck exactly when
    // the contribution it shadows is.
    stucks.retain(|s| !s.head.pred.starts_with("__"));
    stucks.sort();
    stucks.dedup();
    for st in &stucks {
        let f = st.fact();
        let by = match st.rule {
            Some(i) if i < rule_leaf.len() => rule_leaf[i],
            Some(i) => prov.rule(format!("c{}", i - rules.len()), &st.text),
            None => sigma,
        };
        prov.record(&f, vec![by], vec![]);
        facts.insert(f);
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

    Ok((
        EvalResult {
            facts,
            warnings,
            stuck: stucks,
            circuit: prov.circuit,
        },
        violations,
    ))
}

/// One answer to `query`: the bindings, and the facts matched in body order.
pub type Answer = (BTreeMap<String, Value>, Vec<Atom>);

/// A conjunction evaluated against a final fact store (`dform query`,
/// `dform why`): every way to satisfy it, with the facts each matched, in
/// body order. Read-only: nothing is derived and nothing is recorded stuck.
pub fn query(body: &[Lit], facts: &BTreeSet<Atom>) -> Result<Vec<Answer>> {
    let snapshot: Vec<Atom> = facts.iter().cloned().collect();
    let head = Atom {
        pred: "query".into(),
        args: vec![],
        record: None,
    };
    let known = RefCell::new(stuck::Known::default());
    let aggregates = BTreeSet::new();
    let rec = Rec {
        rule: 0,
        head: &head,
        text: "query",
        known: &known,
        aggregates: &aggregates,
        found: RefCell::new(Vec::new()),
    };
    Ok(eval_body(body, &snapshot, &rec)?
        .into_iter()
        .map(|r| {
            let b = r.s.into_iter().filter(|(k, _)| !k.starts_with("__"));
            let used = r.used.iter().map(|k| snapshot[*k].clone()).collect();
            (b.collect(), used)
        })
        .collect())
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
        defined.contains(p)
            || is_builtin_pred(p)
            || matches!(p, "member" | "enumerate")
            || crate::loader::is_core_pred(p)
    };
    let bodies = rules
        .iter()
        .map(|r| (&r.body, partition::fmt_rule(r)))
        .chain(
            constraints
                .iter()
                .map(|c| (&c.body, partition::fmt_rule(&partition::constraint_rule(c)))),
        );
    let mut errors = Vec::new();
    for (body, text) in bodies {
        for lit in body {
            let (Lit::Pos(a) | Lit::Not(a)) = lit else {
                continue;
            };
            if !is_defined(&a.pred) {
                errors.push(format!(
                    "undefined predicate {}/{} in rule: {text}",
                    a.pred,
                    a.args.len()
                ));
            }
        }
    }
    if !errors.is_empty() {
        bail!(
            "{}\n(declare a predicate a provider feeds with `extern p/N.`)",
            errors.join("\n")
        );
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
        self.0
            .get(a)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
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
        let arg_nodes = strata
            .iter()
            .filter(|(n, _)| n.pred == "arg")
            .map(|(n, s)| (n.clone(), *s))
            .collect();
        AttrAggregate {
            arg_nodes,
            emitted: BTreeMap::new(),
        }
    }

    /// The first stratum at which group `(typ, path)` is complete.
    fn ready_at(&self, typ: &str, path: &str) -> usize {
        let node = Node {
            pred: "arg".into(),
            typ: Some(typ.into()),
            path: Some(path.into()),
        };
        self.arg_nodes
            .iter()
            .filter(|(n, _)| n.unifies(&node))
            .map(|(_, s)| s + 1)
            .max()
            .unwrap_or(0)
    }

    /// Collapse every complete group. Rule 3 per key: a group that a stuck
    /// contribution could still join is undetermined and is not collapsed;
    /// it and a Stuck cell are returned as stuck heads `attr(T, A, P, _)`,
    /// so their readers are undetermined too.
    fn emit_ready(
        &mut self,
        stratum: usize,
        facts: &mut BTreeSet<Atom>,
        origins: &Origins,
        known: &stuck::Known,
        prov: &mut Prov,
        sigma: NodeId,
    ) -> Result<Vec<Stuck>> {
        let lattices = declared_lattices(facts)?;
        let mut out = Vec::new();
        let mut stuck_groups = Vec::new();
        for (key, contribs) in groups(facts)? {
            if self.emitted.contains_key(&key) || self.ready_at(&key.0, &key.2) > stratum {
                continue;
            }
            let (typ, addr, path) = &key;
            let read = Atom {
                pred: "attr".into(),
                args: vec![
                    str_val(typ),
                    Term::Val(addr.clone()),
                    str_val(path),
                    Term::Wildcard,
                ],
                record: None,
            };
            let group_stuck = |nulls: BTreeSet<String>, reason: String| Stuck {
                rule: None,
                head: read.clone(),
                bindings: BTreeMap::new(),
                nulls,
                reason,
                text: format!("attr({typ}, {}, {path}, _)", partition::fmt_value(addr)),
            };
            if known.any(&read) {
                stuck_groups.push(group_stuck(
                    known.blocking(&read),
                    "a stuck rule instance may still contribute to this attribute".into(),
                ));
            } else {
                let lat = lattices
                    .get(&(typ.clone(), path.clone()))
                    .cloned()
                    .unwrap_or_else(|| infer_lattice(&contribs));
                let cell = collapse_group(&key, &contribs, &lat, origins);
                for a in &cell {
                    if a.pred == "attr_stuck"
                        && let Some(Term::Val(Value::List(ls))) = a.args.get(3)
                    {
                        let nulls = ls.iter().filter_map(|l| l.as_str().map(str::to_string));
                        stuck_groups.push(group_stuck(
                            nulls.collect(),
                            "contributions disagree until a null resolves".into(),
                        ));
                    }
                }
                // Σ over the group: every contribution that reached it.
                let children: Vec<NodeId> = std::iter::once(sigma)
                    .chain(contribs.iter().map(|(a, _, _)| prov.id(a)))
                    .collect();
                for a in &cell {
                    prov.record(a, children.clone(), vec![]);
                }
                out.extend(cell);
            }
            self.emitted
                .insert(key, contribs.into_iter().map(|(a, _, _)| a).collect());
        }
        facts.extend(out);
        Ok(stuck_groups)
    }

    /// Guard on the stratifier: no contribution arrived after its group was
    /// collapsed.
    fn check_complete(&self, facts: &BTreeSet<Atom>) -> Result<()> {
        for (key, contribs) in groups(facts)? {
            let now: Vec<Atom> = contribs.into_iter().map(|(a, _, _)| a).collect();
            if self.emitted.get(&key) != Some(&now) {
                bail!(
                    "internal: attribute {} {} {} gained a contribution after it was collapsed",
                    key.0,
                    partition::fmt_value(&key.1),
                    key.2
                );
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
    for a in facts
        .iter()
        .filter(|a| a.pred == "arg" && a.args.len() == 5)
    {
        let vals: Vec<&Value> = a
            .args
            .iter()
            .map(|t| match t {
                Term::Val(v) => Ok(v),
                _ => Err(anyhow!("internal: non-ground contribution")),
            })
            .collect::<Result<_>>()?;
        let (Some(typ), Some(path)) = (vals[0].as_str(), vals[2].as_str()) else {
            bail!(
                "contribution {} needs a string type and path",
                partition::fmt_atom(a)
            );
        };
        let Some(rank) = parse_rank(vals[4]) else {
            bail!(
                "contribution {}: rank must be default, normal or override",
                partition::fmt_atom(a)
            );
        };
        let (path, value) =
            transform::normalize_contribution(typ, path, Term::Val(vals[3].clone()));
        let value =
            eval_term(&value, &HashMap::new()).ok_or_else(|| anyhow!("internal: normalize"))?;
        out.entry((typ.to_string(), vals[1].clone(), path))
            .or_default()
            .push((a.clone(), rank, value));
    }
    Ok(out)
}

/// `type_lattice(T, P, flat|map|set)` and `type_list_key(T, P, Keys)` facts.
fn declared_lattices(facts: &BTreeSet<Atom>) -> Result<BTreeMap<(String, String), Lattice>> {
    let mut out = BTreeMap::new();
    for a in facts
        .iter()
        .filter(|a| LATTICE_DECLS.contains(&a.pred.as_str()))
    {
        let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(p)),
            Term::Val(k),
        ] = a.args.as_slice()
        else {
            bail!(
                "{}/3 expects (Type, Path, ...): {}",
                a.pred,
                partition::fmt_atom(a)
            );
        };
        let lat = match (a.pred.as_str(), k) {
            ("type_lattice", Value::Str(k)) if k == "flat" => Lattice::Flat,
            ("type_lattice", Value::Str(k)) if k == "map" => Lattice::Map(Box::new(Lattice::Flat)),
            ("type_lattice", Value::Str(k)) if k == "set" => Lattice::Set,
            ("type_list_key", Value::List(ks)) => Lattice::Keyed {
                keys: ks.iter().map(value_to_string).collect(),
                elem: Box::new(Lattice::Map(Box::new(Lattice::Flat))),
            },
            ("type_list_key", Value::Str(k)) => Lattice::Keyed {
                keys: vec![k.clone()],
                elem: Box::new(Lattice::Map(Box::new(Lattice::Flat))),
            },
            _ => bail!(
                "{}: unknown lattice {}",
                partition::fmt_atom(a),
                partition::fmt_value(k)
            ),
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
fn collapse_group(
    key: &GroupKey,
    contribs: &[Contribution],
    lat: &Lattice,
    origins: &Origins,
) -> Vec<Atom> {
    let (typ, addr, path) = key;
    let cells: Vec<RankedContribution> = contribs
        .iter()
        .enumerate()
        .map(|(i, (_, r, v))| (i as u32, *r, v.clone()))
        .collect();
    let head = |pred: &str, rest: Vec<Value>| Atom {
        pred: pred.into(),
        args: [str_val(typ), Term::Val(addr.clone()), str_val(path)]
            .into_iter()
            .chain(rest.into_iter().map(Term::Val))
            .collect(),
        record: None,
    };
    let witness = |w: u32| {
        let (a, r, v) = &contribs[w as usize];
        obj(vec![
            ("rank", Value::Str(rank_name(*r).into())),
            ("value", v.clone()),
            (
                "from",
                Value::List(origins.of(a).into_iter().map(Value::Str).collect()),
            ),
        ])
    };
    let witnesses = |ws: &Witnesses| Value::List(ws.iter().map(|w| witness(*w)).collect());
    let ctx = |extra: Vec<(&str, Value)>| {
        let mut kv = vec![
            ("type", Value::Str(typ.clone())),
            ("addr", addr.clone()),
            ("path", Value::Str(path.clone())),
        ];
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
        Collapsed2::Val {
            value, shadowed, ..
        } => {
            out.push(head("attr", vec![value]));
            shadowed
        }
        Collapsed2::Stuck {
            nulls, shadowed, ..
        } => {
            out.push(head(
                "attr_stuck",
                vec![Value::List(nulls.into_iter().map(Value::Str).collect())],
            ));
            shadowed
        }
        Collapsed2::Conflict {
            a,
            b,
            reason,
            witnesses: ws,
            shadowed,
            ..
        } => {
            let first = |w: &Witnesses| {
                w.iter()
                    .next()
                    .map(|w| witness(*w))
                    .unwrap_or(Value::Obj(BTreeMap::new()))
            };
            out.push(head("attr_conflict", vec![first(&a.1), first(&b.1)]));
            out.push(policy(
                "deny",
                "conflicting attribute contributions",
                ctx(vec![
                    ("reason", Value::Str(reason)),
                    ("witnesses", witnesses(&ws)),
                ]),
            ));
            shadowed
        }
    };
    for sh in shadowed {
        let (rank, what, ws) = match sh {
            Shadowed::Stuck {
                rank,
                nulls,
                witnesses,
            } => (
                rank,
                format!(
                    "undecided until {}",
                    nulls
                        .iter()
                        .map(|n| format!("?{n}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                witnesses,
            ),
            Shadowed::Conflict {
                rank,
                path,
                reason,
                witnesses,
            } => (rank, format!("{reason} at {path}"), witnesses),
        };
        out.push(policy(
            "warn",
            "attr_shadowed: contributions at a losing rank disagree and are overridden",
            ctx(vec![
                ("rank", Value::Str(rank_name(rank).into())),
                ("reason", Value::Str(what)),
                ("witnesses", witnesses(&ws)),
            ]),
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

pub fn value_to_json(v: &Value) -> serde_json::Value {
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

/// Rule 2's recorder for the rule being evaluated (E §2.7, F DR-2 revised):
/// every instance that needs a null's content is recorded here instead of
/// firing. `known` holds the stuck heads of lower strata, for Rule 3.
struct Rec<'a> {
    rule: usize,
    head: &'a Atom,
    text: &'a str,
    known: &'a RefCell<stuck::Known>,
    /// Predicates defined by an aggregate rule (and `attr`): a positive read
    /// of one of their stuck groups is undetermined.
    aggregates: &'a BTreeSet<String>,
    found: RefCell<Vec<Stuck>>,
}

impl Rec<'_> {
    fn stuck(
        &self,
        state: &HashMap<String, Value>,
        nulls: BTreeSet<String>,
        reason: impl Into<String>,
    ) {
        self.stuck_as(self.head, state, nulls, reason);
    }

    fn stuck_as(
        &self,
        head: &Atom,
        state: &HashMap<String, Value>,
        nulls: BTreeSet<String>,
        reason: impl Into<String>,
    ) {
        let s = Stuck {
            rule: Some(self.rule),
            head: stuck::head_pattern(head, state, eval_term),
            bindings: state
                .iter()
                .filter(|(k, _)| !k.starts_with("__"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            nulls,
            reason: reason.into(),
            text: self.text.to_string(),
        };
        let mut found = self.found.borrow_mut();
        if !found.contains(&s) {
            found.push(s);
        }
    }

    /// Rule 2 for a term: if a builtin inside it cannot evaluate because an
    /// argument carries a null, record the instance and say so.
    fn blocked(&self, t: &Term, state: &HashMap<String, Value>) -> bool {
        match blocked_by_null(t, state) {
            Some((name, nulls)) => {
                let why = if name == "scoped" || name == "ref" {
                    "resource address carries a null".to_string()
                } else {
                    format!("builtin {name}() over a null")
                };
                self.stuck(state, nulls, why);
                true
            }
            None => false,
        }
    }

    fn any_blocked<'t>(
        &self,
        ts: impl IntoIterator<Item = &'t Term>,
        state: &HashMap<String, Value>,
    ) -> bool {
        ts.into_iter().any(|t| self.blocked(t, state))
    }

    /// Three-valued equality: Unknown records the instance (Rule 2).
    fn eq(&self, a: &Value, b: &Value, state: &HashMap<String, Value>, what: &str) -> bool {
        match crate::lattice::eq3(a, b) {
            Truth::True => true,
            Truth::False => false,
            Truth::Unknown => {
                let mut nulls = nulls_in(a);
                nulls.extend(nulls_in(b));
                self.stuck(state, nulls, format!("{what} against an open/secret null"));
                false
            }
        }
    }
}

/// Builtins that carry nulls instead of reading them: the aggregates (whose
/// content positions `eval_rule_collect` decides) and the compiler's own.
fn forwards_nulls(name: &str) -> bool {
    matches!(
        name,
        "collect" | "collect_set" | "collect_list" | "count" | "__null" | "__label" | "__path"
    )
}

/// The innermost builtin application in `t` whose arguments are ground but
/// carry a null, with those nulls: every builtin argument is a content
/// position (Rule 2).
fn blocked_by_null(t: &Term, state: &HashMap<String, Value>) -> Option<(String, BTreeSet<String>)> {
    match t {
        Term::Func { name, args } => {
            if let Some(inner) = args.iter().find_map(|a| blocked_by_null(a, state)) {
                return Some(inner);
            }
            if forwards_nulls(name) {
                return None;
            }
            let mut nulls = BTreeSet::new();
            for a in args {
                nulls.extend(nulls_in(&eval_term(a, state)?));
            }
            (!nulls.is_empty()).then(|| (name.clone(), nulls))
        }
        Term::List(xs) => xs.iter().find_map(|x| blocked_by_null(x, state)),
        Term::Obj(m) => m.values().find_map(|x| blocked_by_null(x, state)),
        _ => None,
    }
}

/// A body atom as a pattern under `state`: bound terms are values, the
/// rest wildcards.
fn read_pattern(atom: &Atom, state: &HashMap<String, Value>) -> Atom {
    Atom {
        pred: atom.pred.clone(),
        args: atom
            .args
            .iter()
            .map(|t| match eval_term(t, state) {
                Some(v) => Term::Val(v),
                None => Term::Wildcard,
            })
            .collect(),
        record: None,
    }
}

/// One derived head with what it was derived from, for the circuit.
struct Derived {
    head: Atom,
    /// Indices into the `facts` the body was evaluated against.
    used: Vec<usize>,
    absent: Vec<Atom>,
    bindings: Vec<(String, Value)>,
}

fn eval_rule(rule: &RuleStmt, facts: &[Atom], rec: &Rec) -> Result<Vec<Derived>> {
    let collect = find_collect(&rule.head);
    if let Some((idx, kind)) = collect {
        return eval_rule_collect(rule, facts, idx, kind, rec);
    }

    let mut out = Vec::new();
    let rows = eval_body(&rule.body, facts, rec)?;
    for Row { s: b, used, absent } in rows {
        // Rule 2: an address argument is a content position. A head whose
        // address carries a null is stuck, not derived.
        if matches!(rule.head.pred.as_str(), "want" | "arg" | "adopt")
            && rule.head.args.len() >= 2
            && let Some(v) = eval_term(&rule.head.args[1], &b)
        {
            let nulls = nulls_in(&v);
            if !nulls.is_empty() {
                rec.stuck(&b, nulls, "resource address carries a null");
                continue;
            }
        }
        if rec.any_blocked(&rule.head.args, &b) {
            continue;
        }
        let head = instantiate_atom(&rule.head, &b)
            .with_context(|| format!("instantiate head {}", rule.head.pred))?;
        let mut bindings: Vec<(String, Value)> = b
            .into_iter()
            .filter(|(k, _)| !k.starts_with("__"))
            .collect();
        bindings.sort();
        out.push(Derived {
            head,
            used,
            absent,
            bindings,
        });
    }
    Ok(out)
}

#[derive(Debug, Copy, Clone)]
enum CollectKind {
    Set,
    List,
    Count,
}

/// An aggregate rule. Rule 2: the group key is a content position, and so
/// is the aggregated value of `count`; `collect_set`/`collect_list` forward
/// nulls. Rule 3: a group is undetermined, and not derived, when its key
/// unifies with a stuck instance of this rule or with a stuck head of a
/// predicate the body reads.
fn eval_rule_collect(
    rule: &RuleStmt,
    facts: &[Atom],
    idx: usize,
    kind: CollectKind,
    rec: &Rec,
) -> Result<Vec<Derived>> {
    let Term::Func { name: _, args } = &rule.head.args[idx] else {
        bail!("internal: collect idx not func");
    };
    if args.len() != 1 {
        bail!("collect*(...) must have exactly one argument");
    }
    let item_term = args[0].clone();

    let rows = eval_body(&rule.body, facts, rec)?;
    let mut groups_set: BTreeMap<Vec<Value>, BTreeSet<Value>> = BTreeMap::new();
    let mut groups_list: BTreeMap<Vec<Value>, Vec<Value>> = BTreeMap::new();
    // Σ over the group: every body fact and negation of every member.
    let mut group_prov: BTreeMap<Vec<Value>, (BTreeSet<usize>, BTreeSet<Atom>)> = BTreeMap::new();
    for Row { s: b, used, absent } in rows {
        if rec.any_blocked(&rule.head.args, &b) {
            continue;
        }
        let mut key = Vec::new();
        let mut key_nulls = BTreeSet::new();
        for (i, t) in rule.head.args.iter().enumerate() {
            if i == idx {
                continue;
            }
            let v = eval_term(t, &b).ok_or_else(|| anyhow!("non-ground head term"))?;
            key_nulls.extend(nulls_in(&v));
            key.push(v);
        }
        let item = eval_term(&item_term, &b).ok_or_else(|| anyhow!("non-ground collect item"))?;
        if !key_nulls.is_empty() {
            rec.stuck(&b, key_nulls, "aggregate group key carries a null");
            continue;
        }
        if matches!(kind, CollectKind::Count) && stuck::has_null(&item) {
            rec.stuck(&b, nulls_in(&item), "count over a null");
            continue;
        }
        let prov = group_prov.entry(key.clone()).or_default();
        prov.0.extend(used);
        prov.1.extend(absent);
        match kind {
            CollectKind::Set => {
                groups_set.entry(key).or_default().insert(item);
            }
            CollectKind::List | CollectKind::Count => {
                groups_list.entry(key).or_default().push(item);
            }
        }
    }

    // Rule 3: a stuck head of a body predicate makes the groups it could
    // feed undetermined.
    for lit in &rule.body {
        let Lit::Pos(a) = lit else { continue };
        if a.pred == "member" || a.pred == "enumerate" || is_builtin_pred(&a.pred) {
            continue;
        }
        let pat = read_pattern(a, &HashMap::new());
        for (p, nulls) in rec.known.borrow().matching(&pat) {
            let mut b = HashMap::new();
            for (t, v) in a.args.iter().zip(&p.args) {
                if let (Term::Var(x), Term::Val(v)) = (t, v) {
                    b.insert(x.clone(), v.clone());
                }
            }
            rec.stuck(
                &b,
                nulls,
                format!("aggregate over {}, which has a stuck instance", a.pred),
            );
        }
    }
    let undetermined: Vec<Atom> = rec.found.borrow().iter().map(|s| s.head.clone()).collect();

    let mut out = Vec::new();
    let mut emit_group = |key: Vec<Value>, mut items: Vec<Value>| {
        let (used, absent) = group_prov.remove(&key).unwrap_or_default();
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
        let group = Atom {
            pred: rule.head.pred.clone(),
            args: args_out,
            record: None,
        };
        let mut key_pat = group.clone();
        key_pat.args[idx] = Term::Wildcard;
        if undetermined
            .iter()
            .any(|u| stuck::patterns_unify(u, &key_pat))
        {
            return;
        }
        out.push(Derived {
            head: group,
            used: used.into_iter().collect(),
            absent: absent.into_iter().collect(),
            bindings: vec![],
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

fn constraint_violated(c: &Constraint, facts: &[Atom], rec: &Rec) -> Result<bool> {
    let rows = eval_body(&c.body, facts, rec)?;
    Ok(!rows.is_empty())
}

/// One way to satisfy a body: the bindings, and for provenance the facts
/// it matched (indices into the `facts` it was evaluated against) and the
/// negations that held.
struct Row {
    s: HashMap<String, Value>,
    used: Vec<usize>,
    absent: Vec<Atom>,
}

impl Row {
    fn with(&self, s: HashMap<String, Value>) -> Row {
        Row {
            s,
            used: self.used.clone(),
            absent: self.absent.clone(),
        }
    }
}

fn eval_body(body: &[Lit], facts: &[Atom], rec: &Rec) -> Result<Vec<Row>> {
    let mut states: Vec<Row> = vec![Row {
        s: HashMap::new(),
        used: Vec::new(),
        absent: Vec::new(),
    }];
    for lit in body {
        let mut next = Vec::new();
        match lit {
            Lit::Pos(atom) => {
                if atom.pred == "member" || atom.pred == "enumerate" {
                    for row in &states {
                        if !rec.any_blocked(&atom.args, &row.s) {
                            let mut out = Vec::new();
                            eval_member_like(atom, &row.s, &mut out, rec)?;
                            next.extend(out.into_iter().map(|s| row.with(s)));
                        }
                    }
                } else if is_builtin_pred(&atom.pred) {
                    for row in &states {
                        if !rec.any_blocked(&atom.args, &row.s) && eval_builtin_pred(atom, &row.s)?
                        {
                            next.push(row.with(row.s.clone()));
                        }
                    }
                } else {
                    for row in &states {
                        let s = &row.s;
                        if rec.any_blocked(&atom.args, s) {
                            continue;
                        }
                        // Rule 3: a positive reader of an undetermined
                        // aggregate group is undetermined. It still reads
                        // the groups that were decided.
                        if rec.aggregates.contains(&atom.pred) {
                            let nulls = rec.known.borrow().blocking(&read_pattern(atom, s));
                            if !nulls.is_empty() {
                                rec.stuck(
                                    s,
                                    nulls,
                                    format!("reads undetermined aggregate {}", atom.pred),
                                );
                            }
                        }
                        for (k, f) in facts
                            .iter()
                            .enumerate()
                            .filter(|(_, x)| x.pred == atom.pred)
                        {
                            if let Some(s2) = unify_atom(atom, f, s, rec)? {
                                let mut r = row.with(s2);
                                r.used.push(k);
                                next.push(r);
                            }
                        }
                    }
                }
            }
            Lit::Not(atom) => {
                for row in &states {
                    let s = &row.s;
                    if rec.any_blocked(&atom.args, s) {
                        continue;
                    }
                    if atom.pred == "member" {
                        let holds = match atom.args.len() {
                            2 => eval_not_member2(atom, s, rec)?,
                            3 => eval_not_member3(atom, s)?,
                            _ => bail!("member/2 or member/3 expected"),
                        };
                        if holds {
                            next.push(row.with(s.clone()));
                        }
                        continue;
                    }
                    if atom.pred == "enumerate" {
                        // `enumerate/3` is a generator; `not enumerate(...)` is meaningless
                        // (it would require checking existence over an implicit domain).
                        bail!("negation not supported for enumerate/3");
                    }
                    if is_builtin_pred(&atom.pred) {
                        // Negation-as-failure for builtin predicates is just boolean negation.
                        if !eval_builtin_pred(atom, s)? {
                            next.push(row.with(s.clone()));
                        }
                        continue;
                    }
                    let grounded = ground_atom(atom, s)
                        .with_context(|| format!("unsafe negation: not {}(...)", atom.pred))?;
                    if eval_not(&grounded, facts, s, rec) {
                        let mut r = row.with(s.clone());
                        r.absent.push(grounded);
                        next.push(r);
                    }
                }
            }
            Lit::Eq(a, b) => {
                for row in &states {
                    if let Some(s2) = eval_eq(a, b, &row.s, rec)? {
                        next.push(row.with(s2));
                    }
                }
            }
            Lit::Neq(a, b) => {
                for row in &states {
                    if let Some(s2) = eval_neq(a, b, &row.s, rec)? {
                        next.push(row.with(s2));
                    }
                }
            }
            Lit::Gt(a, b) | Lit::Ge(a, b) | Lit::Lt(a, b) | Lit::Le(a, b) => {
                for row in &states {
                    if eval_cmp(lit, a, b, &row.s, rec)? {
                        next.push(row.with(row.s.clone()));
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

/// `not p(t)` over ground `t`. Rule 2: a pattern holding an open or secret
/// null, or a fact that equals it only Unknown-ly, needs content. Rule 3:
/// the negation is undetermined while a stuck head of `p` unifies with
/// `p(t)`. Fresh nulls are decided under the Unique Name Assumption.
fn eval_not(grounded: &Atom, facts: &[Atom], s: &HashMap<String, Value>, rec: &Rec) -> bool {
    let mut open = BTreeSet::new();
    for t in &grounded.args {
        if let Term::Val(v) = t
            && stuck::has_open_or_secret(v)
        {
            open.extend(nulls_in(v));
        }
    }
    if !open.is_empty() {
        rec.stuck(
            s,
            open,
            format!(
                "negation pattern not {}(..) holds an open/secret null",
                grounded.pred
            ),
        );
        return false;
    }
    let mut unknown = BTreeSet::new();
    for f in facts
        .iter()
        .filter(|f| f.pred == grounded.pred && f.args.len() == grounded.args.len())
    {
        let mut t = Truth::True;
        for (a, b) in f.args.iter().zip(&grounded.args) {
            let (Term::Val(a), Term::Val(b)) = (a, b) else {
                continue;
            };
            match crate::lattice::eq3(a, b) {
                Truth::False => {
                    t = Truth::False;
                    break;
                }
                Truth::Unknown => t = Truth::Unknown,
                Truth::True => {}
            }
        }
        match t {
            Truth::True => return false,
            Truth::Unknown => {
                for x in f.args.iter().chain(&grounded.args) {
                    if let Term::Val(v) = x {
                        unknown.extend(nulls_in(v));
                    }
                }
            }
            Truth::False => {}
        }
    }
    if !unknown.is_empty() {
        rec.stuck(
            s,
            unknown,
            format!("not {}(..) against an open/secret null", grounded.pred),
        );
        return false;
    }
    let known = rec.known.borrow();
    let nulls = known.blocking(grounded);
    if !nulls.is_empty() || known.any(grounded) {
        rec.stuck(
            s,
            nulls,
            format!(
                "not {}: {} has a stuck instance that may derive it",
                partition::fmt_atom(grounded),
                grounded.pred
            ),
        );
        return false;
    }
    true
}

fn is_builtin_pred(pred: &str) -> bool {
    matches!(pred, "inet_overlaps" | "inet_contains" | "ip_unspecified")
}

fn eval_builtin_pred(atom: &Atom, state: &HashMap<String, Value>) -> Result<bool> {
    // Builtin predicates are functions that return Bool.
    let Some(v) = eval_func(&atom.pred, &atom.args, state) else {
        bail!("unsafe builtin predicate {}(...)", atom.pred);
    };
    match v {
        Value::Bool(b) => Ok(b),
        other => bail!(
            "builtin predicate {} returned non-bool: {other:?}",
            atom.pred
        ),
    }
}

fn eval_member_like(
    atom: &Atom,
    state: &HashMap<String, Value>,
    out: &mut Vec<HashMap<String, Value>>,
    rec: &Rec,
) -> Result<()> {
    match atom.pred.as_str() {
        "member" => {
            if atom.args.len() == 2 {
                return eval_member2(atom, state, out, rec);
            }
            if atom.args.len() == 3 {
                return eval_member3(atom, state, out, rec);
            }
            bail!("member/2 or member/3 expected");
        }
        "enumerate" => {
            if atom.args.len() != 3 {
                bail!("enumerate/3 expected");
            }
            eval_member3(atom, state, out, rec)
        }
        _ => bail!("internal: eval_member_like called for non-member"),
    }
}

fn eval_member2(
    atom: &Atom,
    state: &HashMap<String, Value>,
    out: &mut Vec<HashMap<String, Value>>,
    rec: &Rec,
) -> Result<()> {
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe member: list is not ground"))?;
    if let Value::Null { .. } = &list_v {
        // Rule 2: member over a null list is a content position.
        rec.stuck(state, nulls_in(&list_v), "member/2 over a null list");
        return Ok(());
    }
    let Value::List(items) = list_v else {
        bail!("member/2 first argument must be a list");
    };
    for item in &items {
        let mut s2 = state.clone();
        if unify_term(&atom.args[1], item, &mut s2, rec)? {
            out.push(s2);
        }
    }
    Ok(())
}

fn eval_not_member2(atom: &Atom, state: &HashMap<String, Value>, rec: &Rec) -> Result<bool> {
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe not member: list is not ground"))?;
    if let Value::Null { .. } = &list_v {
        rec.stuck(state, nulls_in(&list_v), "not member/2 over a null list");
        return Ok(false);
    }
    let Value::List(items) = list_v else {
        bail!("member/2 first argument must be a list");
    };
    let item_v = eval_term(&atom.args[1], state)
        .ok_or_else(|| anyhow!("unsafe not member: item is not ground"))?;
    let mut unknown = BTreeSet::new();
    for x in &items {
        match crate::lattice::eq3(x, &item_v) {
            Truth::True => return Ok(false),
            Truth::Unknown => {
                unknown.extend(nulls_in(x));
                unknown.extend(nulls_in(&item_v));
            }
            Truth::False => {}
        }
    }
    if !unknown.is_empty() {
        rec.stuck(state, unknown, "not member/2: membership undecidable");
        return Ok(false);
    }
    Ok(true)
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
    rec: &Rec,
) -> Result<()> {
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe member: list is not ground"))?;
    if let Value::Null { .. } = &list_v {
        rec.stuck(state, nulls_in(&list_v), "member/3 over a null list");
        return Ok(());
    }
    let Value::List(items) = list_v else {
        bail!("member/3 first argument must be a list");
    };
    for (i, item) in items.iter().enumerate() {
        let mut s2 = state.clone();
        if !unify_term(&atom.args[1], &Value::Int(i as i64), &mut s2, rec)? {
            continue;
        }
        if !unify_term(&atom.args[2], item, &mut s2, rec)? {
            continue;
        }
        out.push(s2);
    }
    Ok(())
}

fn unify_atom(
    pattern: &Atom,
    fact: &Atom,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Result<Option<HashMap<String, Value>>> {
    if pattern.args.len() != fact.args.len() {
        return Ok(None);
    }
    let mut out = state.clone();
    for (p, f) in pattern.args.iter().zip(&fact.args) {
        let Term::Val(fv) = f else {
            bail!("internal: non-ground fact");
        };
        if !unify_term(p, fv, &mut out, rec)? {
            return Ok(None);
        }
    }
    Ok(Some(out))
}

fn unify_term(pat: &Term, fv: &Value, out: &mut HashMap<String, Value>, rec: &Rec) -> Result<bool> {
    match pat {
        Term::Val(v) => Ok(rec.eq(v, fv, out, "unification")),
        Term::Var(name) => {
            if let Some(bound) = out.get(name) {
                Ok(rec.eq(bound, fv, out, "unification"))
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
                if !unify_term(t, v, &mut tmp, rec)? {
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
                if !unify_term(t, v, &mut tmp, rec)? {
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
                return unify_term(&args[1], &Value::Str(suffix.to_string()), out, rec);
            }

            let pv = match eval_term(pat, out) {
                Some(v) => v,
                None => return Ok(false),
            };
            Ok(rec.eq(&pv, fv, out, "unification"))
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

fn eval_eq(
    a: &Term,
    b: &Term,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Result<Option<HashMap<String, Value>>> {
    if rec.any_blocked([a, b], state) {
        return Ok(None);
    }
    let mut out = state.clone();
    match (eval_term(a, &out), eval_term(b, &out)) {
        (Some(av), Some(bv)) => Ok(rec.eq(&av, &bv, &out, "=").then_some(out)),
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
                    let args: Vec<String> = args.iter().map(partition::fmt_value).collect();
                    bail!(
                        "{name}({}) is not defined for these arguments",
                        args.join(", ")
                    );
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
    let Term::Func { name, args } = t else {
        return None;
    };
    if let Some(inner) = args.iter().find_map(|a| failed_builtin(a, state)) {
        return Some(inner);
    }
    let vals: Option<Vec<Value>> = args.iter().map(|a| eval_term(a, state)).collect();
    let vals = vals?;
    eval_func(name, args, state)
        .is_none()
        .then(|| (name.clone(), vals))
}

fn eval_neq(
    a: &Term,
    b: &Term,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Result<Option<HashMap<String, Value>>> {
    if rec.any_blocked([a, b], state) {
        return Ok(None);
    }
    match (eval_term(a, state), eval_term(b, state)) {
        (Some(av), Some(bv)) => Ok(match crate::lattice::eq3(&av, &bv) {
            Truth::False => Some(state.clone()),
            Truth::True => None,
            Truth::Unknown => {
                let mut nulls = nulls_in(&av);
                nulls.extend(nulls_in(&bv));
                rec.stuck(state, nulls, "!= against an open/secret null");
                None
            }
        }),
        _ => bail!("unsafe !=: both sides must be ground"),
    }
}

fn eval_cmp(
    op_lit: &Lit,
    a: &Term,
    b: &Term,
    state: &HashMap<String, Value>,
    rec: &Rec,
) -> Result<bool> {
    if rec.any_blocked([a, b], state) {
        return Ok(false);
    }
    let Some(av) = eval_term(a, state) else {
        bail!("unsafe comparison: left not ground");
    };
    let Some(bv) = eval_term(b, state) else {
        bail!("unsafe comparison: right not ground");
    };
    // Rule 2: an ordering comparison is a content position.
    if stuck::has_null(&av) || stuck::has_null(&bv) {
        let mut nulls = nulls_in(&av);
        nulls.extend(nulls_in(&bv));
        rec.stuck(state, nulls, "ordering comparison over a null");
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
    // Rule 2: every builtin argument is a content position. A builtin
    // over a null has no value; the literal that needs it is stuck.
    if !forwards_nulls(name) {
        for a in args {
            if stuck::has_null(&eval_term(a, state)?) {
                return None;
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
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix as u32)
            };
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
        // The prelude's null for (T, A, P) (E §2.5): class and type come
        // from the schema row the rule was expanded from.
        "__null" if args.len() == 5 => {
            let t = eval_term(&args[0], state)?;
            let a = eval_term(&args[1], state)?;
            let p = eval_term(&args[2], state)?;
            let class = crate::value::NullClass::parse(eval_term(&args[3], state)?.as_str()?)?;
            let ty = eval_term(&args[4], state)?.as_str()?.to_string();
            Some(Value::Null {
                label: crate::value::null_label(t.as_str()?, &value_to_string(&a), p.as_str()?),
                class,
                ty,
            })
        }
        "__label" if args.len() == 3 => {
            let t = eval_term(&args[0], state)?;
            let a = eval_term(&args[1], state)?;
            let p = eval_term(&args[2], state)?;
            Some(Value::Str(crate::value::null_label(
                t.as_str()?,
                &value_to_string(&a),
                p.as_str()?,
            )))
        }
        // `ref(T, A, "a.b")` after the rewrite: walk the rest of the path
        // inside the top-level attribute's value.
        "__path" if args.len() == 2 => {
            let mut v = eval_term(&args[0], state)?;
            for seg in eval_term(&args[1], state)?.as_str()?.split('.') {
                let Value::Obj(mut m) = v else {
                    return None;
                };
                v = m.remove(seg)?;
            }
            Some(v)
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
            Some(Value::Str(cidrsubnet(
                &cidr,
                newbits as u32,
                netnum as u32,
            )?))
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
                Some(Value::Str(if name == "lower" {
                    s.to_lowercase()
                } else {
                    s.to_uppercase()
                }))
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
                Some(Value::List(
                    s.split(sep.as_str())
                        .map(|x| Value::Str(x.to_string()))
                        .collect(),
                ))
            }
            _ => None,
        },
        "join" => match args {
            [l, sep] => {
                let Value::List(xs) = eval_term(l, state)? else {
                    return None;
                };
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
        Value::Str(_)
        | Value::Int(_)
        | Value::Bool(_)
        | Value::Ip(_)
        | Value::IpNet { .. }
        | Value::IpRange { .. } => Some(value_to_string(v)),
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
        r.facts
            .iter()
            .filter(|a| a.pred == pred)
            .map(partition::fmt_atom)
            .collect()
    }

    /// DESIGN.org "Aggregates are not stratified": a consumer of an
    /// aggregate used to see every partial result mid-fixpoint.
    #[test]
    fn aggregate_consumer_sees_one_complete_result() {
        let (r, _) = run("n(1).
             n(2) :- n(1).
             n(3) :- n(2).
             all(collect_set(X)) :- n(X).
             snap(L) :- all(L).")
        .unwrap();
        assert_eq!(facts_of(&r, "snap"), vec!["snap([1, 2, 3])".to_string()]);
    }

    /// A cycle through negation is a compile error naming the cycle with
    /// the text of every rule on it.
    #[test]
    fn negative_cycle_is_an_error_with_rule_text() {
        let err = run("q(1).
             p(X) :- q(X), not r(X).
             r(X) :- p(X).")
        .unwrap_err()
        .to_string();
        assert!(err.contains("negative cycle"), "{err}");
        assert!(err.contains("p(X) :- q(X), not r(X)"), "{err}");
    }

    /// Rules run in the stratum of their head's partition node: a `want`
    /// of one type may negate, or aggregate over, `want` of another type.
    #[test]
    fn want_is_partitioned_by_type() {
        let (r, _) = run("want(net.subnet, a).
             want(net.subnet, b).
             subnets(collect_set(S)) :- want(net.subnet, S).
             want(db.postgres, db) :- subnets(L), member(L, a), not want(net.subnet, c).")
        .unwrap();
        assert!(facts_of(&r, "want").contains(&"want(\"db.postgres\", \"db\")".to_string()));
    }

    fn input(k: &str, v: Value) -> Atom {
        Atom {
            pred: "input".into(),
            args: vec![str_val(k), Term::Val(v)],
            record: None,
        }
    }

    /// Two rules set one attribute to different values: no attr fact, an
    /// attr_conflict, and a deny naming the resource, the path and both
    /// contributing rules.
    #[test]
    fn conflicting_contributions_derive_a_deny_naming_every_witness() {
        let (r, violations) = run("resource net.vpc main { cidr = \"10.0.0.0/16\" }.
             arg(net.vpc, main, cidr, \"10.1.0.0/16\") :- want(net.vpc, main).")
        .unwrap();
        assert!(
            facts_of(&r, "attr").iter().all(|a| !a.contains("cidr")),
            "{:?}",
            facts_of(&r, "attr")
        );
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
            .flat_map(|w| {
                w["from"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|f| f.as_str().unwrap().to_string())
            })
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
        let (r, violations) = run("type_lattice(settings, sinks, set).
             settings prod { sinks += [\"cloudwatch\"], days = 14 }.
             setting_add(prod, sinks, [\"s3\"]).
             component network main { output(ids, [a, b]). }.
             got(S, D) :- setting(prod, sinks, S), setting(prod, days, D).
             ids(L) :- output(network.main, ids, L).
             deny(\"no audit\") :- not setting(prod, audit, true).")
        .unwrap();
        assert_eq!(
            facts_of(&r, "got"),
            vec!["got([\"cloudwatch\", \"s3\"], 14)".to_string()]
        );
        assert_eq!(facts_of(&r, "ids"), vec!["ids([\"a\", \"b\"])".to_string()]);
        assert_eq!(violations, vec!["no audit".to_string()]);
    }

    /// E §2.5 path normalization: `tags.team` contributes `{team: V}` to
    /// `tags`, which is a Map, so it meets the block's other tags per leaf.
    #[test]
    fn dotted_path_contributes_to_its_top_level_attribute() {
        let (r, _) = run("resource net.vpc main { tags = { env: dev } }.
             arg(net.vpc, main, \"tags.team\", platform) :- want(net.vpc, main).")
        .unwrap();
        assert_eq!(
            facts_of(&r, "attr"),
            vec![
                "attr(\"net.vpc\", \"main\", \"tags\", {env: \"dev\", team: \"platform\"})"
                    .to_string()
            ]
        );
    }

    /// Ranks in the core form: the winning rank decides; two disagreeing
    /// defaults under a normal value are a warning, not an error (F DR-9).
    #[test]
    fn highest_rank_wins_and_a_shadowed_disagreement_warns() {
        let (r, violations) = run("want(net.vpc, main).
             arg(net.vpc, main, cidr, \"10.0.0.0/16\", default).
             arg(net.vpc, main, cidr, \"10.9.0.0/16\", default).
             arg(net.vpc, main, cidr, \"10.1.0.0/16\").")
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(
            facts_of(&r, "attr"),
            vec!["attr(\"net.vpc\", \"main\", \"cidr\", \"10.1.0.0/16\")".to_string()]
        );
        assert_eq!(r.warnings.len(), 1);
        assert!(
            r.warnings[0].starts_with("attr_shadowed"),
            "{:?}",
            r.warnings
        );
    }

    /// A null contribution (a computed attribute at plan time) is carried
    /// through the aggregate as a value.
    #[test]
    fn a_null_contribution_is_carried_through_attr() {
        let null = Value::Null {
            label: "net.vpc/main#id".into(),
            class: crate::value::NullClass::Fresh,
            ty: "string".into(),
        };
        let program = crate::parser::parse_program(
            "want(net.subnet, a).
             arg(net.subnet, a, vpc_id, V) :- input(vpc, V).
             seen(V) :- arg(net.subnet, a, vpc_id, V).",
        )
        .unwrap();
        let (r, violations) = eval(&program, &[input("vpc", null.clone())]).unwrap();
        assert!(violations.is_empty());
        assert_eq!(
            facts_of(&r, "seen"),
            vec![format!("seen({})", partition::fmt_value(&null))]
        );
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
        fn shuffle(stmts: &mut [Stmt], seed: &mut u64) {
            for i in (1..stmts.len()).rev() {
                *seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
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
        let (r, violations) = run("resource net.vpc main @default {
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
             got(E, D, Z) :- setting(E, days, D), setting(E, zones, Z).")
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(
            facts_of(&r, "attr")
                .into_iter()
                .filter(|a| a.contains("net.vpc"))
                .collect::<Vec<_>>(),
            vec![
                "attr(\"net.vpc\", \"main\", \"cidr\", \"10.1.0.0/16\")".to_string(),
                "attr(\"net.vpc\", \"main\", \"public\", true)".to_string(),
                "attr(\"net.vpc\", \"main\", \"tags\", {env: \"dev\", team: \"platform\"})"
                    .to_string(),
            ]
        );
        assert_eq!(
            facts_of(&r, "got"),
            vec![
                "got(\"dev\", 3, [\"a\"])".to_string(),
                "got(\"prod\", 14, [\"a\"])".to_string()
            ]
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
        let old_src = format!("{}{}{}", &src[..start], copied, &src[end..])
            .replace("import \"", &format!("import \"{}/", root.display()));
        std::fs::write(&old, old_src).unwrap();
        let resources = |path: &std::path::Path, env: Option<&str>| {
            let program = crate::loader::load_program(&[path.to_path_buf()]).unwrap();
            let extra: Vec<Atom> = env
                .map(|e| input("env", Value::Str(e.into())))
                .into_iter()
                .collect();
            let (r, violations) = eval(&program, &extra).unwrap();
            let docs: Vec<String> = crate::ir::compile_resources(
                r.facts.iter().cloned(),
                &crate::schema::Schema::default(),
            )
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "{} {} {}",
                    r.addr.typ,
                    r.addr.name,
                    partition::fmt_value(&r.attrs)
                )
            })
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
        let (r, violations) = run("type_lattice(net.vpc, sgs, set).
             resource net.vpc a { sgs = [base] }.
             resource net.vpc b { }.
             policy_pack p {
               arg(T, N, sgs, [default_sg, ssh], default) :- want(T, N).
               arg(T, N, sgs, [audit]) :- want(T, N), N = \"a\".
             }.
             apply_policy p.")
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
        let err = run("env(prod).
             resource net.vpc main { cidr = \"10.0.0.0/16\" } :- envv(prod).")
        .unwrap_err()
        .to_string();
        assert!(err.contains("undefined predicate envv/1"), "{err}");
        assert!(
            err.contains("want(\"net.vpc\", \"main\") :- envv(\"prod\")"),
            "{err}"
        );
    }

    /// `extern p/N.` declares a provider-fed predicate; provider-injected
    /// predicates are defined with no rows.
    #[test]
    fn extern_and_provider_predicates_are_defined() {
        let (r, _) = run("extern allowed/1.
             want(net.vpc, a).
             lonely(N) :- want(net.vpc, N), not allowed(N), not cloud_exists(net.vpc, N).")
        .unwrap();
        assert_eq!(facts_of(&r, "lonely"), vec!["lonely(\"a\")".to_string()]);
    }

    /// DESIGN.org "Silent string-to-int coercion": arithmetic takes
    /// integers; conversions are explicit builtins.
    #[test]
    fn coercion_is_explicit() {
        let err = run("s(\"10\"). n(X) :- s(S), X = S + 1.")
            .unwrap_err()
            .to_string();
        assert!(err.contains("add(\"10\", 1) is not defined"), "{err}");
        let err = run("n(X) :- X = to_int(\"abc\") + 1.")
            .unwrap_err()
            .to_string();
        assert!(err.contains("to_int(\"abc\") is not defined"), "{err}");
        let (r, _) = run("s(\"10\").
             explicit(X) :- s(S), X = to_int(S) + 1.
             text(T) :- T = to_string(14).
             sizes(A, B, C) :- A = len([x, y]), B = len(\"héllo\"), C = len({k: 1}).
             cases(L, U) :- L = lower(\"AbC\"), U = upper(\"AbC\").
             parts(P) :- P = split(\"a,b,c\", \",\").
             joined(J) :- J = join([a, 1, true], \"-\").")
        .unwrap();
        assert_eq!(facts_of(&r, "explicit"), vec!["explicit(11)".to_string()]);
        assert_eq!(facts_of(&r, "text"), vec!["text(\"14\")".to_string()]);
        assert_eq!(facts_of(&r, "sizes"), vec!["sizes(2, 5, 1)".to_string()]);
        assert_eq!(
            facts_of(&r, "cases"),
            vec!["cases(\"abc\", \"ABC\")".to_string()]
        );
        assert_eq!(
            facts_of(&r, "parts"),
            vec!["parts([\"a\", \"b\", \"c\"])".to_string()]
        );
        assert_eq!(
            facts_of(&r, "joined"),
            vec!["joined(\"a-1-true\")".to_string()]
        );
    }

    /// A builtin over a null is a content position (Rule 2): the instance
    /// is recorded as stuck instead of deriving.
    #[test]
    fn a_builtin_over_a_null_is_stuck() {
        let (r, _) = run_with(
            "want(net.vpc, a).
             id_len(N) :- want(net.vpc, A), N = len(ref(net.vpc, A, id)).",
            &crate::schema::fake().facts,
        )
        .unwrap();
        assert!(r.facts.iter().all(|a| a.pred != "id_len"));
        assert!(
            r.stuck
                .iter()
                .any(|s| s.reason == "builtin len() over a null"),
            "{:?}",
            r.stuck
        );
    }

    /// DESIGN.org "Fixpoint iteration cap is arbitrary": a derivation chain
    /// deeper than the old 200-iteration cap converges.
    #[test]
    fn a_300_deep_chain_converges() {
        let (r, _) = run("n(0).
             n(Y) :- n(X), X < 300, Y = X + 1.
             deepest(X) :- n(X), X >= 300.")
        .unwrap();
        assert_eq!(facts_of(&r, "n").len(), 301);
        assert_eq!(facts_of(&r, "deepest"), vec!["deepest(300)".to_string()]);
    }

    /// Dotted paths under one attribute meet in nested maps: two fields of
    /// `spec.template` do not conflict on `template`.
    #[test]
    fn nested_dotted_paths_merge_recursively() {
        let (r, violations) = run("resource k8s.deployment web {
               spec.replicas = 3,
               spec.template.metadata.labels = {app: web},
               spec.template.spec.containers = [{name: web}]
             }.")
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(
            facts_of(&r, "attr"),
            vec![
                "attr(\"k8s.deployment\", \"web\", \"spec\", {replicas: 3, template: {metadata: {labels: {app: \"web\"}}, spec: {containers: [{name: \"web\"}]}}})"
                    .to_string()
            ]
        );
    }

    /// A record atom in a resource body is rewritten to positional form like
    /// any other body, so the resource is derived (pngu.df's peerings).
    #[test]
    fn a_record_atom_in_a_resource_body_matches() {
        let (r, _) = run("decl peering { env, name }.
             peering{ env: prod, name: legacy }.
             resource net.peering Name { env = Env } :- peering{ env: Env, name: Name }.")
        .unwrap();
        assert_eq!(
            facts_of(&r, "attr"),
            vec!["attr(\"net.peering\", \"legacy\", \"env\", \"prod\")".to_string()]
        );
    }

    fn run_with(src: &str, extra: &[Atom]) -> Result<(EvalResult, Vec<String>)> {
        let program = crate::parser::parse_program(src)?;
        eval(&program, extra)
    }

    fn schema_facts(src: &str) -> Vec<Atom> {
        crate::schema::Schema::parse(src, "test").unwrap().facts
    }

    /// E §2.5 / F14: one null per (want, computed path), at rank normal for
    /// `computed` and `@default` for `optional_computed`; a program's value
    /// for an Optional+Computed path wins, and a ref reads the collapsed cell.
    #[test]
    fn optional_computed_mints_a_default_null() {
        let schema = schema_facts(
            "type_provider(vm, mock).
             type_attr(vm, id, string, [computed, id]).
             type_attr(vm, zone, string, [optional_computed]).",
        );
        let (r, violations) = run_with(
            "resource vm a { size = 1 }.
             resource vm b { zone = \"z1\" }.
             resource vm c { peer_zone = ref(vm, a, zone), other_zone = ref(vm, b, zone), a_id = ref(vm, a, id) }.",
            &schema,
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        let attrs = facts_of(&r, "attr");
        for want in [
            "attr(\"vm\", \"a\", \"zone\", ?vm/a#zone:Open)",
            "attr(\"vm\", \"a\", \"id\", ?vm/a#id:Fresh)",
            "attr(\"vm\", \"b\", \"zone\", \"z1\")",
            "attr(\"vm\", \"c\", \"peer_zone\", ?vm/a#zone:Open)",
            "attr(\"vm\", \"c\", \"other_zone\", \"z1\")",
            "attr(\"vm\", \"c\", \"a_id\", ?vm/a#id:Fresh)",
        ] {
            assert!(
                attrs.contains(&want.to_string()),
                "{want} not in {attrs:#?}"
            );
        }
        // The user's value beat the @default null without a conflict or a
        // shadowed warning (the null is alone on its shelf).
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        let docs = crate::ir::compile_resources(
            r.facts.iter().cloned(),
            &crate::schema::Schema::from_facts(&schema).unwrap(),
        )
        .unwrap();
        // assemble drops computed paths and a zone the provider will pick.
        let doc = |n: &str| {
            let d = docs.iter().find(|d| d.addr.name == n).unwrap();
            partition::fmt_value(&d.attrs)
        };
        assert_eq!(doc("a"), "{size: 1}");
        assert_eq!(doc("b"), "{zone: \"z1\"}");
    }

    /// The mock Kubernetes schema: `metadata.name` is Optional+Computed, so
    /// a Deployment without a name carries `?k8s.deployment/api#metadata.name`
    /// and a Service that names one reads it.
    #[test]
    fn k8s_metadata_name_is_a_default_null_until_set() {
        let schema = crate::schema::load_provider("k8s").unwrap().facts;
        let (r, _) = run_with(
            "resource k8s.deployment api { metadata.namespace = \"shop\" }.
             resource k8s.deployment web { metadata.name = \"web\" }.
             resource k8s.service api { spec.selector.app = ref(k8s.deployment, api, \"metadata.name\"),
                                        spec.selector.web = ref(k8s.deployment, web, \"metadata.name\") }.",
            &schema,
        )
        .unwrap();
        let attrs = facts_of(&r, "attr");
        let get = |addr: &str, path: &str| {
            attrs
                .iter()
                .find(|a| {
                    a.starts_with(&format!(
                        "attr(\"{}\", \"{}\", \"{path}\"",
                        addr.split(' ').next().unwrap(),
                        addr.split(' ').nth(1).unwrap()
                    ))
                })
                .cloned()
                .unwrap_or_default()
        };
        assert!(
            get("k8s.deployment api", "metadata")
                .contains("name: ?k8s.deployment/api#metadata.name:Fresh")
        );
        assert!(get("k8s.deployment web", "metadata").contains("name: \"web\""));
        let svc = get("k8s.service api", "spec");
        assert!(
            svc.contains("app: ?k8s.deployment/api#metadata.name:Fresh"),
            "{svc}"
        );
        assert!(svc.contains("web: \"web\""), "{svc}");
    }

    /// aws-mock's Optional+Computed attributes, the Terraform shape.
    #[test]
    fn aws_optional_computed_is_the_programs_when_set() {
        let schema = crate::schema::load_provider("aws-mock").unwrap().facts;
        let (r, violations) = run_with(
            "resource aws_vpc main { cidr_block = \"10.0.0.0/16\" }.
             resource aws_security_group web { vpc_id = ref(aws_vpc, main, id) }.",
            &schema,
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        let attrs = facts_of(&r, "attr");
        assert!(
            attrs.contains(
                &"attr(\"aws_vpc\", \"main\", \"cidr_block\", \"10.0.0.0/16\")".to_string()
            ),
            "{attrs:#?}"
        );
        assert!(attrs.contains(&"attr(\"aws_security_group\", \"web\", \"name\", ?aws_security_group/web#name:Open)".to_string()), "{attrs:#?}");
        assert!(
            attrs.contains(
                &"attr(\"aws_security_group\", \"web\", \"vpc_id\", ?aws_vpc/main#id:Fresh)"
                    .to_string()
            ),
            "{attrs:#?}"
        );
    }

    /// A plain `computed` path is the provider's: writing it is a compile
    /// error naming the resource and the path.
    #[test]
    fn writing_a_computed_path_is_an_error() {
        let schema = schema_facts(
            "type_provider(vm, mock).
             type_attr(vm, id, string, [computed, id]).
             type_attr(vm, meta.uid, string, [computed, id]).",
        );
        let err = run_with("resource vm a { id = \"x\" }.", &schema)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("resource vm \"a\": attribute id is computed"),
            "{err}"
        );
        let err = run_with("resource vm a { meta.uid = \"x\" }.", &schema)
            .unwrap_err()
            .to_string();
        assert!(err.contains("attribute meta.uid is computed"), "{err}");
    }

    /// Round 0 (E Rule 4): a resource the world has resolves its computed
    /// attributes through the identity mapping; a secret never does.
    #[test]
    fn round_zero_resolves_through_identity() {
        let mut extra = schema_facts(
            "type_provider(vm, mock).
             type_attr(vm, id, string, [computed, id]).
             type_attr(vm, pw, string, [computed, sensitive]).",
        );
        let s = |x: &str| Term::Val(Value::Str(x.into()));
        extra.push(Atom {
            pred: "identity".into(),
            args: vec![s("vm"), s("a"), s("remote-a")],
            record: None,
        });
        extra.push(Atom {
            pred: "world_attr".into(),
            args: vec![s("vm"), s("remote-a"), s("id"), s("vm-123")],
            record: None,
        });
        let (r, _) = run_with(
            "resource vm a { size = 1 }.
             resource vm b { peer = ref(vm, a, id), secret = ref(vm, a, pw) }.",
            &extra,
        )
        .unwrap();
        let attrs = facts_of(&r, "attr");
        assert!(
            attrs.contains(&"attr(\"vm\", \"b\", \"peer\", \"vm-123\")".to_string()),
            "{attrs:#?}"
        );
        assert!(
            attrs.contains(&"attr(\"vm\", \"b\", \"secret\", ?vm/a#pw:Secret)".to_string()),
            "{attrs:#?}"
        );
        assert!(
            attrs.contains(&"attr(\"vm\", \"b\", \"id\", ?vm/b#id:Fresh)".to_string()),
            "{attrs:#?}"
        );
    }

    fn repo_file(rel: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
    }

    /// Evaluate a repository program against a provider schema, and its
    /// sections (E §2.7) on an empty world.
    fn run_file(
        rel: &str,
        schema: &crate::schema::Schema,
        extra: &[Atom],
    ) -> (EvalResult, Vec<String>, stuck::Sections) {
        let program = crate::loader::load_program(&[repo_file(rel)]).unwrap();
        let mut extra = extra.to_vec();
        extra.extend(schema.facts.clone());
        let (r, violations) = eval(&program, &extra).unwrap();
        let docs = crate::ir::compile_resources(r.facts.iter().cloned(), schema)
            .unwrap()
            .into_iter()
            .map(|d| ((d.addr.typ, d.addr.name), d.attrs))
            .collect();
        let sections = stuck::sections(&r.stuck, &r.facts, &docs, schema);
        (r, violations, sections)
    }

    /// Orchestrator regression (the mock-provider hand-back): `format` over a
    /// ref to a computed attribute used to plan the constant
    /// "ref(net.vpc,v,id)-x". It is a content position: the rule is stuck,
    /// recorded as `stuck/4`, and derives nothing.
    #[test]
    fn format_over_a_computed_ref_is_stuck() {
        let (r, violations) = run_with(
            "resource net.vpc v { cidr = \"10.0.0.0/16\" }.
             resource net.subnet s { name = format(\"%s-x\", ref(net.vpc, v, id)) }.",
            &crate::schema::fake().facts,
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert!(
            !facts_of(&r, "attr").iter().any(|a| a.contains("\"name\"")),
            "{:?}",
            facts_of(&r, "attr")
        );
        let stuck = facts_of(&r, "stuck");
        assert_eq!(stuck.len(), 1, "{stuck:?}");
        assert!(
            stuck[0]
                .contains("arg(\\\"net.subnet\\\", \\\"s\\\", \\\"name\\\", _, \\\"normal\\\")")
                && stuck[0].contains("[\"net.vpc/v#id\"]"),
            "{stuck:?}"
        );
        assert_eq!(r.stuck[0].reason, "builtin format() over a null");
    }

    /// E §7.1 / F 4.1: every ref in dform.df is to a fresh `id` and is
    /// forwarded, so nothing is stuck and all 14 resources are definite.
    /// (Was sim.rs's dform_df_under_nulls_is_single_phase_...)
    #[test]
    fn dform_df_under_nulls_is_single_phase() {
        let (r, violations, s) = run_file(
            "dform.df",
            &crate::schema::fake(),
            &[input("env", Value::Str("prod".into()))],
        );
        assert!(violations.is_empty(), "{violations:?}");
        assert!(r.stuck.is_empty(), "{:?}", r.stuck);
        assert_eq!(facts_of(&r, "want").len(), 14);
        assert!(s.pending.is_empty() && s.pending_groups.is_empty() && s.undetermined.is_empty());
    }

    /// The other example programs under nulls: single-phase, nothing stuck.
    #[test]
    fn other_examples_have_no_stuck_instances() {
        let prod = [input("env", Value::Str("prod".into()))];
        let cases: [(&str, crate::schema::Schema, &[Atom]); 4] = [
            ("dform-advanced.df", crate::schema::fake(), &prod),
            ("pngu.df", crate::schema::gke(), &prod),
            ("examples/decl_demo.df", crate::schema::fake(), &[]),
            ("examples/adopt_demo.df", crate::schema::fake(), &prod),
        ];
        for (file, schema, extra) in cases {
            let (r, _, s) = run_file(file, &schema, extra);
            assert!(r.stuck.is_empty(), "{file}: {:?}", r.stuck);
            assert!(s.pending.is_empty(), "{file}: {:?}", s.pending);
        }
    }

    /// E §7.4 / F 4.4 per key: three definite, three pending on the
    /// cluster's endpoint and ca (the kubernetes provider's configuration),
    /// one pending group (a nodepool per zone), one undetermined policy. The
    /// deletion_protection policy is decided (under coarse Rule 3 it was
    /// spuriously undetermined). (Was sim.rs's
    /// gke_two_phase_rule3_coarse_fires_spuriously.)
    #[test]
    fn gke_two_phase_sections_per_key() {
        let (r, violations, s) = run_file(
            "examples/adversarial/gke_two_phase.df",
            &crate::schema::gke(),
            &[],
        );
        assert!(violations.is_empty(), "{violations:?}");
        let pending: Vec<String> = s.pending.keys().map(|(t, a)| format!("{t} {a}")).collect();
        assert_eq!(
            pending,
            [
                "k8s.deployment api",
                "k8s.namespace pngu",
                "k8s.secret db_credentials"
            ]
        );
        for on in s.pending.values() {
            assert_eq!(
                on.iter().cloned().collect::<Vec<_>>(),
                [
                    "gke_cluster/pngu#ca_certificate",
                    "gke_cluster/pngu#endpoint"
                ]
            );
        }
        assert_eq!(facts_of(&r, "want").len(), 6);
        assert_eq!(s.pending_groups.len(), 1, "{:?}", s.pending_groups);
        assert!(
            s.pending_groups[0]
                .starts_with("want(\"gke_nodepool\", _) x unknown, on ?gke_cluster/pngu#zones")
        );
        assert_eq!(s.undetermined.len(), 1, "{:?}", s.undetermined);
        assert!(s.undetermined[0].starts_with("deny \"cluster must be in at least two zones\""));
        assert!(
            !r.stuck.iter().any(|x| x.text.contains("deletion")),
            "{:?}",
            r.stuck
        );
    }

    /// adv2: per-key Rule 3. An unrelated negation and an unrelated
    /// aggregate over `want` are decided; a negation whose pattern unifies
    /// with the stuck nodepool head is undetermined. (Was sim.rs's
    /// adv2_rule3_coarse_vs_perkey.)
    #[test]
    fn adv2_rule3_per_key() {
        let (r, violations, s) = run_file(
            "examples/adversarial/adv2_rule3_coarse.df",
            &crate::schema::gke(),
            &[],
        );
        // Decided, and it holds: there is no deployment.
        assert!(
            violations
                .iter()
                .any(|v| v.starts_with("namespace without deployment")),
            "{violations:?}"
        );
        // Decided, and it does not hold: ns_count is [pngu].
        assert_eq!(facts_of(&r, "ns_count"), ["ns_count([\"pngu\"])"]);
        assert!(
            !violations
                .iter()
                .any(|v| v.contains("need the pngu namespace"))
        );
        assert_eq!(s.undetermined.len(), 1, "{:?}", s.undetermined);
        assert!(
            s.undetermined[0].starts_with("deny \"no nodepool in zone b\""),
            "{:?}",
            s.undetermined
        );
    }

    fn why_leaves(r: &EvalResult, fact: &str) -> BTreeSet<Leaf> {
        let f = r
            .facts
            .iter()
            .find(|a| partition::fmt_atom(a) == fact)
            .unwrap_or_else(|| {
                let all: Vec<String> = r.facts.iter().map(partition::fmt_atom).collect();
                panic!("no fact {fact} in {all:?}")
            });
        r.circuit
            .why(&circuit_fact(f))
            .into_iter()
            .flatten()
            .collect()
    }

    /// DR-10: provenance is always on; every fact the evaluator returns has
    /// a node in the circuit, and the circuit holds no other fact.
    #[test]
    fn every_fact_has_a_circuit_node() {
        let (r, _, _) = run_file(
            "dform.df",
            &crate::schema::fake(),
            &[input("env", Value::Str("prod".into()))],
        );
        for a in &r.facts {
            assert!(
                r.circuit.has(&circuit_fact(a)),
                "no node: {}",
                partition::fmt_atom(a)
            );
        }
        assert_eq!(r.circuit.facts().len(), r.facts.len());
    }

    #[test]
    fn a_firing_records_its_rule_body_facts_and_negations() {
        let (r, _) = run("p(1). p(2). s(2). q(X) :- p(X), not s(X).").unwrap();
        let why = why_leaves(&r, "q(1)");
        assert!(why.contains(&Leaf::Base {
            span: "statement 0 (p)".into()
        }));
        assert!(why.contains(&Leaf::Absent {
            pattern: "s(1)".into()
        }));
        let Some(Leaf::Rule { id }) = why.iter().find(|l| matches!(l, Leaf::Rule { .. })) else {
            panic!("no rule leaf: {why:?}");
        };
        assert_eq!(r.circuit.rule_text(id), Some("q(X) :- p(X), not s(X)"));
        let q = r
            .circuit
            .fact_id(&circuit_fact(
                &r.facts.iter().find(|a| a.pred == "q").unwrap().clone(),
            ))
            .unwrap();
        let crate::circuit::View::Fact { alts, .. } = r.circuit.view(q) else {
            panic!()
        };
        let crate::circuit::View::Times { bindings, .. } = r.circuit.view(alts[0]) else {
            panic!()
        };
        assert_eq!(bindings, &[("X".to_string(), Value::Int(1))]);
    }

    #[test]
    fn an_attribute_carries_every_contribution() {
        let src = r#"
            want(t, a).
            arg(t, a, tags, {x: 1}, normal).
            arg(t, a, tags, {y: 2}, normal) :- want(t, a).
        "#;
        let (r, _) = run(src).unwrap();
        let why = why_leaves(&r, r#"attr("t", "a", "tags", {x: 1, y: 2})"#);
        assert!(why.contains(&Leaf::Rule {
            id: ATTR_SIGMA.into()
        }));
        let bases = why
            .iter()
            .filter(|l| matches!(l, Leaf::Base { .. }))
            .count();
        assert_eq!(bases, 2, "the fact contribution and want: {why:?}");
    }

    #[test]
    fn a_given_fact_is_an_input_leaf() {
        let (r, _) = run_with(
            "env(E) :- input(env, E).",
            &[input("env", Value::Str("prod".into()))],
        )
        .unwrap();
        assert!(why_leaves(&r, "env(\"prod\")").contains(&Leaf::Input {
            source: "--set env=prod".into()
        }));
    }
}

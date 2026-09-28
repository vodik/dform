//! The stuck-rule simulation of proposal E §2.7 (Rules 1-3), run on the
//! existing naive engine.
//!
//! The engine is instrumented (see the `sim::` calls in engine.rs) so that,
//! when a simulation is active:
//!   * `ref(T, A, Attr)` evaluates to `Value::Null` with the class the schema
//!     gives it (Rule 1: nulls are values and are forwarded);
//!   * every content position of Rule 2 (comparison, builtin argument,
//!     `member` over a null list, an address argument, a negation pattern
//!     holding an open/secret null, and, when `agg_is_content` is set, an
//!     aggregated value) records a `Stuck` instance instead of failing;
//!   * every decided negation and every aggregate firing is recorded so that
//!     Rule 3 can be evaluated post hoc, both in E's predicate-coarse form
//!     and in the per-key refinement E sketched (§10, item 1).
//!
//! The computed-attribute prelude (E §2.5, third rule) is added as generated
//! rules `arg(T, A, P, ref(T, A, P)) :- want(T, A)` per schema row.

use crate::ast::{Atom, Lit, Program, RuleStmt, Stmt, Term};
use crate::lattice::{eq3, nulls_in, Truth};
use crate::partition::{fmt_atom, fmt_value, rule_short};
use crate::schema::Schema;
use crate::value::{NullClass, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone)]
pub struct Stuck {
    pub rule: usize,
    pub head: Atom,
    pub bindings: BTreeMap<String, Value>,
    pub nulls: BTreeSet<String>,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct NegEvent {
    pub rule: usize,
    pub head_pred: String,
    pub grounded: Atom,
}

#[derive(Debug, Clone)]
pub struct AggEvent {
    pub rule: usize,
    pub head_pred: String,
    pub body_preds: Vec<Atom>,
}

#[derive(Debug, Clone, Default)]
pub struct SimOpts {
    /// Rule 2 as literally written: "an aggregate's ... aggregated value" is a
    /// content position. E §7.2 contradicts this for collect_set; the flag
    /// lets both readings run.
    pub agg_is_content: bool,
}

#[derive(Debug, Default)]
pub struct Sim {
    pub schema: Schema,
    pub opts: SimOpts,
    pub stuck: Vec<Stuck>,
    pub negs: Vec<NegEvent>,
    pub aggs: Vec<AggEvent>,
    pub current: Option<(usize, Atom)>,
    pub rules: Vec<RuleStmt>,
}

thread_local! {
    static SIM: RefCell<Option<Sim>> = const { RefCell::new(None) };
}

pub fn active() -> bool {
    SIM.with(|s| s.borrow().is_some())
}

pub fn with<R>(f: impl FnOnce(&mut Sim) -> R) -> Option<R> {
    SIM.with(|s| s.borrow_mut().as_mut().map(f))
}

pub fn set_current(rule: usize, head: &Atom) {
    with(|s| s.current = Some((rule, head.clone())));
}

pub fn null_class(typ: &str, attr: &str) -> Option<NullClass> {
    with(|s| s.schema.class_of(typ, attr)).flatten()
}

pub fn null_for(typ: &str, name: &str, attr: &str) -> Option<Value> {
    let class = null_class(typ, attr)?;
    Some(Value::Null { label: format!("{typ}/{name}#{attr}"), class, ty: "any".into() })
}

/// Record a stuck instance for the current rule.
pub fn record_stuck(state: &HashMap<String, Value>, nulls: BTreeSet<String>, reason: impl Into<String>) {
    let reason = reason.into();
    with(|s| {
        let Some((rule, head)) = s.current.clone() else { return };
        let bindings: BTreeMap<String, Value> = state.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let dup = s
            .stuck
            .iter()
            .any(|x| x.rule == rule && x.reason == reason && x.nulls == nulls && x.bindings == bindings);
        if !dup {
            s.stuck.push(Stuck { rule, head, bindings, nulls, reason });
        }
    });
}

pub fn record_neg(grounded: &Atom) {
    with(|s| {
        let Some((rule, head)) = s.current.clone() else { return };
        s.negs.push(NegEvent { rule, head_pred: head.pred.clone(), grounded: grounded.clone() });
    });
}

pub fn record_agg(body: &[Lit]) {
    with(|s| {
        let Some((rule, head)) = s.current.clone() else { return };
        let body_preds: Vec<Atom> = body
            .iter()
            .filter_map(|l| match l {
                Lit::Pos(a) => Some(a.clone()),
                _ => None,
            })
            .collect();
        if !s.aggs.iter().any(|a| a.rule == rule) {
            s.aggs.push(AggEvent { rule, head_pred: head.pred.clone(), body_preds });
        }
    });
}

pub fn agg_is_content() -> bool {
    with(|s| s.opts.agg_is_content).unwrap_or(false)
}

/// Value has any null at all.
pub fn has_null(v: &Value) -> bool {
    !nulls_in(v).is_empty()
}

/// Value has an open or secret null (content unknown even under UNA).
pub fn has_open_or_secret(v: &Value) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::Null { class, .. } => *class != NullClass::Fresh,
            Value::List(xs) => xs.iter().any(walk),
            Value::Obj(m) => m.values().any(walk),
            _ => false,
        }
    }
    walk(v)
}

/// The prelude: `arg(T, A, P, ref(T, A, P)) :- want(T, A)` for every computed
/// (T, P) in the schema. In the current engine `ref/3` is the only way to
/// build a null, so the prelude is spelled with it.
pub fn prelude(schema: &Schema) -> Vec<Stmt> {
    let mut out = Vec::new();
    for t in schema.types() {
        for (attr, _) in schema.computed_of(&t) {
            let ty = Term::Val(Value::Str(t.clone()));
            let head = Atom {
                pred: "arg".into(),
                args: vec![
                    ty.clone(),
                    Term::Var("__A".into()),
                    Term::Val(Value::Str(attr.clone())),
                    Term::Func { name: "ref".into(), args: vec![ty.clone(), Term::Var("__A".into()), Term::Val(Value::Str(attr.clone()))] },
                ],
                record: None,
            };
            let body = vec![Lit::Pos(Atom { pred: "want".into(), args: vec![ty, Term::Var("__A".into())], record: None })];
            out.push(Stmt::Rule(RuleStmt { head, body }));
        }
    }
    out
}

pub struct SimResult {
    pub facts: BTreeSet<Atom>,
    pub violations: Vec<String>,
    pub warnings: Vec<String>,
    pub sim: Sim,
}

/// Run the engine with the simulation active.
pub fn eval_sim(program: &Program, extra: &[Atom], schema: Schema, opts: SimOpts) -> anyhow::Result<SimResult> {
    let mut program = program.clone();
    program.statements.extend(prelude(&schema));
    SIM.with(|s| *s.borrow_mut() = Some(Sim { schema, opts, ..Default::default() }));
    let r = crate::engine::eval(&program, extra);
    let sim = SIM.with(|s| s.borrow_mut().take()).unwrap();
    let (res, violations) = r?;
    Ok(SimResult { facts: res.facts, violations, warnings: res.warnings, sim })
}

// ---------------------------------------------------------------------------
// Post-hoc analysis: Rule 3, coarse and per-key; phases.
// ---------------------------------------------------------------------------

/// The head of a stuck instance with its bindings applied: bound vars become
/// values, unbound vars and nulls become wildcards. This is the "bound key
/// columns" E's per-key refinement records.
pub fn stuck_head_pattern(s: &Stuck) -> Atom {
    fn inst(t: &Term, b: &BTreeMap<String, Value>) -> Term {
        match t {
            Term::Var(v) => match b.get(v) {
                Some(val) if !has_null(val) => Term::Val(val.clone()),
                _ => Term::Wildcard,
            },
            Term::Val(v) if !has_null(v) => Term::Val(v.clone()),
            Term::Func { name, args } if name == "scoped" => {
                // scoped(Scope, Name): if Name is bound and constant, the
                // address is the concatenation; else wildcard.
                let scope = inst(&args[0], b);
                let name = inst(&args[1], b);
                match (scope, name) {
                    (Term::Val(Value::Str(s)), Term::Val(Value::Str(n))) => Term::Val(Value::Str(format!("{s}::{n}"))),
                    _ => Term::Wildcard,
                }
            }
            _ => Term::Wildcard,
        }
    }
    Atom { pred: s.head.pred.clone(), args: s.head.args.iter().map(|t| inst(t, &s.bindings)).collect(), record: None }
}

/// Does a partially-known pattern unify with a ground atom? Unknown equality
/// counts as unifying (conservative).
pub fn pattern_unifies(pat: &Atom, ground: &Atom) -> bool {
    if pat.pred != ground.pred || pat.args.len() != ground.args.len() {
        return false;
    }
    pat.args.iter().zip(&ground.args).all(|(p, g)| match (p, g) {
        (Term::Wildcard, _) => true,
        (Term::Val(a), Term::Val(b)) => eq3(a, b) != Truth::False,
        _ => true,
    })
}

/// Same, between two patterns (wildcards on both sides).
pub fn patterns_unify(a: &Atom, b: &Atom) -> bool {
    if a.pred != b.pred || a.args.len() != b.args.len() {
        return false;
    }
    a.args.iter().zip(&b.args).all(|(p, q)| match (p, q) {
        (Term::Val(x), Term::Val(y)) => eq3(x, y) != Truth::False,
        _ => true,
    })
}

#[derive(Debug, Clone)]
pub struct Rule3Verdict {
    pub what: String,
    pub coarse_undetermined: bool,
    pub perkey_undetermined: bool,
}

/// A stuck contribution `arg(T, A, P, V, Rank)` is read as `attr(T, A, P, V)`:
/// bodies read the aggregate, never the contributions.
fn as_read(p: Atom) -> Atom {
    if p.pred == "arg" && p.args.len() == 5 {
        return Atom { pred: "attr".into(), args: p.args[..4].to_vec(), record: None };
    }
    p
}

pub fn rule3(sim: &Sim) -> Vec<Rule3Verdict> {
    let stuck_pats: Vec<Atom> = sim.stuck.iter().map(|s| as_read(stuck_head_pattern(s))).collect();
    let stuck_preds: BTreeSet<String> = stuck_pats.iter().map(|p| p.pred.clone()).collect();
    let mut out = Vec::new();
    // Negations.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for n in &sim.negs {
        let key = format!("rule #{} ({}): not {}", n.rule, n.head_pred, fmt_atom(&n.grounded));
        if !seen.insert(key.clone()) {
            continue;
        }
        let coarse = stuck_preds.contains(&n.grounded.pred);
        let perkey = stuck_pats.iter().any(|p| pattern_unifies(p, &n.grounded));
        out.push(Rule3Verdict { what: key, coarse_undetermined: coarse, perkey_undetermined: perkey });
    }
    // Readers of an aggregate whose own rule has a stuck instance (E §2.7,
    // worked example item 5: zone_count is stuck, so deny(two_zones) is
    // undetermined). A positive read of an ordinary predicate with stuck
    // instances is NOT undetermined (monotone: the facts are just not there
    // yet); only aggregates and negations are.
    let agg_heads: BTreeSet<String> = sim.rules.iter().filter(|r| crate::partition::is_aggregate_head(&r.head)).map(|r| r.head.pred.clone()).collect();
    for (i, r) in sim.rules.iter().enumerate() {
        for l in &r.body {
            let Lit::Pos(b) = l else { continue };
            if !agg_heads.contains(&b.pred) {
                continue;
            }
            let coarse = stuck_preds.contains(&b.pred);
            let bp = Atom {
                pred: b.pred.clone(),
                args: b.args.iter().map(|t| match t {
                    Term::Val(v) => Term::Val(v.clone()),
                    _ => Term::Wildcard,
                }).collect(),
                record: None,
            };
            let perkey = stuck_pats.iter().any(|p| patterns_unify(p, &bp));
            if coarse || perkey {
                out.push(Rule3Verdict {
                    what: format!("rule #{} ({}): reads stuck aggregate {}", i, r.head.pred, fmt_atom(&bp)),
                    coarse_undetermined: coarse,
                    perkey_undetermined: perkey,
                });
            }
        }
    }
    // Aggregates over user predicates.
    for a in &sim.aggs {
        for b in &a.body_preds {
            let coarse = stuck_preds.contains(&b.pred);
            let bp = Atom {
                pred: b.pred.clone(),
                args: b.args.iter().map(|t| match t {
                    Term::Val(v) => Term::Val(v.clone()),
                    _ => Term::Wildcard,
                }).collect(),
                record: None,
            };
            let perkey = stuck_pats.iter().any(|p| patterns_unify(p, &bp));
            out.push(Rule3Verdict {
                what: format!("rule #{} aggregate {} over {}", a.rule, a.head_pred, fmt_atom(&bp)),
                coarse_undetermined: coarse,
                perkey_undetermined: perkey,
            });
        }
    }
    out
}

/// The attribute aggregate per address: `attr(T, A, _, lub(..)) :- arg(T, A, _, _)`.
/// Coarse Rule 3: any stuck `arg` instance anywhere makes every group
/// undetermined. Per-key: only groups whose (T, A) unify with a stuck head.
pub fn rule3_attr_groups(sim: &Sim, facts: &BTreeSet<Atom>) -> Vec<Rule3Verdict> {
    let stuck_pats: Vec<Atom> = sim
        .stuck
        .iter()
        .filter(|s| matches!(s.head.pred.as_str(), "arg" | "arg_add"))
        .map(stuck_head_pattern)
        .collect();
    let any_arg_stuck = !stuck_pats.is_empty();
    let mut out = Vec::new();
    for w in facts.iter().filter(|a| a.pred == "want") {
        let group = Atom { pred: "arg".into(), args: vec![w.args[0].clone(), w.args[1].clone(), Term::Wildcard, Term::Wildcard], record: None };
        let perkey = stuck_pats.iter().any(|p| {
            let mut p2 = p.clone();
            p2.pred = "arg".into();
            patterns_unify(&p2, &group)
        });
        out.push(Rule3Verdict {
            what: format!("attr group {}", fmt_atom(&group)),
            coarse_undetermined: any_arg_stuck,
            perkey_undetermined: perkey,
        });
    }
    out
}

/// Owner resource of a null label "type/name#attr".
pub fn null_owner(label: &str) -> Option<(String, String)> {
    let (ta, _) = label.split_once('#')?;
    let (t, n) = ta.split_once('/')?;
    Some((t.to_string(), n.to_string()))
}

#[derive(Debug, Clone)]
pub struct Sections {
    pub definite: Vec<String>,
    pub pending: Vec<String>,
    pub pending_groups: Vec<String>,
    pub undetermined: Vec<String>,
    pub blocking: BTreeSet<String>,
}

/// E §2.7 phase assignment, applied to the simulation's facts.
pub fn sections(sim: &Sim, facts: &BTreeSet<Atom>, use_perkey: bool) -> Sections {
    let r3 = rule3(sim);
    let undetermined_rules: BTreeSet<usize> = r3
        .iter()
        .filter(|v| if use_perkey { v.perkey_undetermined } else { v.coarse_undetermined })
        .filter_map(|v| v.what.strip_prefix("rule #").and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse().ok()))
        .collect();

    // blocking = nulls named by stuck instances (no attr_stuck / deferred in the sim).
    let blocking: BTreeSet<String> = sim.stuck.iter().flat_map(|s| s.nulls.iter().cloned()).collect();
    let boundary_owners: BTreeSet<(String, String)> = blocking.iter().filter_map(|l| null_owner(l)).collect();

    // Per resource: doc nulls.
    let mut docs: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for w in facts.iter().filter(|a| a.pred == "want") {
        let (Term::Val(Value::Str(t)), Term::Val(Value::Str(n))) = (&w.args[0], &w.args[1]) else { continue };
        docs.entry((t.clone(), n.clone())).or_default();
    }
    for a in facts.iter().filter(|a| a.pred == "arg" || a.pred == "arg_add") {
        let (Term::Val(Value::Str(t)), Term::Val(Value::Str(n)), Term::Val(v)) = (&a.args[0], &a.args[1], &a.args[3]) else { continue };
        if let Some(d) = docs.get_mut(&(t.clone(), n.clone())) {
            d.extend(nulls_in(v));
        }
    }
    // Provider configs carrying nulls.
    let mut provider_nulls: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for a in facts.iter().filter(|a| a.pred == "provider_config") {
        let (Term::Val(Value::Str(p)), Term::Val(v)) = (&a.args[0], &a.args[1]) else { continue };
        provider_nulls.entry(p.clone()).or_default().extend(nulls_in(v));
    }
    // Downstream-of-boundary closure over doc-null owner edges.
    let mut held: BTreeSet<(String, String)> = BTreeSet::new();
    loop {
        let mut changed = false;
        for (addr, nulls) in &docs {
            if held.contains(addr) {
                continue;
            }
            // A resource's own computed nulls are not dependency edges: the
            // action that resolves them IS this deformation.
            let owners: BTreeSet<(String, String)> = nulls.iter().filter_map(|l| null_owner(l)).filter(|o| o != addr).collect();
            let downstream = owners.iter().any(|o| boundary_owners.contains(o) || held.contains(o));
            let prov = sim.schema.provider_of.get(&addr.0).cloned();
            let prov_pending = prov
                .and_then(|p| provider_nulls.get(&p))
                .map(|ns| ns.iter().filter_map(|l| null_owner(l)).any(|o| boundary_owners.contains(&o) || held.contains(&o)))
                .unwrap_or(false);
            if downstream || prov_pending {
                held.insert(addr.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut definite = Vec::new();
    let mut pending = Vec::new();
    for (addr, nulls) in &docs {
        let line = format!("{} {}{}", addr.0, addr.1, if nulls.is_empty() { String::new() } else { format!("   carries {}", nulls.iter().map(|n| format!("?{n}")).collect::<Vec<_>>().join(" ")) });
        if held.contains(addr) {
            pending.push(line);
        } else {
            definite.push(line);
        }
    }
    let mut pending_groups = Vec::new();
    let mut undetermined = Vec::new();
    for s in &sim.stuck {
        let pat = stuck_head_pattern(s);
        match s.head.pred.as_str() {
            "want" => pending_groups.push(format!("{}  x unknown, on {}  ({}; rule #{}: {})", fmt_atom(&pat), s.nulls.iter().map(|n| format!("?{n}")).collect::<Vec<_>>().join(" "), s.reason, s.rule, rule_short(&sim.rules[s.rule]))),
            "deny" | "warn" | "constraint" => undetermined.push(format!("{} {}  stuck on {}  ({})", s.head.pred, fmt_atom(&pat), s.nulls.iter().map(|n| format!("?{n}")).collect::<Vec<_>>().join(" "), s.reason)),
            _ => {}
        }
    }
    for i in &undetermined_rules {
        let r = &sim.rules[*i];
        if matches!(r.head.pred.as_str(), "deny" | "warn") {
            undetermined.push(format!("{}  undetermined by Rule 3 ({}): rule #{}: {}", r.head.pred, if use_perkey { "per-key" } else { "coarse" }, i, rule_short(r)));
        }
    }
    Sections { definite, pending, pending_groups, undetermined, blocking }
}

pub fn report(name: &str, r: &SimResult) -> String {
    let mut out = String::new();
    out.push_str(&format!("== stuck simulation: {name}\n"));
    out.push_str(&format!("   facts: {}  violations: {:?}  warnings: {:?}\n", r.facts.len(), r.violations, r.warnings));
    out.push_str(&format!("   stuck instances: {}\n", r.sim.stuck.len()));
    for s in &r.sim.stuck {
        out.push_str(&format!(
            "     - rule #{} head {} stuck on {}  [{}]\n         bindings {}\n         rule: {}\n",
            s.rule,
            fmt_atom(&stuck_head_pattern(s)),
            s.nulls.iter().map(|n| format!("?{n}")).collect::<Vec<_>>().join(" "),
            s.reason,
            s.bindings.iter().map(|(k, v)| format!("{k}={}", fmt_value(v))).collect::<Vec<_>>().join(" "),
            rule_short(&r.sim.rules[s.rule])
        ));
    }
    out.push_str("   Rule 3 (negations and aggregates):        coarse   per-key\n");
    for v in rule3(&r.sim) {
        out.push_str(&format!("     {:<70} {:<8} {}\n", v.what, if v.coarse_undetermined { "UNDET" } else { "ok" }, if v.perkey_undetermined { "UNDET" } else { "ok" }));
    }
    out.push_str("   Rule 3 (attr aggregate groups):           coarse   per-key\n");
    for v in rule3_attr_groups(&r.sim, &r.facts) {
        out.push_str(&format!("     {:<70} {:<8} {}\n", v.what, if v.coarse_undetermined { "UNDET" } else { "ok" }, if v.perkey_undetermined { "UNDET" } else { "ok" }));
    }
    for (label, perkey) in [("coarse Rule 3", false), ("per-key Rule 3", true)] {
        let s = sections(&r.sim, &r.facts, perkey);
        out.push_str(&format!("   sections under {label}: blocking = {{{}}}\n", s.blocking.iter().map(|n| format!("?{n}")).collect::<Vec<_>>().join(" ")));
        out.push_str(&format!("     definite ({}):\n", s.definite.len()));
        for l in &s.definite {
            out.push_str(&format!("       + {l}\n"));
        }
        out.push_str(&format!("     pending ({}):\n", s.pending.len()));
        for l in &s.pending {
            out.push_str(&format!("       ~ {l}\n"));
        }
        for l in &s.pending_groups {
            out.push_str(&format!("       ? {l}\n"));
        }
        out.push_str(&format!("     undetermined ({}):\n", s.undetermined.len()));
        for l in &s.undetermined {
            out.push_str(&format!("       ? {l}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }
    fn input(k: &str, v: &str) -> Atom {
        Atom { pred: "input".into(), args: vec![Term::Val(Value::Str(k.into())), Term::Val(Value::Str(v.into()))], record: None }
    }

    #[test]
    fn dform_df_under_nulls_is_single_phase_unless_rule2_is_read_literally() {
        let program = crate::loader::load_program(&[root().join("dform.df")]).unwrap();
        // Rule 2 read as E §7.2 does (collect_set forwards nulls).
        let r = eval_sim(&program, &[input("env", "prod")], crate::schema::fake(), SimOpts { agg_is_content: false }).unwrap();
        println!("{}", report("dform.df --set env=prod (collect forwards nulls)", &r));
        assert!(r.sim.stuck.is_empty(), "dform.df should have no stuck instances");
        let s = sections(&r.sim, &r.facts, true);
        assert_eq!(s.definite.len(), 14);
        assert!(s.pending.is_empty());
        // Rule 2 read literally: "an aggregate's ... aggregated value" is a
        // content position, so the IAM statements comprehension and the
        // network module's private_subnet_ids output are stuck.
        let r = eval_sim(&program, &[input("env", "prod")], crate::schema::fake(), SimOpts { agg_is_content: true }).unwrap();
        println!("{}", report("dform.df --set env=prod (Rule 2 literal)", &r));
        assert!(!r.sim.stuck.is_empty());
    }

    /// The other example programs under nulls: every ref in the repo is to
    /// `id` (fresh) and is forwarded, so nothing is stuck and every stack is
    /// single-phase, as E §2.7 / DR-2 claim.
    #[test]
    fn other_examples_have_no_stuck_instances() {
        let cases: Vec<(&str, PathBuf, Schema, Vec<Atom>)> = vec![
            ("dform-advanced.df --set env=prod", root().join("dform-advanced.df"), crate::schema::fake(), vec![input("env", "prod")]),
            ("pngu.df --set env=prod", root().join("pngu.df"), crate::schema::gke(), vec![input("env", "prod")]),
            ("examples/decl_demo.df", root().join("examples/decl_demo.df"), crate::schema::fake(), vec![]),
            ("examples/adopt_demo.df --set env=prod", root().join("examples/adopt_demo.df"), crate::schema::fake(), vec![input("env", "prod")]),
        ];
        for (name, path, schema, extra) in cases {
            let program = crate::loader::load_program(&[path]).unwrap();
            let r = eval_sim(&program, &extra, schema, SimOpts::default()).unwrap();
            let s = sections(&r.sim, &r.facts, true);
            println!("{name}: facts {}  stuck {}  definite {}  pending {}  groups {}  undetermined {}  violations {:?}",
                r.facts.len(), r.sim.stuck.len(), s.definite.len(), s.pending.len(), s.pending_groups.len(), s.undetermined.len(), r.violations);
            assert!(r.sim.stuck.is_empty(), "{name}");
            assert!(s.pending.is_empty(), "{name}");
        }
    }

    #[test]
    fn gke_two_phase_rule3_coarse_fires_spuriously() {
        let program = crate::loader::load_program(&[root().join("examples/adversarial/gke_two_phase.df")]).unwrap();
        let r = eval_sim(&program, &[], crate::schema::gke(), SimOpts::default()).unwrap();
        println!("{}", report("gke_two_phase.df", &r));
        // The nodepool rule and zone_count are stuck on ?gke_cluster/pngu#zones.
        assert!(r.sim.stuck.iter().any(|s| s.head.pred == "want" && s.nulls.contains("gke_cluster/pngu#zones")));
        // Coarse Rule 3: the `arg` aggregate group of EVERY address is
        // undetermined (an arg rule for the nodepool is stuck), so nothing is
        // definite. Per-key: only the nodepool's group is.
        let groups = rule3_attr_groups(&r.sim, &r.facts);
        assert!(groups.iter().all(|g| g.coarse_undetermined));
        assert!(groups.iter().filter(|g| g.perkey_undetermined).count() == 0, "no want row exists for a stuck nodepool, so no group is per-key undetermined");
        // The unrelated deletion_protection deny is undetermined coarsely and decided per-key.
        let v = rule3(&r.sim);
        let dp = v.iter().find(|x| x.what.contains("deletion_protection")).unwrap();
        assert!(dp.coarse_undetermined && !dp.perkey_undetermined);
    }

    #[test]
    fn adv2_rule3_coarse_vs_perkey() {
        let program = crate::loader::load_program(&[root().join("examples/adversarial/adv2_rule3_coarse.df")]).unwrap();
        let r = eval_sim(&program, &[], crate::schema::gke(), SimOpts::default()).unwrap();
        println!("{}", report("adv2_rule3_coarse.df", &r));
        let v = rule3(&r.sim);
        let unrelated_neg = v.iter().find(|x| x.what.contains("not want(\"k8s.deployment\"")).unwrap();
        assert!(unrelated_neg.coarse_undetermined && !unrelated_neg.perkey_undetermined);
        let unrelated_agg = v.iter().find(|x| x.what.contains("aggregate ns_count")).unwrap();
        assert!(unrelated_agg.coarse_undetermined && !unrelated_agg.perkey_undetermined);
        let related_neg = v.iter().find(|x| x.what.contains("not want(\"gke_nodepool\"")).unwrap();
        assert!(related_neg.coarse_undetermined && related_neg.perkey_undetermined);
    }
}

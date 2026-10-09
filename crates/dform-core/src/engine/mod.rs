mod aggregate;
mod body;
mod builtins;
mod collapse;
mod contributions;
mod errors;
mod membership;
mod negation;
mod nulls;
mod policy;
mod provenance;
mod undetermined;
mod unify;

use crate::ast::{Atom, Helper, Lit, Program, RuleStmt, Span, Term};
use crate::circuit::{Circuit, Leaf, NodeId};
use crate::ir::ops;
use crate::ir::store::{Store, Window};
use crate::partition::{self, Node};
use crate::spell;
pub use crate::spell::value_to_json;
use crate::stuck::{self, Stuck};
use crate::transform;
use crate::value::Value;
use anyhow::{Context, Result, bail};
use body::{Derived, Src, cmp_order, eval_body, eval_rule, string_cells};
use builtins::eval_term;
pub(crate) use builtins::order;
use contributions::{AttrAggregate, Origin, Origins, is_contribution};
use errors::{check_defined, check_heads, self_spread, with_place};
pub use membership::holds;
use nulls::Rec;
use policy::format_policy_fact;
pub use provenance::circuit_fact;
use provenance::{Prov, given_leaf};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use undetermined::{derive_stuck, may_derive, may_derive_over, record_stucks};

#[derive(Debug, Clone)]
pub struct EvalResult {
    pub facts: BTreeSet<Atom>,
    pub warnings: Vec<String>,
    /// Rule instances that need a null's content (E §2.7 Rule 2) or read
    /// something undetermined (Rule 3); also derived as `stuck/4`.
    pub stuck: Vec<Stuck>,
    /// Rule instances that may derive after a boundary: a positive read of
    /// a predicate with a stuck instance (F DR-2 revised, last clause).
    pub may_derive: Vec<stuck::MayDerive>,
    /// Provenance (E §3.1, DR-10): every fact above has a node here.
    pub circuit: Circuit,
    /// The lowered rules, by the index in their circuit id (`r{i}`).
    pub rules: std::sync::Arc<Vec<RuleStmt>>,
    /// Work done: index lookups plus tuples read (a deterministic cost
    /// for the scale benchmark).
    pub reads: u64,
}

/// The aggregate marker Σ of the attribute aggregate (E §3.1).
pub const ATTR_SIGMA: &str = "Σattr";

/// Predicates the attribute aggregate derives; no rule may.
const AGGREGATE_OUTPUTS: [&str; 5] = [
    "attr",
    transform::ATTR_BASE,
    "attr_conflict",
    "attr_stuck",
    crate::refine::DEFERRED,
];

/// Schema facts that choose a path's lattice. They are read by the
/// aggregate, not by rules, so they must be facts.
const LATTICE_DECLS: [&str; 2] = ["type_lattice", "type_list_key"];

/// Refinements the aggregate joins into its cells (`crate::refine`): read
/// by the aggregate, not by rules, so they must be facts too.
const REFINE_DECLS: [&str; 2] = [crate::refine::TYPE_REFINE, crate::refine::ATTR_REFINE];

pub fn eval(program: &Program, extra_facts: &[Atom]) -> Result<(EvalResult, Vec<String>)> {
    eval_at(program, extra_facts, None)
}

/// `eval`, with the facts the planner injects (`zset::POLICY_INPUTS`)
/// labelled as given at apply tick `tick`.
pub fn eval_at(
    program: &Program,
    extra_facts: &[Atom],
    tick: Option<usize>,
) -> Result<(EvalResult, Vec<String>)> {
    let (c, mut st) = start(program, extra_facts, tick)?;
    run(&c, &mut st, 0)?;
    finish(&c, st)
}

/// An evaluation that can be continued with more given facts: the policy
/// pass (E §2.8), which hands the plan's deformation back to the program.
/// The rules that read those facts, and those that read what they derive,
/// run after every other rule (their strata in order, R-123), so the
/// policy pass evaluates only them again, over a copy of the store
/// (indexes included) as the others left it. A program where that is not
/// sound (one of them contributes to an attribute, or a rule reads
/// `stuck/4`) runs the strata from the first such rule's again instead.
pub struct Resumable {
    c: Compiled,
    /// The first stratum a rule evaluated again is in.
    at: usize,
    state: State,
    /// The rules evaluated again, by index: those reading the later
    /// predicates, and their readers; `None`: every rule from `at`.
    again: Option<Vec<bool>>,
}

/// `eval`, keeping what `Resumable::with` needs to evaluate again with
/// more given facts of the predicates `later` (which the program must
/// define: `decl` or a core predicate).
pub fn eval_resumable(
    program: &Program,
    extra_facts: &[Atom],
    later: &[&str],
) -> Result<(EvalResult, Vec<String>, Resumable)> {
    let (c, mut st) = start(program, extra_facts, None)?;
    let again = reading(&c, later);
    let first = |of: &dyn Fn(usize) -> bool| {
        (0..c.rules.len())
            .filter(|&i| of(i))
            .filter_map(|i| c.rule_strata[i].first().copied())
            .min()
            .unwrap_or(c.fixes.len())
    };
    let (at, state) = match &again {
        Some(again) => {
            let rest: Vec<bool> = again.iter().map(|a| !a).collect();
            run_only(&c, &mut st, 0, Some(&rest))?;
            (first(&|i| again[i]), st.clone())
        }
        None => {
            let reads_later = |body: &[Lit]| {
                body.iter().any(|l| match l {
                    Lit::Pos(a) | Lit::Not(a) => later.contains(&a.pred.as_str()),
                    _ => false,
                })
            };
            let at = first(&|i| reads_later(&c.rules[i].body));
            run_strata(&c, &mut st, 0..at, None)?;
            (at, st.clone())
        }
    };
    run_only(&c, &mut st, at, again.as_deref())?;
    let (res, violations) = finish(&c, st)?;
    Ok((
        res,
        violations,
        Resumable {
            c,
            at,
            state,
            again,
        },
    ))
}

/// The rules that read a predicate of `later`, or one such a rule
/// derives, by index ([`Resumable`]); `None` when evaluating them after
/// the others is not the same evaluation: one contributes to an
/// attribute (the aggregate the others read), or a rule reads `stuck/4`,
/// which every rule's stuck instances make.
fn reading(c: &Compiled, later: &[&str]) -> Option<Vec<bool>> {
    if c.stuck_at.is_some() {
        return None;
    }
    let mut preds: BTreeSet<&str> = later.iter().copied().collect();
    let mut again = vec![false; c.rules.len()];
    loop {
        let mut grew = false;
        for (i, r) in c.rules.iter().enumerate() {
            let reads = r.body.iter().any(|l| match l {
                Lit::Pos(a) | Lit::Not(a) => preds.contains(a.pred.as_str()),
                _ => false,
            });
            if again[i] || !reads {
                continue;
            }
            if r.head.pred == "arg" {
                return None;
            }
            again[i] = true;
            grew |= preds.insert(r.head.pred.as_str());
        }
        if !grew {
            break;
        }
    }
    Some(again)
}

impl Resumable {
    /// The evaluation with `more` given facts as well: the same result as
    /// `eval` with `more` appended to the given facts, except that their
    /// circuit nodes, and those of the rules evaluated again, come after
    /// the others'.
    pub fn with(&self, more: &[Atom]) -> Result<(EvalResult, Vec<String>)> {
        self.with_at(more, None)
    }

    /// `with`, the planner's facts in `more` given at apply tick `tick`.
    pub fn with_at(&self, more: &[Atom], tick: Option<usize>) -> Result<(EvalResult, Vec<String>)> {
        let mut st = self.state.clone();
        for f in more {
            let g = ensure_ground(f)?;
            let leaf = given_leaf(&g, &self.c.externs, tick);
            st.prov.given(g, leaf);
        }
        run_only(&self.c, &mut st, self.at, self.again.as_deref())?;
        finish(&self.c, st)
    }
}

/// The program compiled for evaluation: rules, their
/// operator IR, the strata, and the circuit's rule leaves.
struct Compiled {
    rules: std::sync::Arc<Vec<RuleStmt>>,
    externs: BTreeSet<crate::ast::Extern>,
    plans: Vec<ops::Rule>,
    /// The strata each rule runs at, by its index.
    rule_strata: Vec<BTreeSet<usize>>,
    fixes: Vec<ops::Fix>,
    rule_text: Vec<String>,
    rule_leaf: Vec<NodeId>,
    sigma: NodeId,
    /// Predicates defined by an aggregate rule, and `attr`.
    aggregates: BTreeSet<String>,
    /// Predicates a `not { .. }` helper defines, in whatever scope
    /// (`__neg_0`, a module's `m::__neg_0`, a call site's `S::__neg_0`).
    negations: BTreeSet<String>,
    /// The stratum `stuck/4` is derived at, when a rule reads it.
    stuck_at: Option<usize>,
}

impl Compiled {
    /// The rules planned over `strata`: the stratum each runs at, their
    /// operator IR and a Fix per stratum, the indexes the bodies read
    /// through (made in `prov`'s store), and each rule's leaf in its circuit.
    fn new(
        rules: Vec<RuleStmt>,
        externs: BTreeSet<crate::ast::Extern>,
        graph: &partition::Graph,
        strata: &BTreeMap<Node, usize>,
        prov: &mut Prov,
    ) -> Self {
        // A rule runs at the stratum of each node it defines: one, or one per
        // address it reads (`partition::Graph::heads`).
        let rule_strata: Vec<BTreeSet<usize>> = graph
            .heads
            .iter()
            .map(|hs| {
                hs.iter()
                    .map(|h| strata.get(h).copied().unwrap_or(0))
                    .collect()
            })
            .collect();
        // The operator IR: one body per rule, a Fix per
        // stratum, and the indexes the bodies read through.
        let extern_preds: BTreeSet<String> = externs.iter().map(|e| e.pred.clone()).collect();
        let plans: Vec<ops::Rule> = rules
            .iter()
            .map(|r| ops::compile_rule(r, &extern_preds))
            .collect();
        let fixes = ops::fixes(&rules, &rule_strata, &plans);
        index_reads(&mut prov.store, &rules, &plans);

        let rule_text: Vec<String> = rules.iter().map(spell::rule).collect();
        let rule_leaf: Vec<NodeId> = rule_text
            .iter()
            .enumerate()
            .map(|(i, t)| prov.rule(format!("r{i}"), t, rules[i].head.span))
            .collect();
        let sigma = prov.rule(
            ATTR_SIGMA.into(),
            "attribute aggregate (lub_ranked, E §2.5)",
            Span::default(),
        );
        let aggregates: BTreeSet<String> = rules
            .iter()
            .filter(|r| partition::is_aggregate_head(&r.head))
            .map(|r| r.head.pred.clone())
            .chain(["attr".to_string(), transform::ATTR_BASE.to_string()])
            .collect();
        let negations = Helper::Negation.heads(&rules);
        let stuck_at = strata.get(&Node::plain(partition::STUCK)).copied();
        Compiled {
            rules: std::sync::Arc::new(rules),
            externs,
            plans,
            rule_strata,
            fixes,
            rule_text,
            rule_leaf,
            sigma,
            aggregates,
            negations,
            stuck_at,
        }
    }

    /// The recorder of rule `i`'s instances that wait, reading `known`.
    fn rec<'a>(&'a self, i: usize, known: &'a RefCell<stuck::Known>) -> Rec<'a> {
        Rec {
            rule: i,
            head: &self.rules[i].head,
            text: &self.rule_text[i],
            known,
            aggregates: &self.aggregates,
            negations: &self.negations,
            found: RefCell::new(Vec::new()),
        }
    }

    /// Whether `s` is a helper's: the rule it was written for is stuck
    /// when it is, and says so in the program's words.
    fn is_helper(&self, s: &Stuck) -> bool {
        s.rule
            .and_then(|i| self.rules.get(i))
            .is_some_and(|r| r.helper.is_some())
    }
}

/// Everything an evaluation accumulates.
#[derive(Clone)]
struct State {
    prov: Prov,
    origins: Origins,
    known: RefCell<stuck::Known>,
    stucks: Vec<Stuck>,
    attrs: AttrAggregate,
    /// The instances `stuck/4` was derived with, when a rule reads it.
    stuck_facts: Option<BTreeSet<Stuck>>,
}

/// Compile the program, insert the given and stated facts, and stratify.
fn start(
    program: &Program,
    extra_facts: &[Atom],
    tick: Option<usize>,
) -> Result<(Compiled, State)> {
    // The program as it runs and the partition graph it is stratified by,
    // built in one place (`dform dev strata` prints the same graph).
    let compiled = partition::compile(program, extra_facts)?;
    let externs = compiled.externs;
    let mut origins = Origins::default();
    let mut prov = Prov::default();
    for f in extra_facts {
        let g = ensure_ground(f)?;
        let leaf = given_leaf(&g, &externs, tick);
        prov.given(g, leaf);
    }

    let (rules, fact_atoms) = (compiled.rules, compiled.facts);
    check_heads(&rules)?;
    for a in &fact_atoms {
        let g = ensure_ground(a)?;
        let (at, contribution) = (g.span, is_contribution(&g));
        let t = prov.stated(g);
        if contribution {
            origins.note(t, Origin::Fact(at));
        }
    }

    check_defined(&rules, prov.store.atoms(), &externs)?;

    // Stratified evaluation over the partition graph (E §2.6, F DR-12
    // revised). Every rule runs in the stratum of its head node.
    let graph = compiled.graph;
    let strata = stratify(&graph)?;
    let c = Compiled::new(rules, externs, &graph, &strata, &mut prov);
    origins.rules = c
        .rule_text
        .iter()
        .zip(c.rules.iter())
        .map(|(t, r)| with_place(t.clone(), r.head.span))
        .collect();
    let attrs = AttrAggregate::new(&strata, &graph.split);
    let state = State {
        prov,
        origins,
        known: RefCell::new(stuck::Known::default()),
        stucks: Vec::new(),
        attrs,
        stuck_facts: None,
    };
    Ok((c, state))
}

/// The partition graph's strata, or the cycle through a negation or an
/// aggregate that has none (with the help for a rule spreading its own
/// value).
fn stratify(graph: &partition::Graph) -> Result<BTreeMap<Node, usize>> {
    match partition::stratify(graph) {
        partition::Verdict::Stratified { strata } => Ok(strata),
        partition::Verdict::Rejected {
            scc,
            negative_edges,
        } => {
            let help = self_spread(graph, &negative_edges).unwrap_or_default();
            bail!(
                "{}{help}",
                partition::cycle_error(graph, &scc, &negative_edges)
            )
        }
    }
}

/// Evaluate every stratum from `from`, then collapse what is left of the
/// attribute aggregate.
fn run(c: &Compiled, st: &mut State, from: usize) -> Result<()> {
    run_only(c, st, from, None)
}

/// [`run`], only the rules `only` marks (`None`: every rule).
fn run_only(c: &Compiled, st: &mut State, from: usize, only: Option<&[bool]>) -> Result<()> {
    run_strata(c, st, from..c.fixes.len(), only)?;
    let State {
        prov,
        origins,
        known,
        stucks,
        attrs,
        ..
    } = st;
    let ready = attrs.emit_ready(usize::MAX, prov, origins, &known.borrow(), c.sigma)?;
    stucks.extend(ready);
    attrs.check_complete()
}

/// Evaluate strata `range` in order, only the rules `only` marks (`None`:
/// every rule).
fn run_strata(
    c: &Compiled,
    st: &mut State,
    range: std::ops::Range<usize>,
    only: Option<&[bool]>,
) -> Result<()> {
    let State {
        prov,
        origins,
        known,
        stucks,
        attrs,
        stuck_facts,
    } = st;
    let known: &RefCell<stuck::Known> = known;
    for s in range {
        let mut fix = std::borrow::Cow::Borrowed(&c.fixes[s]);
        if let Some(only) = only {
            fix.to_mut().rules.retain(|&i| only[i]);
        }
        let fix = &*fix;
        // Attribute groups whose contributors all sit below this stratum
        // are complete: collapse them before any rule here reads them.
        let ready = attrs.emit_ready(s, prov, origins, &known.borrow(), c.sigma)?;
        for st in ready {
            known.borrow_mut().add(&st);
            stucks.push(st);
        }
        if c.stuck_at == Some(s) {
            *stuck_facts = Some(derive_stuck(c, prov, known, stucks, s)?);
        }
        if fix.rules.is_empty() {
            continue;
        }
        let recs: Vec<Rec> = fix.rules.iter().map(|&i| c.rec(i, known)).collect();
        fixpoint(c, fix, &recs, prov, origins, known)?;
        stucks.extend(recs.into_iter().flat_map(|r| r.found.into_inner()));
        // Rule 3 per key sees a head this stratum may derive after a
        // boundary as it sees a stuck head (F DR-2 revised): a negation or
        // an aggregate group above that unifies with it is undetermined.
        if !known.borrow().is_empty() {
            may_derive_over(c, &prov.store, known, known, &fix.rules, &BTreeSet::new())?;
        }
    }
    Ok(())
}

/// Semi-naive iteration to the stratum's fixpoint. It terminates: facts
/// only grow, and a stratum derives finitely many unless a builtin invents
/// values without bound (`n(Y) :- n(X), Y = X + 1`).
///
/// The first round reads every tuple. After it, a round joins each rule
/// once per body position against the tuples the last round derived there
/// (the delta): the positions before it read the tuples older than the
/// delta, the positions after it every tuple. So every combination of
/// tuples is joined in exactly one round, the round after its newest tuple
/// arrived.
///
/// An aggregate may share a stratum with its readers, so a stuck instance
/// found here is known to the next round (Rule 3). Rule 3 can turn a
/// combination that fired into a stuck one, so a round after a stuck
/// instance is found reads every tuple again, as the first does.
fn fixpoint(
    c: &Compiled,
    fix: &ops::Fix,
    recs: &[Rec],
    prov: &mut Prov,
    origins: &mut Origins,
    known: &RefCell<stuck::Known>,
) -> Result<()> {
    let mut seen = vec![0usize; recs.len()];
    let mut full = true;
    let mut delta = Window::below(0);
    loop {
        let hi = prov.store.len();
        let derived = round(c, fix, recs, &prov.store, full, delta)?;
        let changed = record_round(c, prov, origins, derived);
        let grew = learn_stucks(recs, &mut seen, known);
        if !changed && !grew {
            return Ok(());
        }
        full = grew;
        delta = Window {
            lo: hi,
            hi: prov.store.len(),
        };
    }
}

/// One round of the stratum's rules over `store`: every tuple when `full`,
/// else each body position against `delta`; what they derive, in the order
/// a naive round would derive it (rule, then the tuples each body literal
/// matched, in fact order), which decides circuit node ids, and so the
/// order `why` prints children in.
fn round(
    c: &Compiled,
    fix: &ops::Fix,
    recs: &[Rec],
    store: &Store,
    full: bool,
    delta: Window,
) -> Result<Vec<(usize, Derived)>> {
    let hi = store.len();
    let mut derived: Vec<(usize, Derived)> = Vec::new();
    for (&i, rec) in fix.rules.iter().zip(recs) {
        let plan = &c.plans[i];
        let wins = |w: &dyn Fn(usize) -> Window| -> Vec<Window> {
            (0..c.rules[i].body.len()).map(w).collect()
        };
        if full {
            let src = Src::whole(store, &plan.body, c.rules[i].body.len());
            let out = eval_rule(&c.rules[i], plan, &src, rec)?;
            derived.extend(out.into_iter().map(|d| (i, d)));
            continue;
        }
        if matches!(plan.head, ops::Head::Agg { .. }) {
            // Every relation an aggregate reads is complete below
            // this stratum: the first round decided it.
            continue;
        }
        for read in plan.body.reads() {
            if !store.any_in(&read.rel, delta) {
                continue;
            }
            let at = read.lit;
            let src = Src {
                store,
                body: &plan.body,
                win: wins(&|l| match l.cmp(&at) {
                    Ordering::Less => Window::below(delta.lo),
                    Ordering::Equal => delta,
                    Ordering::Greater => Window::below(hi),
                }),
                all: Window::below(hi),
            };
            let out = eval_rule(&c.rules[i], plan, &src, rec)?;
            derived.extend(out.into_iter().map(|d| (i, d)));
        }
    }
    derived.sort_by(|(i, a), (j, b)| i.cmp(j).then_with(|| cmp_order(store, &a.order, &b.order)));
    Ok(derived)
}

/// Record a round's heads with their firings (and a contribution's rule);
/// whether one is new.
fn record_round(
    c: &Compiled,
    prov: &mut Prov,
    origins: &mut Origins,
    derived: Vec<(usize, Derived)>,
) -> bool {
    let mut changed = false;
    for (i, d) in derived {
        let contribution = is_contribution(&d.head);
        let mut children = vec![c.rule_leaf[i]];
        children.extend(d.used.iter().map(|&t| prov.id(t)));
        for a in &d.absent {
            children.push(prov.absent(a));
        }
        let (t, new) = prov.record(d.head, children, d.bindings);
        if contribution {
            origins.note(t, Origin::Rule(i));
        }
        changed |= new;
    }
    changed
}

/// Make the stuck instances the rules found since `seen` known to the next
/// round (Rule 3); whether there were any.
fn learn_stucks(recs: &[Rec], seen: &mut [usize], known: &RefCell<stuck::Known>) -> bool {
    let mut grew = false;
    for (rec, n) in recs.iter().zip(seen.iter_mut()) {
        let found = rec.found.borrow();
        for st in &found[*n..] {
            known.borrow_mut().add(st);
            grew = true;
        }
        *n = found.len();
    }
    grew
}

/// Read the policy facts of the final fact set, and derive `stuck/4`.
fn finish(c: &Compiled, st: State) -> Result<(EvalResult, Vec<String>)> {
    let State {
        mut prov,
        known,
        mut stucks,
        stuck_facts,
        ..
    } = st;
    let mut violations = Vec::new();
    let mut facts: BTreeSet<Atom> = BTreeSet::new();
    for a in prov.store.atoms() {
        facts.insert(a.clone());
    }
    // Policy facts: deny/warn.
    let mut warnings = Vec::new();
    for a in &facts {
        match a.pred.as_str() {
            "warn" => warnings.push(format_policy_fact(a)?),
            "deny" => violations.push(format_policy_fact(a)?),
            _ => {}
        }
    }

    // A helper (a companion `__ref_dep`, a `not { .. }` body) is stuck
    // exactly when the rule it was written for is.
    stucks.retain(|s| !c.is_helper(s));
    stucks.sort();
    stucks.dedup();
    // Guard on the stratifier: stuck/4 was derived with every instance.
    if let Some(derived) = &stuck_facts
        && let Some(late) = stucks.iter().find(|s| !derived.contains(*s))
    {
        bail!(
            "internal: {} was found stuck after stuck/4 was derived",
            late.text
        );
    }
    for f in record_stucks(c, &mut prov, &stucks) {
        facts.insert(f);
    }
    let may_derive = may_derive(c, &prov, &known, &stucks)?;

    let reads = prov.store.reads.get();
    Ok((
        EvalResult {
            facts,
            warnings,
            stuck: stucks,
            may_derive,
            circuit: prov.circuit,
            rules: c.rules.clone(),
            reads,
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
    let plan = ops::compile_body(body, &BTreeSet::new());
    // Tuple ids in fact order: answers come out in fact order.
    let mut store = Store::default();
    for (rel, keys) in ops::indexes([&plan]) {
        for key in keys {
            store.index(&rel, &key);
        }
    }
    for (rel, key) in string_cells(body, &plan) {
        store.index(&rel, &key);
    }
    // Only the relations the body reads: an extern's demand asks this of
    // every fact of an evaluation, a manifest's documents among them.
    let read: BTreeSet<&str> = body
        .iter()
        .filter_map(|l| match l {
            Lit::Pos(a) | Lit::Not(a) => Some(a.pred.as_str()),
            _ => None,
        })
        .collect();
    for a in facts.iter().filter(|a| read.contains(a.pred.as_str())) {
        store.insert(a.clone());
    }
    let head = Atom {
        pred: "query".into(),
        args: vec![],
        record: None,
        span: Default::default(),
    };
    let known = RefCell::new(stuck::Known::default());
    let (aggregates, negations) = (BTreeSet::new(), BTreeSet::new());
    let rec = Rec {
        rule: 0,
        head: &head,
        text: "query",
        known: &known,
        aggregates: &aggregates,
        negations: &negations,
        found: RefCell::new(Vec::new()),
    };
    let src = Src::whole(&store, &plan, body.len());
    Ok(eval_body(body, &src, &rec)?
        .into_iter()
        .map(|r| {
            let b = r.s.into_iter().filter(|(k, _)| !k.starts_with("__"));
            let used = r.used.iter().map(|&t| store.get(t).clone()).collect();
            (b.collect(), used)
        })
        .collect())
}

/// The indexes the rules' bodies read through: the operator IR's, and
/// those a read of a string cell looks a reference up by.
fn index_reads(store: &mut Store, rules: &[RuleStmt], plans: &[ops::Rule]) {
    let bodies = plans.iter().map(|p| &p.body);
    for (rel, keys) in ops::indexes(bodies) {
        for key in keys {
            store.index(&rel, &key);
        }
    }
    for r in rules.iter().zip(plans) {
        for (rel, key) in string_cells(&r.0.body, &r.1.body) {
            store.index(&rel, &key);
        }
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
        span: a.span,
    })
}

#[cfg(test)]
mod tests;

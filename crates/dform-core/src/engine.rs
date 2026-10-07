use crate::ast::{Atom, Lit, Program, RuleStmt, Span, Term, str_term};
use crate::circuit::{self, Circuit, Leaf, NodeId};
use crate::diag;
use crate::ir::ops::{self, AggKind};
use crate::ir::store::{Store, TupleId, Window};
use crate::lattice::{self, Collapsed, Lattice, Rank, RankedContribution, Shadowed, Witnesses};
use crate::lattice::{Truth, nulls_in};
use crate::partition::{self, Node};
use crate::stuck::{self, Stuck};
use crate::transform;
use crate::value::Value;
use anyhow::{Context, Result, anyhow, bail};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

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
pub const ATTR_SIGMA: &str = "Σattr";

/// The fact store and its provenance: every tuple in the store has a node
/// in the circuit, recorded when the tuple is inserted and once more per
/// distinct firing that derives it again.
#[derive(Default, Clone)]
struct Prov {
    circuit: Circuit,
    store: Store,
    /// Circuit node per tuple id.
    node: Vec<NodeId>,
}

impl Prov {
    /// Record one firing of `a` and insert it: its tuple id, and whether
    /// it is new.
    fn record(
        &mut self,
        a: Atom,
        children: Vec<NodeId>,
        bindings: Vec<(String, Value)>,
    ) -> (TupleId, bool) {
        match self.store.id(&a) {
            Some(id) => {
                self.circuit
                    .fire(self.node[id as usize], children, bindings);
                (id, false)
            }
            None => {
                let n = self
                    .circuit
                    .derive_with(circuit_fact(&a), children, bindings);
                self.node.push(n);
                (self.store.insert(a).0, true)
            }
        }
    }

    fn given(&mut self, a: Atom, leaf: Leaf) -> TupleId {
        let l = self.circuit.leaf(leaf);
        self.record(a, vec![l], vec![]).0
    }

    fn rule(&mut self, id: String, text: &str, span: Span) -> NodeId {
        self.circuit.name_rule(&id, text);
        if let Some(at) = diag::place(span) {
            self.circuit.locate_rule(&id, at);
        }
        if let (Some((file, text)), Some((_, line, _))) =
            (diag::source_of(span), diag::location(span))
        {
            let end = (span.end as usize).min(text.len());
            let src = circuit::RuleSource {
                file,
                start: (span.start as usize).min(end),
                end,
                line,
                origin: diag::origin(span),
                text,
            };
            self.circuit.source_rule(&id, src);
        }
        self.circuit.leaf(Leaf::Rule { id })
    }

    fn absent(&mut self, a: &Atom) -> NodeId {
        self.circuit.leaf(Leaf::Absent {
            pattern: partition::fmt_atom(a),
        })
    }

    fn id(&self, t: TupleId) -> NodeId {
        self.node[t as usize]
    }
}

/// The leaf for a fact given to this run rather than stated by the program.
/// `tick`: the apply tick the planner injected its facts at (`None`: the
/// plan).
fn given_leaf(a: &Atom, externs: &BTreeSet<crate::ast::Extern>, tick: Option<usize>) -> Leaf {
    let text = partition::fmt_atom(a);
    if let Some(event) = crate::stack::published(a) {
        return Leaf::World { event };
    }
    match a.pred.as_str() {
        p if crate::zset::POLICY_INPUTS.contains(&p) => Leaf::Plan { fact: text, tick },
        "input" | "data" => {
            let flag = if a.pred == "input" { "set" } else { "data" };
            let kv = match a.args.as_slice() {
                [Term::Val(k), Term::Val(v)] => {
                    format!("{}={}", partition::fmt_bare(k), partition::fmt_bare(v))
                }
                _ => text,
            };
            Leaf::Input {
                source: format!("--{flag} {kv}"),
            }
        }
        p if p.starts_with("type_") => Leaf::Schema { span: text },
        // A kept value says when it was kept, never the value or the
        // candidate (either may be a secret).
        crate::memo::FIRST => match a.args.first() {
            Some(Term::Val(Value::Str(k))) => Leaf::Extern {
                call: crate::memo::provenance(k),
            },
            _ => Leaf::Extern { call: text },
        },
        // A table's row: stated where its file states it.
        p if externs.iter().any(|e| e.pred == p) => match crate::tables::at(a) {
            Some(span) => Leaf::Base { span },
            None => Leaf::Extern { call: text },
        },
        _ => Leaf::World { event: text },
    }
}

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
/// Only the strata from the first one that reads those facts are
/// evaluated again, over a copy of the store (indexes included) as it was
/// before that stratum.
pub struct Resumable {
    c: Compiled,
    /// The first stratum a rule reading a later predicate is in.
    at: usize,
    state: State,
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
    let reads_later = |body: &[Lit]| {
        body.iter().any(|l| match l {
            Lit::Pos(a) | Lit::Not(a) => later.contains(&a.pred.as_str()),
            _ => false,
        })
    };
    let at = (0..c.rules.len())
        .filter(|&i| reads_later(&c.rules[i].body))
        .filter_map(|i| c.rule_strata[i].first().copied())
        .min()
        .unwrap_or(c.fixes.len());
    run_strata(&c, &mut st, 0..at)?;
    let state = st.clone();
    run(&c, &mut st, at)?;
    let (res, violations) = finish(&c, st)?;
    Ok((res, violations, Resumable { c, at, state }))
}

impl Resumable {
    /// The evaluation with `more` given facts as well: the same result as
    /// `eval` with `more` appended to the given facts, except that their
    /// circuit nodes come after those of the strata below the first reader.
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
        run(&self.c, &mut st, self.at)?;
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
    /// The stratum `stuck/4` is derived at, when a rule reads it.
    stuck_at: Option<usize>,
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
    for r in &rules {
        if AGGREGATE_OUTPUTS.contains(&r.head.pred.as_str()) {
            bail!(
                "{} is derived by the attribute aggregate; contribute with arg instead: {}{}",
                r.head.pred,
                partition::fmt_rule(r),
                at_suffix(r.head.span)
            );
        }
        if r.head.pred == partition::STUCK || r.head.pred == stuck::MAY_DERIVE {
            bail!(
                "{} is derived by the evaluator; no rule may: {}{}",
                if r.head.pred == partition::STUCK {
                    "stuck/4"
                } else {
                    "may_derive/3"
                },
                partition::fmt_rule(r),
                at_suffix(r.head.span)
            );
        }
        if LATTICE_DECLS.contains(&r.head.pred.as_str())
            || REFINE_DECLS.contains(&r.head.pred.as_str())
        {
            bail!(
                "{} must be a fact, not a rule: {}{}",
                r.head.pred,
                partition::fmt_rule(r),
                at_suffix(r.head.span)
            );
        }
    }

    for a in &fact_atoms {
        let g = ensure_ground(a)?;
        let at = g.span;
        let span = match (diag::at(g.span), diag::origin(g.span)) {
            (Some(at), Some(o)) => format!("{at} ({}, {o})", g.pred),
            (Some(at), None) => format!("{at} ({})", g.pred),
            (None, _) => format!("compiler ({})", g.pred),
        };
        let contribution = is_contribution(&g);
        let t = prov.given(g, Leaf::Base { span });
        if contribution {
            origins.note(t, Origin::Fact(at));
        }
    }

    check_defined(&rules, prov.store.atoms(), &externs)?;

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
    let bodies = plans.iter().map(|p| &p.body);
    for (rel, keys) in ops::indexes(bodies) {
        for key in keys {
            prov.store.index(&rel, &key);
        }
    }

    let rule_text: Vec<String> = rules.iter().map(partition::fmt_rule).collect();
    origins.rules = rule_text
        .iter()
        .zip(&rules)
        .map(|(t, r)| with_place(t.clone(), r.head.span))
        .collect();
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
    let attrs = AttrAggregate::new(&strata, &graph.split);
    let stuck_at = strata.get(&Node::plain(partition::STUCK)).copied();
    Ok((
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
            stuck_at,
        },
        State {
            prov,
            origins,
            known: RefCell::new(stuck::Known::default()),
            stucks: Vec::new(),
            attrs,
            stuck_facts: None,
        },
    ))
}

/// Evaluate every stratum from `from`, then collapse what is left of the
/// attribute aggregate.
fn run(c: &Compiled, st: &mut State, from: usize) -> Result<()> {
    run_strata(c, st, from..c.fixes.len())?;
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

/// Evaluate strata `range` in order.
fn run_strata(c: &Compiled, st: &mut State, range: std::ops::Range<usize>) -> Result<()> {
    let State {
        prov,
        origins,
        known,
        stucks,
        attrs,
        stuck_facts,
    } = st;
    let known: &RefCell<stuck::Known> = known;
    let (rules, plans) = (&c.rules, &c.plans);
    for s in range {
        let fix = &c.fixes[s];
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
        let recs: Vec<Rec> = fix
            .rules
            .iter()
            .map(|&i| Rec {
                rule: i,
                head: &rules[i].head,
                text: &c.rule_text[i],
                known,
                aggregates: &c.aggregates,
                found: RefCell::new(Vec::new()),
            })
            .collect();

        // Semi-naive iteration to the stratum's fixpoint. It terminates:
        // facts only grow, and a stratum derives finitely many unless a
        // builtin invents values without bound (`n(Y) :- n(X), Y = X + 1`).
        //
        // The first round reads every tuple. After it, a round joins each
        // rule once per body position against the tuples the last round
        // derived there (the delta): the positions before it read the
        // tuples older than the delta, the positions after it every tuple.
        // So every combination of tuples is joined in exactly one round,
        // the round after its newest tuple arrived.
        //
        // An aggregate may share a stratum with its readers, so a stuck
        // instance found here is known to the next round (Rule 3). Rule 3
        // can turn a combination that fired into a stuck one, so a round
        // after a stuck instance is found reads every tuple again, as the
        // first does.
        let mut seen = vec![0usize; recs.len()];
        let mut full = true;
        let mut delta = Window::below(0);
        loop {
            let hi = prov.store.len();
            let mut derived: Vec<(usize, Derived)> = Vec::new();
            for (&i, rec) in fix.rules.iter().zip(&recs) {
                let plan = &plans[i];
                let wins = |w: &dyn Fn(usize) -> Window| -> Vec<Window> {
                    (0..rules[i].body.len()).map(w).collect()
                };
                if full {
                    let src = Src {
                        store: &prov.store,
                        body: &plan.body,
                        win: wins(&|_| Window::below(hi)),
                        all: Window::below(hi),
                    };
                    let out = eval_rule(&rules[i], plan, &src, rec)?;
                    derived.extend(out.into_iter().map(|d| (i, d)));
                    continue;
                }
                if matches!(plan.head, ops::Head::Agg { .. }) {
                    // Every relation an aggregate reads is complete below
                    // this stratum: the first round decided it.
                    continue;
                }
                for read in plan.body.reads() {
                    if !prov.store.any_in(&read.rel, delta) {
                        continue;
                    }
                    let at = read.lit;
                    let src = Src {
                        store: &prov.store,
                        body: &plan.body,
                        win: wins(&|l| match l.cmp(&at) {
                            Ordering::Less => Window::below(delta.lo),
                            Ordering::Equal => delta,
                            Ordering::Greater => Window::below(hi),
                        }),
                        all: Window::below(hi),
                    };
                    let out = eval_rule(&rules[i], plan, &src, rec)?;
                    derived.extend(out.into_iter().map(|d| (i, d)));
                }
            }
            // The order a naive round would derive in (rule, then the
            // tuples each body literal matched, in fact order): it decides
            // circuit node ids, and so the order `why` prints children in.
            let store = &prov.store;
            derived.sort_by(|(i, a), (j, b)| {
                i.cmp(j).then_with(|| cmp_order(store, &a.order, &b.order))
            });
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
            let mut grew = false;
            for (rec, n) in recs.iter().zip(seen.iter_mut()) {
                let found = rec.found.borrow();
                for st in &found[*n..] {
                    known.borrow_mut().add(st);
                    grew = true;
                }
                *n = found.len();
            }
            if !changed && !grew {
                break;
            }
            full = grew;
            delta = Window {
                lo: hi,
                hi: prov.store.len(),
            };
        }
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

    // A compiler-generated companion (`__ref_dep`) is stuck exactly when
    // the contribution it shadows is.
    stucks.retain(|s| !s.head.pred.starts_with("__"));
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

/// F DR-2 revised, last clause, for every rule: an instance whose body
/// positively reads a predicate with a stuck instance may derive after the
/// boundary that resolves it. Per rule and read position: the literals
/// before the read are evaluated (Rule 2 and 3 as they ran), the read is
/// matched against the stuck heads under those bindings, and the head is
/// instantiated with what that binds; the literals after it are not asked
/// (the answer over-approximates). A head found this way is read the same
/// way in turn, so a helper of a helper is found. A rule with a stuck
/// instance is already reported as one, and a ground head already derived
/// adds nothing.
fn may_derive(
    c: &Compiled,
    prov: &Prov,
    known: &RefCell<stuck::Known>,
    stucks: &[Stuck],
) -> Result<Vec<stuck::MayDerive>> {
    if stucks.is_empty() {
        return Ok(Vec::new());
    }
    let mut heads = stuck::Known::default();
    for s in stucks {
        heads.add(s);
    }
    let heads = RefCell::new(heads);
    let skip: BTreeSet<usize> = stucks.iter().filter_map(|s| s.rule).collect();
    let over: Vec<usize> = (0..c.rules.len()).collect();
    let mut out = may_derive_over(c, &prov.store, known, &heads, &over, &skip)?;
    out.sort();
    Ok(out)
}

/// The may-derive instances of the rules `over` (indices into the rules;
/// those in `skip` left out), reading the heads in
/// `heads`, to a fixpoint: each head found is added to `heads` and read in
/// turn. `known` is what the literals before a read are evaluated with
/// (Rule 3); it may be `heads` itself, as it is while the strata run.
fn may_derive_over(
    c: &Compiled,
    store: &Store,
    known: &RefCell<stuck::Known>,
    heads: &RefCell<stuck::Known>,
    over: &[usize],
    skip: &BTreeSet<usize>,
) -> Result<Vec<stuck::MayDerive>> {
    let all = Window::below(store.len());
    let rule_of = |i: usize| -> (&RuleStmt, &ops::Body) { (&c.rules[i], &c.plans[i].body) };
    let mut out: Vec<stuck::MayDerive> = Vec::new();
    let mut transitive: BTreeSet<Atom> = BTreeSet::new();
    loop {
        let before = out.len();
        for &i in over {
            let (r, body) = rule_of(i);
            if skip.contains(&i) || r.head.pred.starts_with("__") {
                continue;
            }
            for (j, lit) in r.body.iter().enumerate() {
                let Lit::Pos(a) = lit else { continue };
                if !heads.borrow().has_pred(&a.pred) {
                    continue;
                }
                // The prefix as it ran; what it finds stuck is already known.
                let rec = Rec {
                    rule: i,
                    head: &r.head,
                    text: "",
                    known,
                    aggregates: &c.aggregates,
                    found: RefCell::new(Vec::new()),
                };
                let src = Src {
                    store,
                    body,
                    win: vec![all; r.body.len()],
                    all,
                };
                for row in eval_body(&r.body[..j], &src, &rec)? {
                    let pat = stuck::as_read(&read_pattern(a, &row.s));
                    for (read, nulls) in heads.borrow().matching(&pat) {
                        let mut s = row.s.clone();
                        for (t, v) in a.args.iter().zip(&read.args) {
                            if let (Term::Var(x), Term::Val(v)) = (t, v) {
                                s.entry(x.clone()).or_insert_with(|| v.clone());
                            }
                        }
                        let head = stuck::head_pattern(&r.head, &s, eval_term);
                        let ground = head.args.iter().all(|t| matches!(t, Term::Val(_)));
                        if ground && store.id(&head).is_some() {
                            continue;
                        }
                        let m = stuck::MayDerive {
                            rule: i,
                            head,
                            nulls,
                            transitive: transitive.contains(&read),
                            reads: read,
                        };
                        if !out.contains(&m) {
                            out.push(m);
                        }
                    }
                }
            }
        }
        if out.len() == before {
            break;
        }
        let mut heads = heads.borrow_mut();
        for m in &out[before..] {
            heads.add_head(&m.head, &m.nulls);
            transitive.insert(stuck::as_read(&m.head));
        }
    }
    Ok(out)
}

/// `stuck(RuleId, HeadPattern, Bindings, Nulls)` for each instance, with
/// its rule (or the aggregate) as its firing; the facts.
fn record_stucks(c: &Compiled, prov: &mut Prov, stucks: &[Stuck]) -> Vec<Atom> {
    let mut out = Vec::new();
    for s in stucks {
        let f = s.fact();
        let by = match s.rule {
            Some(i) => c.rule_leaf[i],
            None => c.sigma,
        };
        prov.record(f.clone(), vec![by], vec![]);
        out.push(f);
    }
    out
}

/// Derive `stuck/4` at stratum `s`, below which every body a stuck
/// companion reads is complete (the partition graph's edges): the
/// instances found so far, and those of every rule that
/// can stick and has not run yet, by evaluating its body now (its stuck
/// companion; it finds the same instances again when it runs). Returns
/// the instances derived.
///
/// `s` is the top stratum: a stratum above it needs a negative edge from a
/// node at or above it, and a rule reading that way can stick, which puts
/// `stuck` above the node. So a resumed evaluation starting at the first
/// rule that reads its later facts derives stuck/4 again.
fn derive_stuck(
    c: &Compiled,
    prov: &mut Prov,
    known: &RefCell<stuck::Known>,
    stucks: &[Stuck],
    s: usize,
) -> Result<BTreeSet<Stuck>> {
    let hi = prov.store.len();
    let all = Window::below(hi);
    let mut found: Vec<Stuck> = stucks.to_vec();
    for (i, r) in c.rules.iter().enumerate() {
        if c.rule_strata[i].last().is_some_and(|&t| t < s) {
            continue;
        }
        if !stuck::can_stick(&r.head, &r.body, &c.aggregates) {
            continue;
        }
        let rec = Rec {
            rule: i,
            head: &r.head,
            text: &c.rule_text[i],
            known,
            aggregates: &c.aggregates,
            found: RefCell::new(Vec::new()),
        };
        let src = Src {
            store: &prov.store,
            body: &c.plans[i].body,
            win: vec![all; r.body.len()],
            all,
        };
        eval_rule(r, &c.plans[i], &src, &rec)?;
        found.extend(rec.found.into_inner());
    }
    found.retain(|s| !s.head.pred.starts_with("__"));
    found.sort();
    found.dedup();
    record_stucks(c, prov, &found);
    Ok(found.into_iter().collect())
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
    for a in facts {
        store.insert(a.clone());
    }
    let head = Atom {
        pred: "query".into(),
        args: vec![],
        record: None,
        span: Default::default(),
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
    let all = Window::below(store.len());
    let src = Src {
        store: &store,
        body: &plan,
        win: vec![all; body.len()],
        all,
    };
    Ok(eval_body(body, &src, &rec)?
        .into_iter()
        .map(|r| {
            let b = r.s.into_iter().filter(|(k, _)| !k.starts_with("__"));
            let used = r.used.iter().map(|&t| store.get(t).clone()).collect();
            (b.collect(), used)
        })
        .collect())
}

/// E §2.6: a body predicate with no definition is a compile error. Defined
/// means: a fact or a rule head, a builtin, a compiler-owned or
/// provider-injected predicate, a fact given to this run, or `decl p(..)`.
fn check_defined(
    rules: &[RuleStmt],
    facts: &[Atom],
    externs: &BTreeSet<crate::ast::Extern>,
) -> Result<()> {
    let mut defined: BTreeSet<&str> = facts.iter().map(|a| a.pred.as_str()).collect();
    defined.extend(rules.iter().map(|r| r.head.pred.as_str()));
    defined.extend(externs.iter().map(|e| e.pred.as_str()));
    let is_defined = |p: &str| {
        defined.contains(p)
            || ops::is_builtin_pred(p)
            || matches!(p, "member" | "enumerate")
            || crate::loader::is_core_pred(p)
    };
    let text = |k: usize| partition::fmt_rule(&rules[k]);
    let bodies = rules.iter().map(|r| &r.body);
    let mut errors = Vec::new();
    for (k, body) in bodies.enumerate() {
        for lit in body {
            let (Lit::Pos(a) | Lit::Not(a)) = lit else {
                continue;
            };
            if !is_defined(&a.pred) {
                errors.push(
                    diag::Diagnostic::error(
                        a.span,
                        format!("undefined predicate {}/{}", a.pred, a.args.len()),
                    )
                    .with_note(format!("in rule: {}", text(k)))
                    .with_help(format!(
                        "define it, or declare a predicate a provider feeds with `decl {}({})`",
                        a.pred,
                        crate::transform::columns(a.args.len())
                    )),
                );
            }
        }
    }
    if !errors.is_empty() {
        return Err(diag::Diagnostics(errors).into());
    }
    Ok(())
}

/// `text (at file:line:col, origin)`, or `text` for what the compiler wrote.
fn with_place(text: String, span: Span) -> String {
    match diag::place(span) {
        Some(at) => format!("{text} (at {at})"),
        None => text,
    }
}

/// ` (at file:line:col)` for an error message, or nothing.
fn at_suffix(span: Span) -> String {
    diag::place(span)
        .map(|at| format!(" (at {at})"))
        .unwrap_or_default()
}

/// A contribution to the attribute aggregate: `arg/5`.
fn is_contribution(a: &Atom) -> bool {
    a.pred == "arg" && a.args.len() == 5
}

/// One place a contribution came from.
#[derive(Debug, Clone, Copy)]
enum Origin {
    /// The program states it, here.
    Fact(Span),
    /// Rule `i` derived it.
    Rule(usize),
}

/// Where each contribution came from: the text of every rule that derived
/// it, or of the fact, with where it is written. Spelled out only for a
/// witness the aggregate names.
#[derive(Default, Clone)]
struct Origins {
    by: HashMap<TupleId, Vec<Origin>>,
    /// Each rule's text with its place.
    rules: Vec<String>,
}

impl Origins {
    fn note(&mut self, t: TupleId, o: Origin) {
        self.by.entry(t).or_default().push(o);
    }

    /// The origins of tuple `t` (which is `a`), sorted.
    fn of(&self, t: TupleId, a: &Atom) -> Vec<String> {
        let texts: BTreeSet<String> = self
            .by
            .get(&t)
            .into_iter()
            .flatten()
            .map(|o| match o {
                Origin::Fact(span) => with_place(partition::fmt_atom(a), *span),
                Origin::Rule(i) => self.rules[*i].clone(),
            })
            .collect();
        texts.into_iter().collect()
    }
}

/// One attribute group `(T, A, P)`: the type, the address, the normalized path.
type GroupKey = (String, Value, String);

/// One contribution to a group: the `arg/5` tuple, its rank, and its value
/// after path normalization.
type Contribution = (TupleId, Rank, Value);

/// An element write to a group (R-35): the `arg/5` tuple, its rank, the
/// keyed list's path, the key and the content.
type ElemContribution = (TupleId, Rank, String, Value, Value);

/// An element write's list path and key.
type ElemOf = Option<(String, Value)>;

/// The attribute aggregate of E §2.5 inside the evaluator: `attr/4`,
/// `attr_conflict/5` and `attr_stuck/4` from the `arg/5` contributions, one
/// ranked lattice cell per group, collapsed with F's shadow-aware rule
/// (DR-9 revised). A group is collapsed once, as soon as every partition
/// node that can contribute to it is in a lower stratum; the stratifier
/// puts every reader of the group above that.
///
/// Contributions are grouped as they arrive (the `arg/5` tuples inserted
/// since the last collapse), so a stratum costs the groups it collapses,
/// not a pass over every fact.
#[derive(Clone)]
struct AttrAggregate {
    arg_nodes: Vec<(Node, usize)>,
    /// The types partitioned by address too (`partition::Options::split`):
    /// a group of one is complete per address.
    split: BTreeSet<String>,
    /// `ready_at` per `T`, then `P` (`A\0P` for a type in `split`).
    ready: HashMap<String, HashMap<String, usize>>,
    /// `arg/5` tuples below this id are grouped.
    grouped: TupleId,
    lattices: Option<BTreeMap<(String, String), Lattice>>,
    /// The checkable refinements the engine checks: every `type_refine` and
    /// `attr_refine` fact but those on a `sensitive` path (the provider's).
    refinements: Option<Vec<(TupleId, crate::refine::Stated)>>,
    pending: HashMap<GroupKey, Vec<Contribution>>,
    /// The element writes per group, beside its other contributions.
    elems: HashMap<GroupKey, Vec<ElemContribution>>,
    /// The pending groups by the stratum they are complete at.
    waiting: BTreeMap<usize, BTreeSet<GroupKey>>,
    /// The groups whose base (`transform::ATTR_BASE`) a rule reads, by the
    /// stratum the base is complete at: every contribution but the element
    /// writes.
    waiting_base: BTreeMap<usize, BTreeSet<GroupKey>>,
    base_nodes: Vec<Node>,
    emitted_base: BTreeSet<GroupKey>,
    /// The lattices declared below a type's top attributes, by path.
    nested: HashMap<String, std::rc::Rc<BTreeMap<String, Lattice>>>,
    emitted: BTreeSet<GroupKey>,
    /// Groups that gained a contribution after they were collapsed.
    late: BTreeSet<GroupKey>,
}

impl AttrAggregate {
    fn new(strata: &BTreeMap<Node, usize>, split: &BTreeSet<String>) -> Self {
        let arg_nodes = strata
            .iter()
            .filter(|(n, _)| n.pred == "arg")
            .map(|(n, s)| (n.clone(), *s))
            .collect();
        let base_nodes = strata
            .keys()
            .filter(|n| n.pred == transform::ATTR_BASE)
            .cloned()
            .collect();
        AttrAggregate {
            arg_nodes,
            split: split.clone(),
            ready: HashMap::new(),
            grouped: 0,
            lattices: None,
            refinements: None,
            pending: HashMap::new(),
            elems: HashMap::new(),
            waiting: BTreeMap::new(),
            waiting_base: BTreeMap::new(),
            base_nodes,
            emitted_base: BTreeSet::new(),
            nested: HashMap::new(),
            emitted: BTreeSet::new(),
            late: BTreeSet::new(),
        }
    }

    /// The first stratum at which group `(typ, addr, path)` is complete.
    fn ready_at(&mut self, typ: &str, addr: &Value, path: &str) -> usize {
        let addr = match (self.split.contains(typ), addr) {
            (true, Value::Str(a)) => Some(a.as_str()),
            _ => None,
        };
        let key = match addr {
            Some(a) => format!("{a}\0{path}"),
            None => path.to_string(),
        };
        if let Some(s) = self.ready.get(typ).and_then(|p| p.get(&key)) {
            return *s;
        }
        let node = Node {
            pred: "arg".into(),
            typ: Some(typ.into()),
            path: Some(path.into()),
            addr: addr.map(partition::Addr::exact),
        };
        let s = self
            .arg_nodes
            .iter()
            .filter(|(n, _)| n.unifies(&node))
            .map(|(_, s)| s + 1)
            .max()
            .unwrap_or(0);
        self.ready
            .entry(typ.to_string())
            .or_default()
            .insert(key, s);
        s
    }

    /// Group the contributions inserted since the last call, in fact order.
    fn group_new(&mut self, store: &Store) -> Result<()> {
        let rel = ops::Rel {
            pred: "arg".into(),
            arity: 5,
        };
        let w = Window {
            lo: self.grouped,
            hi: store.len(),
        };
        self.grouped = store.len();
        let mut new: Vec<TupleId> = store.ids(&rel, w).to_vec();
        new.sort_by(|a, b| store.get(*a).cmp(store.get(*b)));
        for t in new {
            let (key, rank, value, elem) = contribution(store.get(t))?;
            if self.emitted.contains(&key) || (elem.is_none() && self.emitted_base.contains(&key)) {
                self.late.insert(key);
            } else {
                if !self.pending.contains_key(&key) && !self.elems.contains_key(&key) {
                    // An input, `let` or output cell is partitioned by its
                    // scope too (`partition::type_path_node`): a module's
                    // input `k` of the copy `m` is the node `m::k`.
                    let path = match key.1.as_str() {
                        Some(scope)
                            if !scope.is_empty() && crate::partition::scoped_cell(&key.0) =>
                        {
                            format!("{scope}::{}", key.2)
                        }
                        _ => key.2.clone(),
                    };
                    let base = self.ready_at(&key.0, &key.1, &path);
                    let elems =
                        self.ready_at(&key.0, &key.1, &format!("{path}{}", transform::ELEM));
                    self.waiting
                        .entry(base.max(elems))
                        .or_default()
                        .insert(key.clone());
                    let read = Node {
                        pred: transform::ATTR_BASE.into(),
                        addr: match (self.split.contains(&key.0), &key.1) {
                            (true, Value::Str(a)) => Some(partition::Addr::exact(a)),
                            _ => None,
                        },
                        typ: Some(key.0.clone()),
                        path: Some(path),
                    };
                    if self.base_nodes.iter().any(|n| n.unifies(&read)) {
                        self.waiting_base
                            .entry(base)
                            .or_default()
                            .insert(key.clone());
                    }
                }
                match elem {
                    Some((list, k)) => {
                        let v = (t, rank, list, k, value);
                        self.elems.entry(key).or_default().push(v);
                    }
                    None => self.pending.entry(key).or_default().push((t, rank, value)),
                }
            }
        }
        Ok(())
    }

    /// The lattices declared below `typ`'s top attributes, by path.
    fn nested(&mut self, typ: &str) -> std::rc::Rc<BTreeMap<String, Lattice>> {
        let lattices = self.lattices.as_ref();
        let n = self.nested.entry(typ.to_string()).or_insert_with(|| {
            let m = lattices
                .into_iter()
                .flat_map(|l| l.range((typ.to_string(), String::new())..))
                .take_while(|((t, _), _)| t == typ)
                .filter(|((_, p), _)| p.contains('.'))
                .map(|((_, p), l)| (p.clone(), l.clone()))
                .collect();
            std::rc::Rc::new(m)
        });
        n.clone()
    }

    /// Collapse every complete group. Rule 3 per key: a group that a stuck
    /// contribution could still join is undetermined and is not collapsed;
    /// it and a Stuck cell are returned as stuck heads `attr(T, A, P, _)`,
    /// so their readers are undetermined too.
    fn emit_ready(
        &mut self,
        stratum: usize,
        prov: &mut Prov,
        origins: &Origins,
        known: &stuck::Known,
        sigma: NodeId,
    ) -> Result<Vec<Stuck>> {
        if self.lattices.is_none() {
            self.lattices = Some(declared_lattices(prov.store.atoms())?);
        }
        if self.refinements.is_none() {
            self.refinements = Some(declared_refinements(&prov.store)?);
        }
        self.group_new(&prov.store)?;
        // The groups complete at this stratum, in group order.
        let next = stratum.checked_add(1);
        let take = |w: &mut BTreeMap<usize, BTreeSet<GroupKey>>| -> BTreeSet<GroupKey> {
            let later = next.map(|n| w.split_off(&n)).unwrap_or_default();
            std::mem::replace(w, later)
                .into_values()
                .flatten()
                .collect()
        };
        let bases = take(&mut self.waiting_base);
        let keys = take(&mut self.waiting);
        let mut out = Vec::new();
        let mut stuck_groups = Vec::new();
        // Bases first: a group's base is complete no later than the group.
        let groups = bases
            .into_iter()
            .map(|k| (k, true))
            .chain(keys.into_iter().map(|k| (k, false)));
        for (key, base) in groups {
            let mut contribs = match base {
                true => self.pending.get(&key).cloned().unwrap_or_default(),
                false => self.pending.remove(&key).unwrap_or_default(),
            };
            contribs.sort_by(|a, b| prov.store.get(a.0).cmp(prov.store.get(b.0)));
            let mut elems = match base {
                true => vec![],
                false => self.elems.remove(&key).unwrap_or_default(),
            };
            elems.sort_by(|a, b| prov.store.get(a.0).cmp(prov.store.get(b.0)));
            let (typ, addr, path) = &key;
            let pred = if base { transform::ATTR_BASE } else { "attr" };
            let read = Atom {
                pred: pred.into(),
                args: vec![
                    str_term(typ),
                    Term::Val(addr.clone()),
                    str_term(path),
                    Term::Wildcard,
                ],
                record: None,
                span: Default::default(),
            };
            let group_stuck = |nulls: BTreeSet<String>, reason: String| Stuck {
                rule: None,
                head: read.clone(),
                bindings: BTreeMap::new(),
                nulls,
                reason,
                text: format!("attr({typ}, {}, {path}, _)", partition::fmt_value(addr)),
            };
            let any = Atom {
                pred: "attr".into(),
                ..read.clone()
            };
            if known.any(&any) {
                stuck_groups.push(group_stuck(
                    known.blocking(&any),
                    "a stuck rule instance may still contribute to this attribute".into(),
                ));
            } else {
                let lat = self
                    .lattices
                    .as_ref()
                    .and_then(|l| l.get(&(typ.clone(), path.clone())))
                    .cloned()
                    .unwrap_or_else(|| infer_lattice(&contribs));
                // An element is written by its key: the list declares one.
                for (t, _, list, _, _) in &elems {
                    let keyed = self
                        .lattices
                        .as_ref()
                        .and_then(|l| l.get(&(typ.clone(), list.clone())));
                    if !matches!(keyed, Some(Lattice::Keyed { .. })) {
                        let a = prov.store.get(*t);
                        bail!(
                            "resource {typ}[{}]: {list} is not a keyed list, so an element of it \
                             is not written by its key; declare the field that names an element, \
                             type_list_key({typ}, \"{list}\", [\"FIELD\"]), or write the whole \
                             list\n  in: {}",
                            partition::fmt_value(addr),
                            origins.of(*t, a).join("; ")
                        );
                    }
                }
                let nested = self.nested(typ);
                let refs: Vec<&(TupleId, crate::refine::Stated)> = match base {
                    true => vec![],
                    false => self
                        .refinements
                        .iter()
                        .flatten()
                        .filter(|(_, r)| r.applies(typ, addr, path))
                        .collect(),
                };
                let mut cell = collapse_group(
                    &key,
                    &contribs,
                    &elems,
                    &refs,
                    &lat,
                    &nested,
                    origins,
                    &prov.store,
                );
                if base {
                    // The base's value, or nothing: its conflicts and
                    // warnings are the group's, said once.
                    cell.retain(|a| a.pred == "attr" || a.pred == "attr_stuck");
                    for a in cell.iter_mut().filter(|a| a.pred == "attr") {
                        a.pred = transform::ATTR_BASE.into();
                    }
                }
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
                // Σ over the group: every contribution that reached it,
                // and every refinement joined into it.
                let children: Vec<NodeId> = std::iter::once(sigma)
                    .chain(contribs.iter().map(|(t, _, _)| prov.id(*t)))
                    .chain(elems.iter().map(|(t, ..)| prov.id(*t)))
                    .chain(refs.iter().map(|(t, _)| prov.id(*t)))
                    .collect();
                for a in cell
                    .into_iter()
                    .filter(|a| !(base && a.pred == "attr_stuck"))
                {
                    out.push((a, children.clone()));
                }
            }
            match base {
                true => self.emitted_base.insert(key),
                false => self.emitted.insert(key),
            };
        }
        for (a, children) in out {
            prov.record(a, children, vec![]);
        }
        Ok(stuck_groups)
    }

    /// Guard on the stratifier: no contribution arrived after its group was
    /// collapsed.
    fn check_complete(&self) -> Result<()> {
        if let Some(key) = self.late.iter().next() {
            bail!(
                "internal: attribute {} {} {} gained a contribution after it was collapsed",
                key.0,
                partition::fmt_value(&key.1),
                key.2
            );
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

/// An `arg/5` contribution's group `(T, A, normalized P)`, rank and value;
/// for an element write (`transform::ELEM`) its list's path and key, and
/// the value is the element's content.
fn contribution(a: &Atom) -> Result<(GroupKey, Rank, Value, ElemOf)> {
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
    if let Some(list) = path.strip_suffix(transform::ELEM) {
        let Value::List(kv) = vals[3] else {
            bail!("internal: element write {}", partition::fmt_atom(a));
        };
        let [k, v] = kv.as_slice() else {
            bail!("internal: element write {}", partition::fmt_atom(a));
        };
        let top = list.split('.').next().unwrap_or(list).to_string();
        let group = (typ.to_string(), vals[1].clone(), top);
        return Ok((group, rank, v.clone(), Some((list.to_string(), k.clone()))));
    }
    let (path, value) = transform::normalize_contribution(typ, path, Term::Val(vals[3].clone()));
    let value = eval_term(&value, &HashMap::new()).ok_or_else(|| anyhow!("internal: normalize"))?;
    Ok(((typ.to_string(), vals[1].clone(), path), rank, value, None))
}

/// `type_lattice(T, P, flat|map|set)` and `type_list_key(T, P, Keys)`
/// facts, a keyed list's with its keys' `type_default(T, P.K, V)`.
fn declared_lattices(facts: &[Atom]) -> Result<BTreeMap<(String, String), Lattice>> {
    let mut defaults: BTreeMap<(&str, &str), &Value> = BTreeMap::new();
    for a in facts.iter().filter(|a| a.pred == "type_default") {
        if let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(p)),
            Term::Val(v),
        ] = a.args.as_slice()
        {
            defaults.insert((t, p), v);
        }
    }
    let key_defaults = |t: &str, p: &str, keys: &[String]| -> BTreeMap<String, Value> {
        keys.iter()
            .filter_map(|k| {
                let at = format!("{p}.{k}");
                let d = defaults.get(&(t, at.as_str()))?;
                Some((k.clone(), (*d).clone()))
            })
            .collect()
    };
    let mut decls: Vec<&Atom> = facts
        .iter()
        .filter(|a| LATTICE_DECLS.contains(&a.pred.as_str()))
        .collect();
    decls.sort();
    let mut out = BTreeMap::new();
    for a in decls {
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
            ("type_list_key", Value::List(ks)) => {
                let keys: Vec<String> = ks.iter().map(crate::functions::value_to_string).collect();
                Lattice::Keyed {
                    defaults: key_defaults(t, p, &keys),
                    keys,
                    elem: Box::new(Lattice::Map(Box::new(Lattice::Flat))),
                }
            }
            ("type_list_key", Value::Str(k)) => Lattice::Keyed {
                keys: vec![k.clone()],
                defaults: key_defaults(t, p, std::slice::from_ref(k)),
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

/// `type_refine/3` and `attr_refine/4` facts, but those on a path a
/// `type_attr` fact marks `sensitive` (or below one): the engine never
/// checks a secret; the provider does, from an Apply assertion (F DR-13
/// revised).
fn declared_refinements(store: &Store) -> Result<Vec<(TupleId, crate::refine::Stated)>> {
    let mut sensitive: BTreeSet<(&str, &str)> = BTreeSet::new();
    for a in store.atoms().iter().filter(|a| a.pred == "type_attr") {
        if let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(p)),
            _,
            Term::Val(Value::List(flags)),
        ] = a.args.as_slice()
            && flags.iter().any(|f| f.as_str() == Some("sensitive"))
        {
            sensitive.insert((t, p));
        }
    }
    let is_sensitive = |t: &str, p: &str| {
        std::iter::successors(Some(p), |p| p.rsplit_once('.').map(|x| x.0))
            .any(|p| sensitive.contains(&(t, p)))
    };
    let mut out = Vec::new();
    for (t, a) in store.atoms().iter().enumerate() {
        let Some(r) = crate::refine::Stated::of(a) else {
            continue;
        };
        let r = r.map_err(|e| anyhow!("{}: {e}", partition::fmt_atom(a)))?;
        if !is_sensitive(&r.typ, &r.path) {
            out.push((t as TupleId, r));
        }
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

fn obj(kv: Vec<(&str, Value)>) -> Value {
    Value::Obj(kv.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// The collapsed cell as facts: the value, or the conflict with a `deny`
/// naming every witness, or the stuck disagreement; plus a warning per
/// shadowed disagreement at a losing rank.
#[allow(clippy::too_many_arguments)]
fn collapse_group(
    key: &GroupKey,
    contribs: &[Contribution],
    elems: &[ElemContribution],
    refs: &[&(TupleId, crate::refine::Stated)],
    lat: &Lattice,
    nested: &BTreeMap<String, Lattice>,
    origins: &Origins,
    store: &Store,
) -> Vec<Atom> {
    let (typ, addr, path) = key;
    let cells: Vec<RankedContribution> = contribs
        .iter()
        .enumerate()
        .map(|(i, (_, r, v))| (i as u32, *r, v.clone()))
        .collect();
    // An element write's witness follows the contributions', a
    // refinement's the element writes'.
    let writes: Vec<lattice::ElemWrite> = elems
        .iter()
        .enumerate()
        .map(|(i, (_, r, list, k, v))| lattice::ElemWrite {
            witness: (contribs.len() + i) as u32,
            rank: *r,
            list: list.clone(),
            key: k.clone(),
            value: v.clone(),
        })
        .collect();
    let first_ref = contribs.len() + elems.len();
    let refinements: Vec<lattice::Refinement> = refs
        .iter()
        .enumerate()
        .map(|(i, (_, r))| lattice::Refinement {
            path: r.path.clone(),
            constraint: r.constraint.clone(),
            witness: (first_ref + i) as u32,
        })
        .collect();
    let refinement_of = |w: u32| refs.get((w as usize).checked_sub(first_ref)?);
    let head = |pred: &str, rest: Vec<Value>| Atom {
        pred: pred.into(),
        args: [str_term(typ), Term::Val(addr.clone()), str_term(path)]
            .into_iter()
            .chain(rest.into_iter().map(Term::Val))
            .collect(),
        record: None,
        span: Default::default(),
    };
    let witness = |w: u32| {
        if let Some((t, r)) = refinement_of(w) {
            let a = store.get(*t);
            return obj(vec![
                ("rank", Value::Str("refinement".into())),
                ("value", Value::Str(r.constraint.to_string())),
                (
                    "from",
                    Value::List(vec![Value::Str(with_place(partition::fmt_atom(a), a.span))]),
                ),
            ]);
        }
        let (t, r, v) = match contribs.get(w as usize) {
            Some((t, r, v)) => (t, r, v),
            None => {
                let (t, r, _, _, v) = &elems[w as usize - contribs.len()];
                (t, r, v)
            }
        };
        obj(vec![
            ("rank", Value::Str(rank_name(*r).into())),
            ("value", v.clone()),
            (
                "from",
                Value::List(
                    origins
                        .of(*t, store.get(*t))
                        .into_iter()
                        .map(Value::Str)
                        .collect(),
                ),
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
        args: vec![str_term(msg), Term::Val(ctx)],
        record: None,
        span: Default::default(),
    };
    let mut out = Vec::new();
    let below = lattice::Below {
        lattices: Some(nested),
        elems: &writes,
    };
    let shadowed = match lattice::lub_ranked_refined(lat, path, &cells, &refinements, below) {
        Collapsed::Bottom => vec![],
        Collapsed::Val {
            value,
            shadowed,
            deferred,
            ..
        } => {
            out.push(head("attr", vec![value]));
            for d in deferred {
                let nulls = lattice::nulls_in(&d.value);
                out.push(Atom {
                    pred: crate::refine::DEFERRED.into(),
                    args: [
                        Value::Str(typ.clone()),
                        addr.clone(),
                        Value::Str(d.path),
                        Value::Str(d.constraint.to_string()),
                        Value::List(nulls.into_iter().map(Value::Str).collect()),
                    ]
                    .into_iter()
                    .map(Term::Val)
                    .collect(),
                    record: None,
                    span: Default::default(),
                });
            }
            shadowed
        }
        Collapsed::Violated {
            path: at,
            constraint,
            value,
            witnesses: ws,
            refinement,
            shadowed,
        } => {
            let first = |w: &Witnesses| {
                w.iter()
                    .next()
                    .map(|w| witness(*w))
                    .unwrap_or(Value::Obj(BTreeMap::new()))
            };
            out.push(head("attr_conflict", vec![first(&ws), first(&refinement)]));
            let place = refinement
                .iter()
                .find_map(|w| refinement_of(*w))
                .and_then(|(t, _)| diag::place(store.get(*t).span))
                .unwrap_or_default();
            let mut kv = vec![
                ("type", Value::Str(typ.clone())),
                ("addr", addr.clone()),
                ("path", Value::Str(at.clone())),
                ("constraint", Value::Str(constraint.to_string())),
                (
                    "reason",
                    Value::Str(format!(
                        "{} violates {constraint}",
                        partition::fmt_value(&value)
                    )),
                ),
                ("value", value),
                (
                    "witnesses",
                    witnesses(&ws.union(&refinement).copied().collect()),
                ),
            ];
            if !place.is_empty() {
                kv.push(("at", Value::Str(place)));
            }
            out.push(policy("deny", crate::refine::VIOLATED, obj(kv)));
            shadowed
        }
        Collapsed::Stuck {
            nulls, shadowed, ..
        } => {
            out.push(head(
                "attr_stuck",
                vec![Value::List(nulls.into_iter().map(Value::Str).collect())],
            ));
            shadowed
        }
        Collapsed::Conflict {
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
                        .map(|n| format!("?{}", crate::ir::label(n)))
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
        Value::Float(f) => serde_json::Number::from_f64(f.get())
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
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
        Value::Quantity(q) => serde_json::Value::String(q.to_string()),
        Value::Time(t) => serde_json::Value::String(t.to_string()),
        Value::Url(u) | Value::Oci(u) => serde_json::Value::String(u.clone()),
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

#[cfg(feature = "test-hooks")]
use crate::hooks::{Rule3 as Rule3Clause, planted};

/// A clause of Rule 3 the property test can plant a bug in (`hooks`).
#[cfg(not(feature = "test-hooks"))]
enum Rule3Clause {
    Negation,
    Reader,
}

/// Without `test-hooks`, no clause is ever skipped.
#[cfg(not(feature = "test-hooks"))]
#[inline(always)]
fn planted(_: Rule3Clause) -> bool {
    false
}

/// Rule 2's recorder for the rule being evaluated (E §2.7, F DR-2 revised):
/// every instance that needs a null's content is recorded here instead of
/// firing. `known` holds the stuck and may-derive heads of lower strata,
/// for Rule 3.
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
/// content positions `eval_rule_collect` decides) and the functions
/// declared `forwards nulls`.
fn forwards_nulls(name: &str) -> bool {
    partition::AGGREGATES.contains(&name)
        || crate::functions::get(name).is_some_and(|f| f.forwards_nulls)
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
        span: Default::default(),
    }
}

/// One choice a body made on the way to a row: the tuple a relation read
/// matched, or the element a `member`/`enumerate` expanded to.
#[derive(Debug, Clone, Copy)]
enum Choice {
    Tuple(TupleId),
    Nth(u32),
}

/// The order a naive evaluation over the facts in fact order would produce
/// rows in: lexicographic in the choices, a tuple by its fact.
fn cmp_order(store: &Store, a: &[Choice], b: &[Choice]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let o = match (x, y) {
            (Choice::Tuple(x), Choice::Tuple(y)) if x == y => Ordering::Equal,
            (Choice::Tuple(x), Choice::Tuple(y)) => store.get(*x).cmp(store.get(*y)),
            (Choice::Nth(x), Choice::Nth(y)) => x.cmp(y),
            (Choice::Tuple(_), Choice::Nth(_)) => Ordering::Less,
            (Choice::Nth(_), Choice::Tuple(_)) => Ordering::Greater,
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// One derived head with what it was derived from, for the circuit.
struct Derived {
    head: Atom,
    /// The tuples the body matched.
    used: Vec<TupleId>,
    absent: Vec<Atom>,
    bindings: Vec<(String, Value)>,
    order: Vec<Choice>,
}

/// What a body reads: the store, the compiled body, and per body literal
/// the window of tuples a relation read there sees (semi-naive: the tuples
/// before the delta, the delta, or every tuple). Negation reads `all`.
struct Src<'a> {
    store: &'a Store,
    body: &'a ops::Body,
    win: Vec<Window>,
    all: Window,
}

fn eval_rule(rule: &RuleStmt, plan: &ops::Rule, src: &Src, rec: &Rec) -> Result<Vec<Derived>> {
    if let ops::Head::Agg { col, kind } = plan.head {
        return eval_rule_collect(rule, src, col, kind, rec);
    }

    let mut out = Vec::new();
    let rows = eval_body(&rule.body, src, rec)?;
    for Row {
        s: b,
        used,
        absent,
        order,
    } in rows
    {
        // Rule 2: an address argument is a content position. A head whose
        // address carries a null is stuck, not derived.
        if let Some(t) = crate::zset::address_arg(&rule.head)
            && let Some(v) = eval_term(t, &b)
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
        let head = match instantiate_atom(&rule.head, &b) {
            Ok(h) => h,
            Err(e) => match head_error(&rule.head, &b) {
                Some(why) => bail!(why),
                None => return Err(e.context(format!("instantiate head {}", rule.head.pred))),
            },
        };
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
            order,
        });
    }
    Ok(out)
}

/// An aggregate rule. Rule 2: the group key is a content position, and so
/// is the aggregated value of `count`, `sum`, `min`, `max`, `any` and `all`
/// (a fresh null too: its order and its sum are content);
/// `collect_set`/`collect_list` forward nulls. Rule 3: a group is undetermined, and not derived, when
/// its key unifies with a stuck instance of this rule or with a stuck head
/// of a predicate the body reads.
///
/// `count` and `sum` fold every body match of the group, `min` and `max`
/// their least and greatest, `any` and `all` their bools; `collect_list`
/// keeps the rows' order. A group has at least one match, so an empty
/// group derives nothing. A group `sum` has a non-int in, `min`/`max` one
/// that is neither an int nor a string or a mix of the two, or `any`/`all`
/// a non-bool, derives a deny naming the group and the value instead of
/// its head.
fn eval_rule_collect(
    rule: &RuleStmt,
    src: &Src,
    idx: usize,
    kind: AggKind,
    rec: &Rec,
) -> Result<Vec<Derived>> {
    let Term::Func { name, args } = &rule.head.args[idx] else {
        bail!("internal: collect idx not func");
    };
    if args.len() != 1 {
        bail!("collect*(...) must have exactly one argument");
    }
    let item_term = args[0].clone();

    let rows = eval_body(&rule.body, src, rec)?;
    let mut groups_set: BTreeMap<Vec<Value>, BTreeSet<Value>> = BTreeMap::new();
    // `collect_list` keeps each item's row order (`cmp_order`).
    let mut groups_list: BTreeMap<Vec<Value>, Vec<(Vec<Choice>, Value)>> = BTreeMap::new();
    // Σ over the group: every body fact and negation of every member.
    let mut group_prov: BTreeMap<Vec<Value>, (BTreeSet<TupleId>, BTreeSet<Atom>)> = BTreeMap::new();
    for Row {
        s: b,
        used,
        absent,
        order,
    } in rows
    {
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
        if !matches!(kind, AggKind::Set | AggKind::List) && stuck::has_null(&item) {
            rec.stuck(&b, nulls_in(&item), format!("{name} over a null"));
            continue;
        }
        let prov = group_prov.entry(key.clone()).or_default();
        prov.0.extend(used);
        prov.1.extend(absent);
        match kind {
            AggKind::Set => {
                groups_set.entry(key).or_default().insert(item);
            }
            _ => {
                groups_list.entry(key).or_default().push((order, item));
            }
        }
    }

    // Rule 3: a stuck head of a body predicate makes the groups it could
    // feed undetermined.
    for lit in &rule.body {
        let Lit::Pos(a) = lit else { continue };
        if a.pred == "member" || a.pred == "enumerate" || ops::is_builtin_pred(&a.pred) {
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
    let mut emit_group = |key: Vec<Value>, items: Vec<Value>| {
        let (used, absent) = group_prov.remove(&key).unwrap_or_default();

        let mut key_pat = Atom {
            pred: rule.head.pred.clone(),
            args: Vec::with_capacity(rule.head.args.len()),
            record: None,
            span: Default::default(),
        };
        let mut k = 0usize;
        for i in 0..rule.head.args.len() {
            if i == idx {
                key_pat.args.push(Term::Wildcard);
            } else {
                key_pat.args.push(Term::Val(key[k].clone()));
                k += 1;
            }
        }
        if undetermined
            .iter()
            .any(|u| stuck::patterns_unify(u, &key_pat))
        {
            return;
        }
        let head = match fold(name, kind, items) {
            Ok(v) => {
                let mut group = key_pat;
                group.args[idx] = Term::Val(v);
                group
            }
            Err(msg) => Atom {
                pred: "deny".into(),
                args: vec![
                    Term::Val(Value::Str(format!(
                        "{}: {msg}",
                        partition::fmt_atom(&key_pat)
                    ))),
                    Term::Val(obj(vec![
                        ("pred", Value::Str(rule.head.pred.clone())),
                        ("group", Value::List(key)),
                        ("rule", Value::Str(rec.text.to_string())),
                    ])),
                ],
                record: None,
                span: rule.head.span,
            },
        };
        let n = out.len() as u32;
        out.push(Derived {
            head,
            used: used.into_iter().collect(),
            absent: absent.into_iter().collect(),
            bindings: vec![],
            order: vec![Choice::Nth(n)],
        });
    };

    for (key, items) in groups_set {
        emit_group(key, items.into_iter().collect());
    }
    for (key, mut items) in groups_list {
        // `collect_list` is in the order of the body's rows: the first
        // relation's rows in order (a list's elements in its order), then
        // the next's. The other folds see their items sorted.
        if kind == AggKind::List {
            items.sort_by(|(a, x), (b, y)| cmp_order(src.store, a, b).then_with(|| x.cmp(y)));
        } else {
            items.sort_by(|(_, x), (_, y)| x.cmp(y));
        }
        emit_group(key, items.into_iter().map(|(_, v)| v).collect());
    }

    Ok(out)
}

/// A group's aggregated value, from its items sorted; `Err` with what is
/// wrong with them.
fn fold(name: &str, kind: AggKind, items: Vec<Value>) -> std::result::Result<Value, String> {
    match kind {
        AggKind::Set | AggKind::List => Ok(Value::List(items)),
        AggKind::Count => Ok(Value::Int(items.len() as i64)),
        // A sum of quantities is of their dimension (R-66).
        AggKind::Sum if matches!(items.first(), Some(Value::Quantity(_))) => {
            let mut total: Option<crate::quantity::Quantity> = None;
            for v in &items {
                let Value::Quantity(q) = v else {
                    return Err(format!(
                        "{name}() over {}, which is not a quantity",
                        partition::fmt_value(v)
                    ));
                };
                total = Some(match total {
                    None => *q,
                    Some(t) => crate::quantity::add(&t, q, false).ok_or_else(|| {
                        format!(
                            "{name}() over {t} and {q}, which do not add: {} and {}",
                            t.dim().name(),
                            q.dim().name()
                        )
                    })?,
                });
            }
            Ok(Value::Quantity(total.expect("a group has a row")))
        }
        // A sum with a float in it is a float (R-75).
        AggKind::Sum if items.iter().any(|v| matches!(v, Value::Float(_))) => {
            let mut total = 0f64;
            for v in &items {
                total += match v {
                    Value::Int(n) => *n as f64,
                    Value::Float(f) => f.get(),
                    v => {
                        return Err(format!(
                            "{name}() over {}, which is not a number",
                            partition::fmt_value(v)
                        ));
                    }
                };
            }
            crate::value::Float::new(total)
                .map(Value::Float)
                .ok_or_else(|| format!("{name}() overflows"))
        }
        // The least and greatest quantity or time (R-66, R-62), in its
        // dimension's order, and number with a float among them by value.
        AggKind::Min | AggKind::Max
            if matches!(items.first(), Some(Value::Quantity(_) | Value::Time(_)))
                || items.iter().any(|v| matches!(v, Value::Float(_))) =>
        {
            let mut best = items[0].clone();
            for v in &items[1..] {
                let o = order(v, &best).map_err(|e| format!("{name}() over {e}"))?;
                if (kind == AggKind::Min && o.is_lt()) || (kind == AggKind::Max && o.is_gt()) {
                    best = v.clone();
                }
            }
            Ok(best)
        }
        AggKind::Sum => {
            let mut total = 0i64;
            for v in &items {
                let Value::Int(n) = v else {
                    return Err(format!(
                        "{name}() over {}, which is not an int",
                        partition::fmt_value(v)
                    ));
                };
                total = total
                    .checked_add(*n)
                    .ok_or_else(|| format!("{name}() overflows at {}", partition::fmt_value(v)))?;
            }
            Ok(Value::Int(total))
        }
        AggKind::Min | AggKind::Max => {
            // Sorted, kind first: every item is of one kind iff the first
            // and the last are.
            let (Some(first), Some(last)) = (items.first(), items.last()) else {
                return Err(format!("{name}() over no value"));
            };
            for v in [first, last] {
                if !matches!(v, Value::Int(_) | Value::Str(_)) {
                    return Err(format!(
                        "{name}() over {}, which is neither an int nor a string",
                        partition::fmt_value(v)
                    ));
                }
            }
            if std::mem::discriminant(first) != std::mem::discriminant(last) {
                return Err(format!(
                    "{name}() over {} and {}, an int and a string",
                    partition::fmt_value(first),
                    partition::fmt_value(last)
                ));
            }
            let v = if kind == AggKind::Min { first } else { last };
            Ok(v.clone())
        }
        AggKind::Any | AggKind::All => {
            let mut bools = Vec::with_capacity(items.len());
            for v in &items {
                let Value::Bool(b) = v else {
                    return Err(format!(
                        "{name}() over {}, which is not a bool",
                        partition::fmt_value(v)
                    ));
                };
                bools.push(*b);
            }
            Ok(Value::Bool(if kind == AggKind::Any {
                bools.contains(&true)
            } else {
                !bools.contains(&false)
            }))
        }
    }
}

/// One way to satisfy a body: the bindings, and for provenance the tuples
/// it matched and the negations that held; `order` for the naive order.
struct Row {
    s: HashMap<String, Value>,
    used: Vec<TupleId>,
    absent: Vec<Atom>,
    order: Vec<Choice>,
}

impl Row {
    fn with(&self, s: HashMap<String, Value>) -> Row {
        Row {
            s,
            used: self.used.clone(),
            absent: self.absent.clone(),
            order: self.order.clone(),
        }
    }
}

/// The key columns of a relation read under `s`, when every one is bound
/// to a value without a null (otherwise the read scans).
fn probe(atom: &Atom, read: &ops::Read, s: &HashMap<String, Value>) -> Option<Vec<Value>> {
    if read.key.is_empty() || read.scan_all {
        return None;
    }
    let mut out = Vec::with_capacity(read.key.len());
    for &c in &read.key {
        let v = match &atom.args[c] {
            Term::Val(v) => v.clone(),
            Term::Var(x) => s.get(x)?.clone(),
            _ => return None,
        };
        if stuck::has_null(&v) {
            return None;
        }
        out.push(v);
    }
    // A loose read compares other columns too: a null bound there can
    // make a tuple outside the bucket undecided.
    if read.loose && atom.args.iter().any(|t| term_has_bound_null(t, s)) {
        return None;
    }
    Some(out)
}

/// Does a variable of `t` hold a value with a null under `s`?
fn term_has_bound_null(t: &Term, s: &HashMap<String, Value>) -> bool {
    match t {
        Term::Var(x) => s.get(x).is_some_and(stuck::has_null),
        Term::Func { args, .. } | Term::List(args) => {
            args.iter().any(|a| term_has_bound_null(a, s))
        }
        Term::Obj(m) => m.values().any(|a| term_has_bound_null(a, s)),
        Term::Val(_) | Term::Wildcard | Term::ListComp { .. } => false,
    }
}

fn eval_body(body: &[Lit], src: &Src, rec: &Rec) -> Result<Vec<Row>> {
    let mut states: Vec<Row> = vec![Row {
        s: HashMap::new(),
        used: Vec::new(),
        absent: Vec::new(),
        order: Vec::new(),
    }];
    for (i, lit) in body.iter().enumerate() {
        let mut next = Vec::new();
        match lit {
            Lit::Pos(atom) => {
                if atom.pred == "member" || atom.pred == "enumerate" {
                    for row in &states {
                        if !rec.any_blocked(&atom.args, &row.s) {
                            let mut out = Vec::new();
                            eval_member_like(atom, &row.s, &mut out, rec)?;
                            next.extend(out.into_iter().enumerate().map(|(n, s)| {
                                let mut r = row.with(s);
                                r.order.push(Choice::Nth(n as u32));
                                r
                            }));
                        }
                    }
                } else if ops::is_builtin_pred(&atom.pred) {
                    for row in &states {
                        if !rec.any_blocked(&atom.args, &row.s) && eval_builtin_pred(atom, &row.s)?
                        {
                            next.push(row.with(row.s.clone()));
                        }
                    }
                } else {
                    let read =
                        src.body.op(i).read().ok_or_else(|| {
                            anyhow!("internal: {} is not a relation read", atom.pred)
                        })?;
                    for row in &states {
                        let s = &row.s;
                        if rec.any_blocked(&atom.args, s) {
                            continue;
                        }
                        // Rule 3: a positive reader of an undetermined
                        // aggregate group is undetermined. It still reads
                        // the groups that were decided.
                        if rec.aggregates.contains(&atom.pred) && !planted(Rule3Clause::Reader) {
                            let nulls = rec.known.borrow().blocking(&read_pattern(atom, s));
                            if !nulls.is_empty() {
                                rec.stuck(
                                    s,
                                    nulls,
                                    format!("reads undetermined aggregate {}", atom.pred),
                                );
                            }
                        }
                        let key = probe(atom, read, s);
                        for t in src.store.candidates(
                            &read.rel,
                            &read.key,
                            key.as_deref(),
                            read.loose,
                            src.win[i],
                        ) {
                            if let Some(s2) = unify_atom(atom, src.store.get(t), s, rec)? {
                                let mut r = row.with(s2);
                                r.used.push(t);
                                r.order.push(Choice::Tuple(t));
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
                    if ops::is_builtin_pred(&atom.pred) {
                        // Negation-as-failure for builtin predicates is just boolean negation.
                        if !eval_builtin_pred(atom, s)? {
                            next.push(row.with(s.clone()));
                        }
                        continue;
                    }
                    let grounded = ground_atom(atom, s)
                        .with_context(|| format!("unsafe negation: not {}(...)", atom.pred))?;
                    if eval_not(&grounded, src, s, rec) {
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
fn eval_not(grounded: &Atom, src: &Src, s: &HashMap<String, Value>, rec: &Rec) -> bool {
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
    // Every column is bound: an index lookup on all of them.
    let rel = ops::Rel::of(grounded);
    let key: ops::Key = (0..rel.arity).collect();
    let probe: Option<Vec<Value>> = grounded
        .args
        .iter()
        .map(|t| match t {
            Term::Val(v) if !stuck::has_null(v) => Some(v.clone()),
            _ => None,
        })
        .collect();
    let probe = probe.filter(|p| !p.is_empty());
    let mut unknown = BTreeSet::new();
    for t in src
        .store
        .candidates(&rel, &key, probe.as_deref(), false, src.all)
    {
        let f = src.store.get(t);
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
    if (!nulls.is_empty() || known.any(grounded)) && !planted(Rule3Clause::Negation) {
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
    if missing_walk(&atom.args[0], state) {
        return Ok(());
    }
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe member: list is not ground"))?;
    if let Value::Null { .. } = &list_v {
        // Rule 2: member over a null list is a content position.
        rec.stuck(state, nulls_in(&list_v), "member/2 over a null list");
        return Ok(());
    }
    let items = match list_v {
        Value::List(items) => items,
        Value::Obj(_) => bail!(
            "`x in e` over an object, {}, in `{}`: an object's entries are matched by a \
             pattern, `(key, value) in e` (R-58)",
            partition::fmt_value(&list_v),
            rec.text
        ),
        _ => bail!("member/2 first argument must be a list"),
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

/// `not (k, v) in e`: no entry matches; a `_` in either pattern matches
/// anything.
fn eval_not_member3(atom: &Atom, state: &HashMap<String, Value>) -> Result<bool> {
    if missing_walk(&atom.args[0], state) {
        return Ok(true);
    }
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe not member: list is not ground"))?;
    let entries = entries(list_v)
        .ok_or_else(|| anyhow!("member/3 first argument must be a list or an object"))?;
    for (k, v) in &entries {
        if matches_ground(&atom.args[1], k, state)? && matches_ground(&atom.args[2], v, state)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A list's indexes and elements, or an object's keys and values in key
/// order: what `(k, v) in e` enumerates.
fn entries(v: Value) -> Option<Vec<(Value, Value)>> {
    match v {
        Value::List(items) => Some(
            items
                .into_iter()
                .enumerate()
                .map(|(i, x)| (Value::Int(i as i64), x))
                .collect(),
        ),
        Value::Obj(m) => Some(m.into_iter().map(|(k, x)| (Value::Str(k), x)).collect()),
        _ => None,
    }
}

/// Whether a negated pattern matches `v`: `_` anything, a tuple element by
/// element, anything else by its value.
fn matches_ground(t: &Term, v: &Value, state: &HashMap<String, Value>) -> Result<bool> {
    match t {
        Term::Wildcard => Ok(true),
        Term::List(ts) => {
            let Value::List(vs) = v else { return Ok(false) };
            if ts.len() != vs.len() {
                return Ok(false);
            }
            for (t, v) in ts.iter().zip(vs) {
                if !matches_ground(t, v, state)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        t => {
            let w = eval_term(t, state)
                .ok_or_else(|| anyhow!("unsafe not member: pattern is not ground"))?;
            Ok(&w == v)
        }
    }
}

fn eval_member3(
    atom: &Atom,
    state: &HashMap<String, Value>,
    out: &mut Vec<HashMap<String, Value>>,
    rec: &Rec,
) -> Result<()> {
    if missing_walk(&atom.args[0], state) {
        return Ok(());
    }
    let list_v = eval_term(&atom.args[0], state)
        .ok_or_else(|| anyhow!("unsafe member: list is not ground"))?;
    if let Value::Null { .. } = &list_v {
        rec.stuck(state, nulls_in(&list_v), "member/3 over a null list");
        return Ok(());
    }
    // An object's keys and values (R-58), a list's indexes and elements.
    let entries = entries(list_v)
        .ok_or_else(|| anyhow!("member/3 first argument must be a list or an object"))?;
    for (k, item) in &entries {
        let mut s2 = state.clone();
        if !unify_term(&atom.args[1], k, &mut s2, rec)? {
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
                let prefix = crate::ir::scoped(&scope, "");
                if let Some(suffix) = full.strip_prefix(&prefix)
                    && !crate::ir::is_scoped(suffix)
                {
                    let mut tmp = out.clone();
                    if unify_term(&args[1], &Value::Str(suffix.to_string()), &mut tmp, rec)? {
                        *out = tmp;
                        return Ok(true);
                    }
                }
                // As the function: a bound name that is already an address
                // is itself (R-65), another copy's resource read through
                // its output. An unbound one ranges over the scope's own.
                return Ok(crate::ir::is_scoped(full)
                    && eval_term(&args[1], out).is_some_and(|v| v == *fv));
            }
            // `ref(T, A, P)` as a pattern takes a reference apart (R-42):
            // `deformation(k, ref("aws.vpc", A, ""), _)` binds `A`.
            if name == "ref"
                && args.len() == 3
                && let Value::Ref { typ, name, attr } = fv
                && eval_term(pat, out).is_none()
            {
                let mut tmp = out.clone();
                for (t, v) in args.iter().zip([typ, name, attr]) {
                    if !unify_term(t, &Value::Str(v.clone()), &mut tmp, rec)? {
                        return Ok(false);
                    }
                }
                *out = tmp;
                return Ok(true);
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

/// A negated atom's pattern: every argument bound, but a wildcard, which
/// matches anything (`not p(x, _)`: no `p` row with `x` first).
fn ground_atom(atom: &Atom, state: &HashMap<String, Value>) -> Result<Atom> {
    let mut args = Vec::with_capacity(atom.args.len());
    for t in &atom.args {
        if matches!(t, Term::Wildcard) {
            args.push(Term::Wildcard);
            continue;
        }
        let v = eval_term(t, state).ok_or_else(|| anyhow!("unbound var in negation"))?;
        args.push(Term::Val(v));
    }
    Ok(Atom {
        pred: atom.pred.clone(),
        args,
        record: None,
        span: Default::default(),
    })
}

/// Why a head argument has no value (R-119), located at the head: a call
/// that answered nothing (`oci.with_digest(.., "16.0.4")`, a `T?`
/// function's none) reaching a cell, named with the attribute it was to
/// give; or a variable nothing bound, which the compiler must not let
/// through.
fn head_error(head: &Atom, state: &HashMap<String, Value>) -> Option<String> {
    enum NoValue {
        Call(String),
        Unbound(String),
    }
    fn no_value(t: &Term, state: &HashMap<String, Value>) -> Option<NoValue> {
        if eval_term(t, state).is_some() {
            return None;
        }
        match t {
            Term::Var(v) => Some(NoValue::Unbound(v.clone())),
            Term::Func { name, args } => match args.iter().find_map(|a| no_value(a, state)) {
                Some(inner) => Some(inner),
                None => {
                    let shown: Vec<String> = args
                        .iter()
                        .filter_map(|a| eval_term(a, state))
                        .map(|v| partition::fmt_term(&Term::Val(v)))
                        .collect();
                    Some(NoValue::Call(format!("{name}({})", shown.join(", "))))
                }
            },
            Term::List(xs) => xs.iter().find_map(|x| no_value(x, state)),
            Term::Obj(m) => m.values().find_map(|x| no_value(x, state)),
            _ => None,
        }
    }
    let at = crate::diag::place(head.span).unwrap_or_else(|| head.pred.clone());
    let s = |t: &Term| match eval_term(t, state) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    };
    let cell = match (head.pred.as_str(), head.args.as_slice()) {
        ("arg", [t, a, k, _, _]) => match (s(t), s(a), s(k)) {
            (Some(typ), Some(name), Some(k)) => Some(match typ.as_str() {
                crate::modules::INPUT | crate::transform::OUTPUT => format!("{typ} {k}"),
                _ => crate::report::attribute(&crate::ir::Address { typ, name }, &k),
            }),
            _ => None,
        },
        _ => None,
    };
    let what = cell.unwrap_or_else(|| format!("`{}`", head.pred));
    head.args
        .iter()
        .find_map(|t| no_value(t, state))
        .map(|n| match n {
            NoValue::Call(call) => format!(
                "{at}: {call} answered nothing, so {what} has no value (the function's \
                 result is optional, `T?`): give it arguments it answers, or test it with \
                 `has` in a clause"
            ),
            NoValue::Unbound(v) => format!(
                "internal error: {at}: the head of the rule for {what} leaves `{v}` unbound \
                 (a compiler bug: please report it with the program)"
            ),
        })
}

fn instantiate_atom(atom: &Atom, state: &HashMap<String, Value>) -> Result<Atom> {
    let mut args = Vec::with_capacity(atom.args.len());
    for t in &atom.args {
        let v = eval_term(t, state).ok_or_else(|| anyhow!("non-ground head"))?;
        args.push(Term::Val(v));
    }
    Ok(Atom {
        pred: atom.pred.clone(),
        args,
        record: None,
        span: Default::default(),
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
        // Two numbers compare by value, an int with a float (R-75); a
        // join matches a value as it is.
        (Some(av), Some(bv)) => Ok(match crate::value::compare_numbers(&av, &bv) {
            Some(o) => o.is_eq(),
            None => rec.eq(&av, &bv, &out, "="),
        }
        .then_some(out)),
        (Some(av), None) => {
            if bind_term(b, av, &mut out, rec)? {
                Ok(Some(out))
            } else {
                Ok(None)
            }
        }
        (None, Some(bv)) => {
            if bind_term(a, bv, &mut out, rec)? {
                Ok(Some(out))
            } else {
                Ok(None)
            }
        }
        (None, None) => {
            for t in [a, b] {
                if missing_walk(t, &out) {
                    return Ok(None);
                }
                if let Some((name, args)) = failed_builtin(t, &out) {
                    // A tuple pattern against a partial function off its
                    // domain is a test: the match fails (R-58). A name
                    // bound to it alone is an error naming the call.
                    let tuple = matches!(a, Term::List(_)) || matches!(b, Term::List(_));
                    if tuple && crate::functions::get(&name).is_some_and(|f| f.partial) {
                        return Ok(None);
                    }
                    let args: Vec<String> = args.iter().map(partition::fmt_value).collect();
                    if name == crate::ir::RESOURCE_BODY {
                        bail!(
                            "the body of a resource is a value of its type, an object: not {}{}",
                            args.join(", "),
                            at_suffix(rec.head.span)
                        );
                    }
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

/// A walk to a path the value does not have (`has x.f`, `x.f.g`, `some c
/// in x.f`): no value, so the literal holding it does not hold.
fn missing_walk(t: &Term, state: &HashMap<String, Value>) -> bool {
    failed_builtin(t, state).is_some_and(|(name, _)| name == "__path")
}

/// The innermost function application in `t` whose arguments are all ground
/// but which has no value: a builtin applied to the wrong kind of value
/// (`"10" + 1`, `int("abc")`). `None` when the term is merely unbound.
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
        (Some(av), Some(bv)) => Ok(match crate::value::compare_numbers(&av, &bv) {
            Some(o) if o.is_eq() => None,
            Some(_) => Some(state.clone()),
            None => match crate::lattice::eq3(&av, &bv) {
                Truth::False => Some(state.clone()),
                Truth::True => None,
                Truth::Unknown => {
                    let mut nulls = nulls_in(&av);
                    nulls.extend(nulls_in(&bv));
                    rec.stuck(state, nulls, "!= against an open/secret null");
                    None
                }
            },
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
    let o = order(&av, &bv).map_err(|e| anyhow::anyhow!(e))?;
    Ok(match op_lit {
        Lit::Gt(_, _) => o.is_gt(),
        Lit::Ge(_, _) => o.is_ge(),
        Lit::Lt(_, _) => o.is_lt(),
        Lit::Le(_, _) => o.is_le(),
        _ => unreachable!(),
    })
}

/// `t = v` with `t` unbound: a variable binds, a tuple pattern `[A, _]`
/// unifies element by element with a list of its length (R-58).
fn bind_term(t: &Term, v: Value, out: &mut HashMap<String, Value>, rec: &Rec) -> Result<bool> {
    match t {
        Term::List(_) => unify_term(t, &v, out, rec),
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

/// What a [`Reference`] entry documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// A function a program may call, declared in `std/*.df` (`functions`).
    Function,
    /// An aggregate, bound in a body: `n = count(x)` (`partition::AGGREGATES`).
    Aggregate,
    /// A built-in provider's extern (`env.var`).
    Extern,
    /// A keyword (`lexer::KEYWORDS`).
    Keyword,
}

/// One entry of the language's reference: a function, an aggregate, a
/// builtin extern or a keyword. The language server's hover, completion
/// detail and signature help read it ([`references`]); a builtin without
/// an entry fails `tests::every_builtin_and_keyword_has_a_reference`.
#[derive(Debug, Clone, Copy)]
pub struct Reference {
    pub name: &'static str,
    pub kind: RefKind,
    /// A call's `name(param: type, ...) -> type` (signature help splits
    /// its parameters at the top-level commas), or a keyword's syntax.
    pub signature: &'static str,
    pub summary: &'static str,
    pub example: &'static str,
}

const fn r(
    name: &'static str,
    kind: RefKind,
    signature: &'static str,
    summary: &'static str,
    example: &'static str,
) -> Reference {
    Reference {
        name,
        kind,
        signature,
        summary,
        example,
    }
}

use RefKind::{Aggregate, Extern as Ext, Keyword as Kw};

/// The reference beside the functions (which `std/*.df` documents): the
/// aggregates and builtin externs, then the keywords.
const REFERENCE: &[Reference] = &[
    r(
        "collect_set",
        Aggregate,
        "collect_set(x: any) -> set",
        "The set of every `x` the body binds, per group of the head's other variables.",
        "ids(l) where l = collect_set(s.id), s in net.subnet",
    ),
    r(
        "collect_list",
        Aggregate,
        "collect_list(x: any) -> list",
        "The list of every `x` the body binds per group, in the order of the body's rows; a comprehension lowers to it.",
        "names(l) where l = collect_list(n), host(n)",
    ),
    r(
        "count",
        Aggregate,
        "count(x: any) -> int",
        "The number of the body's matches per group of the head's other variables.",
        "subnets(v, n) where n = count(s), s in net.subnet, s.vpc_id == v",
    ),
    r(
        "sum",
        Aggregate,
        "sum(x: int) -> int",
        "The sum of `x` over every match of the body per group; a non-int is a deny.",
        "total(n) where n = sum(x), size(_, x)",
    ),
    r(
        "min",
        Aggregate,
        "min(x: int | string) -> int | string",
        "The least `x` per group, ints or strings (a mix is a deny).",
        "first(n) where n = min(x), size(_, x)",
    ),
    r(
        "max",
        Aggregate,
        "max(x: int | string) -> int | string",
        "The greatest `x` per group, ints or strings (a mix is a deny).",
        "last(n) where n = max(x), size(_, x)",
    ),
    r(
        "any",
        Aggregate,
        "any(x: bool) -> bool",
        "Whether `x` is true for some match of the body per group (a non-bool is a deny).",
        "exposed(v, p) where p = any(public), subnet(v, public)",
    ),
    r(
        "all",
        Aggregate,
        "all(x: bool) -> bool",
        "Whether `x` is true for every match of the body per group (a non-bool is a deny).",
        "let healthy = all(ok) where check(_, ok)",
    ),
    r(
        "env.var",
        Ext,
        "env.var(name: string) -> secret(string)",
        "The environment variable of the process that plans: the built-in `env` provider's extern, a secret.",
        "let token = env.var(\"API_TOKEN\")",
    ),
    r(
        "import",
        Kw,
        "import \"PATH\"",
        "Include a file, once, where the import stands; paths are from the project's root.",
        "import \"modules/network.df\"",
    ),
    r(
        "key",
        Kw,
        "key NAME: TYPE (= DEFAULT)? (check BODY)?",
        "An input the target gives (`dform plan shop env=prod`), never `--set`: each value is a deployment of the stack, with its own state.",
        "key env: enum(\"dev\", \"prod\") = \"dev\"",
    ),
    r(
        "type",
        Kw,
        "type NAME = TYPE | type TYPE { PATH: TYPE FLAG*, ... }",
        "A type alias, or a resource type's attributes.",
        "type environment = enum(\"dev\", \"prod\")",
    ),
    r(
        "decl",
        Kw,
        "decl NAME(COLUMN [: TYPE], ...) mixed?",
        "Declare a relation by its columns: one fed from outside, or one with both facts and rules (`mixed`); its columns name its arguments.",
        "decl zone_index(zone, index) mixed",
    ),
    r(
        "extern",
        Kw,
        "extern NAME(+IN: TYPE, -OUT: TYPE, ...)",
        "A relation asked of the provider on demand, its `+` columns bound; nothing keeps its answers (`memo.first` does).",
        "extern dns.lookup(+name, -addr: string)",
    ),
    r(
        "input",
        Kw,
        "input NAME: TYPE (= DEFAULT)? (check BODY)? | input NAME(COLUMN: TYPE, ...) from SOURCE",
        "A typed input of the stack or a component; with columns, a relation read from a table or a fact file (`facts(PATH)`).",
        "input env: environment = \"staging\"",
    ),
    r(
        "output",
        Kw,
        "output NAME (: TYPE)? = TERM (where BODY)?",
        "A component's or stack's output, its type and its value in one statement.",
        "output vpc: net.vpc = vpc",
    ),
    r(
        "let",
        Kw,
        "let NAME = TERM (where BODY)?",
        "A value, read by name; one that holds a reference (a resource) is read through with a dot.",
        "let db = db.postgres[\"main\"]",
    ),
    r(
        "set",
        Kw,
        "set REFERENCE.PATH (= | +=) TERM @RANK? (where BODY)?",
        "A contribution to a block declared elsewhere, or to an input (under a `where`).",
        "set r.tags.team = \"platform\" @default where r in resource",
    ),
    r(
        "component",
        Kw,
        "component NAME { STATEMENTS }",
        "A component, an item of a module: a type the program defines, made of resources, with inputs and outputs; `resource C NAME { .. }` makes one, its predicates private to it, its outputs its attributes.",
        "component network { input vpc_net: inet }",
    ),
    r(
        "use",
        Kw,
        "use PATH (as NAME)? { INPUT = TERM, ... }? (where BODY)?",
        "Import a module, a file by its path from the project root, once under NAME: its items read as `NAME.x`, its rules and denies run over what this scope sees, its inputs bound by the block or their defaults, its resources stamped once as `NAME.x`. `use stacks.NAME` binds a stack's deployments, read as `NAME[k=v].output`; `use PROVIDER { SETTING = TERM, ... }` imports and configures a provider.",
        "use baseline",
    ),
    r(
        "resource",
        Kw,
        "resource TYPE NAME @RANK? { PATH = TERM, ... } (where BODY)?",
        "A resource the program wants, one per answer of its clause, its fields contributions; `x in resource` is any resource. TYPE is a provider's type or a component, whose resource's fields are its inputs.",
        "resource net.vpc vpc { cidr = vpc_net }",
    ),
    r(
        "settings",
        Kw,
        "set { PATH = TERM, ... } @RANK? (where BODY)?",
        "Gone (R-38): an input is given by `set`, several under one clause by `set { .. } where ..`, a document's leaves by `set from DOC`.",
        "set { db.multi_az = true } where env == \"prod\"",
    ),
    r(
        "deny",
        Kw,
        "deny \"MESSAGE\" {FIELDS}? where BODY",
        "A check: plan fails with the message when the body holds.",
        "deny \"prod needs multi_az\" where env == \"prod\", pg in db.postgres, not pg.multi_az",
    ),
    r(
        "warn",
        Kw,
        "warn \"MESSAGE\" {FIELDS}? where BODY",
        "A check: plan warns with the message when the body holds.",
        "warn \"no owner tag\" where r in net.vpc, not has r.tags.owner",
    ),
    r(
        "not",
        Kw,
        "not LITERAL | not { BODY }",
        "Negation: holds when the literal, or the whole body, has no match.",
        "unused(s) where s in net.subnet, not attached(s)",
    ),
    r(
        "in",
        Kw,
        "TERM in TYPE | TERM in resource | TERM in world.TYPE | TERM in LIST",
        "Membership: a resource of a type, any resource, a live object, or an element of a list.",
        "vpc_peer(a, b) where a in net.vpc, b in net.vpc",
    ),
    r(
        "has",
        Kw,
        "has REFERENCE.PATH",
        "The attribute has a value.",
        "tagged(r) where r in net.vpc, has r.tags.owner",
    ),
    r(
        "where",
        Kw,
        "HEAD where BODY | BLOCK where BODY",
        "The clause of a rule, check, `let`, `set`, output or block, after it: a query; a block is one resource (row, instance) per answer.",
        "vpc_peer(a, b) where vpc_peer_pair(_, _, a, b)",
    ),
    r(
        "check",
        Kw,
        "input NAME: TYPE (= DEFAULT)? check BODY | PATH: TYPE FLAG* check BODY",
        "A refinement of an input's or a `type` block attribute's type, the value named by its own name.",
        "input replicas: int = 2 check replicas >= 1",
    ),
    r("true", Kw, "true", "The boolean true.", "multi_az = true"),
    r(
        "false",
        Kw,
        "false",
        "The boolean false.",
        "private_api = false",
    ),
];

/// The language's reference: every function a program may call, from
/// `std/*.df`, then [`REFERENCE`]'s aggregates, externs and keywords.
pub fn references() -> &'static [Reference] {
    static ALL: std::sync::LazyLock<Vec<Reference>> = std::sync::LazyLock::new(|| {
        crate::functions::registry()
            .functions()
            .filter(|f| !f.internal)
            .map(|f| Reference {
                name: &f.name,
                kind: RefKind::Function,
                signature: &f.signature,
                summary: &f.summary,
                example: &f.example,
            })
            .chain(REFERENCE.iter().copied())
            .collect()
    });
    &ALL
}

/// The reference entry of `name`: a call's (function, aggregate or
/// extern) when `call`, else a keyword's before a builtin's.
pub fn reference(name: &str, call: bool) -> Option<&'static Reference> {
    let all = references();
    let is = |e: &&Reference| e.name == name;
    if call {
        all.iter().filter(is).find(|e| e.kind != RefKind::Keyword)
    } else {
        all.iter()
            .filter(is)
            .find(|e| e.kind == RefKind::Keyword)
            .or_else(|| all.iter().find(is))
    }
}

fn eval_func(name: &str, args: &[Term], state: &HashMap<String, Value>) -> Option<Value> {
    let body = crate::functions::body(name)?;
    let mut vals = Vec::with_capacity(args.len());
    for a in args {
        vals.push(eval_term(a, state)?);
    }
    // Rule 2: every builtin argument is a content position. A builtin
    // over a null has no value; the literal that needs it is stuck.
    if !forwards_nulls(name) && vals.iter().any(stuck::has_null) {
        return None;
    }
    body(&vals)
}

/// How two values order: numbers by value (an int with a float, R-75),
/// quantities of one dimension, times by their instant; `Err` with why
/// they do not.
fn order(a: &Value, b: &Value) -> std::result::Result<Ordering, String> {
    if let Some(o) = crate::value::compare_numbers(a, b) {
        return Ok(o);
    }
    match (a, b) {
        (Value::Time(x), Value::Time(y)) => Ok(x.instant().cmp(&y.instant())),
        (Value::Quantity(x), Value::Quantity(y)) => {
            crate::quantity::compare(x, y).ok_or_else(|| {
                if x.dim() == y.dim() {
                    format!(
                        "{x} and {y} do not compare: a month's length depends on the date \
                     (add both to a time with `time.add`)"
                    )
                } else {
                    format!(
                        "{x} is {} and {y} {}: a quantity compares only within its dimension",
                        x.dim().name(),
                        y.dim().name()
                    )
                }
            })
        }
        _ => Err("comparison only supports numbers, quantities and times".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Stmt;

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

    /// Hover, completion and signature help read `references()`: every
    /// function a program may call (from `std/*.df`), aggregate and
    /// keyword has its entry.
    #[test]
    fn every_builtin_and_keyword_has_a_reference() {
        let functions = crate::functions::registry()
            .functions()
            .filter(|f| !f.internal)
            .map(|f| f.name.as_str());
        let calls = functions
            .chain(partition::AGGREGATES.iter().copied())
            .chain([crate::syntax::resolve::ENV_VAR]);
        for name in calls {
            let e = reference(name, true).unwrap_or_else(|| panic!("no reference for {name}"));
            assert!(e.signature.starts_with(&format!("{name}(")), "{e:?}");
        }
        for (name, _) in crate::lexer::KEYWORDS {
            let e = reference(name, false).unwrap_or_else(|| panic!("no reference for {name}"));
            assert_eq!(e.kind, RefKind::Keyword, "{e:?}");
        }
        for e in references() {
            assert!(!e.summary.is_empty() && !e.example.is_empty(), "{e:?}");
        }
    }

    /// DESIGN.org "Aggregates are not stratified": a consumer of an
    /// aggregate used to see every partial result mid-fixpoint.
    #[test]
    fn aggregate_consumer_sees_one_complete_result() {
        let (r, _) = run("decl n(a) mixed\n             n(1)\n             n(2) where n(1)\n             n(3) where n(2)\n             all(r) where r = collect_set(x), n(x)\n             snap(l) where all(l)")
        .unwrap();
        assert_eq!(facts_of(&r, "snap"), vec!["snap([1, 2, 3])".to_string()]);
    }

    /// `sum` folds every body match of a group, as `count` counts them:
    /// two matches of the same size are both summed. An empty group
    /// derives nothing, for `count` as for `sum`.
    #[test]
    fn sum_folds_every_match_per_group() {
        let (r, violations) = run(r#"size("a", "x", 3)
             size("b", "x", 3)
             size("c", "y", 4)
             total(g, r) where r = sum(n), size(_, g, n)
             all(r) where r = sum(n), size(_, _, n)
             sizes(r) where r = count(n), size(_, _, n)
             big(r) where r = sum(n), size(_, _, n), n > 9
             many(r) where r = count(n), size(_, _, n), n > 9"#)
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(
            facts_of(&r, "total"),
            [r#"total("x", 6)"#, r#"total("y", 4)"#]
        );
        assert_eq!(facts_of(&r, "all"), ["all(10)"]);
        assert_eq!(facts_of(&r, "sizes"), ["sizes(3)"]);
        assert!(facts_of(&r, "big").is_empty());
        assert!(facts_of(&r, "many").is_empty());
    }

    /// `min` and `max` over ints and over strings, per group.
    #[test]
    fn min_and_max_order_ints_and_strings() {
        let (r, violations) = run(r#"decl v(g, x: any)
             v("a", 3)
             v("a", 1)
             v("a", 12)
             v("b", "q")
             v("b", "p")
             lo(g, r) where r = min(x), v(g, x)
             hi(g, r) where r = max(x), v(g, x)"#)
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(facts_of(&r, "lo"), [r#"lo("a", 1)"#, r#"lo("b", "p")"#]);
        assert_eq!(facts_of(&r, "hi"), [r#"hi("a", 12)"#, r#"hi("b", "q")"#]);
    }

    /// A group `sum` has a non-int in, or `min`/`max` a mix of ints and
    /// strings or another kind, derives a deny naming the group and the
    /// value, and not its head; the other groups derive theirs.
    #[test]
    fn an_ill_kinded_group_is_a_deny() {
        let (r, violations) = run(r#"decl v(g, x: any)
             v("a", 1)
             v("a", "p")
             v("b", true)
             v("c", 2)
             s(g, r) where r = sum(x), v(g, x)
             m(g, r) where r = max(x), v(g, x)"#)
        .unwrap();
        assert_eq!(facts_of(&r, "s"), [r#"s("c", 2)"#]);
        assert_eq!(facts_of(&r, "m"), [r#"m("c", 2)"#]);
        let has = |m: &str| violations.iter().any(|v| v.starts_with(m));
        assert!(
            has(r#"s("a", _): sum() over "p", which is not an int"#),
            "{violations:?}"
        );
        assert!(
            has(r#"s("b", _): sum() over true, which is not an int"#),
            "{violations:?}"
        );
        assert!(
            has(r#"m("a", _): max() over "p" and 1, an int and a string"#),
            "{violations:?}"
        );
        assert!(
            has(r#"m("b", _): max() over true, which is neither an int nor a string"#),
            "{violations:?}"
        );
        let (_, violations) = run(&format!(
            "v({})\n v(1)\n s(r) where r = sum(x), v(x)",
            i64::MAX
        ))
        .unwrap();
        assert!(
            violations
                .iter()
                .any(|v| v.starts_with("s(_): sum() overflows")),
            "{violations:?}"
        );
    }

    /// `sum` of a value known not to be an int, `min`/`max` of one neither
    /// an int nor a string, is a compile error at the call.
    #[test]
    fn a_statically_ill_kinded_aggregate_is_an_error() {
        for (src, want) in [
            (
                r#"s(r) where r = sum("a"), b(x)"#,
                "`sum` aggregates ints, not a string",
            ),
            (
                r#"s(r) where r = sum("p-${x}"), b(x)"#,
                "`sum` aggregates ints, not a string",
            ),
            (
                r#"s(r) where r = min([x]), b(x)"#,
                "`min` aggregates ints or strings, not a list",
            ),
            (
                r#"s(r) where r = max(true), b(x)"#,
                "`max` aggregates ints or strings, not a bool",
            ),
        ] {
            let err = run(&format!("b(1)\n{src}")).unwrap_err();
            assert!(format!("{err:#}").contains(want), "{src}: {err:#}");
        }
        run("b(1)\ns(r) where r = sum(x), b(x)\nt(r) where r = max(\"p-${x}\"), b(x)").unwrap();
    }

    /// Rule 2: the aggregated value of `sum`, `min` and `max` is a content
    /// position. A group with a null in, open or fresh, is stuck and derives
    /// nothing (a fresh null's order is content too); the others derive.
    #[test]
    fn sum_min_max_over_a_null_is_stuck() {
        let null = |class, label: &str| Value::Null {
            label: label.into(),
            class,
            ty: "int".into(),
        };
        let fact = |g: &str, v: Value| Atom {
            pred: "size".into(),
            args: vec![str_term(g), Term::Val(v)],
            record: None,
            span: Default::default(),
        };
        let extra = [
            fact("a", null(crate::value::NullClass::Open, "t/a#size")),
            fact("a", Value::Int(1)),
            fact("b", null(crate::value::NullClass::Fresh, "t/b#size")),
            fact("c", Value::Int(2)),
        ];
        let (r, violations) = run_with(
            "decl size(a, b)\n             total(g, r) where r = sum(n), size(g, n)\n             lo(g, r) where r = min(n), size(g, n)\n             hi(g, r) where r = max(n), size(g, n)\n             all(r) where r = sum(n), size(_, n)",
            &extra,
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(facts_of(&r, "total"), [r#"total("c", 2)"#]);
        assert_eq!(facts_of(&r, "lo"), [r#"lo("c", 2)"#]);
        assert_eq!(facts_of(&r, "hi"), [r#"hi("c", 2)"#]);
        assert!(facts_of(&r, "all").is_empty());
        for (pred, name) in [
            ("total", "sum"),
            ("lo", "min"),
            ("hi", "max"),
            ("all", "sum"),
        ] {
            assert!(
                r.stuck
                    .iter()
                    .any(|s| s.head.pred == pred && s.reason == format!("{name} over a null")),
                "{pred}: {:?}",
                r.stuck
            );
        }
    }

    /// A wildcard in a negated atom matches anything: `not has k` is
    /// `not k(_)` (docs/grammar.md), `not p(x, _)` asks for no row with `x`
    /// first.
    #[test]
    fn a_wildcard_in_a_negation_matches_anything() {
        let (r, _) = run("p(1, 2)\n             k(0) where p(9, 9)\n             none(1) where not k(_)\n             lonely(x) where x in [1, 3], not p(x, _)\n             let active = \"x\" where p(9, 9)\n             let next = \"blue\" where not has active")
        .unwrap();
        assert_eq!(facts_of(&r, "none"), vec!["none(1)".to_string()]);
        assert_eq!(facts_of(&r, "lonely"), vec!["lonely(3)".to_string()]);
        assert_eq!(facts_of(&r, "next"), vec!["next(\"blue\")".to_string()]);
    }

    /// `has x.f` of a value without `f` does not hold (and `not has` does):
    /// a walk to a missing path is no value, not an error.
    #[test]
    fn a_walk_to_a_missing_path_is_no_value() {
        let (r, _) = run("p({a: {b: 1}})
             deep(x) where p(x), has x.a.b
             open(x) where p(x), not has x.a.c
             shallow(x) where p(x), has x.c")
        .unwrap();
        assert_eq!(facts_of(&r, "deep").len(), 1);
        assert_eq!(facts_of(&r, "open").len(), 1);
        assert!(facts_of(&r, "shallow").is_empty());
        let (r, _) = run("p({a: {b: [1]}})\n             elem(e) where p(x), e in x.a.b\n             none(e) where p(x), e in x.a.c")
        .unwrap();
        assert_eq!(facts_of(&r, "elem"), vec!["elem(1)".to_string()]);
        assert!(facts_of(&r, "none").is_empty());
    }

    /// A cycle through negation is a compile error naming the cycle with
    /// the text of every rule on it.
    #[test]
    fn negative_cycle_is_an_error_with_rule_text() {
        let err = run("q(1)
             p(x) where q(x), not r(x)
             r(x) where p(x)")
        .unwrap_err()
        .to_string();
        assert!(err.contains("negative cycle"), "{err}");
        assert!(err.contains("p(X) :- q(X), not r(X)"), "{err}");
    }

    /// Rules run in the stratum of their head's partition node: a `want`
    /// of one type may negate, or aggregate over, `want` of another type.
    #[test]
    fn want_is_partitioned_by_type() {
        let (r, _) = run("want(\"net.subnet\", \"a\")
             want(\"net.subnet\", \"b\")
             subnets(r) where r = collect_set(s), want(\"net.subnet\", s)
             want(\"db.postgres\", \"db\") where subnets(l), member(l, \"a\"), not want(\"net.subnet\", \"c\")")
        .unwrap();
        assert!(facts_of(&r, "want").contains(&"want(\"db.postgres\", \"db\")".to_string()));
    }

    fn input(k: &str, v: Value) -> Atom {
        Atom {
            pred: "input".into(),
            args: vec![str_term(k), Term::Val(v)],
            record: None,
            span: Default::default(),
        }
    }

    /// Two rules set one attribute to different values: no attr fact, an
    /// attr_conflict, and a deny naming the resource, the path and both
    /// contributing rules with where each is written.
    #[test]
    fn conflicting_contributions_derive_a_deny_naming_every_witness() {
        let (r, violations) = run("resource net.vpc main { cidr = \"10.0.0.0/16\" }
             arg(net.vpc, \"main\", \"cidr\", \"10.1.0.0/16\") where want(net.vpc, \"main\")")
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
                "arg(\"net.vpc\", \"main\", \"cidr\", \"10.0.0.0/16\", \"normal\") (at <input>:1:25)".to_string(),
                "arg(\"net.vpc\", \"main\", \"cidr\", \"10.1.0.0/16\", \"normal\") :- want(\"net.vpc\", \"main\") (at <input>:2:14)".to_string(),
            ]
        );
    }

    /// Readers see the collapsed value, never a raw contribution: an input
    /// and an output are the same aggregate on pseudo-types, and a `set`'s
    /// block's contribution wins over the declaration's default (R-38).
    #[test]
    fn input_and_output_readers_read_the_collapsed_value() {
        let (r, violations) = run("input days: int = 3
             input audit: bool = false
             set days = 14 where audit == false
             component network {\n output ids: list(string) = [\"a\", \"b\"]\n }
             resource network main {}
             got(d) where d = days
             ids(l) where output(\"main\", \"ids\", l)
             deny \"no audit\" where not audit")
        .unwrap();
        assert_eq!(facts_of(&r, "got"), vec!["got(14)".to_string()]);
        assert_eq!(facts_of(&r, "ids"), vec!["ids([\"a\", \"b\"])".to_string()]);
        assert_eq!(violations, vec!["no audit".to_string()]);
    }

    /// E §2.5 path normalization: `tags.team` contributes `{team: V}` to
    /// `tags`, which is a Map, so it meets the block's other tags per leaf.
    #[test]
    fn dotted_path_contributes_to_its_top_level_attribute() {
        let (r, _) = run("resource net.vpc main { tags = { env: \"dev\" } }
             arg(net.vpc, \"main\", \"tags.team\", \"platform\") where want(net.vpc, \"main\")")
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
        let (r, violations) = run("want(net.vpc, \"main\")
             arg(net.vpc, \"main\", \"cidr\", \"10.0.0.0/16\", \"default\")
             arg(net.vpc, \"main\", \"cidr\", \"10.9.0.0/16\", \"default\")
             arg(net.vpc, \"main\", \"cidr\", \"10.1.0.0/16\")")
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
            "want(\"net.subnet\", \"a\")
             arg(\"net.subnet\", \"a\", \"vpc_id\", v) where input(\"vpc\", v)
             seen(v) where arg(\"net.subnet\", \"a\", \"vpc_id\", v)",
        )
        .unwrap();
        let (r, violations) = eval(&program, &[input("vpc", null.clone())]).unwrap();
        assert!(violations.is_empty());
        assert_eq!(
            facts_of(&r, "seen"),
            vec![format!("seen({})", partition::fmt_value(&null))]
        );
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
                if let Stmt::Module(c) = s {
                    shuffle(&mut c.body, seed);
                }
            }
        }
        let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
        let program =
            crate::loader::load_program(&[root.join("examples/demo/stacks/dform.df")]).unwrap();
        for env in ["staging", "prod"] {
            let extra = [input("env", Value::Str(env.into()))];
            // The settings document is a table: answered.
            let attrs = |p: &Program| {
                let (r, _) = eval_tables(p, &extra).unwrap();
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

    /// E §2.4 syntax: `@default` / `@override` after a value, after a
    /// `resource` header for every leaf without its own, and after a
    /// `set` block's (R-38).
    #[test]
    fn ranks_in_blocks() {
        let (r, violations) = run("input days: int = 1\n             input zones: list(string)\n             resource net.vpc main @default {\n               cidr = \"10.0.0.0/16\"\n               tags = { env: \"dev\", team: \"net\" }\n               public = true @override\n             }\n             resource net.vpc main {\n               cidr = \"10.1.0.0/16\"\n               tags = { team: \"platform\" }\n               public = false\n             }\n             set {\n               days = 3\n               zones = [\"a\"]\n             } @default where on(1)\n             set { days = 14 } where on(1)\n             on(1)\n             got(d, z) where d = days, z = zones")
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
        assert_eq!(facts_of(&r, "got"), vec!["got(14, [\"a\"])".to_string()]);
    }

    /// `eval`, with the tables the program reads (`crate::tables`):
    /// dform.df's `set from` document. Any other extern has no answer, as
    /// under `eval`.
    fn eval_tables(program: &Program, extra: &[Atom]) -> Result<(EvalResult, Vec<String>)> {
        let lowered = crate::transform::lower(program)?;
        let tables = crate::tables::Tables::default();
        let externs =
            crate::externs::Externs::new(&lowered.program, &lowered.extern_fns, |f, ins| {
                tables.answer(f, ins).unwrap_or(Ok(Vec::new()))
            });
        externs.eval(program, extra)
    }

    /// dform.df's inputs are their defaults, a block for the modules', and
    /// each environment's document, `set from yaml("config/dform/${env}.yaml")`
    /// (R-38). Every environment compiles to exactly the resources the
    /// `set` blocks the documents say would.
    #[test]
    fn dform_df_set_from_matches_the_blocks() {
        let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
        let src = std::fs::read_to_string(root.join("examples/demo/stacks/dform.df")).unwrap();
        let from = "set from yaml(\"config/dform/${env}.yaml\")\n";
        assert!(src.contains(from));
        let blocks = "set {
              cidrs.main = \"10.20.0.0/16\"
              cidrs.peer = \"10.21.0.0/16\"
              database.backup_days = 14
              database.multi_az = true
              kubernetes.private_api = true
              kubernetes.nodepool_min = 3
              kubernetes.nodepool_max = 10
              baseline.audit.sinks = [\"s3\", \"cloudwatch\"]
            } where env == \"prod\"
            set { cidrs.main = \"10.90.0.0/16\" } where env == \"dev\"
            ";
        let dir = std::env::temp_dir().join(format!("dform-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("dform.df");
        std::fs::write(&old, src.replace(from, blocks)).unwrap();
        // The modules and components it names by path, beside it.
        for f in std::fs::read_dir(root.join("examples/demo")).unwrap() {
            let f = f.unwrap().path();
            if f.extension().is_some_and(|e| e == "df") {
                std::fs::copy(&f, dir.join(f.file_name().unwrap())).unwrap();
            }
        }
        let resources = |path: &std::path::Path, env: Option<&str>| {
            let program = crate::loader::load_program(&[path.to_path_buf()]).unwrap();
            let extra: Vec<Atom> = env
                .map(|e| input("env", Value::Str(e.into())))
                .into_iter()
                .collect();
            let (r, violations) = eval_tables(&program, &extra).unwrap();
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
            let new = resources(&root.join("examples/demo/stacks/dform.df"), env);
            assert!(!new.0.is_empty());
            assert_eq!(new, resources(&old, env), "env={env:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A ranked Set is three shelves: a policy pack's `@default` set is
    /// replaced wholesale by a normal one, and same-shelf sets union.
    #[test]
    fn a_default_set_is_replaced_not_unioned() {
        let (r, violations) = run("type_lattice(net.vpc, \"sgs\", \"set\")\n             resource net.vpc a { sgs = [\"base\"] }\n             resource net.vpc b { }\n             arg(t, n, \"sgs\", [\"default_sg\", \"ssh\"], \"default\") where want(t, n)\n             arg(t, n, \"sgs\", [\"audit\"]) where want(t, n), n = \"a\"")
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
        let err = run("env(\"prod\")\n             resource net.vpc main {\n               cidr = \"10.0.0.0/16\"\n             } where envv(\"prod\")")
        .unwrap_err()
        .to_string();
        assert!(err.contains("undefined predicate envv/1"), "{err}");
        assert!(
            err.contains("want(\"net.vpc\", \"main\") :- envv(\"prod\")"),
            "{err}"
        );
    }

    /// `decl p/N` declares a provider-fed predicate; provider-injected
    /// predicates are defined with no rows.
    #[test]
    fn extern_and_provider_predicates_are_defined() {
        let (r, _) = run("decl allowed(a)\n             want(net.vpc, \"a\")\n             lonely(n) where want(net.vpc, n), not allowed(n), not cloud_exists(net.vpc, n)")
        .unwrap();
        assert_eq!(facts_of(&r, "lonely"), vec!["lonely(\"a\")".to_string()]);
    }

    /// DESIGN.org "Silent string-to-int coercion": arithmetic takes
    /// integers; conversions are explicit builtins.
    #[test]
    fn coercion_is_explicit() {
        // A column of strings in arithmetic is a compile error (R-34).
        let err = run("s(\"10\")\nn(x) where s(s), x = s + 1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`s + 1`: `s` is string"), "{err}");
        let err = run("n(x) where x = int(\"abc\") + 1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("int(\"abc\") is not defined"), "{err}");
        let (r, _) = run("s(\"10\")
             explicit(x) where s(s), x = int(s) + 1
             text(t) where t = string(14)
             sizes(a, b, c) where a = len([\"x\", \"y\"]), b = len(\"héllo\"), c = list.len({k: 1})
             cases(l, u) where l = str.lower(\"AbC\"), u = str.upper(\"AbC\")
             parts(p) where p = str.split(\"a,b,c\", \",\")
             joined(j) where j = list.join([\"a\", 1, true], \"-\")")
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
            "want(net.vpc, \"a\")
             id_len(n) where want(net.vpc, a), n = len(ref(net.vpc, a, \"id\"))",
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
        let (r, _) = run("decl n(a) mixed\n             n(0)\n             n(y) where n(x), x < 300, y = x + 1\n             deepest(x) where n(x), x >= 300")
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
               spec.template.metadata.labels = {app: \"web\"},
               spec.template.spec.containers = [{name: \"web\"}]
             }")
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
        let (r, _) = run("decl peering(env: symbol, name: symbol)\n             peering( env: \"prod\", name: \"legacy\" )\n             resource net.peering \"${name}\" {\n               env = env\n             } where peering( env: env, name: name )")
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
            "type_provider(\"vm\", \"mock\")
             type_attr(\"vm\", \"id\", \"string\", [\"computed\", \"id\"])
             type_attr(\"vm\", \"zone\", \"string\", [\"optional_computed\"])",
        );
        let (r, violations) = run_with(
            "resource vm a { size = 1 }
             resource vm b { zone = \"z1\" }
             resource vm c { peer_zone = ref(\"vm\", \"a\", \"zone\"), other_zone = ref(\"vm\", \"b\", \"zone\"), a_id = ref(\"vm\", \"a\", \"id\") }",
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
            "resource k8s.deployment api { metadata.namespace = \"shop\" }
             resource k8s.deployment web { metadata.name = \"web\" }
             resource k8s.service api { spec.selector.app = ref(k8s.deployment, \"api\", \"metadata.name\"),
                                        spec.selector.web = ref(k8s.deployment, \"web\", \"metadata.name\") }",
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
            "resource aws.vpc main { cidr_block = \"10.0.0.0/16\" }
             resource aws.security_group web { vpc_id = ref(\"aws.vpc\", \"main\", \"id\") }",
            &schema,
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        let attrs = facts_of(&r, "attr");
        assert!(
            attrs.contains(
                &"attr(\"aws.vpc\", \"main\", \"cidr_block\", \"10.0.0.0/16\")".to_string()
            ),
            "{attrs:#?}"
        );
        assert!(attrs.contains(&"attr(\"aws.security_group\", \"web\", \"name\", ?aws.security_group/web#name:Open)".to_string()), "{attrs:#?}");
        assert!(
            attrs.contains(
                &"attr(\"aws.security_group\", \"web\", \"vpc_id\", ?aws.vpc/main#id:Fresh)"
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
            "type_provider(\"vm\", \"mock\")
             type_attr(\"vm\", \"id\", \"string\", [\"computed\", \"id\"])
             type_attr(\"vm\", \"meta.uid\", \"string\", [\"computed\", \"id\"])",
        );
        let err = run_with("resource vm a { id = \"x\" }", &schema)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("resource vm[\"a\"]: attribute id is computed"),
            "{err}"
        );
        let err = run_with("resource vm a { meta.uid = \"x\" }", &schema)
            .unwrap_err()
            .to_string();
        assert!(err.contains("attribute meta.uid is computed"), "{err}");
    }

    /// Round 0 (E Rule 4): a resource the world has resolves its computed
    /// attributes through the identity mapping; a secret never does.
    #[test]
    fn round_zero_resolves_through_identity() {
        let mut extra = schema_facts(
            "type_provider(\"vm\", \"mock\")
             type_attr(\"vm\", \"id\", \"string\", [\"computed\", \"id\"])
             type_attr(\"vm\", \"pw\", \"string\", [\"computed\", \"sensitive\"])",
        );
        let s = |x: &str| Term::Val(Value::Str(x.into()));
        extra.push(Atom {
            pred: "identity".into(),
            args: vec![s("vm"), s("a"), s("remote-a")],
            record: None,
            span: Default::default(),
        });
        extra.push(Atom {
            pred: "world_attr".into(),
            args: vec![s("vm"), s("remote-a"), s("id"), s("vm-123")],
            record: None,
            span: Default::default(),
        });
        let (r, _) = run_with(
            "resource vm a { size = 1 }
             resource vm b { peer = ref(\"vm\", \"a\", \"id\"), secret = ref(\"vm\", \"a\", \"pw\") }",
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
        std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../..")).join(rel)
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
        let (r, violations) = eval_tables(&program, &extra).unwrap();
        let docs = crate::ir::compile_resources(r.facts.iter().cloned(), schema)
            .unwrap()
            .into_iter()
            .map(|d| ((d.addr.typ, d.addr.name), d.attrs))
            .collect();
        let sections = stuck::sections(&r.stuck, &r.may_derive, &r.facts, &docs, schema);
        (r, violations, sections)
    }

    /// Orchestrator regression (the mock-provider hand-back): `format` over a
    /// ref to a computed attribute used to plan the constant
    /// "ref(net.vpc,v,id)-x". It is a content position: the rule is stuck,
    /// recorded as `stuck/4`, and derives nothing.
    #[test]
    fn format_over_a_computed_ref_is_stuck() {
        let (r, violations) = run_with(
            "resource net.vpc v { cidr = \"10.0.0.0/16\" }
             resource net.subnet s { name = format(\"%s-x\", ref(net.vpc, \"v\", \"id\")) }",
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
            "examples/demo/stacks/dform.df",
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
            (
                "examples/advanced/stacks/dform-advanced.df",
                crate::schema::fake(),
                &prod,
            ),
            ("examples/pngu/stacks/pngu.df", crate::schema::gke(), &prod),
            (
                "examples/decl/stacks/decl_demo.df",
                crate::schema::fake(),
                &[],
            ),
            (
                "examples/adopt/stacks/adopt_demo.df",
                crate::schema::fake(),
                &prod,
            ),
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
            "examples/gke/stacks/gke_two_phase.df",
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
                    "google.container_cluster/pngu#ca_certificate",
                    "google.container_cluster/pngu#endpoint"
                ]
            );
        }
        assert_eq!(facts_of(&r, "want").len(), 6);
        assert_eq!(s.pending_groups.len(), 1, "{:?}", s.pending_groups);
        assert!(
            s.pending_groups[0].starts_with(
                "want(\"google.container_node_pool\", _) x unknown, on ?google.container_cluster[\"pngu\"].zones"
            )
        );
        assert_eq!(s.undetermined.len(), 1, "{:?}", s.undetermined);
        assert!(s.undetermined[0].starts_with("deny \"cluster must be in at least two zones\""));
        assert!(
            !r.stuck.iter().any(|x| x.text.contains("deletion")),
            "{:?}",
            r.stuck
        );
    }

    /// F DR-2 revised, last clause, for a resource rule: a rule that
    /// positively reads a helper with a stuck instance derives nothing yet
    /// and may derive after the boundary; so may a reader of that reader.
    /// Only the helper's instance the rule's key reads is named.
    #[test]
    fn a_resource_rule_reading_a_stuck_helper_may_derive() {
        let (r, violations) = run_with(
            r#"resource db.postgres a {}
               resource db.postgres b {}
               up(d) where attr(db.postgres, d, "endpoint", e), e != ""
               ready(v) where up(v)
               resource net.subnet s {
                 cidr = "10.0.1.0/24"
               } where up("a")
               resource net.subnet t {
                 cidr = "10.0.2.0/24"
               } where ready("b")"#,
            &crate::schema::fake().facts,
        )
        .unwrap();
        assert!(violations.is_empty(), "{violations:?}");
        assert!(
            facts_of(&r, "want")
                .iter()
                .all(|w| !w.contains("net.subnet"))
        );
        let may: Vec<String> = r
            .may_derive
            .iter()
            .map(|m| {
                format!(
                    "{} {} ({})",
                    partition::fmt_atom(&m.head),
                    m.nulls_text(),
                    m.reason()
                )
            })
            .collect();
        assert!(
            may.contains(&r#"want("net.subnet", "s") ?db.postgres["a"].endpoint (reads up("a"), which is stuck)"#.to_string()),
            "{may:#?}"
        );
        assert!(
            may.contains(&r#"want("net.subnet", "t") ?db.postgres["b"].endpoint (reads ready("b"), which may derive after a boundary)"#.to_string()),
            "{may:#?}"
        );
        assert!(
            !may.iter()
                .any(|m| m.contains(r#"db.postgres["b"]"#) && m.contains("\"s\"")),
            "{may:#?}"
        );
        let docs = crate::ir::compile_resources(r.facts.iter().cloned(), &crate::schema::fake())
            .unwrap()
            .into_iter()
            .map(|d| ((d.addr.typ, d.addr.name), d.attrs))
            .collect();
        let s = stuck::sections(
            &r.stuck,
            &r.may_derive,
            &r.facts,
            &docs,
            &crate::schema::fake(),
        );
        assert_eq!(s.pending_groups.len(), 2, "{:?}", s.pending_groups);
    }

    /// adv2: per-key Rule 3. An unrelated negation and an unrelated
    /// aggregate over `want` are decided; a negation whose pattern unifies
    /// with the stuck nodepool head is undetermined. (Was sim.rs's
    /// adv2_rule3_coarse_vs_perkey.)
    #[test]
    fn adv2_rule3_per_key() {
        let (r, violations, s) = run_file(
            "tests/fixtures/adversarial/adv2_rule3_coarse.df",
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
            "examples/demo/stacks/dform.df",
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
        let (r, _) = run("p(1)\np(2)\ns(2)\nq(x) where p(x), not s(x)").unwrap();
        let why = why_leaves(&r, "q(1)");
        assert!(why.contains(&Leaf::Base {
            span: "<input>:1:1 (p)".into()
        }));
        assert!(why.contains(&Leaf::Absent {
            pattern: "s(1)".into()
        }));
        let Some(Leaf::Rule { id }) = why.iter().find(|l| matches!(l, Leaf::Rule { .. })) else {
            panic!("no rule leaf: {why:?}");
        };
        assert_eq!(r.circuit.rule_text(id), Some("q(X) :- p(X), not s(X)"));
        assert_eq!(r.circuit.rule_at(id), Some("<input>:4:1"));
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
            want("t", "a")
            arg("t", "a", "tags", {x: 1}, "normal")
            arg("t", "a", "tags", {y: 2}, "normal") where want("t", "a")
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
            "env(e) where input(\"env\", e)",
            &[input("env", Value::Str("prod".into()))],
        )
        .unwrap();
        assert!(why_leaves(&r, "env(\"prod\")").contains(&Leaf::Input {
            source: "--set env=prod".into()
        }));
    }

    /// gke_two_phase with a program appended, on the gke schema.
    fn gke_with(extra: &str) -> Result<(EvalResult, Vec<String>)> {
        let mut program =
            crate::loader::load_program(&[repo_file("examples/gke/stacks/gke_two_phase.df")])
                .unwrap();
        program
            .statements
            .extend(crate::parser::parse_program(extra).unwrap().statements);
        eval(&program, &crate::schema::gke().facts)
    }

    /// stuck/4 is a relation a policy reads (to refuse a plan with any
    /// stuck instance): it is derived above every rule that can stick, so the
    /// reader sees every instance, those of the rules above it included
    /// (the zone-count deny is stuck in the top stratum).
    #[test]
    fn a_policy_denies_on_any_stuck_instance() {
        let (r, violations) = gke_with(
            r#"deny "stuck" { rule: r, on: n } where stuck(r, _, _, n)
               seen(r, h) where stuck(r, h, _, _)"#,
        )
        .unwrap();
        assert!(!r.stuck.is_empty());
        assert!(
            violations.iter().any(|v| v.starts_with("stuck ctx=")),
            "{violations:?}"
        );
        let seen: BTreeSet<String> = facts_of(&r, "seen").into_iter().collect();
        let want: BTreeSet<String> = r
            .stuck
            .iter()
            .map(|s| {
                let f = s.fact();
                format!(
                    "seen({}, {})",
                    partition::fmt_term(&f.args[0]),
                    partition::fmt_term(&f.args[1])
                )
            })
            .collect();
        assert_eq!(seen, want);
        assert!(
            r.stuck
                .iter()
                .any(|s| s.text.contains("at least two zones")),
            "{:?}",
            r.stuck
        );
    }

    /// A reader of stuck/4 whose head feeds a rule that can stick would
    /// have to see its own consequences: a negative cycle, rejected.
    #[test]
    fn a_stuck_reader_on_a_cycle_is_an_error() {
        let err = gke_with(
            r#"flag(r) where stuck(r, _, _, _)
               deny "flagged" where flag(r), r > 3"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("negative cycle through"), "{err}");
        assert!(err.contains("can stick (stuck/4)"), "{err}");
    }

    #[test]
    fn a_rule_cannot_define_stuck() {
        let err = run("stuck(1, \"a\", \"b\", \"c\") where input(\"x\", \"y\")").unwrap_err();
        assert!(
            err.to_string()
                .contains("stuck/4 is derived by the evaluator")
        );
    }

    /// A resumed evaluation derives stuck/4 again when a constraint reads
    /// its later facts: the constraint is checked after the strata, and its
    /// instances are stuck/4's too.
    #[test]
    fn a_resumed_evaluation_counts_a_constraints_stuck_instance() {
        let program = crate::parser::parse_program(
            r#"decl later(a)
               strict(r) where stuck(r, _, _, _)
               deny "later is positive" where later(x), x > 0"#,
        )
        .unwrap();
        let (_, _, resumable) = eval_resumable(&program, &[], &["later"]).unwrap();
        let open = Value::Null {
            label: "t/a#n".into(),
            class: crate::value::NullClass::Open,
            ty: "int".into(),
        };
        let later = Atom {
            pred: "later".into(),
            args: vec![Term::Val(open)],
            record: None,
            span: Default::default(),
        };
        let (r, _) = resumable.with(&[later]).unwrap();
        assert_eq!(r.stuck.len(), 1, "{:?}", r.stuck);
        assert_eq!(facts_of(&r, "strict").len(), 1);
    }
}

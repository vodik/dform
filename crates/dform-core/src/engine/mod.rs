mod aggregate;
mod body;
mod builtins;
mod collapse;
mod contributions;
mod errors;
mod membership;
mod negation;
mod nulls;
mod unify;

use crate::ast::{Atom, Helper, Lit, Program, RuleStmt, Span, Term, str_term};
use crate::circuit::{self, Circuit, Leaf, NodeId};
use crate::diag;
use crate::ir::ops;
use crate::ir::store::{Store, TupleId, Window};
use crate::partition::{self, Node};
use crate::spell;
use crate::stuck::{self, Stuck};
use crate::transform;
use crate::value::Value;
use anyhow::{Context, Result, bail};
use body::{Derived, Src, cmp_order, eval_body, eval_rule, read_pattern, string_cells};
use builtins::eval_term;
pub(crate) use builtins::order;
use contributions::{AttrAggregate, Origin, Origins, is_contribution};
use errors::{at_suffix, check_defined, self_spread, with_place};
pub use membership::holds;
use nulls::Rec;
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
                other => Value::Str(spell::term(other)),
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
            pattern: spell::atom(a),
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
    let text = spell::atom(a);
    if let Some(event) = crate::stack::published(a) {
        return Leaf::World { event };
    }
    match a.pred.as_str() {
        p if crate::zset::POLICY_INPUTS.contains(&p) => Leaf::Plan { fact: text, tick },
        "input" | "data" => {
            let flag = if a.pred == "input" { "set" } else { "data" };
            let kv = match a.args.as_slice() {
                [Term::Val(k), Term::Val(v)] => {
                    format!("{}={}", spell::bare(k), spell::bare(v))
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
    for r in &rules {
        if AGGREGATE_OUTPUTS.contains(&r.head.pred.as_str()) {
            bail!(
                "{} is derived by the attribute aggregate; contribute with arg instead: {}{}",
                r.head.pred,
                spell::rule(r),
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
                spell::rule(r),
                at_suffix(r.head.span)
            );
        }
        if LATTICE_DECLS.contains(&r.head.pred.as_str())
            || REFINE_DECLS.contains(&r.head.pred.as_str())
        {
            bail!(
                "{} must be a fact, not a rule: {}{}",
                r.head.pred,
                spell::rule(r),
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
            let help = self_spread(&graph, &negative_edges).unwrap_or_default();
            bail!(
                "{}{help}",
                partition::cycle_error(&graph, &scc, &negative_edges)
            )
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
    for r in rules.iter().zip(&plans) {
        for (rel, key) in string_cells(&r.0.body, &r.1.body) {
            prov.store.index(&rel, &key);
        }
    }

    let rule_text: Vec<String> = rules.iter().map(spell::rule).collect();
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
    let negations = Helper::Negation.heads(&rules);
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
            negations,
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
    let (rules, plans) = (&c.rules, &c.plans);
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
        let recs: Vec<Rec> = fix
            .rules
            .iter()
            .map(|&i| Rec {
                rule: i,
                head: &rules[i].head,
                text: &c.rule_text[i],
                known,
                aggregates: &c.aggregates,
                negations: &c.negations,
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

/// F DR-2 revised, last clause, for every rule: an instance whose body
/// positively reads a predicate with a stuck instance may derive after the
/// boundary that resolves it. Per rule and read position: the literals
/// before the read are evaluated (Rule 2 and 3 as they ran), the read is
/// matched against the stuck heads under those bindings, and the head is
/// instantiated with what that binds; the literals after it are not asked
/// (the answer over-approximates). A head found this way is read the same
/// way in turn, so a helper of a helper is found. A rule with a stuck
/// instance is already reported as one, a helper by the rule it was written
/// for, and a ground head already derived adds nothing.
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
    let helpers = c
        .rules
        .iter()
        .enumerate()
        .filter(|(_, r)| r.helper.is_some());
    let skip: BTreeSet<usize> = (stucks.iter().filter_map(|s| s.rule))
        .chain(helpers.map(|(i, _)| i))
        .collect();
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
            if skip.contains(&i) {
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
                    negations: &c.negations,
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
            negations: &c.negations,
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
    found.retain(|s| !c.is_helper(s));
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

/// A policy fact: `deny(msg, ctx)` or `warn(msg, ctx)`.
fn policy_fact(pred: &str, msg: &str, ctx: Value) -> Atom {
    Atom {
        pred: pred.into(),
        args: vec![str_term(msg), Term::Val(ctx)],
        record: None,
        span: Default::default(),
    }
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
        Value::Range(r) => serde_json::Value::String(r.to_string()),
        Value::Ref { typ, name, attr } => {
            serde_json::Value::String(format!("ref({typ},{name},{attr})"))
        }
        Value::CloudRef { typ, name, attr } => {
            serde_json::Value::String(format!("cloud_ref({typ},{name},{attr})"))
        }
        Value::Null { label, .. } => serde_json::Value::String(format!("?{label}")),
        Value::Quantity(q) => serde_json::Value::String(q.to_string()),
        Value::Time(t) => serde_json::Value::String(t.to_string()),
        Value::Uri(u) => serde_json::Value::String(u.to_string()),
        Value::Oci(u) => serde_json::Value::String(u.clone()),
        Value::Semver(v) => serde_json::Value::String(v.to_string()),
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

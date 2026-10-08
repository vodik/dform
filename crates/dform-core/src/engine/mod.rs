mod aggregate;
mod builtins;
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
use crate::lattice::nulls_in;
use crate::lattice::{self, Collapsed, Lattice, Rank, RankedContribution, Shadowed, Witnesses};
use crate::partition::{self, Node};
use crate::spell;
use crate::stuck::{self, Stuck};
use crate::transform;
use crate::value::Value;
use aggregate::eval_rule_collect;
use anyhow::{Context, Result, anyhow, bail};
pub(crate) use builtins::order;
use builtins::{eval_builtin_pred, eval_cmp, eval_eq, eval_neq, eval_term};
use errors::{
    at_suffix, check_defined, head_error, reference_at_string_cell, self_spread, with_place,
};
pub use membership::holds;
use membership::{eval_member_like, eval_not_member2, eval_not_member3};
use negation::eval_not;
use nulls::{Rec, Rule3Clause, planted, term_has_bound_null};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use unify::{ground_atom, instantiate_atom, unify_atom};

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
                Origin::Fact(span) => with_place(spell::atom(a), *span),
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
                text: format!("attr({typ}, {}, {path}, _)", spell::value(addr)),
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
                            spell::value(addr),
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
                spell::value(&key.1),
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
            spell::atom(a)
        );
    };
    let Some(rank) = parse_rank(vals[4]) else {
        bail!(
            "contribution {}: rank must be default, normal or override",
            spell::atom(a)
        );
    };
    if let Some(list) = path.strip_suffix(transform::ELEM) {
        let Value::List(kv) = vals[3] else {
            bail!("internal: element write {}", spell::atom(a));
        };
        let [k, v] = kv.as_slice() else {
            bail!("internal: element write {}", spell::atom(a));
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
/// facts, a keyed list's with its keys' `type_default(T, P.K, V)`, and
/// a set for each `type_attr(T, P, "set(..)", _)` that declares none.
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
            bail!("{}/3 expects (Type, Path, ...): {}", a.pred, spell::atom(a));
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
            _ => bail!("{}: unknown lattice {}", spell::atom(a), spell::value(k)),
        };
        let key = (t.clone(), p.clone());
        if out.get(&key).is_some_and(|l| *l != lat) {
            bail!("path {t} {p} declares two lattices");
        }
        out.insert(key, lat);
    }
    // An attribute the schema types `set(T)` (R-158) is a set unless a
    // lattice is declared on it: its contributions union, so several
    // modules each add an element (`policies`, `peerings`, `routes`).
    for a in facts.iter().filter(|a| a.pred == "type_attr") {
        if let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(p)),
            Term::Val(Value::Str(ty)),
            ..,
        ] = a.args.as_slice()
            && ty.split('(').next().is_some_and(|k| k.trim() == "set")
        {
            out.entry((t.clone(), p.clone())).or_insert(Lattice::Set);
        }
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
        let r = r.map_err(|e| anyhow!("{}: {e}", spell::atom(a)))?;
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
    let path = &key.2;
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
    let below = lattice::Below {
        lattices: Some(nested),
        elems: &writes,
    };
    let mut cell = Collapse {
        key,
        contribs,
        elems,
        refs,
        first_ref,
        origins,
        store,
        out: Vec::new(),
    };
    let shadowed = match lattice::lub_ranked_refined(lat, path, &cells, &refinements, below) {
        Collapsed::Bottom => vec![],
        Collapsed::Val {
            value,
            shadowed,
            deferred,
            ..
        } => {
            cell.value(value, deferred);
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
            cell.violated(at, &constraint, value, &ws, &refinement);
            shadowed
        }
        Collapsed::Stuck {
            nulls, shadowed, ..
        } => {
            cell.out.push(cell.head(
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
            cell.conflict(&a.1, &b.1, reason, &ws);
            shadowed
        }
    };
    for sh in shadowed {
        cell.shadowed(sh);
    }
    cell.out
}

/// A cell being collapsed, its witnesses numbered: the contributions', the
/// element writes' after them, the refinements' last; and its facts.
struct Collapse<'a> {
    key: &'a GroupKey,
    contribs: &'a [Contribution],
    elems: &'a [ElemContribution],
    refs: &'a [&'a (TupleId, crate::refine::Stated)],
    /// The witness of the first refinement.
    first_ref: usize,
    origins: &'a Origins,
    store: &'a Store,
    out: Vec<Atom>,
}

impl Collapse<'_> {
    /// A fact of the cell: `pred(T, A, P, rest..)`.
    fn head(&self, pred: &str, rest: Vec<Value>) -> Atom {
        let (typ, addr, path) = self.key;
        Atom {
            pred: pred.into(),
            args: [str_term(typ), Term::Val(addr.clone()), str_term(path)]
                .into_iter()
                .chain(rest.into_iter().map(Term::Val))
                .collect(),
            record: None,
            span: Default::default(),
        }
    }

    /// The refinement witness `w` is, if it is one.
    fn refinement_of(&self, w: u32) -> Option<&(TupleId, crate::refine::Stated)> {
        self.refs
            .get((w as usize).checked_sub(self.first_ref)?)
            .copied()
    }

    /// The witness `w` as a policy's context says it: its rank, its value
    /// and where it is from.
    fn witness(&self, w: u32) -> Value {
        let store = self.store;
        if let Some((t, r)) = self.refinement_of(w) {
            let a = store.get(*t);
            return obj(vec![
                ("rank", Value::Str("refinement".into())),
                ("value", Value::Str(r.constraint.to_string())),
                (
                    "from",
                    Value::List(vec![Value::Str(with_place(spell::atom(a), a.span))]),
                ),
            ]);
        }
        let (t, r, v) = match self.contribs.get(w as usize) {
            Some((t, r, v)) => (t, r, v),
            None => {
                let (t, r, _, _, v) = &self.elems[w as usize - self.contribs.len()];
                (t, r, v)
            }
        };
        obj(vec![
            ("rank", Value::Str(rank_name(*r).into())),
            ("value", v.clone()),
            (
                "from",
                Value::List(
                    self.origins
                        .of(*t, store.get(*t))
                        .into_iter()
                        .map(Value::Str)
                        .collect(),
                ),
            ),
        ])
    }

    fn witnesses(&self, ws: &Witnesses) -> Value {
        Value::List(ws.iter().map(|w| self.witness(*w)).collect())
    }

    /// The first of `ws`, or an empty object.
    fn first(&self, ws: &Witnesses) -> Value {
        ws.iter()
            .next()
            .map(|w| self.witness(*w))
            .unwrap_or(Value::Obj(BTreeMap::new()))
    }

    /// A policy's context of the cell: its type, address and path, then
    /// `extra`.
    fn ctx(&self, extra: Vec<(&str, Value)>) -> Value {
        let (typ, addr, path) = self.key;
        let mut kv = vec![
            ("type", Value::Str(typ.clone())),
            ("addr", addr.clone()),
            ("path", Value::Str(path.clone())),
        ];
        kv.extend(extra);
        obj(kv)
    }

    /// The cell's value, and a refinement whose value it does not know yet,
    /// deferred.
    fn value(&mut self, value: Value, deferred: Vec<lattice::Deferred>) {
        let (typ, addr, _) = self.key;
        self.out.push(self.head("attr", vec![value]));
        for d in deferred {
            let nulls = lattice::nulls_in(&d.value);
            self.out.push(Atom {
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
    }

    /// A value that violates a refinement at `at`: the conflict, and a deny
    /// naming the refinement's place.
    fn violated(
        &mut self,
        at: String,
        constraint: &crate::lattice::Constraint,
        value: Value,
        ws: &Witnesses,
        refinement: &Witnesses,
    ) {
        let (typ, addr, _) = self.key;
        self.out.push(self.head(
            "attr_conflict",
            vec![self.first(ws), self.first(refinement)],
        ));
        let place = refinement
            .iter()
            .find_map(|w| self.refinement_of(*w))
            .and_then(|(t, _)| diag::place(self.store.get(*t).span))
            .unwrap_or_default();
        let mut kv = vec![
            ("type", Value::Str(typ.clone())),
            ("addr", addr.clone()),
            ("path", Value::Str(at)),
            ("constraint", Value::Str(constraint.to_string())),
            (
                "reason",
                Value::Str(format!("{} violates {constraint}", spell::value(&value))),
            ),
            ("value", value),
            (
                "witnesses",
                self.witnesses(&ws.union(refinement).copied().collect()),
            ),
        ];
        if !place.is_empty() {
            kv.push(("at", Value::Str(place)));
        }
        self.out
            .push(policy_fact("deny", crate::refine::VIOLATED, obj(kv)));
    }

    /// Contributions that conflict: the conflict, and a deny naming every
    /// witness.
    fn conflict(&mut self, a: &Witnesses, b: &Witnesses, reason: String, ws: &Witnesses) {
        self.out
            .push(self.head("attr_conflict", vec![self.first(a), self.first(b)]));
        let ctx = self.ctx(vec![
            ("reason", Value::Str(reason)),
            ("witnesses", self.witnesses(ws)),
        ]);
        self.out.push(policy_fact(
            "deny",
            "conflicting attribute contributions",
            ctx,
        ));
    }

    /// A disagreement at a losing rank, overridden: a warning.
    fn shadowed(&mut self, sh: Shadowed) {
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
        let ctx = self.ctx(vec![
            ("rank", Value::Str(rank_name(rank).into())),
            ("reason", Value::Str(what)),
            ("witnesses", self.witnesses(&ws)),
        ]);
        self.out.push(policy_fact(
            "warn",
            "attr_shadowed: contributions at a losing rank disagree and are overridden",
            ctx,
        ));
    }
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

fn eval_body(body: &[Lit], src: &Src, rec: &Rec) -> Result<Vec<Row>> {
    let mut states: Vec<Row> = vec![Row {
        s: HashMap::new(),
        used: Vec::new(),
        absent: Vec::new(),
        order: Vec::new(),
    }];
    for (i, lit) in body.iter().enumerate() {
        let mut next = Vec::new();
        // A literal that keeps or extends a row takes it: its bindings
        // (a document among them) are moved on, not copied.
        let rows = std::mem::take(&mut states);
        match lit {
            Lit::Pos(atom) => {
                if atom.pred == "member" || atom.pred == "enumerate" {
                    for row in &rows {
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
                    for row in rows {
                        if !rec.any_blocked(&atom.args, &row.s)
                            && eval_builtin_pred(atom, &row.s, rec)? == Some(true)
                        {
                            next.push(row);
                        }
                    }
                } else {
                    let read =
                        src.body.op(i).read().ok_or_else(|| {
                            anyhow!("internal: {} is not a relation read", atom.pred)
                        })?;
                    for row in &rows {
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
                        let before = next.len();
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
                        if next.len() == before
                            && let Some(cell) = string_cell(atom, read)
                        {
                            reference_at_string_cell(atom, &cell, s, src, src.win[i], rec)?;
                        }
                    }
                }
            }
            Lit::Not(atom) => {
                for mut row in rows {
                    let s = &row.s;
                    if rec.any_blocked(&atom.args, s) {
                        continue;
                    }
                    if atom.pred == "member" {
                        let holds = match atom.args.len() {
                            2 => eval_not_member2(atom, s, rec)?,
                            3 => eval_not_member3(atom, s, rec)?,
                            _ => bail!("member/2 or member/3 expected"),
                        };
                        if holds {
                            next.push(row);
                        }
                        continue;
                    }
                    if atom.pred == "enumerate" {
                        // `enumerate/3` is a generator; `not enumerate(...)` is meaningless
                        // (it would require checking existence over an implicit domain).
                        bail!("negation not supported for enumerate/3");
                    }
                    if ops::is_builtin_pred(&atom.pred) {
                        // Negation-as-failure for builtin predicates is just boolean
                        // negation; a call over a null is undetermined either way.
                        if !rec.any_blocked(&atom.args, s)
                            && eval_builtin_pred(atom, s, rec)? == Some(false)
                        {
                            next.push(row);
                        }
                        continue;
                    }
                    let grounded = ground_atom(atom, s)
                        .with_context(|| format!("unsafe negation: not {}(...)", atom.pred))?;
                    if eval_not(&grounded, src, s, rec) {
                        row.absent.push(grounded);
                        next.push(row);
                    }
                }
            }
            Lit::Eq(a, b) => {
                for mut row in rows {
                    let s = std::mem::take(&mut row.s);
                    if let Some(s2) = eval_eq(a, b, s, rec)? {
                        row.s = s2;
                        next.push(row);
                    }
                }
            }
            Lit::Neq(a, b) => {
                for row in rows {
                    if eval_neq(a, b, &row.s, rec)? {
                        next.push(row);
                    }
                }
            }
            Lit::Gt(a, b) | Lit::Ge(a, b) | Lit::Lt(a, b) | Lit::Le(a, b) => {
                for row in rows {
                    if eval_cmp(lit, a, b, &row.s, rec)? {
                        next.push(row);
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

/// A read `attr(T, X, P, "s")` whose type is a variable (`x in resource,
/// x.vpc == "main"`), its value a string the index looks up: the key that
/// finds the cell without its value (R-204). Where the type is known the
/// compiler says a reference is never a string; here only the cell can.
fn string_cell(atom: &Atom, read: &ops::Read) -> Option<ops::Key> {
    let [Term::Var(_), _, _, Term::Val(Value::Str(_))] = atom.args.as_slice() else {
        return None;
    };
    if atom.pred != "attr" || read.scan_all {
        return None;
    }
    let key: ops::Key = read.key.iter().copied().filter(|&c| c != 3).collect();
    (key.len() < read.key.len() && !key.is_empty()).then_some(key)
}

/// The indexes [`string_cell`] reads through, for a body and its plan.
fn string_cells(body: &[Lit], plan: &ops::Body) -> Vec<(ops::Rel, ops::Key)> {
    body.iter()
        .enumerate()
        .filter_map(|(i, l)| match l {
            Lit::Pos(a) => {
                let read = plan.op(i).read()?;
                Some((read.rel.clone(), string_cell(a, read)?))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests;

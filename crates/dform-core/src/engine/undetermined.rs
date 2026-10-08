//! What an evaluation leaves undetermined (E §2.7, F DR-2 revised): `stuck/4`, derived once
//! every body a stuck companion reads is complete and recorded with its firing, and the rule
//! instances that may derive after a boundary.

use super::Compiled;
use super::body::{Src, eval_body, eval_rule, read_pattern};
use super::builtins::eval_term;
use super::nulls::Rec;
use super::provenance::Prov;
use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::ir::ops;
use crate::ir::store::{Store, Window};
use crate::stuck::{self, Stuck};
use anyhow::Result;
use std::cell::RefCell;
use std::collections::BTreeSet;

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
pub(super) fn may_derive(
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
pub(super) fn may_derive_over(
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
pub(super) fn record_stucks(c: &Compiled, prov: &mut Prov, stucks: &[Stuck]) -> Vec<Atom> {
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
pub(super) fn derive_stuck(
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

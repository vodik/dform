//! Negation as failure over the store, `not p(t)`: Rule 2 for a pattern holding a null,
//! Rule 3 while a stuck head may derive it, a `not { .. }` helper's pattern matched as it is.

use super::body::Src;
use super::nulls::{Rec, Rule3Clause, planted};
use crate::ast::{Atom, Term};
use crate::ir::ops;
use crate::lattice::{Truth, nulls_in};
use crate::spell;
use crate::stuck;
use crate::value::Value;
use std::collections::{BTreeSet, HashMap};

/// `not p(t)` over ground `t`. Rule 2: a pattern holding an open or secret
/// null, or a fact that equals it only Unknown-ly, needs content. Rule 3:
/// the negation is undetermined while a stuck head of `p` unifies with
/// `p(t)`. Fresh nulls are decided under the Unique Name Assumption.
///
/// A helper the compiler wrote for a `not { .. }` body (`__neg_N`, in
/// whatever scope: [`Helper::Negation`]) is
/// derived from the very values the outer row binds, so its pattern is
/// matched as it is, nulls and all: whether `__neg_N(v)` holds is its body
/// over `v`, which is stuck itself when it reads a null of `v` (Rule 3
/// then). A pod spec holding the server's computed `dnsPolicy` does not
/// make `not p.securityContext.runAsNonRoot == true` wait on it (R-193).
pub(super) fn eval_not(grounded: &Atom, src: &Src, s: &HashMap<String, Value>, rec: &Rec) -> bool {
    let helper = rec.negations.contains(&grounded.pred);
    let mut open = BTreeSet::new();
    for t in grounded.args.iter().filter(|_| !helper) {
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
            // Another row's value: equal to this one only were their
            // nulls the same, when the body would agree on both.
            Truth::Unknown if helper => {}
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
                spell::atom(grounded),
                grounded.pred
            ),
        );
        return false;
    }
    true
}

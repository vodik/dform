//! Rule 2 and Rule 3 as a rule runs (E §2.7, F DR-2 revised): the recorder `Rec` of the
//! instances that wait on a null or on what is undetermined, and where a null blocks a term.

use super::builtins::eval_term;
use crate::ast::{Atom, Term};
use crate::lattice::{Truth, nulls_in};
use crate::partition;
use crate::stuck::{self, Stuck};
use crate::value::Value;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

#[cfg(feature = "test-hooks")]
pub(super) use crate::hooks::{Rule3 as Rule3Clause, planted};

/// A clause of Rule 3 the property test can plant a bug in (`hooks`).
#[cfg(not(feature = "test-hooks"))]
pub(super) enum Rule3Clause {
    Negation,
    Reader,
}

/// Without `test-hooks`, no clause is ever skipped.
#[cfg(not(feature = "test-hooks"))]
#[inline(always)]
pub(super) fn planted(_: Rule3Clause) -> bool {
    false
}

/// Rule 2's recorder for the rule being evaluated (E §2.7, F DR-2 revised):
/// every instance that needs a null's content is recorded here instead of
/// firing. `known` holds the stuck and may-derive heads of lower strata,
/// for Rule 3.
pub(super) struct Rec<'a> {
    pub(super) rule: usize,
    pub(super) head: &'a Atom,
    pub(super) text: &'a str,
    pub(super) known: &'a RefCell<stuck::Known>,
    /// Predicates defined by an aggregate rule (and `attr`): a positive read
    /// of one of their stuck groups is undetermined.
    pub(super) aggregates: &'a BTreeSet<String>,
    /// Predicates a `not { .. }` helper defines ([`Helper::Negation`]).
    pub(super) negations: &'a BTreeSet<String>,
    pub(super) found: RefCell<Vec<Stuck>>,
}

impl Rec<'_> {
    pub(super) fn stuck(
        &self,
        state: &HashMap<String, Value>,
        nulls: BTreeSet<String>,
        reason: impl Into<String>,
    ) {
        self.stuck_as(self.head, state, nulls, reason, None);
    }

    /// Stuck because the builtin `func` reads a null's content.
    pub(super) fn stuck_in(
        &self,
        state: &HashMap<String, Value>,
        nulls: BTreeSet<String>,
        func: &str,
    ) {
        let why = format!("builtin {} over a null", crate::functions::shown_call(func));
        self.stuck_as(self.head, state, nulls, why, Some(func.to_string()));
    }

    fn stuck_as(
        &self,
        head: &Atom,
        state: &HashMap<String, Value>,
        nulls: BTreeSet<String>,
        reason: impl Into<String>,
        func: Option<String>,
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
            func,
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
                match name == crate::ir::SCOPED || name == crate::ir::REF {
                    true => self.stuck(state, nulls, "resource address carries a null"),
                    false => self.stuck_in(state, nulls, &name),
                }
                true
            }
            None => false,
        }
    }

    pub(super) fn any_blocked<'t>(
        &self,
        ts: impl IntoIterator<Item = &'t Term>,
        state: &HashMap<String, Value>,
    ) -> bool {
        ts.into_iter().any(|t| self.blocked(t, state))
    }

    /// Three-valued equality: Unknown records the instance (Rule 2).
    pub(super) fn eq(
        &self,
        a: &Value,
        b: &Value,
        state: &HashMap<String, Value>,
        what: &str,
    ) -> bool {
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

/// A spread's lowering (R-199): it carries a null leaf in a part, and a
/// part that is a null blocks it ([`blocked_by_null`]).
pub(super) fn spreads(name: &str) -> bool {
    name == crate::functions::MERGE || name == crate::functions::CONCAT
}

/// Builtins that carry nulls instead of reading them: the aggregates (whose
/// content positions `eval_rule_collect` decides) and the functions
/// declared `forwards nulls`.
pub(super) fn forwards_nulls(name: &str) -> bool {
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
            if spreads(name) {
                // A spread's part not known yet (R-199): the literal's
                // fields are not known either, so it waits; a null leaf
                // inside a part is carried (R-183).
                let mut nulls = BTreeSet::new();
                for a in args {
                    if let v @ Value::Null { .. } = eval_term(a, state)? {
                        nulls.extend(nulls_in(&v));
                    }
                }
                return (!nulls.is_empty()).then(|| (name.clone(), nulls));
            }
            if forwards_nulls(name) {
                return None;
            }
            // A template over held secrets has its value (R-218).
            if name == crate::ir::FORMAT && eval_term(t, state).is_some() {
                return None;
            }
            let mut nulls = BTreeSet::new();
            for a in args {
                // A bound variable's value is read where it is, not copied.
                match a {
                    Term::Var(x) => nulls.extend(nulls_in(state.get(x)?)),
                    Term::Val(v) => nulls.extend(nulls_in(v)),
                    a => nulls.extend(nulls_in(&eval_term(a, state)?)),
                }
            }
            (!nulls.is_empty()).then(|| (name.clone(), nulls))
        }
        Term::List(xs) => xs.iter().find_map(|x| blocked_by_null(x, state)),
        Term::Obj(m) => m.values().find_map(|x| blocked_by_null(x, state)),
        _ => None,
    }
}

/// Does a variable of `t` hold a value with a null under `s`?
pub(super) fn term_has_bound_null(t: &Term, s: &HashMap<String, Value>) -> bool {
    match t {
        Term::Var(x) => s.get(x).is_some_and(stuck::has_null),
        Term::Func { args, .. } | Term::List(args) => {
            args.iter().any(|a| term_has_bound_null(a, s))
        }
        Term::Obj(m) => m.values().any(|a| term_has_bound_null(a, s)),
        Term::Val(_) | Term::Wildcard | Term::ListComp { .. } => false,
    }
}

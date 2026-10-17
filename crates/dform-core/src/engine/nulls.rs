//! Rule 2 and Rule 3 as a rule runs (E §2.7, F DR-2 revised): the recorder `Rec` of the
//! instances that wait on a null or on what is undetermined, and where a null blocks a term.

use super::builtins::eval_term;
use crate::ast::{Atom, Term};
use crate::lattice::{Truth, nulls_in};
use crate::partition;
use crate::stuck::{self, Stuck};
use crate::value::Value;
use std::borrow::Cow;
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
    pub(super) aggregates: &'a BTreeSet<crate::ast::Name>,
    /// Predicates a `not { .. }` helper defines ([`Helper::Negation`]).
    pub(super) negations: &'a BTreeSet<crate::ast::Name>,
    pub(super) found: RefCell<Vec<Stuck>>,
    /// The run's secrets, which its `random.*` calls derive with.
    pub(super) secrets: &'a crate::secrets::Run,
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
            head: stuck::head_pattern(head, state, |t, s| eval_term(t, s, self.secrets)),
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
        match blocked_by_null(t, state, self.secrets) {
            Some((name, nulls)) => {
                match name == crate::address::SCOPED || name == crate::address::REF {
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

/// Rule 2 for a call, the one rule every builtin is read by (R-199,
/// R-183): an argument that is entirely unknown, a null itself, leaves the
/// call nothing to compute from, so the call waits for it, whatever the
/// function; a null inside an argument otherwise known is a content
/// position too, unless the function carries it where it puts the argument
/// (`forwards nulls`). The labels of the nulls the call waits on, none
/// when it runs. The aggregates are not calls: `eval_rule_collect` decides
/// their content positions.
pub(super) fn waits_on<'v>(
    name: &str,
    args: impl IntoIterator<Item = &'v Value>,
) -> BTreeSet<String> {
    let mut nulls = BTreeSet::new();
    if partition::AGGREGATES.contains(&name) {
        return nulls;
    }
    let carries = crate::functions::get(name).is_some_and(|f| f.forwards_nulls);
    for v in args {
        if !carries || matches!(v, Value::Null { .. }) {
            nulls.extend(nulls_in(v));
        }
    }
    nulls
}

/// The innermost builtin application in `t` whose arguments are ground
/// and that waits on a null ([`waits_on`]), with those nulls.
fn blocked_by_null(
    t: &Term,
    state: &HashMap<String, Value>,
    secrets: &crate::secrets::Run,
) -> Option<(String, BTreeSet<String>)> {
    match t {
        Term::Func { name, args } => {
            if let Some(inner) = args.iter().find_map(|a| blocked_by_null(a, state, secrets)) {
                return Some(inner);
            }
            // A template over held secrets has its value (R-218).
            if name == crate::address::FORMAT && eval_term(t, state, secrets).is_some() {
                return None;
            }
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                // A bound variable's value is read where it is, not copied.
                vals.push(match a {
                    Term::Var(x) => Cow::Borrowed(state.get(x)?),
                    Term::Val(v) => Cow::Borrowed(v),
                    a => Cow::Owned(eval_term(a, state, secrets)?),
                });
            }
            let nulls = waits_on(name, vals.iter().map(|v| &**v));
            (!nulls.is_empty()).then(|| (name.clone(), nulls))
        }
        Term::List(xs) => xs.iter().find_map(|x| blocked_by_null(x, state, secrets)),
        Term::Obj(m) => m.values().find_map(|x| blocked_by_null(x, state, secrets)),
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

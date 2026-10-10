//! A pending group: a stuck resource rule, or one that may derive after a
//! boundary, a group of resources of unknown number; its pattern and the address
//! or copy the plan prints it as.

use super::tree::Site;
use super::waits::{Resolves, nulls};
use crate::address::Address;
use crate::ast::{Atom, Term};
use crate::engine::EvalResult;
use crate::spell;
use crate::value::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Group {
    /// `type.?` for a stuck resource rule, else the head pattern.
    pub pattern: String,
    pub on: Vec<String>,
    pub reason: String,
    pub resolves_after: Option<usize>,
    /// The rule (`r12`), with what its stuck instance binds.
    pub rule: Option<String>,
    pub bindings: Vec<(String, Value)>,
    /// What it reads that may derive after a boundary, as the body reads
    /// it (`release("crud_api", "schema", _)`).
    pub reads: Option<String>,
    /// Where the rule is written ([`Report::explain`](crate::report::Report::explain)).
    pub site: Option<Site>,
}

/// Stuck resource rules, and resource rules that may derive after a
/// boundary: a group of unknown cardinality.
pub(super) fn groups(
    res: &EvalResult,
    tick_of: &BTreeMap<(String, String), usize>,
    resolves: &Resolves,
) -> Vec<Group> {
    let stuck = res.stuck.iter().filter(|s| s.head.pred == "want").map(|s| {
        (
            &s.head,
            nulls(s),
            s.reason.clone(),
            s.rule,
            s.bindings.clone().into_iter().collect(),
            None,
        )
    });
    let may = res
        .may_derive
        .iter()
        .filter(|m| m.head.pred == "want")
        .map(|m| {
            let reads = crate::modules::private_text(&m.reads, &spell::atom, " ")
                .unwrap_or_else(|| spell::atom(&m.reads));
            (
                &m.head,
                m.nulls.iter().cloned().collect(),
                m.reason(),
                Some(m.rule),
                Vec::new(),
                Some(reads),
            )
        });
    let mut out: Vec<Group> = Vec::new();
    for (head, on, reason, rule, bindings, reads) in stuck.chain(may) {
        let g = Group {
            pattern: group_pattern(head),
            resolves_after: resolves(&on, tick_of),
            on,
            reason,
            rule: rule.map(|r| format!("r{r}")),
            bindings,
            reads,
            site: None,
        };
        if !out
            .iter()
            .any(|x| x.pattern == g.pattern && x.on == g.on && x.reason == g.reason)
        {
            out.push(g);
        }
    }
    out
}

/// A pending group's `want/2` head as the plan prints it: an address, or
/// `T[?]`, an unknown number of `T`; any other head as itself.
pub fn group_pattern(head: &Atom) -> String {
    match head.args.as_slice() {
        [Term::Val(Value::Str(t)), a] => match a {
            Term::Val(v) => Address {
                typ: t.clone(),
                name: spell::value(v).trim_matches('"').to_string(),
            }
            .to_string(),
            _ => format!("{t}[?]"),
        },
        _ => spell::atom(head),
    }
}

/// The copy a pending group's resources are of, when what it waits on is
/// whether the copy derives (`resource app blue`): `app["blue"]`.
pub(super) fn group_copy(g: &Group) -> Option<String> {
    let rest = g.reads.as_deref()?.strip_prefix("resource ")?;
    let (path, name) = rest.split_once(' ')?;
    Some(format!("{path}[\"{name}\"]"))
}

/// A pending group's address: the address its rule's statement names
/// (`k8s.job["migrate-v${schema}"]`) where the head leaves the name open.
pub(super) fn group_address(g: &Group) -> String {
    let Some(s) = g.site.as_ref().filter(|_| g.pattern.ends_with("[?]")) else {
        return g.pattern.clone();
    };
    let mut words = s.statement.split_whitespace();
    let (Some("resource"), Some(t), Some(name)) = (words.next(), words.next(), words.next()) else {
        return g.pattern.clone();
    };
    if !g.pattern.starts_with(&format!("{t}[")) {
        return g.pattern.clone();
    }
    // The name as the statement writes it: a template stays one.
    match name.starts_with('"') {
        true => format!("{t}[{name}]"),
        false => format!("{t}[\"{name}\"]"),
    }
}

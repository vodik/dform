//! What a run that refuses says (R-109, R-111): each violation as a
//! diagnostic, printed as every error is ([`crate::diag`]). A conflict at
//! the sites of the writes that disagree, each value under its own; a
//! deny on one line with where it is written, what it fired for under
//! it; any other violation (the executor's) on its line.

use super::Why;
use super::errors::{Diag, REFINEMENT, conflict_of, statement_at, violation_parts};
use super::labels::attribute;
use super::mask::Shown;
use crate::ast::{RuleStmt, Term};
use crate::diag::{self, Diagnostic, Kind};
use crate::query::Redactor;
use crate::value::Value;
use serde_json::Value as Json;

/// What `lattice` says of two writes at one rank that disagree.
const DISAGREE: &str = "two contributions disagree";

/// The violations `vs` that refuse a run, as diagnostics: one per
/// conflict, one per deny however many times it fired, in order.
/// `rules` are the evaluation's, where a deny is written.
pub fn refusals(vs: &[String], rules: &[RuleStmt], r: &Redactor) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = Vec::new();
    for v in vs {
        if let Some(c) = conflict_of(v, r) {
            out.push(conflict(&c));
            continue;
        }
        let (message, bindings) = violation_parts(v, r);
        let i = match out
            .iter()
            .position(|d| d.kind == Kind::Refused && d.message == message)
        {
            Some(i) => i,
            None => {
                out.push(denied(&message, rules));
                out.len() - 1
            }
        };
        if !bindings.is_empty() {
            out[i].given.push((bindings.join(", "), String::new()));
        }
    }
    out
}

/// Deny `message` refusing: its site, where `rules` write it, said on
/// the line as the policy block says it.
fn denied(message: &str, rules: &[RuleStmt]) -> Diagnostic {
    let span = rules
        .iter()
        .map(|r| &r.head)
        .find(|h| {
            h.pred == "deny"
                && !h.span.is_none()
                && matches!(h.args.first(), Some(Term::Val(Value::Str(m))) if m == message)
        })
        .map(|h| h.span)
        .unwrap_or_default();
    Diagnostic::new(Kind::Refused, span, message).inline()
}

/// Conflict `d`: the attribute and why, each write that disagrees at its
/// site, its value underlined where the source spells it, else said
/// beside it.
pub(super) fn conflict(d: &Diag) -> Diagnostic {
    let rank = d
        .rank
        .as_ref()
        .map(|r| format!(" at rank {r}"))
        .unwrap_or_default();
    // A value its type's check refuses is a refusal, two writes that
    // disagree a conflict (the plan lists both under `conflicts`).
    let checked = d.at.is_some() || d.witnesses.iter().any(|w| w.rank == REFINEMENT);
    let kind = match checked {
        true => Kind::Refused,
        false => Kind::Conflict,
    };
    let mut out = Diagnostic::bare(
        kind,
        format!("{}{rank}: {}", attribute(&d.addr, &d.path), d.reason),
    );
    for w in &d.witnesses {
        let (value, label) = match (w.rank.as_str(), &w.value) {
            (REFINEMENT, Shown::Value(Json::String(c))) => (c.clone(), "checked here".into()),
            (rank, v) => {
                let value = match &w.laid {
                    Some(laid) => crate::fmt::value::layout("", laid, usize::MAX).join(" "),
                    None => v.said(Why::Line),
                };
                let rank = match rank {
                    "normal" | "" => String::new(),
                    r => format!("at rank {r}: "),
                };
                (value.clone(), format!("{rank}{value}"))
            }
        };
        let at = w.from.iter().find_map(|f| statement_at(f));
        out = match (at, at.and_then(diag::span_at)) {
            (_, Some(line)) => match diag::find_in(line, &value) {
                // The value underlined says itself; a rank it is at, or
                // a check, is said after it.
                Some(v) if label == value => out.with_label(v, ""),
                Some(v) => out.with_label(v, label),
                None => out.with_label(line, label),
            },
            (Some(at), None) => out.with_given(at, label),
            (None, None) => out.with_given(label, ""),
        };
    }
    if let Some(span) = d.at.as_deref().and_then(diag::span_at)
        && d.witnesses.is_empty()
    {
        out = out.with_label(span, "checked here");
    }
    if d.rank.is_none() && d.reason == DISAGREE {
        out = out.with_help("rank one of them, `@default` or `@override`");
    }
    out
}

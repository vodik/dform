//! What a run that refuses says (R-109, R-111): each violation as a
//! diagnostic, printed as every error is ([`crate::diag`]). A conflict at
//! the sites of the writes that disagree, each value under its own; a
//! deny on one line with where it is written, what it fired for under
//! it; any other violation (the executor's) on its line.

use super::Why;
use super::errors::{Diag, REFINEMENT, Witness, conflict_of, statement_at, violation_parts};
use super::labels::attribute;
use super::layout::WIDTH;
use super::mask::Shown;
use super::tree::PROVIDER_DEFAULT;
use crate::ast::{Rank, RuleStmt, Term};
use crate::diag::{self, Diagnostic, Kind};
use crate::query::Redactor;
use crate::value::Value;
use serde_json::Value as Json;

/// What `lattice` says of two writes at one rank that disagree.
const DISAGREE: &str = "two contributions disagree";

/// What a refinement's site says.
const CHECKED: &str = "checked here";

/// The site column of a write no statement of the program made.
const NO_SITE: &str = "(no site)";

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

/// Conflict `d`: the attribute and why, then each write that disagrees
/// ([`witness`]).
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
        out = witness(out, w);
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

/// Write `w` of a conflict as a site of `out`: its statement, the value
/// underlined where the source spells it (its rank, when not normal,
/// after it), else the rank and the value laid out on rows of their own
/// under it, so no row says two things and wraps.
fn witness(out: Diagnostic, w: &Witness) -> Diagnostic {
    let at = w.from.iter().find_map(|f| statement_at(f));
    let line = at.and_then(diag::span_at);
    if let (REFINEMENT, Shown::Value(Json::String(c))) = (w.rank.as_str(), &w.value) {
        return match (at, line) {
            (_, Some(line)) => out.with_label(diag::find_in(line, c).unwrap_or(line), CHECKED),
            (Some(at), None) => out.with_given(at, CHECKED),
            (None, None) => out.with_given(CHECKED, ""),
        };
    }
    let rank = match w.rank.as_str() {
        "" => Rank::Normal.name(),
        r => r,
    };
    if let Some(v) = line.and_then(|line| diag::find_in(line, &w.value.said(Why::Line))) {
        let said = match rank == Rank::Normal.name() {
            true => String::new(),
            false => format!("at rank {rank}"),
        };
        return out.with_label(v, said);
    }
    let mut rows = vec![format!("at rank {rank}")];
    match &w.laid {
        Some(laid) => {
            let lines = crate::fmt::value::layout("", laid, WIDTH);
            rows.extend(lines.iter().enumerate().map(|(i, l)| match i {
                0 => format!("= {l}"),
                _ => format!("  {l}"),
            }));
        }
        None => rows.push(format!("= {}", w.value.said(Why::Line))),
    }
    let rows = rows.join("\n");
    match (at, line) {
        (_, Some(line)) => out.with_label(line, rows),
        (Some(at), None) => out.with_given(at, rows),
        (None, None) if rank == Rank::Internal.name() => out.with_given(PROVIDER_DEFAULT, rows),
        (None, None) => out.with_given(NO_SITE, rows),
    }
}

//! The policy block (R-200): each policy of the program (a deny, a
//! `check` a value waits on) is a rule, so it prints as a resource does:
//! one line, a mark, its text and its site, what it is about nested
//! under it. A policy's line is a tally over what it ranges over (`3
//! hold · 1 fails`), its mark the worst of them: one failure makes a
//! failing policy. Under it only what does not hold, each with why, and
//! the rest as one `N hold` line; holding policies are the block's count
//! (`-v` lists everything).

use super::{Address, Paint, Policy, Row, Style, Why, address, label, until_text, violation_parts};
use crate::ast::{Lit, Program, Stmt, Term};
use crate::engine::EvalResult;
use crate::query::Redactor;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// What one policy says of the plan, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mark {
    Fails,
    Undetermined,
    Holds,
}

impl Mark {
    /// The word in the mark's column.
    pub fn word(&self) -> &'static str {
        match self {
            Mark::Fails => "fails",
            Mark::Undetermined => "undetermined",
            Mark::Holds => "holds",
        }
    }

    fn paint(&self) -> Paint {
        match self {
            Mark::Fails => Paint::Error,
            Mark::Undetermined => Paint::Dim,
            Mark::Holds => Paint::Create,
        }
    }
}

/// One policy's line and what is under it.
#[derive(Debug, Clone)]
pub struct Line {
    /// Its text: a deny's message, a check's condition.
    pub text: String,
    /// Where it is written, `FILE:LINE`.
    pub at: String,
    /// What holds: the resources it ranges over that it holds for (their
    /// addresses, when it ranges over a type), else how many.
    pub holds: Vec<String>,
    pub hold: usize,
    /// Each failure: the resource it is of, when its context names one
    /// of those the policy ranges over, and the context (`workload =
    /// "web"`).
    pub fails: Vec<(Option<String>, String)>,
    /// Each thing it is undetermined for, and until what is known
    /// (`k8s.stateful_set db`, `until spec.template.spec.securityContext
    /// is known (tick 2)`).
    pub undetermined: Vec<(String, String)>,
}

impl Line {
    /// The worst of what it says.
    pub fn mark(&self) -> Mark {
        match (self.fails.is_empty(), self.undetermined.is_empty()) {
            (false, _) => Mark::Fails,
            (true, false) => Mark::Undetermined,
            (true, true) => Mark::Holds,
        }
    }

    /// `3 hold · 1 fails · 2 undetermined`, the parts there are.
    pub fn tally(&self) -> String {
        tally_text(
            self.hold,
            self.fails.len(),
            self.undetermined
                .iter()
                .map(|(s, _)| s)
                .collect::<BTreeSet<_>>()
                .len(),
        )
    }
}

/// `12 hold · 1 fails · 2 undetermined`: the parts that are not zero, or
/// `0 hold` for none.
pub fn tally_text(hold: usize, fails: usize, undetermined: usize) -> String {
    let parts: Vec<String> = [
        (hold, "hold"),
        (fails, "fails"),
        (undetermined, "undetermined"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, w)| format!("{n} {w}"))
    .collect();
    match parts.is_empty() {
        true => "0 hold".to_string(),
        false => parts.join(" · "),
    }
}

/// The block's count: how many policies hold, fail, are undetermined.
pub fn count(lines: &[Line]) -> (usize, usize, usize) {
    let n = |m: Mark| lines.iter().filter(|l| l.mark() == m).count();
    (n(Mark::Holds), n(Mark::Fails), n(Mark::Undetermined))
}

/// The program's policies over the evaluation `res`: each deny statement
/// with a message, and each check a value waits on (`undetermined`, the
/// report's [`Policy`] rows, a deny's own and a refinement's).
pub fn lines(
    program: &Program,
    res: &EvalResult,
    undetermined: &[Policy],
    r: &Redactor,
) -> Vec<Line> {
    // The resources of each type the program wants.
    let mut wanted: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for a in res.facts.iter().filter(|a| a.pred == "want") {
        if let [Term::Val(Value::Str(t)), Term::Val(Value::Str(n))] = a.args.as_slice() {
            wanted.entry(t).or_default().push(n);
        }
    }
    // Each deny by its message: where it is first written, and the types
    // it ranges over.
    let mut out: Vec<Line> = Vec::new();
    let mut ranges: Vec<BTreeSet<String>> = Vec::new();
    for s in &program.statements {
        let Stmt::Rule(rule) = s else { continue };
        let [Term::Val(Value::Str(message)), ..] = rule.head.args.as_slice() else {
            continue;
        };
        if rule.head.pred != "deny" || rule.head.span.is_none() || conflict(message) {
            continue;
        }
        let i = match out.iter().position(|l| l.text == *message) {
            Some(i) => i,
            None => {
                out.push(Line {
                    text: message.clone(),
                    at: crate::diag::location(rule.head.span)
                        .map(|(f, l, _)| format!("{f}:{l}"))
                        .unwrap_or_default(),
                    holds: Vec::new(),
                    hold: 0,
                    fails: Vec::new(),
                    undetermined: Vec::new(),
                });
                ranges.push(BTreeSet::new());
                out.len() - 1
            }
        };
        if let Some(t) = ranged(&rule.body) {
            ranges[i].insert(t);
        }
    }
    // A check a value waits on is a policy of its own.
    for p in undetermined.iter().filter(|p| p.refinement) {
        if !out.iter().any(|l| l.text == p.message) {
            out.push(Line {
                text: p.message.clone(),
                at: p.site.as_ref().map(|s| s.at.clone()).unwrap_or_default(),
                holds: Vec::new(),
                hold: 0,
                fails: Vec::new(),
                undetermined: Vec::new(),
            });
            ranges.push(BTreeSet::new());
        }
    }
    for a in res.facts.iter().filter(|a| a.pred == "deny") {
        let Some(Term::Val(Value::Str(message))) = a.args.first() else {
            continue;
        };
        let Some(i) = out.iter().position(|l| l.text == *message) else {
            continue;
        };
        let l = &mut out[i];
        let text = match a.args.get(1) {
            Some(Term::Val(ctx)) => format!("{message} ctx={}", crate::engine::value_to_json(ctx)),
            _ => message.clone(),
        };
        let (_, bindings) = violation_parts(&text, r);
        let of = match a.args.get(1) {
            Some(Term::Val(ctx)) => subject_in(ctx, &ranges[i], &wanted),
            _ => None,
        };
        l.fails.push((of, bindings.join(", ")));
    }
    for p in undetermined {
        let Some(l) = out.iter_mut().find(|l| l.text == p.message) else {
            continue;
        };
        if l.at.is_empty()
            && let Some(s) = &p.site
        {
            l.at = s.at.clone();
        }
        l.undetermined.extend(waits(p));
    }
    for (l, types) in out.iter_mut().zip(&ranges) {
        l.fails.sort();
        l.fails.dedup();
        l.undetermined.sort();
        l.undetermined.dedup();
        let subjects: Vec<String> = types
            .iter()
            .flat_map(|t| {
                wanted.get(t.as_str()).into_iter().flatten().map(|n| {
                    address(&Address {
                        typ: t.clone(),
                        name: n.to_string(),
                    })
                })
            })
            .collect();
        let undetermined: BTreeSet<&String> = l.undetermined.iter().map(|(s, _)| s).collect();
        let failed: BTreeSet<&String> = l.fails.iter().filter_map(|(s, _)| s.as_ref()).collect();
        let open = l.fails.len() + undetermined.len();
        match types.is_empty() {
            // About the deployment as a whole: it holds, or it does not.
            true => l.hold = usize::from(open == 0),
            false => {
                l.holds = subjects
                    .into_iter()
                    .filter(|s| !undetermined.contains(s) && !failed.contains(s))
                    .collect();
                // A failure of no resource it ranges over is one of them.
                let unnamed = l.fails.iter().filter(|(s, _)| s.is_none()).count();
                l.hold = l.holds.len().saturating_sub(unnamed);
            }
        }
    }
    out.sort_by(|a, b| (a.mark(), &a.text).cmp(&(b.mark(), &b.text)));
    out
}

/// A deny's own message for what is no policy of the program's: a
/// conflict, a refinement violated.
fn conflict(message: &str) -> bool {
    message == super::CONFLICT || message == crate::refine::VIOLATED
}

/// The resource of `types` a failure's context names: a reference to it,
/// or its name.
fn subject_in(
    ctx: &Value,
    types: &BTreeSet<String>,
    wanted: &BTreeMap<&str, Vec<&str>>,
) -> Option<String> {
    let of = |typ: &str, name: &str| {
        (types.contains(typ) && wanted.get(typ).is_some_and(|ns| ns.contains(&name))).then(|| {
            address(&Address {
                typ: typ.to_string(),
                name: name.to_string(),
            })
        })
    };
    match ctx {
        Value::Ref { typ, name, .. } => of(typ, name),
        Value::Str(name) => types.iter().find_map(|t| of(t, name)),
        Value::Obj(m) => m.values().find_map(|v| subject_in(v, types, wanted)),
        Value::List(xs) => xs.iter().find_map(|v| subject_in(v, types, wanted)),
        _ => None,
    }
}

/// The type a deny ranges over: its body's first `x in T`.
fn ranged(body: &[Lit]) -> Option<String> {
    body.iter().find_map(|l| match l {
        Lit::Pos(a) if a.pred == "want" => match a.args.as_slice() {
            [Term::Val(Value::Str(t)), Term::Var(_)] => Some(t.clone()),
            _ => None,
        },
        _ => None,
    })
}

/// What an undetermined policy waits on, by what it is about: each
/// resource with the cell it waits on (`until spec.x is known (tick 2)`),
/// else the value.
fn waits(p: &Policy) -> Vec<(String, String)> {
    let when = match p.after {
        Some(t) => format!("(tick {})", t + 1),
        None => match until_text(&p.until) {
            u if u.is_empty() => String::new(),
            u => format!("({u})"),
        },
    };
    let until = |cell: &str| {
        format!("until {cell} is known {when}")
            .trim_end()
            .to_string()
    };
    p.on.iter()
        .map(|n| match crate::value::null_parts(n) {
            Some((typ, name, path))
                if !name.is_empty()
                    && typ != crate::stack::UNAPPLIED
                    && typ != crate::transform::OUTPUT
                    && !path.is_empty() =>
            {
                (address(&Address { typ, name }), until(&path))
            }
            _ => (label(n), until(&label(n))),
        })
        .collect()
}

/// The block's rows: its header with the count, then each policy that
/// does not hold (each that does too, at `-v`), what does not hold under
/// it with why, and the rest as `N hold` (each, at `-v`).
pub(super) fn rows(lines: &[Line], why: Why, style: Style) -> Vec<Row> {
    let mut rows = Vec::new();
    if lines.is_empty() {
        return rows;
    }
    let (hold, fails, undetermined) = count(lines);
    let head = format!("policy  {}", tally_text(hold, fails, undetermined));
    rows.push(Row::new(&head, style.paint(Paint::Bold, &head)));
    let every = why >= Why::How;
    let width = lines
        .iter()
        .filter(|l| every || l.mark() != Mark::Holds)
        .map(|l| l.mark().word().len())
        .max()
        .unwrap_or(0);
    for l in lines {
        let mark = l.mark();
        if mark == Mark::Holds && !every {
            continue;
        }
        let word = mark.word();
        let pad = " ".repeat(width - word.len());
        let plain = format!("  {word}{pad}  {}", l.text);
        let painted = format!("  {}{pad}  {}", style.paint(mark.paint(), word), l.text);
        let right = format!("{}  {}", l.at, l.tally()).trim().to_string();
        rows.push(Row::new(&plain, painted).with(vec![right, l.at.clone()]));
        for (of, why) in &l.fails {
            let row = match (of, why.is_empty()) {
                (Some(of), true) => Row::plain(format!("    {of}")),
                (Some(of), false) => Row::plain(format!("    {of}")).with(vec![why.clone()]),
                (None, true) => Row::plain("    fails".to_string()),
                (None, false) => Row::plain(format!("    {why}")),
            };
            rows.push(row.kept());
        }
        for (subject, until) in &l.undetermined {
            rows.push(
                Row::plain(format!("    {subject}"))
                    .with(vec![until.clone()])
                    .kept(),
            );
        }
        match every {
            true => rows.extend(
                l.holds
                    .iter()
                    .map(|h| Row::plain(format!("    {h}")).with(vec!["holds".into()])),
            ),
            false if l.hold > 0 && mark != Mark::Holds => {
                rows.push(Row::plain(format!("    {} hold", l.hold)))
            }
            false => {}
        }
    }
    rows
}

impl Line {
    /// The line as `--json` says it: its mark, text, site and tally, and
    /// what is under it.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "mark": self.mark().word(),
            "text": self.text,
            "at": self.at,
            "hold": self.hold,
            "holds": self.holds,
            "fails": self.fails.iter().map(|(of, why)| serde_json::json!({
                "resource": of,
                "context": why,
            })).collect::<Vec<_>>(),
            "undetermined": self.undetermined.iter().map(|(of, until)| serde_json::json!({
                "of": of,
                "until": until,
            })).collect::<Vec<_>>(),
        })
    }
}

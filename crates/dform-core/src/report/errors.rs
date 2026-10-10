//! What the report says went wrong (R-109): a failure in one shape (`Failure`,
//! `said_of`, `sites`), a conflict or a shadowed disagreement with its witnesses
//! (`Diag`), a violation a run refuses on, a read of a row that does not exist
//! (`Unanswered`).

use super::Why;
use super::explain::relative_place;
use super::fold::whole;
use super::labels::{address, attribute};
use super::layout::{Row, WIDTH, layout};
use super::mask::{Shown, shown_value};
use super::style::{Paint, Style};
use super::tree;
use crate::address::Address;
use crate::ast::{Atom, Term};
use crate::engine::EvalResult;
use crate::fmt::value::Tree;
use crate::provider::json_to_value;
use crate::query::Redactor;
use crate::spell;
use crate::value::Value;
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

/// An error in one shape (R-109): what happened, to what, with the
/// address as the plan prints it (`apply ovh.domain_record
/// k3s."k8s-lab.vodik.xyz": refused, nothing changed`); the provider's or
/// the rule's message on its own line; where it is written
/// (`k3s.df:66`), when that is known. Each on its own line, the second
/// and third indented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub what: String,
    pub message: String,
    pub site: Option<String>,
    /// The resource it is about, for its site.
    pub addr: Option<Address>,
    /// The program's own error, found before any provider is asked
    /// ([`Failure::located`]): said as the compiler says one, its site
    /// first.
    pub located: bool,
}

impl Failure {
    /// The failure of `verb` on `addr` (`apply`), what happened said
    /// after it, the message being a provider's: its own naming of the
    /// address dropped from its front, and its every mention of the
    /// address in the stored form (`T["A"]`) as the plan prints it.
    pub fn of(verb: &str, addr: &Address, happened: &str, message: &str) -> Failure {
        let at = address(addr);
        let mut what = format!("{verb} {at}");
        if !happened.is_empty() {
            what.push_str(&format!(": {happened}"));
        }
        Failure {
            what,
            message: said_of(addr, message),
            site: None,
            addr: Some(addr.clone()),
            located: false,
        }
    }

    /// What the program leaves wrong in the resource at `addr`, each line
    /// of `message` one error said at the resource's site as the
    /// compiler says one (R-184): `backups.df:53, k8s.cron_job
    /// forgejo_backup.job: spec.jobTemplate.spec.template is unset
    /// (required: ..)`; a last line `help: ..` is the fix, under them.
    pub fn located(addr: &Address, message: String) -> Failure {
        Failure {
            what: address(addr),
            message,
            site: None,
            addr: Some(addr.clone()),
            located: true,
        }
    }

    /// The same, at `site`, unless it says one already.
    pub fn at(mut self, site: Option<String>) -> Failure {
        if self.site.is_none() {
            self.site = site.filter(|s| !s.is_empty());
        }
        self
    }

    /// Its lines, each after `lead` (the first) or indented under it.
    pub fn lines(&self, lead: &str) -> Vec<String> {
        if self.located {
            let at = self
                .site
                .as_ref()
                .map(|s| format!("{s}, "))
                .unwrap_or_default();
            let pad = " ".repeat(lead.chars().count());
            return self
                .message
                .lines()
                .enumerate()
                .map(|(i, l)| match l.strip_prefix("help: ") {
                    Some(h) => format!("{pad}  help: {h}"),
                    None => {
                        let lead = if i == 0 { lead } else { &pad };
                        format!("{lead}{at}{}: {l}", self.what)
                    }
                })
                .collect();
        }
        let mut out = vec![format!("{lead}{}", self.what)];
        let pad = " ".repeat(lead.chars().count() + 2);
        out.extend(
            self.message
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| format!("{pad}{l}")),
        );
        out.extend(self.site.iter().map(|s| format!("{pad}{s}")));
        out
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.lines("").join("\n"))
    }
}

impl std::error::Error for Failure {}

/// Where each of `addrs` is derived, `FILE:LINE` as the plan's site
/// column says it (relative to `top`): a failure's third line (R-109).
pub fn sites<'a>(
    res: &EvalResult,
    addrs: impl IntoIterator<Item = &'a Address>,
    top: Option<&std::path::Path>,
) -> BTreeMap<Address, String> {
    let r = Redactor::default();
    let p = tree::Printer {
        circuit: &res.circuit,
        redact: &r,
        all: false,
    };
    addrs
        .into_iter()
        .filter_map(|a| {
            let at = p.want_site(&res.rules, a)?.at;
            let at = top.and_then(|t| relative_place(&at, t)).unwrap_or(at);
            (!at.is_empty()).then(|| (a.clone(), at))
        })
        .collect()
}

/// A provider's message about `addr`, as dform prints it (R-109): its
/// own naming of the change in front dropped (`apply T["A"]: `, as the
/// mock, the Kubernetes and the OVH providers say it), and the address in
/// the stored form wherever it says it, as the plan prints it.
pub fn said_of(addr: &Address, message: &str) -> String {
    let full = addr.to_string();
    let at = address(addr);
    let mut m = message.trim();
    for front in [
        format!("apply {full}: "),
        format!("apply {at}: "),
        format!("plan {full}: "),
        format!("plan {at}: "),
        format!("{full}: "),
    ] {
        if let Some(rest) = m.strip_prefix(&front) {
            m = rest;
            break;
        }
    }
    m.replace(&full, &at)
}

#[derive(Debug, Clone)]
pub struct Diag {
    pub addr: Address,
    pub path: String,
    pub reason: String,
    pub rank: Option<String>,
    pub witnesses: Vec<Witness>,
    /// Where the check it violates is written, `FILE:LINE` (a
    /// refinement's).
    pub at: Option<String>,
}

/// One contribution to a conflicted (or shadowed) cell.
#[derive(Debug, Clone)]
pub struct Witness {
    pub rank: String,
    pub value: Shown,
    /// The statements that made it, as the engine names them, each with
    /// its place (`.. (at p.df:3:25)`): what `--json` and `-vv` print.
    pub from: Vec<String>,
    /// Where each was written, `FILE:LINE` (R-111): what the default
    /// level prints.
    pub at: Vec<String>,
    /// An object or a list as the plan lays a value out
    /// ([`crate::fmt::value`]), each leaf as a change line says it.
    pub laid: Option<Tree>,
}

impl Witness {
    /// Value `v` of a witness, as [`Witness::laid`] holds it: none for a
    /// scalar, and for a value the plan says as a whole (a secret, one
    /// holding a secret).
    fn laid(v: &Value, shown: &Shown, r: &Redactor) -> Option<Tree> {
        if !matches!(shown, Shown::Value(Json::Object(_) | Json::Array(_))) {
            return None;
        }
        let leaf = |x: &Value| match x {
            Value::Obj(_) | Value::List(_) => None,
            Value::Ref { .. } | Value::CloudRef { .. } => Some(r.surface(x)),
            x => Some(whole(&shown_value(x, r), Why::Line)),
        };
        Some(Tree::of(v, &leaf))
    }
}

/// The place `FILE:LINE` of a statement the engine names with its place
/// after it, `.. (at FILE:LINE:COL)` or `.. (at FILE:LINE:COL, use m)`.
fn statement_place(from: &str) -> Option<String> {
    let inner = from.strip_suffix(')')?;
    let at = &inner[inner.rfind(" (at ")? + 5..];
    let at = at.split(", ").next()?;
    let (file_line, col) = at.rsplit_once(':')?;
    col.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| file_line.to_string())
}

/// The rank a conflict's witness has when it is the check the value
/// violates (`engine::collapse`).
const REFINEMENT: &str = "refinement";

/// The deny that names an attribute whose contributions conflict.
pub const CONFLICT: &str = "conflicting attribute contributions";

/// Conflicts (the aggregate's deny) or shadowed disagreements (its warn),
/// from the policy facts it derives, with every witness.
pub(super) fn diags(res: &EvalResult, r: &Redactor, pred: &str, msg: &str) -> Vec<Diag> {
    let mut out = Vec::new();
    for a in res.facts.iter().filter(|a| a.pred == pred) {
        let [Term::Val(Value::Str(m)), Term::Val(Value::Obj(ctx))] = a.args.as_slice() else {
            continue;
        };
        if m != msg {
            continue;
        }
        let d = diag(ctx, r);
        // One line per disagreement, however many facts say it.
        let same = |x: &Diag| {
            (&x.addr, &x.path, &x.reason, &x.rank) == (&d.addr, &d.path, &d.reason, &d.rank)
        };
        if !out.iter().any(same) {
            out.push(d);
        }
    }
    out
}

/// A conflict's or a shadowed disagreement's context (`type`, `addr`,
/// `path`, `reason`, `rank`, `witnesses`) as the report holds it.
pub(super) fn diag(ctx: &BTreeMap<String, Value>, r: &Redactor) -> Diag {
    let s = |k: &str| match ctx.get(k) {
        Some(Value::Str(s)) => s.clone(),
        Some(v) => spell::value(v),
        None => String::new(),
    };
    let witnesses = match ctx.get("witnesses") {
        Some(Value::List(ws)) => ws
            .iter()
            .filter_map(|w| match w {
                Value::Obj(w) => Some(w),
                _ => None,
            })
            .map(|w| {
                let rank = match w.get("rank") {
                    Some(Value::Str(r)) => r.clone(),
                    _ => String::new(),
                };
                let value = match w.get("value") {
                    Some(v) => shown_value(v, r),
                    None => Shown::Absent,
                };
                let laid = w.get("value").and_then(|v| Witness::laid(v, &value, r));
                // The contributing rule's text may spell the value.
                let from: Vec<String> = match w.get("from") {
                    Some(Value::List(fs)) => fs
                        .iter()
                        .filter_map(|f| f.as_str().map(|f| r.text(f)))
                        .collect(),
                    _ => vec![],
                };
                let at = from.iter().filter_map(|f| statement_place(f)).collect();
                Witness {
                    rank,
                    value,
                    from,
                    at,
                    laid,
                }
            })
            .collect(),
        _ => vec![],
    };
    Diag {
        addr: Address {
            typ: s("type"),
            name: s("addr"),
        },
        path: s("path"),
        reason: s("reason"),
        rank: ctx.get("rank").map(|_| s("rank")),
        witnesses,
        at: match ctx.get("at") {
            Some(Value::Str(at)) => statement_place(&format!(" (at {at})")),
            _ => None,
        },
    }
}

/// A constraint violation as the plan's `conflicts` section prints it
/// (R-111), when it is one (`conflicting attribute contributions
/// ctx={..}` or `refinement violated ctx={..}`): what a run that refuses
/// before it plans says in place of the raw context.
pub fn violation_conflict(v: &str, r: &Redactor, why: Why, style: Style) -> Option<String> {
    let (msg, ctx) = v.split_once(" ctx=")?;
    if msg != CONFLICT && msg != crate::refine::VIOLATED {
        return None;
    }
    let ctx: Json = serde_json::from_str(ctx).ok()?;
    let Value::Obj(ctx) = json_to_value(&ctx) else {
        return None;
    };
    Some(diag_lines(&diag(&ctx, r), why, style, true))
}

/// The violations a run that refuses prints under `constraint
/// violations:`, on the plan, apply and destroy paths alike: each as the
/// plan's `!` line, a conflict as the `conflicts` section prints it
/// ([`violation_conflict`]), a deny as its message with its bindings in
/// the site column, `key = value` as the program would write the value
/// and never the context's JSON (After R-149), aligned across the lines;
/// bindings too wide for the column go one to a line beneath it.
pub fn violations(vs: &[String], r: &Redactor, style: Style) -> String {
    let mut out = String::new();
    let mut rows = Vec::new();
    let flush = |rows: &mut Vec<Row>, out: &mut String| {
        out.push_str(&layout(rows, style));
        rows.clear();
    };
    for v in vs {
        if let Some(c) = violation_conflict(v, r, Why::Line, style) {
            flush(&mut rows, &mut out);
            out.push_str(&c);
            continue;
        }
        let (message, bindings) = violation_parts(v, r);
        let left = format!("  ! {message}");
        let row = Row::new(&left, style.paint(Paint::Error, &left));
        let joined = bindings.join(", ");
        if left.chars().count() + 4 + joined.chars().count() <= WIDTH {
            rows.push(row.with(vec![joined]));
            continue;
        }
        rows.push(row);
        for b in bindings {
            rows.push(Row::plain(format!("      {}", style.paint(Paint::Dim, &b))));
        }
    }
    flush(&mut rows, &mut out);
    out
}

/// A violation's message and, for a deny with a context object, its
/// bindings as `key = value` ([`violations`]).
pub(super) fn violation_parts(v: &str, r: &Redactor) -> (String, Vec<String>) {
    let parsed = v
        .split_once(" ctx=")
        .and_then(|(msg, c)| Some((msg, serde_json::from_str::<Json>(c).ok()?)));
    let Some((msg, ctx)) = parsed else {
        return (r.text(v), Vec::new());
    };
    let bindings = match &ctx {
        Json::Object(m) => m
            .iter()
            .map(|(k, x)| format!("{k} = {}", binding(x, r)))
            .collect(),
        x => vec![binding(x, r)],
    };
    (r.text(msg), bindings)
}

/// A value of a deny's context as the program would write it: a string
/// quoted, a reference (`ref(T,N,A)` in the context) as the address it
/// names, a secret as the plan says one ([`Redactor::surface`]).
fn binding(x: &Json, r: &Redactor) -> String {
    if let Json::String(s) = x
        && let Some(inner) = s.strip_prefix("ref(").and_then(|s| s.strip_suffix(')'))
        && let [typ, name, attr] = inner.splitn(3, ',').collect::<Vec<_>>()[..]
    {
        let a = Address {
            typ: typ.to_string(),
            name: name.to_string(),
        };
        return attribute(&a, attr);
    }
    r.surface(&json_to_value(x))
}

/// A violation as a run that refuses says it on one line: a deny as its
/// message and its bindings ([`violations`]); any other as the evaluator
/// words it.
pub fn violation_line(v: &str, r: &Redactor) -> String {
    // A conflict (a refinement violated) as the `conflicts` section says
    // it, its witnesses beneath.
    if let Some(c) = violation_conflict(v, r, Why::Line, Style::PLAIN) {
        return c.trim_start().trim_end().to_string();
    }
    let (message, bindings) = violation_parts(v, r);
    match bindings.is_empty() {
        true => message,
        false => format!("{message}  {}", bindings.join(", ")),
    }
}

/// A contribution's read of a row that does not exist (R-194): a ref to
/// an address no rule wants (`transform::UNANSWERED`), taken apart: the
/// resource whose contribution holds it and the attribute it writes (else
/// what holds it, as words), what it reads, and where it is written.
pub struct Unanswered {
    pub holder: Option<Address>,
    pub(super) attr: Option<String>,
    pub(super) from: String,
    pub to: Address,
    pub(super) path: Option<String>,
    pub(super) at: Option<String>,
}

impl Unanswered {
    pub fn of(a: &Atom) -> Option<Unanswered> {
        if a.pred != crate::transform::UNANSWERED {
            return None;
        }
        let Some(Term::Val(Value::Obj(ctx))) = a.args.first() else {
            return None;
        };
        let s = |k: &str| match ctx.get(k)? {
            Value::Str(s) => Some(s.clone()),
            _ => None,
        };
        let holder = match (s("from_type"), s("from_name")) {
            (Some(typ), Some(name)) => Some(Address { typ, name }),
            _ => None,
        };
        let from = match &holder {
            Some(_) => String::new(),
            None => s("from")?,
        };
        Some(Unanswered {
            holder,
            attr: s("attr"),
            from,
            to: Address {
                typ: s("type")?,
                name: s("addr")?,
            },
            path: s("path").filter(|p| !p.is_empty()),
            at: s("at"),
        })
    }

    /// Each such read in `facts`, as [`Unanswered::message`] says it,
    /// once each, in order.
    pub fn messages<'a>(facts: impl IntoIterator<Item = &'a Atom>) -> Vec<String> {
        let said: BTreeSet<String> = facts
            .into_iter()
            .filter_map(Unanswered::of)
            .map(|u| u.message())
            .collect();
        said.into_iter().collect()
    }

    /// A run that derives such a read refuses it, before any provider is
    /// asked: each at its site.
    pub fn check<'a>(facts: impl IntoIterator<Item = &'a Atom>) -> anyhow::Result<()> {
        match Unanswered::messages(facts).as_slice() {
            [] => Ok(()),
            said => Err(anyhow::anyhow!(said.join("\n"))),
        }
    }

    /// What the reference reads: the attribute, else the resource.
    pub(super) fn read(&self) -> String {
        match &self.path {
            Some(p) => attribute(&self.to, p),
            None => address(&self.to),
        }
    }

    /// The read as an error at its site (R-119's form): it answered
    /// nothing, so the holder's attribute has no value.
    pub fn message(&self) -> String {
        let what = match (&self.holder, &self.attr) {
            (Some(h), Some(a)) => attribute(h, a),
            (Some(h), None) => address(h),
            (None, _) => self.from.clone(),
        };
        let at = self
            .at
            .as_ref()
            .map(|a| format!("{a}: "))
            .unwrap_or_default();
        format!(
            "{at}{} answered nothing, so {what} has no value: nothing derives {}",
            self.read(),
            address(&self.to)
        )
    }
}

/// Whether the violation `v` is a conflict the plan's `conflicts`
/// section lists ([`violation_conflict`]).
pub fn is_conflict(v: &str) -> bool {
    v.split_once(" ctx=")
        .is_some_and(|(m, _)| m == CONFLICT || m == crate::refine::VIOLATED)
}

/// A conflict's lines: `! T path.p: reason`, then each witness, its value
/// and where it was written (R-111); at `full` the statement that made it
/// as the engine names it.
pub(super) fn diag_lines(d: &Diag, why: Why, style: Style, conflict: bool) -> String {
    let bold = |s: &str| style.paint(Paint::Bold, s);
    let error = |s: &str| match conflict {
        true => style.paint(Paint::Error, s),
        false => s.to_string(),
    };
    let mut out = String::new();
    let rank = d
        .rank
        .as_ref()
        .map(|r| format!(" at rank {r}"))
        .unwrap_or_default();
    out.push_str(&error(&format!(
        "  ! {}{rank}: {}",
        attribute(&d.addr, &d.path),
        d.reason
    )));
    // A check's place when no witness says it.
    if let Some(at) = d.at.as_ref().filter(|_| d.witnesses.is_empty()) {
        out.push_str(&format!("  {}", style.paint(Paint::Dim, at)));
    }
    out.push('\n');
    for w in &d.witnesses {
        // The cell's rank is the `!` line's; a witness says its own
        // only when it is another (a losing rank); the check a value
        // violates says it is one, by its constraint.
        let rank = match w.rank.as_str() {
            "normal" | "" => String::new(),
            REFINEMENT => "check ".into(),
            r => format!("{r} "),
        };
        let from = match why {
            Why::Full if !w.from.is_empty() => {
                let names: Vec<String> = w.from.iter().map(|f| bold(f)).collect();
                format!("  from {}", names.join("; "))
            }
            _ if !w.at.is_empty() => {
                format!("  {}", style.paint(Paint::Dim, &w.at.join(", ")))
            }
            _ => String::new(),
        };
        let Some(laid) = &w.laid else {
            let value = match (w.rank.as_str(), &w.value) {
                (REFINEMENT, Shown::Value(Json::String(c))) => c.clone(),
                (_, v) => style.said(v, why),
            };
            out.push_str(&format!("      {rank}{value}{from}\n"));
            continue;
        };
        // An object or a list laid out as the plan's change lines lay
        // one out, where it was written on its first row.
        let rows = crate::fmt::value::layout(&rank, laid, WIDTH.saturating_sub(6));
        for (i, row) in rows.iter().enumerate() {
            let from = if i == 0 { from.as_str() } else { "" };
            out.push_str(&format!("      {row}{from}\n"));
        }
    }
    out
}

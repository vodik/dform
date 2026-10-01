//! The plan printer (proposal E §2.7, §7.4; F DR-2 revised): one report
//! built from an evaluation and the provider's plan, rendered as text for
//! `plan` and every `apply` tick, or as one JSON document for `--json`.
//!
//! Sections, in order: definite deformations grouped by resource; pending
//! deformations grouped by the nulls they wait on; pending groups (stuck
//! resource rules, `np-? x unknown`); undetermined policies, and denies
//! that may derive after a boundary; shadowed disagreements; conflicts.
//! Then the apply order, tick by tick.
//!
//! Every value passes through [`shown`] or [`shown_value`], which ask the
//! one [`Redactor`] that `query`, `why`, `graph` and `show` print through: a
//! null prints as its label, a secret, a value equal to one, or a value at
//! a sensitive path as `(sensitive LABEL)`. Nothing here formats a
//! sensitive value's bytes.

use crate::ast::{Atom, Program, Term};
use crate::engine::EvalResult;
use crate::ir::Address;
use crate::partition::{fmt_atom, fmt_value};
use crate::provider::{Action, ActionKind, Change, NULL_KEY, Plan, marker};
use crate::query::Redactor;
use crate::schema::Schema;
use crate::stuck::{Sections, Stuck};
use crate::value::{Value, null_owner};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};

/// One side of a change, after redaction.
#[derive(Debug, Clone, PartialEq)]
pub enum Shown {
    Absent,
    Value(Json),
    /// A null, by its printed label (`T["A"].p`).
    Null {
        label: String,
        class: String,
    },
    /// A secret (with its printed label) or a value at a sensitive path
    /// (none).
    Sensitive(Option<String>),
}

/// Redact one side of a provider change: a null or secret marker is its
/// label; a value at a sensitive path is `(sensitive)`; a value that is, or
/// holds, a secret the program set elsewhere is that secret's label.
pub fn shown(v: Option<&Json>, sensitive: bool, schema: &Schema, r: &Redactor) -> Shown {
    let Some(v) = v else {
        return Shown::Absent;
    };
    match marker(v) {
        Some((NULL_KEY, l)) => Shown::Null {
            label: crate::ir::label(l),
            class: null_class(l, schema),
        },
        Some((_, l)) => Shown::Sensitive(Some(crate::ir::label(l))),
        None if sensitive || has_secret(v) => Shown::Sensitive(None),
        None => match secret_in(&r.json(&json_value(v))) {
            Some(l) => Shown::Sensitive(Some(l)),
            None => Shown::Value(v.clone()),
        },
    }
}

/// Redact a fact store value: [`Redactor::json`], one side of a line.
pub fn shown_value(v: &Value, r: &Redactor) -> Shown {
    match r.json(v) {
        Json::Object(m) if m.len() == 2 && m.contains_key("null") => Shown::Null {
            label: m["null"].as_str().unwrap_or_default().to_string(),
            class: m["class"].as_str().unwrap_or_default().to_string(),
        },
        j => match secret_in(&j) {
            Some(l) => Shown::Sensitive(Some(l)),
            None => Shown::Value(j),
        },
    }
}

/// The label of the first `{"sensitive": label}` inside a redacted value.
fn secret_in(v: &Json) -> Option<String> {
    match v {
        Json::Object(m) if m.len() == 1 && m.contains_key("sensitive") => {
            Some(m["sensitive"].as_str().unwrap_or_default().to_string())
        }
        Json::Object(m) => m.values().find_map(secret_in),
        Json::Array(xs) => xs.iter().find_map(secret_in),
        _ => None,
    }
}

/// A provider document's JSON as a fact store value, to ask the redactor.
fn json_value(v: &Json) -> Value {
    match v {
        Json::String(s) => Value::Str(s.clone()),
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .unwrap_or(Value::Str(n.to_string())),
        Json::Array(xs) => Value::List(xs.iter().map(json_value).collect()),
        Json::Object(m) => Value::Obj(m.iter().map(|(k, x)| (k.clone(), json_value(x))).collect()),
        Json::Null => Value::Str(String::new()),
    }
}

/// A secret marker anywhere inside a value.
fn has_secret(v: &Json) -> bool {
    match (marker(v), v) {
        (Some((NULL_KEY, _)), _) => false,
        (Some(_), _) => true,
        (None, Json::Array(xs)) => xs.iter().any(has_secret),
        (None, Json::Object(m)) => m.values().any(has_secret),
        _ => false,
    }
}

/// A null's class from the schema, by its label `type/addr#path`.
fn null_class(label: &str, schema: &Schema) -> String {
    let (Some((t, _)), Some((_, p))) = (null_owner(label), label.split_once('#')) else {
        return "unknown".into();
    };
    schema
        .class_of(&t, p)
        .or_else(|| schema.optional_computed_class(&t, p))
        .map(|c| c.name().to_string())
        .unwrap_or_else(|| "unknown".into())
}

impl Shown {
    pub fn text(&self) -> String {
        match self {
            Shown::Absent => "<none>".into(),
            Shown::Value(Json::String(s)) => format!("\"{s}\""),
            Shown::Value(v) => serde_json::to_string(v).unwrap_or_else(|_| "<unprintable>".into()),
            Shown::Null { label, .. } => format!("?{label}"),
            Shown::Sensitive(Some(l)) => format!("(sensitive {l})"),
            Shown::Sensitive(None) => "(sensitive)".into(),
        }
    }

    pub fn json(&self) -> Json {
        match self {
            Shown::Absent => Json::Null,
            Shown::Value(v) => v.clone(),
            Shown::Null { label, class } => json!({"null": label, "class": class}),
            Shown::Sensitive(l) => json!({ "sensitive": l }),
        }
    }
}

/// How the report's text is painted: plain (what `text` returns, every
/// golden, `--json` and the plan file never see colour), or ANSI colour by
/// the plan's own semantics (`--color`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
}

/// What a piece of the plan is, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    /// `+`: green.
    Create,
    /// `~`: yellow.
    Update,
    /// `-`: red.
    Delete,
    /// `-/+` and `+/-`: magenta.
    Replace,
    /// A `?` null: cyan.
    Null,
    /// `(sensitive)`: dim.
    Sensitive,
    /// Conflicts and denies: red.
    Error,
    /// The pending-group line: the warning colour, bold yellow.
    Warn,
    /// Addresses, section headers, witness names: bold.
    Bold,
    /// `apply: complete`: bold green.
    Done,
}

impl Style {
    pub const PLAIN: Style = Style { color: false };

    /// `s` in `p`'s colour; unchanged when plain.
    pub fn paint(&self, p: Paint, s: &str) -> String {
        if !self.color || s.is_empty() {
            return s.to_string();
        }
        let sgr = match p {
            Paint::Create => "32",
            Paint::Update => "33",
            Paint::Delete | Paint::Error => "31",
            Paint::Replace => "35",
            Paint::Null => "36",
            Paint::Sensitive => "2",
            Paint::Warn => "1;33",
            Paint::Bold => "1",
            Paint::Done => "1;32",
        };
        format!("\x1b[{sgr}m{s}\x1b[0m")
    }

    /// An action's marker in its kind's colour.
    fn marker(&self, k: &ActionKind) -> String {
        let p = match k {
            ActionKind::Create | ActionKind::Adopt => Paint::Create,
            ActionKind::Update | ActionKind::Drift | ActionKind::Pending => Paint::Update,
            ActionKind::Delete | ActionKind::DeleteDeposed => Paint::Delete,
            ActionKind::Replace { .. } => Paint::Replace,
            ActionKind::Noop => return marker_of(k).to_string(),
        };
        self.paint(p, marker_of(k))
    }

    /// One side of a change: a null cyan, a sensitive value dim.
    fn shown(&self, v: &Shown) -> String {
        match v {
            Shown::Null { .. } => self.paint(Paint::Null, &v.text()),
            Shown::Sensitive(_) => self.paint(Paint::Sensitive, &v.text()),
            _ => v.text(),
        }
    }
}

/// What a deformation waits on: a boundary (the evaluator's sections), or
/// a comparison against an open null (the Z-set's pending update). `None`
/// when it is definite.
pub fn waits_on(a: &Action, sections: &Sections) -> Option<Vec<String>> {
    let key = (a.addr.typ.clone(), a.addr.name.clone());
    let on: Vec<String> = match (sections.pending.get(&key), &a.kind) {
        (Some(ns), _) => ns.iter().cloned().collect(),
        (None, ActionKind::Pending) => a.on.iter().cloned().collect(),
        // A deposed object's delete held for its dependents (`executor`).
        (None, _) if !a.on.is_empty() => a.on.iter().cloned().collect(),
        (None, _) => return None,
    };
    Some(on)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// A leaf whose value changes (or is set, or removed, per the action).
    Leaf,
    /// An element added to a set or a keyed list.
    Add,
    /// An element removed from a set or a keyed list.
    Remove,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub op: Op,
    pub path: String,
    pub before: Shown,
    pub after: Shown,
    /// An element's leaves, paths relative to the element.
    pub leaves: Vec<Line>,
}

#[derive(Debug, Clone)]
pub struct Deformation {
    pub kind: ActionKind,
    pub addr: Address,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone)]
pub struct PendingBlock {
    pub on: Vec<String>,
    pub resolves_after: Option<usize>,
    pub deformations: Vec<Deformation>,
}

#[derive(Debug, Clone)]
pub struct Group {
    /// `type.?` for a stuck resource rule, else the head pattern.
    pub pattern: String,
    pub on: Vec<String>,
    pub reason: String,
    pub resolves_after: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub message: String,
    pub on: Vec<String>,
    pub reason: String,
    pub after: Option<usize>,
    /// Undetermined (Rule 3), or may derive after a boundary (a positive
    /// read of a predicate with a stuck instance).
    pub may_derive: bool,
    /// A deferred refinement check, not a deny (`message` names it).
    pub refinement: bool,
}

#[derive(Debug, Clone)]
pub struct Diag {
    pub addr: Address,
    pub path: String,
    pub reason: String,
    pub rank: Option<String>,
    /// (rank, value, from)
    pub witnesses: Vec<(String, Shown, Vec<String>)>,
}

/// Everything the plan printer shows, for text or JSON.
#[derive(Debug, Clone)]
pub struct Report {
    pub stack: String,
    pub show_noop: bool,
    pub definite: Vec<Deformation>,
    pub noops: usize,
    pub pending: Vec<PendingBlock>,
    pub groups: Vec<Group>,
    pub policies: Vec<Policy>,
    pub shadowed: Vec<Diag>,
    pub conflicts: Vec<Diag>,
    /// The apply order: per tick, the addresses applied in it.
    pub ticks: Vec<(usize, Vec<String>)>,
    /// Held for a null whose resolution is not scheduled by this plan.
    pub unscheduled: Vec<String>,
    pub undeformed: bool,
    pub moved: Vec<(Address, Address)>,
    pub denies: Vec<String>,
    /// Every null the sections name, with its class, for `--json`.
    pub classes: BTreeMap<String, String>,
}

/// What the report is built from.
pub struct Input<'a> {
    pub plan: &'a Plan,
    pub res: &'a EvalResult,
    pub sections: &'a Sections,
    pub program: &'a Program,
    pub schema: &'a Schema,
    pub stack: &'a str,
    pub show_noop: bool,
    /// The tick this plan's definite deformations run in (1 for `plan`).
    pub tick: usize,
    /// `moved/3` renames applied to state before this plan (E §3.4).
    pub moved: &'a [(Address, Address)],
    /// Denies over the plan itself (`lifecycle prevent_destroy`).
    pub denies: &'a [String],
}

pub fn report(i: &Input) -> Report {
    let r = Redactor::new(&i.res.facts, i.schema);
    let mut conflicts = diags(i.res, &r, "deny", "conflicting attribute contributions");
    conflicts.extend(diags(i.res, &r, "deny", crate::refine::VIOLATED));
    let conflicted: BTreeSet<&Address> = conflicts.iter().map(|d| &d.addr).collect();
    let (held, definite): (Vec<&Action>, Vec<&Action>) = i
        .plan
        .actions
        .iter()
        .filter(|a| !conflicted.contains(&a.addr))
        .partition(|a| waits_on(a, i.sections).is_some());
    let noops = definite
        .iter()
        .filter(|a| matches!(a.kind, ActionKind::Noop))
        .count();

    // The schedule: definite deformations run in this tick; a held one
    // runs after every null it waits on is resolved, which is after the
    // tick its owner runs in.
    let mut tick_of: BTreeMap<(String, String), usize> = definite
        .iter()
        .filter(|a| !matches!(a.kind, ActionKind::Noop))
        .map(|a| ((a.addr.typ.clone(), a.addr.name.clone()), i.tick))
        .collect();
    let resolves = |on: &[String], tick_of: &BTreeMap<(String, String), usize>| {
        on.iter()
            .map(|n| null_owner(n).and_then(|o| tick_of.get(&o).copied()))
            .collect::<Option<Vec<usize>>>()
            .and_then(|ts| ts.into_iter().max())
    };
    loop {
        let mut changed = false;
        for a in &held {
            let key = (a.addr.typ.clone(), a.addr.name.clone());
            if tick_of.contains_key(&key) {
                continue;
            }
            let on = waits_on(a, i.sections).unwrap_or_default();
            if let Some(t) = resolves(&on, &tick_of) {
                tick_of.insert(key, t + 1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut by_nulls: BTreeMap<Vec<String>, Vec<Deformation>> = BTreeMap::new();
    for a in &held {
        let on = waits_on(a, i.sections).unwrap_or_default();
        by_nulls
            .entry(on)
            .or_default()
            .push(deformation(a, i.schema, &r));
    }
    let pending: Vec<PendingBlock> = by_nulls
        .into_iter()
        .map(|(on, deformations)| PendingBlock {
            resolves_after: resolves(&on, &tick_of),
            on,
            deformations,
        })
        .collect();

    let groups = groups(i.res, &tick_of, &resolves);
    let mut policies = policies(i, &tick_of, &resolves);
    policies.extend(deferred(i.res, &tick_of, &resolves));

    let mut ticks: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut unscheduled = Vec::new();
    for a in definite.iter().chain(&held) {
        if matches!(a.kind, ActionKind::Noop) {
            continue;
        }
        let name = a.addr.to_string();
        match tick_of.get(&(a.addr.typ.clone(), a.addr.name.clone())) {
            Some(t) => ticks.entry(*t).or_default().push(name),
            None => unscheduled.push(name),
        }
    }
    // create_before_destroy deposes the old object; it is deleted at the
    // next tick, once what depends on it has moved to the replacement.
    for a in &definite {
        if matches!(a.kind, ActionKind::Replace { create_first: true }) {
            ticks
                .entry(i.tick + 1)
                .or_default()
                .push(format!("{} (deposed)", a.addr));
        }
    }
    for g in &groups {
        match g.resolves_after {
            Some(t) => ticks.entry(t + 1).or_default().push(g.pattern.clone()),
            None => unscheduled.push(g.pattern.clone()),
        }
    }

    // E §2.7: undeformed is the zero Z-set, nothing stuck, no cell stuck.
    let undeformed = noops == definite.len()
        && held.is_empty()
        && conflicts.is_empty()
        && i.sections.blocking.is_empty()
        && i.sections.undetermined.is_empty()
        && i.sections.pending_groups.is_empty();
    let classes = pending
        .iter()
        .flat_map(|b| b.on.iter())
        .chain(groups.iter().flat_map(|g| g.on.iter()))
        .chain(policies.iter().flat_map(|p| p.on.iter()))
        .map(|l| (l.clone(), null_class(l, i.schema)))
        .collect();
    Report {
        stack: i.stack.to_string(),
        show_noop: i.show_noop,
        definite: definite
            .iter()
            .filter(|a| i.show_noop || !matches!(a.kind, ActionKind::Noop))
            .map(|a| deformation(a, i.schema, &r))
            .collect(),
        noops,
        pending,
        groups,
        policies,
        shadowed: diags(
            i.res,
            &r,
            "warn",
            "attr_shadowed: contributions at a losing rank disagree and are overridden",
        ),
        conflicts,
        ticks: ticks.into_iter().collect(),
        unscheduled,
        undeformed,
        moved: i.moved.to_vec(),
        denies: i.denies.to_vec(),
        classes,
    }
}

type Resolves<'a> = dyn Fn(&[String], &BTreeMap<(String, String), usize>) -> Option<usize> + 'a;

fn nulls(s: &Stuck) -> Vec<String> {
    s.nulls.iter().cloned().collect()
}

/// Stuck resource rules, and resource rules that may derive after a
/// boundary: a group of unknown cardinality.
fn groups(
    res: &EvalResult,
    tick_of: &BTreeMap<(String, String), usize>,
    resolves: &Resolves,
) -> Vec<Group> {
    let stuck = res
        .stuck
        .iter()
        .filter(|s| s.head.pred == "want")
        .map(|s| (&s.head, nulls(s), s.reason.clone()));
    let may = res
        .may_derive
        .iter()
        .filter(|m| m.head.pred == "want")
        .map(|m| (&m.head, m.nulls.iter().cloned().collect(), m.reason()));
    let mut out: Vec<Group> = Vec::new();
    for (head, on, reason) in stuck.chain(may) {
        let g = Group {
            pattern: group_pattern(head),
            resolves_after: resolves(&on, tick_of),
            on,
            reason,
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
                name: fmt_value(v).trim_matches('"').to_string(),
            }
            .to_string(),
            _ => format!("{t}[?]"),
        },
        _ => fmt_atom(head),
    }
}

fn deny_message(head: &Atom) -> String {
    match head.args.first() {
        Some(Term::Val(Value::Str(m))) => m.clone(),
        _ => fmt_atom(head),
    }
}

/// Undetermined denies (Rule 3), then denies that may derive after a
/// boundary: a body that positively reads a predicate with a stuck
/// instance in its partition (F DR-2 revised, last clause).
fn policies(
    i: &Input,
    tick_of: &BTreeMap<(String, String), usize>,
    resolves: &Resolves,
) -> Vec<Policy> {
    let mut out: Vec<Policy> = Vec::new();
    for s in i.res.stuck.iter().filter(|s| s.head.pred == "deny") {
        let on = nulls(s);
        let p = Policy {
            message: deny_message(&s.head),
            after: resolves(&on, tick_of),
            on,
            reason: s.reason.clone(),
            may_derive: false,
            refinement: false,
        };
        if !out
            .iter()
            .any(|x| x.message == p.message && x.on == p.on && x.reason == p.reason)
        {
            out.push(p);
        }
    }
    out.sort_by(|a, b| (&a.message, &a.on).cmp(&(&b.message, &b.on)));

    let mut found: BTreeMap<String, (BTreeSet<String>, Vec<String>)> = BTreeMap::new();
    for m in i.res.may_derive.iter().filter(|m| m.head.pred == "deny") {
        let message = deny_message(&m.head);
        if out.iter().any(|p| p.message == message) {
            continue;
        }
        let (on, reads) = found.entry(message).or_default();
        on.extend(m.nulls.iter().cloned());
        reads.push(fmt_atom(&m.reads));
    }
    let mut may: Vec<Policy> = Vec::new();
    for (message, (on, mut reads)) in found {
        if on.is_empty() {
            continue;
        }
        reads.sort();
        reads.dedup();
        let on: Vec<String> = on.into_iter().collect();
        may.push(Policy {
            message,
            after: resolves(&on, tick_of),
            on,
            reason: format!("reads {} with a stuck instance", reads.join(", ")),
            may_derive: true,
            refinement: false,
        });
    }
    may.sort_by(|a, b| a.message.cmp(&b.message));
    may.dedup_by(|a, b| a.message == b.message);
    out.extend(may);
    out
}

/// Refinements the winning value could not decide yet (E §2.4 step 4):
/// `refinement_deferred(T, A, Path, C, Nulls)`, re-checked at the boundary
/// that resolves `Nulls`.
fn deferred(
    res: &EvalResult,
    tick_of: &BTreeMap<(String, String), usize>,
    resolves: &Resolves,
) -> Vec<Policy> {
    let mut out = Vec::new();
    for a in res
        .facts
        .iter()
        .filter(|a| a.pred == crate::refine::DEFERRED)
    {
        let [
            Term::Val(Value::Str(t)),
            Term::Val(addr),
            Term::Val(Value::Str(path)),
            Term::Val(Value::Str(c)),
            Term::Val(Value::List(nulls)),
        ] = a.args.as_slice()
        else {
            continue;
        };
        let on: Vec<String> = nulls
            .iter()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect();
        let addr = match addr {
            Value::Str(s) => s.clone(),
            v => fmt_value(v),
        };
        out.push(Policy {
            message: format!(
                "{c} of {}",
                Address {
                    typ: t.to_string(),
                    name: addr,
                }
                .attr(path)
            ),
            after: resolves(&on, tick_of),
            on,
            reason: "the value carries a null".into(),
            may_derive: false,
            refinement: true,
        });
    }
    out
}

/// Conflicts (the aggregate's deny) or shadowed disagreements (its warn),
/// from the policy facts it derives, with every witness.
fn diags(res: &EvalResult, r: &Redactor, pred: &str, msg: &str) -> Vec<Diag> {
    let mut out = Vec::new();
    for a in res.facts.iter().filter(|a| a.pred == pred) {
        let [Term::Val(Value::Str(m)), Term::Val(Value::Obj(ctx))] = a.args.as_slice() else {
            continue;
        };
        if m != msg {
            continue;
        }
        let s = |k: &str| match ctx.get(k) {
            Some(Value::Str(s)) => s.clone(),
            Some(v) => fmt_value(v),
            None => String::new(),
        };
        let (typ, path) = (s("type"), s("path"));
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
                    // The contributing rule's text may spell the value.
                    let from = match w.get("from") {
                        Some(Value::List(fs)) => fs
                            .iter()
                            .filter_map(|f| f.as_str().map(|f| r.text(f)))
                            .collect(),
                        _ => vec![],
                    };
                    (rank, value, from)
                })
                .collect(),
            _ => vec![],
        };
        out.push(Diag {
            addr: Address {
                typ,
                name: s("addr"),
            },
            path,
            reason: s("reason"),
            rank: ctx.get("rank").map(|_| s("rank")),
            witnesses,
        });
    }
    out
}

/// An action's change lines. An update diffs a keyless set, or a list
/// with merge keys, by element: an element that is new or gone is one
/// `+`/`-` line with its leaves. A keyless set's element is labeled by a
/// hash of its content (`[#k3j2d]`); it prints as `[]` in an update and
/// by position everywhere else.
fn deformation(a: &Action, schema: &Schema, r: &Redactor) -> Deformation {
    let paths = relabel(a.changes.iter().map(|c| c.path.as_str()));
    let leaf = |c: &Change, path: String| Line {
        op: Op::Leaf,
        path,
        before: shown(c.before.as_ref(), c.sensitive, schema, r),
        after: shown(c.after.as_ref(), c.sensitive, schema, r),
        leaves: vec![],
    };
    let by_element = matches!(
        a.kind,
        ActionKind::Update | ActionKind::Drift | ActionKind::Pending | ActionKind::Replace { .. }
    );
    let mut lines = Vec::new();
    // Element prefix -> (its display path, its changes), in first-seen order.
    let mut elements: Vec<(String, String, ElementChanges)> = Vec::new();
    for (c, shown_path) in a.changes.iter().zip(paths) {
        let Some((list, elem, rest)) = by_element
            .then(|| element_of(&a.addr.typ, &c.path, schema))
            .flatten()
        else {
            lines.push(leaf(c, shown_path));
            continue;
        };
        let prefix = format!("{list}[{elem}]");
        let display = if elem.starts_with('#') {
            format!("{list}[]")
        } else {
            prefix.clone()
        };
        // The rest of the path as relabeled with every other change's.
        let rest = match rest.is_empty() {
            true => rest,
            false => {
                let after_elem = shown_path[list.len()..]
                    .find(']')
                    .map(|j| list.len() + j + 1);
                after_elem
                    .map(|j| shown_path[j..].trim_start_matches('.').to_string())
                    .unwrap_or(rest)
            }
        };
        match elements.iter_mut().find(|(p, _, _)| *p == prefix) {
            Some((_, _, cs)) => cs.push((c, rest)),
            None => elements.push((prefix, display, vec![(c, rest)])),
        }
    }
    for (_, display, cs) in elements {
        let added = cs.iter().all(|(c, _)| c.before.is_none());
        let removed = cs.iter().all(|(c, _)| c.after.is_none());
        if !added && !removed {
            lines.extend(
                cs.iter()
                    .map(|(c, rest)| leaf(c, format!("{display}.{rest}"))),
            );
            continue;
        }
        let op = if added { Op::Add } else { Op::Remove };
        // A scalar element is its own leaf.
        if let [(c, rest)] = cs.as_slice()
            && rest.is_empty()
        {
            let l = leaf(c, display);
            lines.push(Line { op, ..l });
            continue;
        }
        let leaves = cs.iter().map(|(c, rest)| leaf(c, rest.clone())).collect();
        lines.push(Line {
            op,
            path: display,
            before: Shown::Absent,
            after: Shown::Absent,
            leaves,
        });
    }
    Deformation {
        kind: a.kind.clone(),
        addr: a.addr.clone(),
        lines,
    }
}

/// An element's changes, each with its path relative to the element.
type ElementChanges<'a> = Vec<(&'a Change, String)>;

/// Replace every content label `[#hash]` by the element's position among
/// the labels under the same list, in order of appearance.
fn relabel<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut out = Vec::new();
    for p in paths {
        let mut s = String::new();
        let mut rest = p;
        while let Some(i) = rest.find("[#") {
            let Some(j) = rest[i..].find(']') else { break };
            s.push_str(&rest[..i]);
            let (list, label) = (s.clone(), rest[i + 1..i + j].to_string());
            let labels = seen.entry(list).or_default();
            let n = match labels.iter().position(|l| *l == label) {
                Some(n) => n,
                None => {
                    labels.push(label);
                    labels.len() - 1
                }
            };
            s.push_str(&format!("[{n}]"));
            rest = &rest[i + j + 1..];
        }
        s.push_str(rest);
        out.push(s);
    }
    out
}

/// `(list path, element label, rest)` when `path`'s first list segment is
/// a keyless set or a list with merge keys.
fn element_of(typ: &str, path: &str, schema: &Schema) -> Option<(String, String, String)> {
    let open = path.find('[')?;
    let close = open + path[open..].find(']')?;
    let list = &path[..open];
    let keyed = schema.list_key(typ, list).is_some();
    let set = schema.attr(typ, list).is_some_and(|a| a.ty == "set");
    if !keyed && !set {
        return None;
    }
    Some((
        list.to_string(),
        path[open + 1..close].to_string(),
        path[close + 1..].trim_start_matches('.').to_string(),
    ))
}

fn marker_of(k: &ActionKind) -> &'static str {
    match k {
        ActionKind::Create => "+",
        ActionKind::Adopt => ">",
        ActionKind::Update | ActionKind::Drift | ActionKind::Pending => "~",
        ActionKind::Delete | ActionKind::DeleteDeposed => "-",
        ActionKind::Replace {
            create_first: false,
        } => "-/+",
        ActionKind::Replace { create_first: true } => "+/-",
        ActionKind::Noop => "=",
    }
}

fn kind_name(k: &ActionKind) -> &'static str {
    match k {
        ActionKind::Create => "create",
        ActionKind::Adopt => "adopt",
        ActionKind::Update => "update",
        ActionKind::Drift => "drift",
        ActionKind::Pending => "update",
        ActionKind::Replace { .. } => "replace",
        ActionKind::Delete | ActionKind::DeleteDeposed => "delete",
        ActionKind::Noop => "no-op",
    }
}

fn nulls_text(on: &[String], style: Style) -> String {
    on.iter()
        .map(|n| style.paint(Paint::Null, &format!("?{}", crate::ir::label(n))))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Report {
    pub fn pending_count(&self) -> usize {
        self.pending
            .iter()
            .map(|b| b.deformations.len())
            .sum::<usize>()
            + self.groups.len()
    }

    /// Definite deformations by kind, in summary order.
    fn kinds(&self) -> Vec<(&'static str, usize)> {
        ["create", "update", "replace", "drift", "delete", "adopt"]
            .into_iter()
            .map(|k| {
                let n = self
                    .definite
                    .iter()
                    .filter(|d| kind_name(&d.kind) == k)
                    .count();
                (k, n)
            })
            .collect()
    }

    /// How many definite deformations the plan has (the summary's count).
    pub fn deformations(&self) -> usize {
        self.kinds().iter().map(|(_, n)| n).sum()
    }

    /// `plan: 3 deformations (2 create, 1 update), 5 pending, 2 undetermined`
    pub fn summary(&self) -> String {
        let kinds: Vec<(&str, usize)> = self.kinds().into_iter().filter(|(_, n)| *n > 0).collect();
        let n = self.deformations();
        let mut out = format!("plan: {n} deformation{}", if n == 1 { "" } else { "s" });
        if !kinds.is_empty() {
            let ks: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
            out.push_str(&format!(" ({})", ks.join(", ")));
        }
        if self.show_noop {
            out.push_str(&format!(", {} no-op", self.noops));
        }
        let pending = self.pending_count();
        if pending > 0 {
            out.push_str(&format!(", {pending} pending"));
        }
        let undetermined = self.policies.len();
        if undetermined > 0 {
            out.push_str(&format!(", {undetermined} undetermined"));
        }
        if !self.conflicts.is_empty() {
            let n = self.conflicts.len();
            out.push_str(&format!(", {n} conflict{}", if n == 1 { "" } else { "s" }));
        }
        out
    }

    /// The report as text, uncoloured: what `plan` prints with no colour,
    /// every golden, and the controller's log line.
    pub fn text(&self) -> String {
        self.render(Style::PLAIN)
    }

    /// The report as text, painted with `style`.
    pub fn render(&self, style: Style) -> String {
        let bold = |s: &str| style.paint(Paint::Bold, s);
        let header = |s: &str| format!("{}\n", bold(s));
        let mut out = moved_text(&self.moved);
        if self.undeformed && !self.show_noop {
            out.push_str(&format!("stack {} is undeformed\n", self.stack));
            return out;
        }
        out.push_str(&self.summary());
        out.push('\n');
        if !self.definite.is_empty() {
            out.push_str(&header("definite:"));
            for d in &self.definite {
                write_deformation(&mut out, d, style);
            }
        }
        for b in &self.pending {
            let after = b
                .resolves_after
                .map(|t| format!(" (resolves after tick {t})"))
                .unwrap_or_default();
            out.push_str(&format!(
                "{} {}{}\n",
                bold("pending on"),
                nulls_text(&b.on, style),
                bold(&format!("{after}:"))
            ));
            for d in &b.deformations {
                write_deformation(&mut out, d, style);
            }
        }
        if !self.groups.is_empty() {
            out.push_str(&header("pending groups:"));
            for g in &self.groups {
                let after = g
                    .resolves_after
                    .map(|t| format!(", resolves after tick {t}"))
                    .unwrap_or_default();
                let line = format!(
                    "? {} x unknown, on {}{after}  ({})",
                    g.pattern,
                    nulls_text(&g.on, Style::PLAIN),
                    g.reason
                );
                out.push_str(&style.paint(Paint::Warn, &line));
                out.push('\n');
            }
        }
        if !self.policies.is_empty() {
            out.push_str(&header("undetermined:"));
            for p in &self.policies {
                let when = match (p.may_derive, p.after) {
                    (false, Some(t)) => format!(", decided after tick {t}"),
                    (true, Some(t)) => format!(", may derive after tick {t}"),
                    (true, None) => ", may derive after a boundary".into(),
                    (false, None) => String::new(),
                };
                if p.refinement {
                    out.push_str(&format!(
                        "? refinement on {} deferred: {}{when}\n",
                        nulls_text(&p.on, style),
                        p.message
                    ));
                    continue;
                }
                out.push_str(&format!(
                    "? deny \"{}\" on {}{when}  ({})\n",
                    p.message,
                    nulls_text(&p.on, style),
                    p.reason
                ));
            }
        }
        for (title, ds) in [("shadowed", &self.shadowed), ("conflicts", &self.conflicts)] {
            if ds.is_empty() {
                continue;
            }
            let conflict = title == "conflicts";
            let error = |s: &str| match conflict {
                true => style.paint(Paint::Error, s),
                false => s.to_string(),
            };
            out.push_str(&header(&error(&format!("{title}:"))));
            for d in ds {
                let rank = d
                    .rank
                    .as_ref()
                    .map(|r| format!(" at rank {r}"))
                    .unwrap_or_default();
                out.push_str(&error(&format!(
                    "! {}{rank}: {}",
                    d.addr.attr(&d.path),
                    d.reason
                )));
                out.push('\n');
                for (r, v, from) in &d.witnesses {
                    let from = if from.is_empty() {
                        String::new()
                    } else {
                        let names: Vec<String> = from.iter().map(|f| bold(f)).collect();
                        format!("  from {}", names.join("; "))
                    };
                    out.push_str(&format!("    {r} {}{from}\n", style.shown(v)));
                }
            }
        }
        if !self.denies.is_empty() {
            out.push_str(&header(&style.paint(Paint::Error, "denied:")));
            for d in &self.denies {
                out.push_str(&style.paint(Paint::Error, &format!("! {d}")));
                out.push('\n');
            }
        }
        if !self.ticks.is_empty() || !self.unscheduled.is_empty() {
            // One address per line, so each pastes into `why` or a program.
            out.push_str(&header("apply order:"));
            let unscheduled = (!self.unscheduled.is_empty())
                .then(|| ("unscheduled".to_string(), &self.unscheduled));
            let ticks = self.ticks.iter().map(|(t, xs)| (format!("tick {t}"), xs));
            for (head, xs) in ticks.chain(unscheduled) {
                out.push_str(&format!("  {head}\n"));
                for x in xs {
                    out.push_str(&format!("    {x}\n"));
                }
            }
        }
        if self.undeformed {
            out.push_str(&format!("stack {} is undeformed\n", self.stack));
        }
        out
    }
}

/// `moved OLD -> NEW`, one line per rename `moved/3` applied to state.
pub fn moved_text(moves: &[(Address, Address)]) -> String {
    moves
        .iter()
        .map(|(old, new)| format!("moved {old} -> {new}\n"))
        .collect()
}

impl Report {
    /// The plan as one JSON document (`plan --json`): the sections as
    /// arrays, in the text's order; a null as `{"null": LABEL, "class":
    /// C}`, a secret or a sensitive value as `{"sensitive": LABEL}`.
    pub fn json(&self) -> Json {
        let nulls = |on: &[String]| -> Json {
            on.iter()
                .map(|l| {
                    let class = self
                        .classes
                        .get(l)
                        .cloned()
                        .unwrap_or_else(|| "unknown".into());
                    json!({"null": crate::ir::label(l), "class": class})
                })
                .collect()
        };
        let deformations =
            |ds: &[Deformation]| -> Json { ds.iter().map(deformation_json).collect() };
        let mut summary = serde_json::Map::new();
        summary.insert("deformations".into(), json!(self.deformations()));
        for (k, n) in self.kinds() {
            summary.insert(k.into(), json!(n));
        }
        summary.insert("no_op".into(), json!(self.noops));
        summary.insert("pending".into(), json!(self.pending_count()));
        summary.insert("undetermined".into(), json!(self.policies.len()));
        summary.insert("conflicts".into(), json!(self.conflicts.len()));
        json!({
            "stack": self.stack,
            "undeformed": self.undeformed,
            "summary": summary,
            "definite": deformations(&self.definite),
            "pending": self.pending.iter().map(|b| json!({
                "on": nulls(&b.on),
                "resolves_after": b.resolves_after,
                "deformations": deformations(&b.deformations),
            })).collect::<Vec<_>>(),
            "pending_groups": self.groups.iter().map(|g| json!({
                "pattern": g.pattern,
                "on": nulls(&g.on),
                "reason": g.reason,
                "resolves_after": g.resolves_after,
            })).collect::<Vec<_>>(),
            "undetermined": self.policies.iter().map(|p| json!({
                "policy": if p.refinement { "refinement" } else { "deny" },
                "message": p.message,
                "kind": if p.refinement {
                    "deferred"
                } else if p.may_derive {
                    "may_derive"
                } else {
                    "undetermined"
                },
                "on": nulls(&p.on),
                "reason": p.reason,
                "after": p.after,
            })).collect::<Vec<_>>(),
            "shadowed": self.shadowed.iter().map(diag_json).collect::<Vec<_>>(),
            "conflicts": self.conflicts.iter().map(diag_json).collect::<Vec<_>>(),
            "apply_order": self.ticks.iter().map(|(t, xs)| json!({
                "tick": t,
                "addresses": xs,
            })).collect::<Vec<_>>(),
            "unscheduled": self.unscheduled,
            "moved": self.moved.iter().map(|(old, new)| json!({
                "from": {"address": old.to_string(), "type": old.typ, "name": old.name},
                "to": {"address": new.to_string(), "type": new.typ, "name": new.name},
            })).collect::<Vec<_>>(),
            "denied": self.denies,
        })
    }
}

fn deformation_json(d: &Deformation) -> Json {
    let mut m = serde_json::Map::new();
    m.insert("action".into(), json!(kind_name(&d.kind)));
    m.insert("address".into(), json!(d.addr.to_string()));
    m.insert("type".into(), json!(d.addr.typ));
    m.insert("name".into(), json!(d.addr.name));
    match d.kind {
        ActionKind::Replace { create_first } => {
            m.insert("create_first".into(), json!(create_first));
        }
        ActionKind::DeleteDeposed => {
            m.insert("deposed".into(), json!(true));
        }
        _ => {}
    }
    m.insert(
        "changes".into(),
        d.lines.iter().map(line_json).collect::<Vec<_>>().into(),
    );
    Json::Object(m)
}

fn line_json(l: &Line) -> Json {
    let op = match l.op {
        Op::Leaf => "set",
        Op::Add => "add",
        Op::Remove => "remove",
    };
    let mut m = serde_json::Map::new();
    m.insert("op".into(), json!(op));
    m.insert("path".into(), json!(l.path));
    m.insert("before".into(), l.before.json());
    m.insert("after".into(), l.after.json());
    if !l.leaves.is_empty() {
        m.insert(
            "leaves".into(),
            l.leaves.iter().map(line_json).collect::<Vec<_>>().into(),
        );
    }
    Json::Object(m)
}

fn diag_json(d: &Diag) -> Json {
    json!({
        "address": d.addr.attr(&d.path),
        "type": d.addr.typ,
        "name": d.addr.name,
        "path": d.path,
        "reason": d.reason,
        "rank": d.rank,
        "witnesses": d.witnesses.iter().map(|(r, v, from)| json!({
            "rank": r,
            "value": v.json(),
            "from": from,
        })).collect::<Vec<_>>(),
    })
}

fn write_deformation(out: &mut String, d: &Deformation, style: Style) {
    let note = match d.kind {
        ActionKind::Drift => {
            "  (drift: a fresh null where the world has a value; its identity is stale)"
        }
        ActionKind::DeleteDeposed => "  (deposed)",
        ActionKind::Replace { .. } => "  (replace)",
        _ => "",
    };
    out.push_str(&format!(
        "{} {}{note}\n",
        style.marker(&d.kind),
        style.paint(Paint::Bold, &d.addr.to_string())
    ));
    // Keep plan output readable.
    let max = 40usize;
    for (i, l) in d.lines.iter().enumerate() {
        if i == max {
            out.push_str(&format!("  ... ({} more changes)\n", d.lines.len() - max));
            break;
        }
        write_line(out, &d.kind, l, "  ", style);
    }
}

fn write_line(out: &mut String, kind: &ActionKind, l: &Line, indent: &str, style: Style) {
    let shown = |v: &Shown| style.shown(v);
    match l.op {
        Op::Add | Op::Remove => {
            let (sign, v) = if l.op == Op::Add {
                (style.paint(Paint::Create, "+"), &l.after)
            } else {
                (style.paint(Paint::Delete, "-"), &l.before)
            };
            if l.leaves.is_empty() {
                out.push_str(&format!("{indent}{sign} {} = {}\n", l.path, shown(v)));
                return;
            }
            out.push_str(&format!("{indent}{sign} {}\n", l.path));
            let inner = if l.op == Op::Add {
                ActionKind::Create
            } else {
                ActionKind::Delete
            };
            for x in &l.leaves {
                write_line(out, &inner, x, &format!("{indent}    "), style);
            }
        }
        Op::Leaf => match kind {
            ActionKind::Create | ActionKind::Adopt => {
                out.push_str(&format!("{indent}{} = {}\n", l.path, shown(&l.after)))
            }
            ActionKind::Delete | ActionKind::DeleteDeposed => {
                out.push_str(&format!("{indent}{} was {}\n", l.path, shown(&l.before)))
            }
            ActionKind::Update
            | ActionKind::Drift
            | ActionKind::Pending
            | ActionKind::Replace { .. } => out.push_str(&format!(
                "{indent}{}: {} -> {}\n",
                l.path,
                shown(&l.before),
                shown(&l.after)
            )),
            ActionKind::Noop => {}
        },
    }
}

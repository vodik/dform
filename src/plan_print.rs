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
//! Every value passes through [`shown`]: a null prints as its label, a
//! secret or a value at a sensitive path as `(sensitive LABEL)`. Nothing
//! here formats a sensitive value's bytes. `show` and `query` redact
//! through [`redact_value`] and [`redact_fact`], which use the same rule.

use crate::ast::{Atom, Lit, Program, Stmt, Term};
use crate::engine::EvalResult;
use crate::ir::Address;
use crate::partition::{fmt_atom, fmt_value};
use crate::provider::{Action, ActionKind, Change, NULL_KEY, Plan, marker};
use crate::schema::Schema;
use crate::stuck::{self, Known, Sections, Stuck};
use crate::value::{Value, null_owner};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};

/// One side of a change, after redaction.
#[derive(Debug, Clone, PartialEq)]
pub enum Shown {
    Absent,
    Value(Json),
    Null {
        label: String,
        class: String,
    },
    /// A secret (with its label) or a value at a sensitive path (none).
    Sensitive(Option<String>),
}

/// Redact one side of a change: a null or secret marker is its label; any
/// other value at a sensitive path is `(sensitive)`.
pub fn shown(v: Option<&Json>, sensitive: bool, schema: &Schema) -> Shown {
    let Some(v) = v else {
        return Shown::Absent;
    };
    match marker(v) {
        Some((NULL_KEY, l)) => Shown::Null {
            label: l.to_string(),
            class: null_class(l, schema),
        },
        Some((_, l)) => Shown::Sensitive(Some(l.to_string())),
        None if sensitive || has_secret(v) => Shown::Sensitive(None),
        None => Shown::Value(v.clone()),
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

/// Redact a document (a resource's attributes, as `show` prints them):
/// every value at a sensitive path, and every secret null, becomes
/// `{"sensitive": LABEL}`; every other null `{"null": LABEL, "class": C}`.
pub fn redact_value(typ: &str, v: &Value, schema: &Schema) -> Json {
    redact_at(typ, "", v, schema)
}

fn redact_at(typ: &str, path: &str, v: &Value, schema: &Schema) -> Json {
    if let Value::Null { label, class, .. } = v {
        return if *class == crate::value::NullClass::Secret {
            json!({ "sensitive": label })
        } else {
            json!({"null": label, "class": class.name()})
        };
    }
    if !path.is_empty() && schema.is_sensitive(typ, path) {
        return json!({ "sensitive": Json::Null });
    }
    let join = |k: &str| {
        if path.is_empty() {
            k.to_string()
        } else {
            format!("{path}.{k}")
        }
    };
    match v {
        Value::Obj(m) => Json::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), redact_at(typ, &join(k), x, schema)))
                .collect(),
        ),
        // A list element's schema path is the list's path.
        Value::List(xs) => {
            Json::Array(xs.iter().map(|x| redact_at(typ, path, x, schema)).collect())
        }
        Value::Str(s) => Json::String(s.clone()),
        Value::Int(i) => json!(i),
        Value::Bool(b) => json!(b),
        other => Json::String(fmt_value(other)),
    }
}

/// A fact as `query` prints it: an attribute fact (`arg/5`, `attr/4`)
/// whose path is sensitive has its value replaced, as does every secret
/// null anywhere in the tuple.
pub fn redact_fact(a: &Atom, schema: &Schema) -> Vec<Json> {
    let sensitive_at = match (a.pred.as_str(), a.args.as_slice()) {
        ("arg" | "attr" | "attr_conflict" | "attr_stuck", [t, _, p, ..]) => match (t, p) {
            (Term::Val(Value::Str(t)), Term::Val(Value::Str(p))) if schema.is_sensitive(t, p) => {
                Some(3)
            }
            _ => None,
        },
        _ => None,
    };
    a.args
        .iter()
        .enumerate()
        .map(|(i, t)| match t {
            _ if Some(i) == sensitive_at => json!({ "sensitive": Json::Null }),
            Term::Val(v) => redact_at("", "", v, schema),
            other => Json::String(crate::partition::fmt_term(other)),
        })
        .collect()
}

/// A fact in `pred(a, b, ...)` form, redacted.
pub fn fact_text(a: &Atom, schema: &Schema) -> String {
    let args: Vec<String> = redact_fact(a, schema).iter().map(json_text).collect();
    format!("{}({})", a.pred, args.join(", "))
}

/// A redacted JSON value in the plan's spelling: nulls as `?label`,
/// sensitive values as `(sensitive LABEL)`.
fn json_text(v: &Json) -> String {
    if let Json::Object(m) = v {
        if m.len() == 1
            && let Some(l) = m.get("sensitive")
        {
            return match l.as_str() {
                Some(l) => format!("(sensitive {l})"),
                None => "(sensitive)".into(),
            };
        }
        if m.len() == 2
            && let (Some(Json::String(l)), Some(_)) = (m.get("null"), m.get("class"))
        {
            return format!("?{l}");
        }
        let kv: Vec<String> = m
            .iter()
            .map(|(k, x)| format!("{k}: {}", json_text(x)))
            .collect();
        return format!("{{{}}}", kv.join(", "));
    }
    match v {
        Json::String(s) => format!("\"{s}\""),
        Json::Array(xs) => format!(
            "[{}]",
            xs.iter().map(json_text).collect::<Vec<_>>().join(", ")
        ),
        other => other.to_string(),
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
    let conflicts = diags(
        i.res,
        i.schema,
        "deny",
        "conflicting attribute contributions",
    );
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
            .push(deformation(a, i.schema));
    }
    let pending: Vec<PendingBlock> = by_nulls
        .into_iter()
        .map(|(on, deformations)| PendingBlock {
            resolves_after: resolves(&on, &tick_of),
            on,
            deformations,
        })
        .collect();

    let groups = groups(&i.res.stuck, &tick_of, &resolves);
    let policies = policies(i, &tick_of, &resolves);

    let mut ticks: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut unscheduled = Vec::new();
    for a in definite.iter().chain(&held) {
        if matches!(a.kind, ActionKind::Noop) {
            continue;
        }
        let name = format!("{}.{}", a.addr.typ, a.addr.name);
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
                .push(format!("{}.{} (deposed)", a.addr.typ, a.addr.name));
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
        && i.sections.undetermined.is_empty();
    Report {
        stack: i.stack.to_string(),
        show_noop: i.show_noop,
        definite: definite
            .iter()
            .filter(|a| i.show_noop || !matches!(a.kind, ActionKind::Noop))
            .map(|a| deformation(a, i.schema))
            .collect(),
        noops,
        pending,
        groups,
        policies,
        shadowed: diags(
            i.res,
            i.schema,
            "warn",
            "attr_shadowed: contributions at a losing rank disagree and are overridden",
        ),
        conflicts,
        ticks: ticks.into_iter().collect(),
        unscheduled,
        undeformed,
        moved: i.moved.to_vec(),
        denies: i.denies.to_vec(),
    }
}

type Resolves<'a> = dyn Fn(&[String], &BTreeMap<(String, String), usize>) -> Option<usize> + 'a;

fn nulls(s: &Stuck) -> Vec<String> {
    s.nulls.iter().cloned().collect()
}

/// Stuck resource rules: a group of unknown cardinality.
fn groups(
    stuck: &[Stuck],
    tick_of: &BTreeMap<(String, String), usize>,
    resolves: &Resolves,
) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    for s in stuck.iter().filter(|s| s.head.pred == "want") {
        let pattern = match s.head.args.as_slice() {
            [Term::Val(Value::Str(t)), a] => {
                let a = match a {
                    Term::Val(v) => fmt_value(v).trim_matches('"').to_string(),
                    _ => "?".into(),
                };
                format!("{t}.{a}")
            }
            _ => fmt_atom(&s.head),
        };
        let on = nulls(s);
        let g = Group {
            pattern,
            resolves_after: resolves(&on, tick_of),
            on,
            reason: s.reason.clone(),
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
        };
        if !out
            .iter()
            .any(|x| x.message == p.message && x.on == p.on && x.reason == p.reason)
        {
            out.push(p);
        }
    }
    out.sort_by(|a, b| (&a.message, &a.on).cmp(&(&b.message, &b.on)));

    let mut known = Known::default();
    for s in &i.res.stuck {
        known.add(s);
    }
    let Ok(lowered) = crate::transform::lower(i.program) else {
        return out;
    };
    let denies = lowered.program.statements.iter().filter_map(|s| match s {
        Stmt::Rule(r) if r.head.pred == "deny" => Some((deny_message(&r.head), &r.body)),
        Stmt::Constraint(c) => Some((c.message.clone(), &c.body)),
        _ => None,
    });
    let mut may: Vec<Policy> = Vec::new();
    for (message, body) in denies {
        if out.iter().any(|p| p.message == message) {
            continue;
        }
        let mut on = BTreeSet::new();
        let mut reads = Vec::new();
        for l in body {
            let Lit::Pos(a) = l else { continue };
            let pat = stuck::as_read(a);
            for (head, nulls) in known.matching(&pat) {
                on.extend(nulls);
                reads.push(fmt_atom(&head));
            }
        }
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
        });
    }
    may.sort_by(|a, b| a.message.cmp(&b.message));
    may.dedup_by(|a, b| a.message == b.message);
    out.extend(may);
    out
}

/// Conflicts (the aggregate's deny) or shadowed disagreements (its warn),
/// from the policy facts it derives, with every witness.
fn diags(res: &EvalResult, schema: &Schema, pred: &str, msg: &str) -> Vec<Diag> {
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
        let sensitive = schema.is_sensitive(&typ, &path);
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
                        _ if sensitive => Shown::Sensitive(None),
                        Some(v) => value_shown(v, schema),
                        None => Shown::Absent,
                    };
                    // The contributing rule's text spells a literal
                    // value: withheld at a sensitive path.
                    let from = match w.get("from") {
                        Some(Value::List(fs)) if !sensitive => fs
                            .iter()
                            .filter_map(|f| f.as_str().map(str::to_string))
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

fn value_shown(v: &Value, schema: &Schema) -> Shown {
    match v {
        Value::Null { label, class, .. } if *class == crate::value::NullClass::Secret => {
            Shown::Sensitive(Some(label.clone()))
        }
        Value::Null { label, class, .. } => Shown::Null {
            label: label.clone(),
            class: class.name().into(),
        },
        other => match redact_at("", "", other, schema) {
            j if has_sensitive(&j) => Shown::Sensitive(None),
            j => Shown::Value(j),
        },
    }
}

fn has_sensitive(v: &Json) -> bool {
    match v {
        Json::Object(m) if m.len() == 1 && m.contains_key("sensitive") => true,
        Json::Object(m) => m.values().any(has_sensitive),
        Json::Array(xs) => xs.iter().any(has_sensitive),
        _ => false,
    }
}

/// An action's change lines. An update diffs a keyless set, or a list
/// with merge keys, by element: an element that is new or gone is one
/// `+`/`-` line with its leaves. A keyless set's element is labeled by a
/// hash of its content (`[#k3j2d]`); it prints as `[]` in an update and
/// by position everywhere else.
fn deformation(a: &Action, schema: &Schema) -> Deformation {
    let paths = relabel(a.changes.iter().map(|c| c.path.as_str()));
    let leaf = |c: &Change, path: String| Line {
        op: Op::Leaf,
        path,
        before: shown(c.before.as_ref(), c.sensitive, schema),
        after: shown(c.after.as_ref(), c.sensitive, schema),
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

fn nulls_text(on: &[String]) -> String {
    on.iter()
        .map(|n| format!("?{n}"))
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

    /// `plan: 3 deformations (2 create, 1 update), 5 pending, 2 undetermined`
    pub fn summary(&self) -> String {
        let deformations: Vec<&Deformation> = self
            .definite
            .iter()
            .filter(|d| !matches!(d.kind, ActionKind::Noop))
            .collect();
        let mut kinds: Vec<(&str, usize)> = Vec::new();
        for k in ["create", "update", "replace", "drift", "delete", "adopt"] {
            let n = deformations
                .iter()
                .filter(|d| kind_name(&d.kind) == k)
                .count();
            if n > 0 {
                kinds.push((k, n));
            }
        }
        let n = deformations.len();
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

    pub fn text(&self) -> String {
        let mut out = moved_text(&self.moved);
        if self.undeformed && !self.show_noop {
            out.push_str(&format!("stack {} is undeformed\n", self.stack));
            return out;
        }
        out.push_str(&self.summary());
        out.push('\n');
        if !self.definite.is_empty() {
            out.push_str("definite:\n");
            for d in &self.definite {
                write_deformation(&mut out, d);
            }
        }
        for b in &self.pending {
            let after = b
                .resolves_after
                .map(|t| format!(" (resolves after tick {t})"))
                .unwrap_or_default();
            out.push_str(&format!("pending on {}{after}:\n", nulls_text(&b.on)));
            for d in &b.deformations {
                write_deformation(&mut out, d);
            }
        }
        if !self.groups.is_empty() {
            out.push_str("pending groups:\n");
            for g in &self.groups {
                let after = g
                    .resolves_after
                    .map(|t| format!(", resolves after tick {t}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "? {} x unknown, on {}{after}  ({})\n",
                    g.pattern,
                    nulls_text(&g.on),
                    g.reason
                ));
            }
        }
        if !self.policies.is_empty() {
            out.push_str("undetermined:\n");
            for p in &self.policies {
                let when = match (p.may_derive, p.after) {
                    (false, Some(t)) => format!(", decided after tick {t}"),
                    (true, Some(t)) => format!(", may derive after tick {t}"),
                    (true, None) => ", may derive after a boundary".into(),
                    (false, None) => String::new(),
                };
                out.push_str(&format!(
                    "? deny \"{}\" on {}{when}  ({})\n",
                    p.message,
                    nulls_text(&p.on),
                    p.reason
                ));
            }
        }
        for (title, ds) in [("shadowed", &self.shadowed), ("conflicts", &self.conflicts)] {
            if ds.is_empty() {
                continue;
            }
            out.push_str(&format!("{title}:\n"));
            for d in ds {
                let rank = d
                    .rank
                    .as_ref()
                    .map(|r| format!(" at rank {r}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "! {}.{} {}{rank}: {}\n",
                    d.addr.typ, d.addr.name, d.path, d.reason
                ));
                for (r, v, from) in &d.witnesses {
                    let from = if from.is_empty() {
                        String::new()
                    } else {
                        format!("  from {}", from.join("; "))
                    };
                    out.push_str(&format!("    {r} {}{from}\n", v.text()));
                }
            }
        }
        if !self.denies.is_empty() {
            out.push_str("denied:\n");
            for d in &self.denies {
                out.push_str(&format!("! {d}\n"));
            }
        }
        if !self.ticks.is_empty() || !self.unscheduled.is_empty() {
            let mut parts: Vec<String> = self
                .ticks
                .iter()
                .map(|(t, xs)| format!("tick {t} [{}]", xs.join(" ")))
                .collect();
            if !self.unscheduled.is_empty() {
                parts.push(format!("unscheduled [{}]", self.unscheduled.join(" ")));
            }
            out.push_str(&format!("apply order: {}\n", parts.join(" ")));
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
        .map(|(old, new)| {
            format!(
                "moved {}.{} -> {}.{}\n",
                old.typ, old.name, new.typ, new.name
            )
        })
        .collect()
}

fn write_deformation(out: &mut String, d: &Deformation) {
    let note = match d.kind {
        ActionKind::Drift => {
            "  (drift: a fresh null where the world has a value; its identity is stale)"
        }
        ActionKind::DeleteDeposed => "  (deposed)",
        ActionKind::Replace { .. } => "  (replace)",
        _ => "",
    };
    out.push_str(&format!(
        "{} {}.{}{note}\n",
        marker_of(&d.kind),
        d.addr.typ,
        d.addr.name
    ));
    // Keep plan output readable.
    let max = 40usize;
    for (i, l) in d.lines.iter().enumerate() {
        if i == max {
            out.push_str(&format!("  ... ({} more changes)\n", d.lines.len() - max));
            break;
        }
        write_line(out, &d.kind, l, "  ");
    }
}

fn write_line(out: &mut String, kind: &ActionKind, l: &Line, indent: &str) {
    match l.op {
        Op::Add | Op::Remove => {
            let (sign, v) = if l.op == Op::Add {
                ("+", &l.after)
            } else {
                ("-", &l.before)
            };
            if l.leaves.is_empty() {
                out.push_str(&format!("{indent}{sign} {} = {}\n", l.path, v.text()));
                return;
            }
            out.push_str(&format!("{indent}{sign} {}\n", l.path));
            let inner = if l.op == Op::Add {
                ActionKind::Create
            } else {
                ActionKind::Delete
            };
            for x in &l.leaves {
                write_line(out, &inner, x, &format!("{indent}    "));
            }
        }
        Op::Leaf => match kind {
            ActionKind::Create | ActionKind::Adopt => {
                out.push_str(&format!("{indent}{} = {}\n", l.path, l.after.text()))
            }
            ActionKind::Delete | ActionKind::DeleteDeposed => {
                out.push_str(&format!("{indent}{} was {}\n", l.path, l.before.text()))
            }
            ActionKind::Update
            | ActionKind::Drift
            | ActionKind::Pending
            | ActionKind::Replace { .. } => out.push_str(&format!(
                "{indent}{}: {} -> {}\n",
                l.path,
                l.before.text(),
                l.after.text()
            )),
            ActionKind::Noop => {}
        },
    }
}

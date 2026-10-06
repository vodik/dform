//! The tool's three printers (R-63); every command prints through one:
//! a result set ([`table`]: rows under a header line, `query`, `output`,
//! the listings), a derivation ([`tree`]: what `why` prints), or a report
//! (this module: `plan`, `apply` and `diff`, which compose the two).
//!
//! The plan report (proposal E §2.7, §7.4; F DR-2 revised): one report
//! built from an evaluation and the provider's plan, rendered as text for
//! `plan` and every `apply` tick, or as one JSON document for `--json`.
//!
//! Grouped by tick (R-79), the only grouping: tick 1, what applies now;
//! each later tick with the values it waits on; `later`, the rules that
//! may derive an unknown number, the denies and checks undetermined until
//! a tick, and held changes this plan does not schedule; the denies, the
//! changes held for approval, shadowed disagreements and conflicts; then
//! what apply does. Each change says where it is derived, each attribute
//! where its value was written, and why it changed since the last apply,
//! as much as [`Why`] asks. `--why=none` is the layout before R-79
//! ([`bare`]).
//!
//! Every value passes through [`shown`] or [`shown_value`], which ask the
//! one [`Redactor`] that `query`, `why`, `graph` and `show` print through: a
//! null prints as its label, a secret, a value equal to one, or a value at
//! a sensitive path as `(sensitive LABEL)`. Nothing here formats a
//! sensitive value's bytes.

use crate::ast::{Atom, Program, RuleStmt, Term};
use crate::engine::EvalResult;
use crate::ir::Address;
use crate::partition::{fmt_atom, fmt_bare, fmt_value};
use crate::provider::{Action, ActionKind, Change, NULL_KEY, Plan, json_to_value, marker};
use crate::query::Redactor;
use crate::schema::Schema;
use crate::stuck::{Sections, Stuck};
use crate::value::{Value, null_owner};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use tree::{Because, Site};

mod bare;
pub mod table;
pub mod tree;

/// A resource's address as the plan, `why`, `query` and the editor print
/// it (R-111): its type, then its path the way the source names it,
/// `ovh.ssh_key k3s.admin`. The full `T["k3s.admin"]` is for the plan
/// file, `--json`, state, and an argument.
pub fn address(a: &Address) -> String {
    format!("{} {}", a.typ, path(&a.name))
}

/// A stored name as the source names it (R-112): its path, a copy's scope
/// before it (`k3s.admin`), a segment holding a dot already quoted
/// (`k3s."k8s-lab.vodik.xyz"`); a segment holding a space quoted too, and
/// an empty name `""`, so the printed path reads back as one.
pub fn path(name: &str) -> String {
    if name.is_empty() {
        return "\"\"".into();
    }
    crate::ir::path_segments(name)
        .into_iter()
        .map(|seg| {
            let bare = !seg.starts_with('"')
                && seg.contains(|c: char| c.is_whitespace() || c.is_control());
            match bare {
                true => crate::ir::string_literal(seg),
                false => seg.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// A reference to resource `a`, or to its attribute `attr`, as a value
/// prints: its path, `k3s.server`, `k3s.server.public_ip`.
pub fn reference(a: &Address, attr: &str) -> String {
    format!("{}{}", path(&a.name), crate::ir::path_suffix(attr))
}

/// A resource's attribute as a diagnostic names it: `T k3s.server.p`;
/// an input's by its path, `input db.backup_days`.
pub fn attribute(a: &Address, attr: &str) -> String {
    match a.name.is_empty() && !attr.is_empty() {
        true => format!("{} {attr}", a.typ),
        false => format!("{} {}", a.typ, reference(a, attr)),
    }
}

/// A null's or a secret's label `T/A#P` as a diagnostic names what it
/// stands for: [`attribute`], `T k3s.server.public_ip`; a resource's
/// identity [`address`]; an input's or an output's as
/// [`crate::ir::label`] says it.
pub fn attribute_label(l: &str) -> String {
    if let Some(call) = extern_label(l) {
        return call;
    }
    match crate::value::null_parts(l) {
        Some((typ, name, p)) if !name.is_empty() && typ != crate::transform::OUTPUT => {
            let a = Address { typ, name };
            match p == crate::schema::IDENTITY {
                true => address(&a),
                false => attribute(&a, &p),
            }
        }
        _ => crate::ir::label(l),
    }
}

/// A null's or a secret's label `T/A#P` as the value it stands for
/// (R-111): the reference it is, `k3s.server.public_ip`; a resource's
/// identity the resource, `k3s.server`; an input's or an output's as
/// [`crate::ir::label`] says it.
pub fn label(l: &str) -> String {
    if let Some(call) = extern_label(l) {
        return call;
    }
    match crate::value::null_parts(l) {
        Some((typ, name, p)) if !name.is_empty() && typ != crate::transform::OUTPUT => {
            let a = Address { typ, name };
            match p == crate::schema::IDENTITY {
                true => reference(&a, ""),
                false => reference(&a, &p),
            }
        }
        _ => crate::ir::label(l),
    }
}

/// The label of an answer of dform's own extern (`ssh.read/INPUTS#N`,
/// what a host still booting has "not yet" said) as its call:
/// `ssh.read("127.0.0.1:22", "ubuntu", "/etc/k3s.yaml")`. Its inputs are
/// no address, so never split at their dots.
fn extern_label(l: &str) -> Option<String> {
    let (pred, inputs, col) = crate::value::null_parts(l)?;
    crate::externs::is_call_label(&pred, &col).then(|| crate::externs::call_text(&pred, &inputs))
}

/// A label as [`crate::ir::label`] printed it (`T["A"].p`), when it is an
/// extern call's ([`extern_label`]).
fn printed_call(l: &str) -> Option<String> {
    let (typ, rest) = l.split_once('[')?;
    let (inputs, col) = rest.rsplit_once("].")?;
    let inputs = crate::syntax::resolve::unescape(inputs).ok()?;
    let col = col.trim_matches('"');
    crate::externs::is_call_label(typ, col).then(|| crate::externs::call_text(typ, &inputs))
}

/// A label as [`crate::ir::label`] printed it (`T["A"].p`), as
/// [`label`] prints it.
fn printed_label(l: &str) -> String {
    if let Some(call) = printed_call(l) {
        return call;
    }
    match crate::ir::parse_address(l) {
        Ok((a, p)) => reference(&a, p.as_deref().unwrap_or_default()),
        Err(_) => l.to_string(),
    }
}

/// A label as [`crate::ir::label`] printed it (`T["A"].p`), as
/// [`attribute_label`] prints it; a call (`random.password("db")`) as
/// itself.
fn printed_attribute(l: &str) -> String {
    if let Some(call) = printed_call(l) {
        return call;
    }
    match crate::ir::parse_address(l) {
        Ok((a, p)) => attribute(&a, p.as_deref().unwrap_or_default()),
        Err(_) => l.to_string(),
    }
}

/// An address the report holds as text (`T["A"]`, a pending group's
/// `T[?]` or `T["name-${x}"]`) as [`address`] prints it: `T ?` for an
/// unknown number, a template as the statement writes it.
pub fn address_text(s: &str) -> String {
    if let Ok(a) = crate::ir::parse_resource_address(s) {
        return address(&a);
    }
    match s.split_once('[') {
        Some((t, rest)) if rest.ends_with(']') && !t.is_empty() => {
            format!("{t} {}", &rest[..rest.len() - 1])
        }
        _ => s.to_string(),
    }
}

/// Past this many characters a string elides its middle at the default
/// level (R-111): a public key, a digest.
const LONG: usize = 60;

/// `s` with its middle elided when it is longer than [`LONG`].
fn elide(s: &str) -> String {
    let n = s.chars().count();
    if n <= LONG {
        return s.to_string();
    }
    let (head, tail) = (LONG / 2, LONG - 1 - LONG / 2);
    let head: String = s.chars().take(head).collect();
    let tail: String = s.chars().skip(n - tail).collect();
    format!("{head}…{tail}")
}

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
    /// A reference whose resource exists: the resource's address, and the
    /// id the provider resolved it to (`--json` shows the id; R-43).
    Ref {
        addr: Address,
        value: Json,
    },
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
        // At a sensitive path, a derived secret by the call that derived
        // it (`random.password("db")`), anything else bare.
        None if sensitive || has_secret(v) => {
            Shown::Sensitive(r.derived(&json_to_value(v)).map(str::to_string))
        }
        None => match secret_in(&r.json(&json_to_value(v))) {
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
            Shown::Value(Json::String(s)) => crate::partition::quote(s),
            Shown::Value(v) => serde_json::to_string(v).unwrap_or_else(|_| "<unprintable>".into()),
            Shown::Null { label, .. } => format!("?{label}"),
            Shown::Sensitive(Some(l)) => format!("(sensitive {l})"),
            Shown::Sensitive(None) => "(sensitive)".into(),
            Shown::Ref { addr, .. } => addr.to_string(),
        }
    }

    /// The value as the plan prints it at level `why` (R-111): a reference
    /// or a value not known yet as the reference it is (`k3s.server`,
    /// `k3s.server.public_ip`), with no `?`; a secret `(sensitive)`, by its
    /// label from `-v`; a long string elided in the middle at the default
    /// level. `-q` prints [`Shown::text`].
    pub fn said(&self, why: Why) -> String {
        match self {
            Shown::Null { label: l, .. } => printed_label(l),
            Shown::Sensitive(Some(l)) if why >= Why::How => {
                format!("(sensitive {})", printed_attribute(l))
            }
            Shown::Sensitive(_) => "(sensitive)".into(),
            Shown::Ref { addr, .. } => reference(addr, ""),
            Shown::Value(Json::String(s)) if why == Why::Line => crate::partition::quote(&elide(s)),
            _ => self.text(),
        }
    }

    pub fn json(&self) -> Json {
        match self {
            Shown::Absent => Json::Null,
            Shown::Value(v) => v.clone(),
            Shown::Null { label, class } => json!({"null": label, "class": class}),
            Shown::Sensitive(l) => json!({ "sensitive": l }),
            Shown::Ref { value, .. } => value.clone(),
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
    /// The site column, `(sensitive)`: dim (R-111).
    Dim,
    /// `because`: cyan.
    Because,
    /// `held for approval`: magenta.
    Held,
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
            Paint::Warn => "33",
            Paint::Bold => "1",
            Paint::Done => "1;32",
            Paint::Dim => "2",
            Paint::Because => "36",
            Paint::Held => "35",
        };
        format!("\x1b[{sgr}m{s}\x1b[0m")
    }

    /// An action's marker in its kind's colour.
    fn marker(&self, k: &ActionKind) -> String {
        match kind_paint(k) {
            Some(p) => self.paint(p, marker_of(k)),
            None => marker_of(k).to_string(),
        }
    }

    /// A change's address, bold in its kind's colour (R-111).
    fn address(&self, k: &ActionKind, s: &str) -> String {
        if !self.color || s.is_empty() {
            return s.to_string();
        }
        let sgr = match kind_paint(k) {
            Some(Paint::Create) => "1;32",
            Some(Paint::Update) => "1;33",
            Some(Paint::Delete) => "1;31",
            Some(_) => "1;35",
            None => "1",
        };
        format!("\x1b[{sgr}m{s}\x1b[0m")
    }

    /// A value as the plan prints it at `why`: `(sensitive)` dim.
    fn said(&self, v: &Shown, why: Why) -> String {
        match v {
            Shown::Sensitive(_) => self.paint(Paint::Dim, &v.said(why)),
            _ => v.said(why),
        }
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

/// The colour of a change of kind `k`: `+` green, `~` yellow, `±`
/// magenta, `-` red.
fn kind_paint(k: &ActionKind) -> Option<Paint> {
    match k {
        ActionKind::Create | ActionKind::Adopt => Some(Paint::Create),
        ActionKind::Update | ActionKind::Drift | ActionKind::Pending => Some(Paint::Update),
        ActionKind::Delete | ActionKind::DeleteDeposed => Some(Paint::Delete),
        ActionKind::Replace { .. } => Some(Paint::Replace),
        ActionKind::Noop => None,
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
    /// Where its value was written (R-79, [`Report::explain`]).
    pub site: Option<Site>,
}

#[derive(Debug, Clone)]
pub struct Deformation {
    pub kind: ActionKind,
    pub addr: Address,
    pub lines: Vec<Line>,
    /// Why it is planned (`plan --why=full`, [`Report::explain`]).
    pub why: Vec<Because>,
    /// Where it is derived (R-79): its `want`'s site; a delete's, where
    /// the last apply derived it.
    pub site: Option<Site>,
    /// The leaf that changed since the last apply ([`Report::because`]).
    pub because: Option<String>,
    /// A replace: the changed paths the schema declares immutable.
    pub forces: Vec<String>,
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
    /// The rule (`r12`), with what its stuck instance binds.
    pub rule: Option<String>,
    pub bindings: Vec<(String, Value)>,
    /// What it reads that may derive after a boundary, as the body reads
    /// it (`release("crud_api", "schema", _)`).
    pub reads: Option<String>,
    /// Where the rule is written ([`Report::explain`]).
    pub site: Option<Site>,
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
    /// The deny's rule (`r12`), and where it is written.
    pub rule: Option<String>,
    pub site: Option<Site>,
}

/// A deny over the plan, the row of the plan it matched and where it is
/// written ([`Report::explain`]).
#[derive(Debug, Clone)]
pub struct Denied {
    pub message: String,
    /// The resource of the `deformation` row it read, when it read one.
    pub addr: String,
    pub site: Option<Site>,
}

/// A change held for an approval (`requires_approval(r, reason)`).
#[derive(Debug, Clone)]
pub struct Approval {
    /// The resource, as the plan prints its address.
    pub addr: String,
    pub reason: String,
    pub site: Option<Site>,
}

/// How much of why each change is planned the report says (R-79, R-111):
/// a ladder, `-q` to `-vv`. `None` (`-q`): the bare diff, addresses and
/// values. `Line` (the default): each change where it is derived, each
/// value written outside its own block where, and the leaf that changed
/// since the last apply. `How` (`-v`): also how, the deriving
/// statement's bindings, the expression behind a value, the writes that
/// lost with their ranks. `Full` (`-vv`): also the derivation, compressed
/// to its leaves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Why {
    None,
    #[default]
    Line,
    How,
    Full,
}

impl Why {
    /// The level `-q` and `-v`/`-vv` ask for, else `why` (`--why=LEVEL`).
    pub fn of(quiet: bool, verbose: u8, why: Why) -> Why {
        match (quiet, verbose) {
            (true, _) => Why::None,
            (_, 0) => why,
            (_, 1) => Why::How,
            _ => Why::Full,
        }
    }
}

impl std::str::FromStr for Why {
    type Err = String;
    fn from_str(s: &str) -> Result<Why, String> {
        match s {
            "none" => Ok(Why::None),
            "line" => Ok(Why::Line),
            "how" => Ok(Why::How),
            "full" => Ok(Why::Full),
            _ => Err(format!("expected none, line, how or full, got {s:?}")),
        }
    }
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
    /// Each of `denies`, as the plan's own rows say it ([`Report::explain`]).
    pub denied: Vec<Denied>,
    /// Every null the sections name, with its class, for `--json`.
    pub classes: BTreeMap<String, String>,
    /// The tick this report's definite changes run in (1 for `plan`).
    pub tick: usize,
    /// What each change says of why it is planned ([`Report::explain`]).
    pub why: Why,
    /// The changes held for an approval.
    pub approvals: Vec<Approval>,
    /// The program's copies: a copy's deformations print under it, a
    /// composite resource (R-67).
    pub instances: crate::zset::Instances,
    /// What the plan empties since the last apply (R-80): a rule all of
    /// whose resources it deletes, a relation it leaves with no rows.
    pub warnings: Vec<crate::zset::Emptied>,
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
    /// The copies state remembers (`state::State::instances`): a removed
    /// copy's deletes print under it.
    pub kept: &'a BTreeMap<String, String>,
}

/// The deny that names an attribute whose contributions conflict.
pub const CONFLICT: &str = "conflicting attribute contributions";

/// The resources an attribute of which conflicts: the report shows their
/// conflicts in place of any deformation of theirs, and the provider is
/// not asked to plan them (its refusal of the document the conflict left
/// incomplete would hide the conflict).
pub fn conflicted(res: &EvalResult) -> BTreeSet<Address> {
    diags(res, &Redactor::default(), "deny", CONFLICT)
        .into_iter()
        .map(|d| d.addr)
        .collect()
}

pub fn report(i: &Input) -> Report {
    let r = Redactor::new(&i.res.facts, i.schema);
    let refs = Refs::new(&i.res.facts);
    let mut conflicts = diags(i.res, &r, "deny", CONFLICT);
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
            .push(deformation(a, i.schema, &r, &refs));
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
            .map(|a| deformation(a, i.schema, &r, &refs))
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
        denied: Vec::new(),
        classes,
        tick: i.tick,
        why: Why::None,
        approvals: crate::approval::needs(&i.res.facts)
            .into_iter()
            .map(|(addr, reason)| Approval {
                addr,
                reason,
                site: None,
            })
            .collect(),
        instances: crate::zset::Instances::from_facts(&i.res.facts).with(i.kept),
        warnings: Vec::new(),
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
            let reads = crate::modules::private_text(&m.reads, &fmt_atom, " ")
                .unwrap_or_else(|| fmt_atom(&m.reads));
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
            rule: s.rule.map(|r| format!("r{r}")),
            site: None,
        };
        if !out
            .iter()
            .any(|x| x.message == p.message && x.on == p.on && x.reason == p.reason)
        {
            out.push(p);
        }
    }
    out.sort_by(|a, b| (&a.message, &a.on).cmp(&(&b.message, &b.on)));

    type Found = (BTreeSet<String>, Vec<String>, Option<usize>);
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    for m in i.res.may_derive.iter().filter(|m| m.head.pred == "deny") {
        let message = deny_message(&m.head);
        if out.iter().any(|p| p.message == message) {
            continue;
        }
        let (on, reads, rule) = found.entry(message).or_default();
        on.extend(m.nulls.iter().cloned());
        reads.push(fmt_atom(&m.reads));
        rule.get_or_insert(m.rule);
    }
    let mut may: Vec<Policy> = Vec::new();
    for (message, (on, mut reads, rule)) in found {
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
            rule: rule.map(|r| format!("r{r}")),
            site: None,
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
        let addr = fmt_bare(addr);
        out.push(Policy {
            message: format!(
                "{c} of {}",
                attribute(
                    &Address {
                        typ: t.to_string(),
                        name: addr,
                    },
                    path
                )
            ),
            after: resolves(&on, tick_of),
            on,
            reason: "the value carries a null".into(),
            may_derive: false,
            refinement: true,
            rule: None,
            site: None,
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
fn diag(ctx: &BTreeMap<String, Value>, r: &Redactor) -> Diag {
    let s = |k: &str| match ctx.get(k) {
        Some(Value::Str(s)) => s.clone(),
        Some(v) => fmt_value(v),
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

/// Whether the violation `v` is a conflict the plan's `conflicts`
/// section lists ([`violation_conflict`]).
pub fn is_conflict(v: &str) -> bool {
    v.split_once(" ctx=")
        .is_some_and(|(m, _)| m == CONFLICT || m == crate::refine::VIOLATED)
}

/// A conflict's lines: `! T path.p: reason`, then each witness, its value
/// and where it was written (R-111); at `full` the statement that made it
/// as the engine names it.
fn diag_lines(d: &Diag, why: Why, style: Style, conflict: bool) -> String {
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
        // only when it is another (a refinement, a losing rank).
        let rank = match w.rank.as_str() {
            "normal" | "" => String::new(),
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
        out.push_str(&format!(
            "      {rank}{}{from}\n",
            style.said(&w.value, why)
        ));
    }
    out
}

/// An action's change lines. An update diffs a keyless set, or a list
/// with merge keys, by element: an element that is new or gone is one
/// `+`/`-` line with its leaves. A keyless set's element is labeled by a
/// hash of its content (`[#k3j2d]`); it prints as `[]` in an update and
/// by position everywhere else.
fn deformation(a: &Action, schema: &Schema, r: &Redactor, refs: &Refs) -> Deformation {
    let paths = relabel(a.changes.iter().map(|c| c.path.as_str()));
    let leaf = |c: &Change, path: String| Line {
        op: Op::Leaf,
        path,
        before: refs.shown(
            &a.addr,
            &c.path,
            shown(c.before.as_ref(), c.sensitive, schema, r),
        ),
        after: refs.shown(
            &a.addr,
            &c.path,
            shown(c.after.as_ref(), c.sensitive, schema, r),
        ),
        leaves: vec![],
        site: None,
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
            site: None,
        });
    }
    Deformation {
        kind: a.kind.clone(),
        addr: a.addr.clone(),
        lines,
        why: Vec::new(),
        site: None,
        because: None,
        forces: match a.kind {
            ActionKind::Replace { .. } => forces(a, schema),
            _ => Vec::new(),
        },
    }
}

/// The paths of replace `a`'s changes the schema declares `force_new`,
/// dotted, without indices.
fn forces(a: &Action, schema: &Schema) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in &a.changes {
        let p = crate::ir::path_keys(&c.path)
            .iter()
            .map(|k| k.split('[').next().unwrap_or(k).to_string())
            .collect::<Vec<_>>()
            .join(".");
        if schema.forces_new(&a.addr.typ, &p) && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// What prints a resolved reference as its resource (R-43): the world's
/// ids, `identity(T, A, R)` joined with `world_attr(T, R, IDENTITY, V)`, by
/// value, and the desired documents' attributes that hold a reference.
struct Refs<'a> {
    ids: BTreeMap<&'a str, Address>,
    desired: BTreeMap<(&'a str, &'a str, &'a str), &'a Value>,
}

impl<'a> Refs<'a> {
    fn new(facts: &'a BTreeSet<Atom>) -> Refs<'a> {
        let s = |t: &'a Term| match t {
            Term::Val(Value::Str(s)) => Some(s.as_str()),
            _ => None,
        };
        let mut names: BTreeMap<(&str, &str), &str> = BTreeMap::new();
        let mut ids: Vec<(&str, &str, &str)> = Vec::new();
        let mut desired = BTreeMap::new();
        for f in facts {
            match (f.pred.as_str(), f.args.as_slice()) {
                ("identity", [t, a, r]) => {
                    if let (Some(t), Some(a), Some(r)) = (s(t), s(a), s(r)) {
                        names.insert((t, r), a);
                    }
                }
                ("world_attr", [t, r, p, v]) if s(p) == Some(crate::schema::IDENTITY) => {
                    if let (Some(t), Some(r), Some(v)) = (s(t), s(r), s(v)) {
                        ids.push((t, r, v));
                    }
                }
                ("attr", [t, a, p, Term::Val(v)]) if holds_ref(v) => {
                    if let (Some(t), Some(a), Some(p)) = (s(t), s(a), s(p)) {
                        desired.insert((t, a, p), v);
                    }
                }
                _ => {}
            }
        }
        let ids = ids
            .into_iter()
            .filter_map(|(t, r, v)| {
                let name = names.get(&(t, r))?;
                Some((
                    v,
                    Address {
                        typ: t.to_string(),
                        name: name.to_string(),
                    },
                ))
            })
            .collect();
        Refs { ids, desired }
    }

    /// `v`, a side of the change at `path` of `addr`: an id where the
    /// program's document holds a reference prints as that resource.
    fn shown(&self, addr: &Address, path: &str, v: Shown) -> Shown {
        let Shown::Value(Json::String(id)) = &v else {
            return v;
        };
        let top = path.split(['.', '[']).next().unwrap_or(path);
        let at = self
            .desired
            .get(&(addr.typ.as_str(), addr.name.as_str(), top))
            .and_then(|d| walk(d, &path[top.len()..]));
        match (at, self.ids.get(id.as_str())) {
            (Some(Value::Ref { attr, .. }), Some(to)) if attr.is_empty() => Shown::Ref {
                addr: to.clone(),
                value: Json::String(id.clone()),
            },
            _ => v,
        }
    }
}

fn holds_ref(v: &Value) -> bool {
    match v {
        Value::Ref { attr, .. } => attr.is_empty(),
        Value::List(xs) => xs.iter().any(holds_ref),
        Value::Obj(m) => m.values().any(holds_ref),
        _ => false,
    }
}

/// The value at `rest` (`.a.b`, `[2]`, as a change's path goes on) of `v`.
fn walk<'v>(v: &'v Value, rest: &str) -> Option<&'v Value> {
    if rest.is_empty() {
        return Some(v);
    }
    if let Some(r) = rest.strip_prefix('[') {
        let (i, r) = r.split_once(']')?;
        let Value::List(xs) = v else { return None };
        return walk(xs.get(i.parse::<usize>().ok()?)?, r);
    }
    let r = rest.strip_prefix('.')?;
    let end = r.find(['.', '[']).unwrap_or(r.len());
    let Value::Obj(m) = v else { return None };
    walk(m.get(&r[..end])?, &r[end..])
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
    let set = schema.attr(typ, list).is_some_and(|a| a.kind() == "set");
    if !keyed && !set {
        return None;
    }
    Some((
        list.to_string(),
        path[open + 1..close].to_string(),
        path[close + 1..].trim_start_matches('.').to_string(),
    ))
}

/// The mark a deformation of kind `k` has in the plan: `+`, `~`, `-`.
pub fn marker_of(k: &ActionKind) -> &'static str {
    match k {
        ActionKind::Create => "+",
        ActionKind::Adopt => ">",
        ActionKind::Update | ActionKind::Drift | ActionKind::Pending => "~",
        ActionKind::Delete | ActionKind::DeleteDeposed => "-",
        ActionKind::Replace { .. } => "±",
        ActionKind::Noop => "=",
    }
}

/// A deformation's kind as the plan names it: `create`, `update`.
pub fn kind_name(k: &ActionKind) -> &'static str {
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

/// A relation as the source names it (R-111): a copy's or a used
/// module's own by its name there, `vpc_net (in green)`, never the core's
/// `green::vpc_net`.
pub fn relation_name(rel: &str) -> String {
    match rel.rsplit_once("::") {
        Some((scope, p)) => format!("{p} (in {scope})"),
        None => rel.to_string(),
    }
}

/// `n thing`, `n things`.
fn count(n: usize, thing: &str) -> String {
    format!("{n} {thing}{}", if n == 1 { "" } else { "s" })
}

/// The values a tick waits on, as the references they are,
/// `k3s.server.public_ip` (R-111).
fn waited(on: &BTreeSet<String>) -> Vec<String> {
    on.iter()
        .map(|n| match extern_label(n) {
            // What dform's own extern has not answered yet (a host still
            // booting, a file not written yet).
            Some(call) => format!("{call} not yet"),
            None => label(n),
        })
        .collect()
}

/// The page width the right column folds at.
const WIDTH: usize = 100;
/// The right column starts here, unless every left column is narrower.
const COLUMN: usize = 52;

/// One printed line: its text, its visible width, and what may follow it
/// in the right column, the longest that fits first.
struct Row {
    left: String,
    width: usize,
    right: Vec<String>,
}

impl Row {
    fn new(plain: &str, painted: String) -> Row {
        Row {
            left: painted,
            width: plain.chars().count(),
            right: Vec::new(),
        }
    }

    fn plain(s: String) -> Row {
        Row {
            width: s.chars().count(),
            left: s,
            right: Vec::new(),
        }
    }

    fn with(mut self, right: Vec<String>) -> Row {
        self.right = right.into_iter().filter(|r| !r.is_empty()).collect();
        self
    }
}

/// The rows, the right column aligned across them and dim (R-111); a
/// right column that does not fit in [`WIDTH`] folds to a shorter one, or
/// to nothing.
fn layout(rows: &[Row], style: Style) -> String {
    let col = rows
        .iter()
        .filter(|r| !r.right.is_empty() && r.width + 2 <= COLUMN)
        .map(|r| r.width + 2)
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for r in rows {
        out.push_str(&r.left);
        let at = col.max(r.width + 2);
        if let Some(x) = r.right.iter().find(|x| at + x.chars().count() <= WIDTH) {
            out.push_str(&" ".repeat(at - r.width));
            out.push_str(&style.paint(Paint::Dim, x));
        }
        out.push('\n');
    }
    out
}

/// Where a change is derived, as its line's site column says it at `why`
/// (R-111): `FILE:LINE` (a value given on the command line, the flag);
/// from `-v`, the statement's bindings after it, `with z = "a"`.
fn place_text(s: &Site, why: Why) -> String {
    let mut out = match s.at.is_empty() {
        true => s.statement.clone(),
        false => s.at.clone(),
    };
    if why >= Why::How && !s.with.is_empty() {
        if !out.is_empty() {
            out.push_str("  ");
        }
        out.push_str(&format!("with {}", s.with.join(", ")));
    }
    out
}

/// The writes a winning value of change `d` overrode, from `-v` (R-111):
/// `  @normal over k3s.df:9 @default`; not one in `d`'s own block (a
/// default the compiler writes there).
fn beat_text(s: &Site, d: &Deformation) -> String {
    let own = |at: &str| {
        let (Some(h), Some((file, line))) = (&d.site, at.rsplit_once(':')) else {
            return false;
        };
        let Some((hf, first)) = h.at.rsplit_once(':') else {
            return false;
        };
        let (Ok(line), Ok(first)) = (line.parse::<usize>(), first.parse::<usize>()) else {
            return false;
        };
        hf == file && (first..=h.last.max(first)).contains(&line)
    };
    match (&s.beat, &s.beat_at) {
        (Some(_), Some(at)) if own(at) => String::new(),
        (Some(b), Some(at)) => {
            let rank = s.rank.as_deref().unwrap_or("normal");
            format!("  @{rank} over {at} @{b}")
        }
        (Some(b), None) => format!("  over @{b}"),
        _ => String::new(),
    }
}

/// What wrote an attribute's value at `-v`: `STATEMENT   FILE:LINE`, then
/// `FILE:LINE` alone (a constant: its entry when it reads something, else
/// its place); the writes it won over after either.
fn written_text(s: &Site, d: &Deformation) -> Vec<String> {
    let beat = beat_text(s, d);
    if s.at.is_empty() {
        return vec![format!("{}{beat}", s.statement)];
    }
    if s.stated {
        let entry = s
            .entry
            .iter()
            .filter(|e| e.split_once(" = ").is_some_and(|(_, rhs)| reads(rhs)))
            .map(|e| format!("{e}   {}{beat}", s.at));
        return entry.chain([format!("{}{beat}", s.at)]).collect();
    }
    let mut out = vec![format!("{}   {}{beat}", s.statement, s.at)];
    out.extend(s.entry.iter().map(|e| format!("{e}   {}{beat}", s.at)));
    out.push(format!("{}{beat}", s.at));
    out
}

impl Report {
    /// Say why each change is planned, at level `why` (R-79), from the
    /// provenance of `res`, the evaluation the plan was made from: each
    /// entry where it is derived, with its bindings; each attribute it
    /// changes where its winning value was written; at `Full`, under
    /// each, the derivation compressed to its leaves (a create by its
    /// `want`, an update by the winning contributions to each attribute it
    /// changes, a delete by state alone). Every line passes through `r`.
    pub fn explain(&mut self, why: Why, res: &EvalResult, r: &Redactor) {
        self.why = why;
        if why == Why::None {
            return;
        }
        let p = tree::Printer {
            circuit: &res.circuit,
            redact: r,
            all: false,
        };
        let rules = &res.rules;
        let changed: BTreeSet<(String, String)> = self
            .definite
            .iter()
            .chain(self.pending.iter().flat_map(|b| b.deformations.iter()))
            .map(|d| (d.addr.typ.clone(), d.addr.name.clone()))
            .collect();
        let mut attrs: BTreeMap<(String, String), Vec<&Atom>> = BTreeMap::new();
        for f in res.facts.iter().filter(|f| f.pred == "attr") {
            if let [Term::Val(Value::Str(t)), Term::Val(Value::Str(a)), ..] = f.args.as_slice() {
                let k = (t.clone(), a.clone());
                if changed.contains(&k) {
                    attrs.entry(k).or_default().push(f);
                }
            }
        }
        let pending = self
            .pending
            .iter_mut()
            .flat_map(|b| b.deformations.iter_mut());
        for d in self.definite.iter_mut().chain(pending) {
            if matches!(d.kind, ActionKind::Delete | ActionKind::DeleteDeposed) {
                if why == Why::Full {
                    d.why = explanation(&p, res, d);
                }
                continue;
            }
            d.site = p.want_site(rules, &d.addr);
            let facts = attrs
                .get(&(d.addr.typ.clone(), d.addr.name.clone()))
                .map(Vec::as_slice)
                .unwrap_or_default();
            for l in d.lines.iter_mut() {
                l.site = attr_site(&p, rules, facts, &l.path);
            }
            if why == Why::Full {
                d.why = explanation(&p, res, d);
            }
        }
        for g in &mut self.groups {
            g.site = g
                .rule
                .as_ref()
                .and_then(|id| p.rule_site(rules, id, &g.bindings));
        }
        for x in &mut self.policies {
            x.site = x.rule.as_ref().and_then(|id| p.rule_site(rules, id, &[]));
        }
        self.denied = self
            .denies
            .iter()
            .map(|text| denied(&p, res, text))
            .collect();
        for a in &mut self.approvals {
            a.site = res
                .facts
                .iter()
                .filter(|f| f.pred == "requires_approval" && f.args.len() == 2)
                .find(|f| {
                    crate::approval::needs(&BTreeSet::from([(*f).clone()]))
                        == [(a.addr.clone(), a.reason.clone())]
                })
                .and_then(|f| res.circuit.fact_id(&crate::engine::circuit_fact(f)))
                .and_then(|id| p.site(rules, id));
        }
    }

    /// Under each change, the leaf that changed since the last apply
    /// (R-79): `then` is the snapshot of the program the last apply ran,
    /// `now` this plan's, both for the plan's addresses. A delete is also
    /// said where the last apply derived it (`was FILE:LINE`).
    pub fn because(&mut self, then: &crate::diff::Snapshot, now: &crate::diff::Snapshot) {
        let pending = self
            .pending
            .iter_mut()
            .flat_map(|b| b.deformations.iter_mut());
        for d in self.definite.iter_mut().chain(pending) {
            let addr = d.addr.to_string();
            if matches!(d.kind, ActionKind::Delete | ActionKind::DeleteDeposed) {
                d.site = then.sites.get(&addr).cloned();
            }
            let paths: Vec<String> = d.lines.iter().map(|l| l.path.clone()).collect();
            d.because = crate::diff::because_since(kind_name(&d.kind), &addr, &paths, then, now);
        }
    }

    /// Every site's place relative to `top`, the project's root (the
    /// program's directory outside a project), whatever directory the run
    /// is in and however it named the program.
    pub fn relative_to(&mut self, top: &std::path::Path) {
        let prefix = format!("{}/", top.display());
        let place = |at: &str| -> Option<String> {
            let (file, line) = at.rsplit_once(':')?;
            if file.starts_with('<') {
                return None;
            }
            let abs = std::path::absolute(file).ok()?;
            let rest = abs.to_str()?.strip_prefix(&prefix)?;
            Some(format!("{rest}:{line}"))
        };
        let fix = |s: &mut Option<Site>| {
            if let Some(s) = s
                && let Some(at) = place(&s.at)
            {
                s.at = at;
            }
        };
        let pending = self
            .pending
            .iter_mut()
            .flat_map(|b| b.deformations.iter_mut());
        for d in self.definite.iter_mut().chain(pending) {
            fix(&mut d.site);
            for l in d.lines.iter_mut() {
                fix(&mut l.site);
            }
            for b in d.why.iter_mut() {
                if let Some(at) = b.at.as_deref().and_then(place) {
                    b.at = Some(at);
                }
            }
        }
        self.groups.iter_mut().for_each(|g| fix(&mut g.site));
        self.policies.iter_mut().for_each(|p| fix(&mut p.site));
        self.approvals.iter_mut().for_each(|a| fix(&mut a.site));
        self.denied.iter_mut().for_each(|d| fix(&mut d.site));
        for d in self.conflicts.iter_mut().chain(self.shadowed.iter_mut()) {
            let witnesses = d.witnesses.iter_mut().flat_map(|w| w.at.iter_mut());
            for at in witnesses.chain(d.at.as_mut()) {
                if let Some(p) = place(at) {
                    *at = p;
                }
            }
        }
    }

    /// The changes the plan counts: every definite one that is not a
    /// no-op, and every held one a later tick of this plan makes (one
    /// waiting on what this plan does not resolve is `later`'s).
    fn counted(&self) -> impl Iterator<Item = &Deformation> {
        self.definite
            .iter()
            .filter(|d| !matches!(d.kind, ActionKind::Noop))
            .chain(
                self.pending
                    .iter()
                    .filter(|b| b.resolves_after.is_some())
                    .flat_map(|b| b.deformations.iter()),
            )
    }

    /// The changes by kind, in summary order.
    fn kinds(&self) -> Vec<(&'static str, usize)> {
        ["create", "update", "replace", "drift", "delete", "adopt"]
            .into_iter()
            .map(|k| {
                (
                    k,
                    self.counted().filter(|d| kind_name(&d.kind) == k).count(),
                )
            })
            .collect()
    }

    /// How many changes the plan has, in every tick (the summary's count).
    pub fn changes(&self) -> usize {
        self.counted().count()
    }

    /// The ticks, each with its changes, what it waits on and the
    /// objects it deletes once their replacements stand; the first is
    /// this report's tick. Held changes waiting on what this plan does
    /// not schedule are not in any.
    fn sections(&self) -> BTreeMap<usize, Section<'_>> {
        let mut out: BTreeMap<usize, Section> = BTreeMap::new();
        let shown: Vec<&Deformation> = self.definite.iter().collect();
        if !shown.is_empty() {
            out.entry(self.tick).or_default().changes = shown;
        }
        for b in &self.pending {
            let Some(t) = b.resolves_after else { continue };
            let s = out.entry(t + 1).or_default();
            s.changes.extend(b.deformations.iter());
            s.waits.extend(b.on.iter().cloned());
        }
        for d in &self.definite {
            if matches!(d.kind, ActionKind::Replace { create_first: true }) {
                out.entry(self.tick + 1).or_default().deposed.push(&d.addr);
            }
        }
        out
    }

    /// `plan: 5 changes (3 create, 2 update) over 2 ticks, 1 approval, 1 undetermined`
    pub fn summary(&self) -> String {
        let kinds: Vec<(&str, usize)> = self.kinds().into_iter().filter(|(_, n)| *n > 0).collect();
        let mut out = format!("plan: {}", count(self.changes(), "change"));
        if !kinds.is_empty() {
            let ks: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
            out.push_str(&format!(" ({})", ks.join(", ")));
        }
        let ticks = self
            .sections()
            .values()
            .filter(|s| {
                !s.deposed.is_empty()
                    || s.changes
                        .iter()
                        .any(|d| !matches!(d.kind, ActionKind::Noop))
            })
            .count();
        if ticks > 0 {
            out.push_str(&format!(" over {}", count(ticks, "tick")));
        }
        if self.show_noop {
            out.push_str(&format!(", {} no-op", self.noops));
        }
        if !self.denies.is_empty() {
            out.push_str(&format!(", {} denied", self.denies.len()));
        }
        if !self.approvals.is_empty() {
            out.push_str(&format!(", {}", count(self.approvals.len(), "approval")));
        }
        if !self.policies.is_empty() {
            out.push_str(&format!(", {} undetermined", self.policies.len()));
        }
        // Changes held on what no tick of this plan makes (a provider
        // waiting on its settings, R-110), listed under `later`.
        let later: usize = self
            .pending
            .iter()
            .filter(|b| b.resolves_after.is_none())
            .map(|b| b.deformations.len())
            .sum();
        if later > 0 {
            out.push_str(&format!(", {later} later"));
        }
        if !self.conflicts.is_empty() {
            out.push_str(&format!(", {}", count(self.conflicts.len(), "conflict")));
        }
        out
    }

    /// What `later` holds: rules that may derive an unknown number of
    /// resources, denies and checks undetermined until a tick, and held
    /// changes whose nulls this plan does not resolve.
    fn has_later(&self) -> bool {
        !self.groups.is_empty()
            || !self.policies.is_empty()
            || self.pending.iter().any(|b| b.resolves_after.is_none())
    }

    /// The last line: what `apply` does with this plan (R-12: it asks
    /// once, before the first tick, for everything the plan lists, and
    /// again before a later tick only for what no plan listed).
    pub fn apply_line(&self) -> String {
        if !self.conflicts.is_empty() || !self.denies.is_empty() {
            return "apply: refused until the conflicts and denies above are resolved".into();
        }
        let ticks: Vec<usize> = self.sections().into_keys().collect();
        let now = match self.approvals.is_empty() {
            true => "now",
            false => "once this plan's digest is approved (`--approval`)",
        };
        let mut out = match ticks.as_slice() {
            [] => "apply: nothing to change now".to_string(),
            [t] => format!("apply: tick {t} {now}"),
            [t, u] => format!("apply: tick {t} {now}, then tick {u} when tick {t} reports"),
            [t, .., u] => format!(
                "apply: tick {t} {now}, then ticks {} to {u}, each when the one before reports",
                t + 1
            ),
        };
        if self.has_later() {
            let after = self
                .groups
                .iter()
                .map(|g| g.resolves_after)
                .chain(self.policies.iter().map(|p| p.after))
                .chain(
                    self.pending
                        .iter()
                        .filter(|b| b.resolves_after.is_none())
                        .map(|_| None),
                )
                .collect::<Option<Vec<usize>>>()
                .and_then(|ts| ts.into_iter().max());
            match after {
                Some(t) => out.push_str(&format!(
                    "; `later` is planned again when tick {t} reports, and apply asks before \
                     what it adds"
                )),
                None => out.push_str(
                    "; `later` waits on what no tick of this plan makes: apply waits for it \
                     (`--wait`) or stops before it",
                ),
            }
        }
        out
    }

    /// The `warning` section's lines (R-80), unindented: each rule the
    /// plan deletes every resource of, with what it deletes and the leaf
    /// that changed since the last apply; each relation it empties.
    fn warning_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for w in &self.warnings {
            match &w.statement {
                Some(statement) => {
                    out.push(format!("{}  {statement}", w.name));
                    let shown: Vec<String> =
                        w.deleted.iter().take(3).map(|a| address_text(a)).collect();
                    let more = match w.deleted.len().saturating_sub(3) {
                        0 => String::new(),
                        n => format!(" and {n} more"),
                    };
                    let n = w.deleted.len();
                    out.push(format!(
                        "    deletes all {n} it derived at the last apply: {}{more}",
                        shown.join(", ")
                    ));
                    if let Some(b) = &w.because {
                        out.push(format!("    because {b}"));
                    }
                }
                None => {
                    let rows = match w.rows {
                        1 => "1 row".to_string(),
                        n => format!("{n} rows"),
                    };
                    out.push(format!(
                        "{}  had {rows} at the last apply, has none now",
                        relation_name(&w.name)
                    ));
                }
            }
        }
        out
    }

    /// The report as text, uncoloured: what `plan` prints with no colour,
    /// every golden, and the controller's log line.
    pub fn text(&self) -> String {
        self.render(Style::PLAIN)
    }

    /// The report as text, painted with `style`: the summary, each tick
    /// with its changes, `later`, the diagnostics, and what apply does.
    pub fn render(&self, style: Style) -> String {
        let bold = |s: &str| style.paint(Paint::Bold, s);
        let mut out = moved_text(&self.moved);
        if self.undeformed && !self.show_noop {
            out.push_str(&format!("stack {} is up to date\n", self.stack));
            return out;
        }
        let mut rows: Vec<Row> = vec![Row::plain(self.summary())];
        for (t, s) in self.sections() {
            rows.push(Row::plain(String::new()));
            let n = s.changes.len();
            let head = match (t == self.tick, n) {
                // A later tick of an apply: the one before has reported.
                (true, _) if t > 1 => format!(
                    "tick {t}  {}, now that tick {} reported",
                    count(n, "change"),
                    t - 1
                ),
                (true, _) => format!("tick {t}  {}, applies now", count(n, "change")),
                (false, 0) => format!("tick {t}  after tick {} reports", t - 1),
                (false, _) => format!(
                    "tick {t}  {}, after tick {} reports",
                    count(n, "change"),
                    t - 1
                ),
            };
            rows.push(Row::new(&head, bold(&head)));
            for (i, w) in waited(&s.waits).into_iter().enumerate() {
                let lead = if i == 0 {
                    "  waits on  "
                } else {
                    "            "
                };
                rows.push(Row::plain(format!("{lead}{w}")));
            }
            self.write_level(&mut rows, &s.changes, None, "  ", style);
            for a in &s.deposed {
                let addr = address(a);
                let plain = format!("  - {addr}  (deposed)");
                let painted = format!(
                    "  {} {}  (deposed)",
                    style.paint(Paint::Delete, "-"),
                    style.address(&ActionKind::Delete, &addr)
                );
                rows.push(Row::new(&plain, painted));
            }
        }
        if self.has_later() {
            rows.push(Row::plain(String::new()));
            let head = "later   changes this plan cannot count yet";
            rows.push(Row::new(head, bold(head)));
            self.write_later(&mut rows, style);
        }
        if !self.warnings.is_empty() {
            rows.push(Row::plain(String::new()));
            let head = "warning  this plan empties what the last apply derived";
            rows.push(Row::new(head, style.paint(Paint::Warn, head)));
            for line in self.warning_lines() {
                rows.push(Row::plain(format!("  {line}")));
            }
        }
        if !self.denies.is_empty() {
            rows.push(Row::plain(String::new()));
            rows.push(Row::new("denied", style.paint(Paint::Error, "denied")));
            let wide = self
                .denied
                .iter()
                .map(|d| address_text(&d.addr).chars().count())
                .max()
                .unwrap_or(0);
            for (i, d) in self.denies.iter().enumerate() {
                let row = self.denied.get(i);
                let message = row.map(|r| r.message.as_str()).unwrap_or(d);
                let left = format!("  {message}");
                let right = row
                    .map(|r| {
                        let at = r.site.as_ref().map(|s| s.at.as_str()).unwrap_or_default();
                        let addr = address_text(&r.addr);
                        let pad = " ".repeat(wide - addr.chars().count());
                        format!("{addr}{pad}    {at}").trim().to_string()
                    })
                    .into_iter()
                    .collect();
                rows.push(Row::new(&left, style.paint(Paint::Error, &left)).with(right));
            }
        }
        if !self.approvals.is_empty() {
            rows.push(Row::plain(String::new()));
            let head = "held for approval";
            rows.push(Row::new(head, style.paint(Paint::Held, head)));
            let wide = self
                .approvals
                .iter()
                .map(|a| a.reason.chars().count())
                .max()
                .unwrap_or(0);
            for a in &self.approvals {
                let at = a.site.as_ref().map(|s| s.at.as_str()).unwrap_or_default();
                let pad = " ".repeat(wide - a.reason.chars().count());
                let right = format!("{}{pad}    {at}", a.reason).trim_end().to_string();
                rows.push(Row::plain(format!("  {}", address_text(&a.addr))).with(vec![right]));
            }
        }
        out.push_str(&layout(&rows, style));
        let header = |s: &str| format!("{}\n", bold(s));
        let diags = [("shadowed", &self.shadowed), ("conflicts", &self.conflicts)];
        for (title, ds) in diags {
            if ds.is_empty() {
                continue;
            }
            out.push('\n');
            let conflict = title == "conflicts";
            let error = |s: &str| match conflict {
                true => style.paint(Paint::Error, s),
                false => s.to_string(),
            };
            out.push_str(&header(&error(title)));
            for d in ds {
                out.push_str(&diag_lines(d, self.why, style, conflict));
            }
        }
        if self.undeformed {
            out.push_str(&format!("\nstack {} is up to date\n", self.stack));
            return out;
        }
        out.push('\n');
        out.push_str(&self.apply_line());
        out.push('\n');
        out
    }

    /// `later`'s rows: each group by the address its rule names (a copy
    /// that may derive once, its resources under it), each undetermined
    /// deny and check, each held change this plan does not schedule.
    fn write_later(&self, rows: &mut Vec<Row>, style: Style) {
        let site = |s: &Option<Site>| s.as_ref().map(|s| s.at.clone()).unwrap_or_default();
        let full = self.why >= Why::How;
        // The right column, the longest that fits first: the place, the
        // condition, and (at `full`) the reason; the condition alone last.
        let both = |at: String, cond: String, reason: &str| {
            let mut out = Vec::new();
            for cond in [
                full.then(|| format!("{cond}  ({reason})")),
                Some(cond.clone()),
            ]
            .into_iter()
            .flatten()
            {
                if !at.is_empty() {
                    out.push(format!("{at}  {cond}"));
                }
                out.push(cond);
            }
            out
        };
        // A copy that may derive (R-67) is said once, its resources under it.
        let copy_of = group_copy;
        let mut copies: BTreeSet<String> = BTreeSet::new();
        for g in &self.groups {
            let copy = copy_of(g);
            if let Some(c) = &copy {
                if !copies.insert(c.clone()) {
                    continue;
                }
                let reads = g.reads.clone().unwrap_or_default();
                let shown = address_text(c);
                let plain = format!("  {shown}");
                let painted = format!("  {}", style.paint(Paint::Bold, &shown));
                rows.push(Row::new(&plain, painted).with(vec![format!("if {reads} derives")]));
                for m in self
                    .groups
                    .iter()
                    .filter(|m| copy_of(m).as_ref() == Some(c))
                {
                    let pattern = address_text(&group_address(m));
                    let plain = format!("    {pattern}");
                    let painted = format!("    {}", style.paint(Paint::Warn, &pattern));
                    rows.push(Row::new(&plain, painted));
                }
                continue;
            }
            let full_pattern = group_address(g);
            let unknown = full_pattern.ends_with("[?]") || full_pattern.contains("${");
            let pattern = address_text(&full_pattern);
            let on = waited(&g.on.iter().cloned().collect()).join(", ");
            let cond = match (&g.reads, unknown) {
                (Some(r), true) => format!("one per {r}"),
                (Some(r), false) => format!("if {r} derives"),
                (None, _) => format!("waits on {on}"),
            };
            let plain = format!("  {pattern}");
            let painted = format!("  {}", style.paint(Paint::Warn, &pattern));
            rows.push(Row::new(&plain, painted).with(both(site(&g.site), cond, &g.reason)));
        }
        for p in &self.policies {
            let on = waited(&p.on.iter().cloned().collect()).join(", ");
            let cond = match (p.may_derive, p.after) {
                (false, Some(t)) => format!("undetermined until tick {}", t + 1),
                (true, Some(t)) => format!("may hold at tick {}", t + 1),
                (_, None) if on.is_empty() => "undetermined".to_string(),
                (_, None) => format!("undetermined, waits on {on}"),
            };
            let left = match p.refinement {
                true => format!("  check {}", p.message),
                false => format!("  deny \"{}\"", p.message),
            };
            rows.push(Row::plain(left).with(both(site(&p.site), cond, &p.reason)));
        }
        for b in self.pending.iter().filter(|b| b.resolves_after.is_none()) {
            let ds: Vec<&Deformation> = b.deformations.iter().collect();
            let on = waited(&b.on.iter().cloned().collect()).join(", ");
            // A header like a tick's, the note in the site column (R-111).
            let lead = format!("  waits on  {on}");
            rows.push(Row::plain(lead).with(vec!["which this plan does not resolve".into()]));
            self.write_level(rows, &ds, None, "  ", style);
        }
    }

    /// Changes in order, a copy's under it (R-67): `+ network blue` at
    /// the place of its first resource, the resources indented beneath, a
    /// copy inside it nested again. The copy's marker is its `deformation`
    /// row's kind (`zset::Instances::row_kind`): `-` when the program wants
    /// none of its resources, `+` when one is created, else `~`.
    fn write_level(
        &self,
        rows: &mut Vec<Row>,
        ds: &[&Deformation],
        outer: Option<&Address>,
        indent: &str,
        style: Style,
    ) {
        // The copy directly under `outer` a change is in, if any.
        let under = |d: &Deformation| -> Option<Address> {
            let chain = self.instances.enclosing(&d.addr);
            let at = match outer {
                None => chain.len(),
                Some(o) => chain.iter().position(|a| a == o)?,
            };
            at.checked_sub(1).map(|i| chain[i].clone())
        };
        let mut done: BTreeSet<Address> = BTreeSet::new();
        for d in ds {
            let Some(copy) = under(d) else {
                self.write_change(rows, d, indent, style);
                continue;
            };
            if !done.insert(copy.clone()) {
                continue;
            }
            let members: Vec<&Deformation> = ds
                .iter()
                .filter(|m| self.instances.enclosing(&m.addr).contains(&copy))
                .copied()
                .collect();
            let kinds: Vec<&str> = members
                .iter()
                .filter_map(|m| crate::zset::deformation_kind(&m.kind, false))
                .collect();
            let kind = match self.instances.row_kind(&copy, &kinds) {
                "delete" => ActionKind::Delete,
                "create" => ActionKind::Create,
                _ => ActionKind::Update,
            };
            let addr = address(&copy);
            let plain = format!("{indent}{} {addr}", marker_of(&kind));
            let painted = format!(
                "{indent}{} {}",
                style.marker(&kind),
                style.paint(Paint::Bold, &addr)
            );
            rows.push(Row::new(&plain, painted));
            self.write_level(rows, &members, Some(&copy), &format!("{indent}  "), style);
        }
    }

    /// One change (R-111): its marker and address with where it is
    /// derived, its attributes each with where its value was written when
    /// that is outside its block, at `-vv` what it rests on, and why it
    /// changed since the last apply.
    fn write_change(&self, rows: &mut Vec<Row>, d: &Deformation, indent: &str, style: Style) {
        let note = match d.kind {
            ActionKind::Drift => {
                "  (drift: a fresh null where the world has a value; its identity is stale)"
            }
            ActionKind::DeleteDeposed => "  (deposed)",
            ActionKind::Replace { create_first: true } => "  (the new one first)",
            _ => "",
        };
        let addr = address(&d.addr);
        let plain = format!("{indent}{} {addr}{note}", marker_of(&d.kind));
        let painted = format!(
            "{indent}{} {}{note}",
            style.marker(&d.kind),
            style.address(&d.kind, &addr)
        );
        let at = d.site.as_ref().map(|s| place_text(s, self.why));
        let right: Vec<String> = match (&d.kind, at) {
            (ActionKind::Replace { .. }, at) => {
                let forces: Vec<String> = d
                    .forces
                    .iter()
                    .map(|p| format!("{p} is immutable"))
                    .collect();
                let both = at
                    .iter()
                    .flat_map(|at| forces.iter().map(move |f| format!("{at}  {f}")));
                both.chain(forces.iter().cloned()).collect()
            }
            (_, Some(at)) => match d.site.as_ref() {
                // A line too long for its bindings keeps its place.
                Some(s) if at != s.at && !s.at.is_empty() => vec![at, s.at.clone()],
                _ => vec![at],
            },
            (_, None) => vec![],
        };
        rows.push(Row::new(&plain, painted).with(right));
        // Keep plan output readable.
        let max = 40usize;
        let inner = format!("{indent}    ");
        for (i, l) in d.lines.iter().enumerate() {
            if i == max {
                rows.push(Row::plain(format!(
                    "{inner}... ({} more changes)",
                    d.lines.len() - max
                )));
                break;
            }
            let right = match &l.site {
                None => vec![],
                Some(s) => attr_text(d, l, s, self.why),
            };
            write_line(rows, &d.kind, l, &inner, style, self.why, right);
        }
        for b in &d.why {
            rows.push(Row::plain(format!("{inner}{}", b.line())));
        }
        if let Some(b) = &d.because {
            let plain = format!("{inner}because {b}");
            let painted = format!("{inner}{} {b}", style.paint(Paint::Because, "because"));
            rows.push(Row::new(&plain, painted));
        }
    }
}

/// Deny `text` over the plan (`MESSAGE`, or `MESSAGE ctx={..}`) as the
/// plan's rows say it: its message, the resource of the `deformation` row
/// its firing read, and where it is written.
fn denied(p: &tree::Printer, res: &EvalResult, text: &str) -> Denied {
    let message = |a: &Atom| match a.args.first() {
        Some(Term::Val(Value::Str(m))) => Some(m.clone()),
        _ => None,
    };
    let fact =
        res.facts.iter().filter(|a| a.pred == "deny").find(|a| {
            message(a).is_some_and(|m| text == m || text.starts_with(&format!("{m} ctx=")))
        });
    let Some(fact) = fact else {
        return Denied {
            message: text.to_string(),
            addr: String::new(),
            site: None,
        };
    };
    let id = res.circuit.fact_id(&crate::engine::circuit_fact(fact));
    let addr = id
        .and_then(|id| match res.circuit.view(id) {
            crate::circuit::View::Fact { alts, .. } => alts.first().copied(),
            _ => None,
        })
        .and_then(|alt| match res.circuit.view(alt) {
            crate::circuit::View::Times { children, .. } => {
                children.iter().find_map(|c| match res.circuit.view(*c) {
                    crate::circuit::View::Fact { fact, .. } if fact.pred == "deformation" => {
                        match fact.args.get(1) {
                            Some(Value::Ref { typ, name, .. }) => Some(
                                Address {
                                    typ: typ.clone(),
                                    name: name.clone(),
                                }
                                .to_string(),
                            ),
                            Some(v) => Some(fmt_bare(v)),
                            None => None,
                        }
                    }
                    _ => None,
                })
            }
            _ => None,
        })
        .unwrap_or_default();
    Denied {
        message: message(fact).unwrap_or_else(|| text.to_string()),
        addr,
        site: id.and_then(|id| p.site(&res.rules, id)),
    }
}

/// The site column of attribute line `l` of change `d`, written at `s`,
/// at level `why` (R-111). By default a value written in its own block
/// says nothing, any other its place. From `-v`, a create's value
/// written in its own block is the entry's expression when it reads
/// something the value does not show; anything else is the statement
/// that wrote it; either with the writes it won over.
fn attr_text(d: &Deformation, l: &Line, s: &Site, why: Why) -> Vec<String> {
    if matches!(d.kind, ActionKind::Delete | ActionKind::DeleteDeposed) {
        return vec![];
    }
    let own = d.site.as_ref().is_some_and(|h| match (&h.stmt, &s.stmt) {
        (Some((hf, first)), Some((sf, line))) => {
            hf == sf && (first == line || (first..=&h.last).contains(&line))
        }
        _ => false,
    });
    if why == Why::Line {
        return match own {
            true => vec![],
            false => vec![place_text(s, why)],
        };
    }
    if own && matches!(d.kind, ActionKind::Create | ActionKind::Adopt) {
        let after = l.after.said(why);
        let rhs = s
            .entry
            .as_deref()
            .and_then(|e| e.split_once(" = "))
            .map(|(_, rhs)| rhs.to_string())
            .filter(|rhs| {
                // A variable is the entry's binding, on the line above; a
                // literal is the value itself; a secret says so already.
                !rhs.chars().all(|c| c.is_alphanumeric() || c == '_')
                    && !rhs.starts_with("(sensitive")
                    && reads(rhs)
                    && !after.contains(rhs.as_str())
                    // A typed literal: `inet("10.0.0.0/16")`.
                    && !rhs.ends_with(&format!("({after})"))
            });
        let beat = beat_text(s, d);
        return match rhs {
            Some(rhs) if !beat.is_empty() => vec![format!("{rhs}{beat}"), rhs],
            Some(rhs) => vec![rhs],
            None if !beat.is_empty() => vec![beat.trim_start().to_string()],
            None => vec![],
        };
    }
    written_text(s, d)
}

/// Whether expression `e` reads anything: a name that is not an object's
/// key (a variable, a function), or an interpolation. `{ team: "a" }`
/// reads nothing; `db.name`, `json("f.json")` and `"shop-${env}"` do.
fn reads(e: &str) -> bool {
    let mut cs = e.chars().peekable();
    while let Some(c) = cs.next() {
        if c == '"' {
            let mut esc = false;
            let mut prev = ' ';
            for c in cs.by_ref() {
                if prev == '$' && c == '{' && !esc {
                    return true;
                }
                if c == '"' && !esc {
                    break;
                }
                esc = c == '\\' && !esc;
                prev = c;
            }
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let mut word = String::from(c);
            while let Some(&n) = cs.peek()
                && (n.is_alphanumeric() || "_.".contains(n))
            {
                word.push(n);
                cs.next();
            }
            while cs.peek().is_some_and(|n| n.is_whitespace()) {
                cs.next();
            }
            let key = cs.peek() == Some(&':');
            if !key && !matches!(word.as_str(), "true" | "false" | "null") {
                return true;
            }
        }
    }
    false
}

/// The copy a pending group's resources are of, when what it waits on is
/// whether the copy derives (`resource app blue`): `app["blue"]`.
fn group_copy(g: &Group) -> Option<String> {
    let rest = g.reads.as_deref()?.strip_prefix("resource ")?;
    let (path, name) = rest.split_once(' ')?;
    Some(format!("{path}[\"{name}\"]"))
}

/// A pending group's address: the address its rule's statement names
/// (`k8s.job["migrate-v${schema}"]`) where the head leaves the name open.
fn group_address(g: &Group) -> String {
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

/// The site of the attribute fact of `facts` that holds change path
/// `path` (`tags.team` of `tags`, `statements[0].action` of
/// `statements`), focused on the keys below it.
fn attr_site(p: &tree::Printer, rules: &[RuleStmt], facts: &[&Atom], path: &str) -> Option<Site> {
    let segs = crate::ir::path_segments(path);
    for k in (1..=segs.len()).rev() {
        let last = segs[k - 1];
        let index = crate::ir::segment_parts(last).1;
        let raw = &last[..last.len() - index.len()];
        let prefix = segs[..k - 1]
            .iter()
            .copied()
            .chain([raw])
            .collect::<Vec<_>>()
            .join(".");
        let found = facts
            .iter()
            .find(|a| matches!(a.args.get(2), Some(Term::Val(Value::Str(p))) if *p == prefix));
        if let Some(a) = found {
            let keys: Vec<String> = match index.is_empty() {
                true => segs[k..]
                    .iter()
                    .take_while(|s| crate::ir::segment_parts(s).1.is_empty())
                    .map(|s| crate::ir::segment_key(s).into_owned())
                    .collect(),
                false => vec![],
            };
            let whole = index.is_empty() && keys.len() == segs.len() - k;
            return p.attr_site(rules, a, &keys, whole);
        }
    }
    None
}

/// One tick of the report.
#[derive(Default)]
struct Section<'a> {
    changes: Vec<&'a Deformation>,
    waits: BTreeSet<String>,
    deposed: Vec<&'a Address>,
}

/// `moved OLD -> NEW`, one line per rename `moved/3` applied to state.
pub fn moved_text(moves: &[(Address, Address)]) -> String {
    moves
        .iter()
        .map(|(old, new)| format!("moved {old} -> {new}\n"))
        .collect()
}

impl Report {
    /// The plan as one JSON document (`plan --json`): the ticks, each
    /// with what it waits on and its changes (kind, address, attribute
    /// changes, where each is derived and why it changed since the last
    /// apply), `later`, the diagnostics; a null as `{"null": LABEL,
    /// "class": C}`, a secret or a sensitive value as `{"sensitive": LABEL}`.
    pub fn json(&self) -> Json {
        let nulls = |on: &mut dyn Iterator<Item = &String>| -> Json {
            on.map(|l| {
                let class = self
                    .classes
                    .get(l)
                    .cloned()
                    .unwrap_or_else(|| "unknown".into());
                json!({"null": crate::ir::label(l), "class": class})
            })
            .collect()
        };
        let changes = |ds: &[&Deformation]| -> Json {
            ds.iter()
                .map(|d| {
                    let mut j = self.change_json(d);
                    // The copy it is a resource of, innermost (R-67).
                    if let Some(i) = self.instances.enclosing(&d.addr).first() {
                        j["instance"] = json!(i.to_string());
                    }
                    j
                })
                .collect()
        };
        let mut summary = serde_json::Map::new();
        summary.insert("changes".into(), json!(self.changes()));
        for (k, n) in self.kinds() {
            summary.insert(k.into(), json!(n));
        }
        summary.insert("no_op".into(), json!(self.noops));
        summary.insert("ticks".into(), json!(self.sections().len()));
        summary.insert("approvals".into(), json!(self.approvals.len()));
        summary.insert("undetermined".into(), json!(self.policies.len()));
        summary.insert("conflicts".into(), json!(self.conflicts.len()));
        let site = |s: &Option<Site>| match s {
            Some(s) if self.why != Why::None => json!(s),
            _ => Json::Null,
        };
        let mut later: Vec<Json> = Vec::new();
        for g in &self.groups {
            later.push(json!({
                "kind": "group",
                "address": group_address(g),
                "instance": group_copy(g),
                "reads": g.reads,
                "on": nulls(&mut g.on.iter()),
                "reason": g.reason,
                "after": g.resolves_after,
                "site": site(&g.site),
            }));
        }
        for p in &self.policies {
            later.push(json!({
                "kind": if p.refinement { "refinement" } else { "deny" },
                "message": p.message,
                "status": if p.refinement {
                    "deferred"
                } else if p.may_derive {
                    "may_derive"
                } else {
                    "undetermined"
                },
                "on": nulls(&mut p.on.iter()),
                "reason": p.reason,
                "after": p.after,
                "site": site(&p.site),
            }));
        }
        for b in self.pending.iter().filter(|b| b.resolves_after.is_none()) {
            let ds: Vec<&Deformation> = b.deformations.iter().collect();
            later.push(json!({
                "kind": "held",
                "on": nulls(&mut b.on.iter()),
                "changes": changes(&ds),
            }));
        }
        let mut j = json!({
            "stack": self.stack,
            "up_to_date": self.undeformed,
            "summary": summary,
            "ticks": self.sections().iter().map(|(t, s)| json!({
                "tick": t,
                "after": (*t != self.tick).then(|| t - 1),
                "waits_on": nulls(&mut s.waits.iter()),
                "changes": changes(&s.changes),
                "deposed": s.deposed.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "later": later,
            "shadowed": self.shadowed.iter().map(diag_json).collect::<Vec<_>>(),
            "conflicts": self.conflicts.iter().map(diag_json).collect::<Vec<_>>(),
            "moved": self.moved.iter().map(|(old, new)| json!({
                "from": {"address": old.to_string(), "type": old.typ, "name": old.name},
                "to": {"address": new.to_string(), "type": new.typ, "name": new.name},
            })).collect::<Vec<_>>(),
            "denied": self.denies.iter().enumerate().map(|(i, text)| {
                let row = self.denied.get(i);
                json!({
                    "text": text,
                    "message": row.map(|r| r.message.clone()),
                    "address": row.map(|r| r.addr.clone()).filter(|a| !a.is_empty()),
                    "site": row.and_then(|r| r.site.clone()).filter(|_| self.why != Why::None),
                })
            }).collect::<Vec<_>>(),
            "held_for_approval": self.approvals.iter().map(|a| json!({
                "address": a.addr,
                "reason": a.reason,
                "site": a.site.as_ref().filter(|_| self.why != Why::None),
            })).collect::<Vec<_>>(),
            "apply": self.apply_line(),
        });
        // What the plan empties (R-80), only when it empties something.
        if !self.warnings.is_empty() {
            j["warnings"] = self
                .warnings
                .iter()
                .map(|w| {
                    json!({
                        "rule": w.statement.as_ref().map(|_| w.name.clone()),
                        "statement": w.statement,
                        "relation": w.statement.is_none().then(|| w.name.clone()),
                        "deletes": w.deleted,
                        "rows_at_last_apply": w.statement.is_none().then_some(w.rows),
                        "because": w.because,
                    })
                })
                .collect::<Vec<_>>()
                .into();
        }
        j
    }

    fn change_json(&self, d: &Deformation) -> Json {
        let explained = self.why != Why::None;
        let mut m = serde_json::Map::new();
        m.insert("kind".into(), json!(kind_name(&d.kind)));
        m.insert("address".into(), json!(d.addr.to_string()));
        m.insert("type".into(), json!(d.addr.typ));
        m.insert("name".into(), json!(d.addr.name));
        match d.kind {
            ActionKind::Replace { create_first } => {
                m.insert("create_first".into(), json!(create_first));
                m.insert("immutable".into(), json!(d.forces));
            }
            ActionKind::DeleteDeposed => {
                m.insert("deposed".into(), json!(true));
            }
            _ => {}
        }
        m.insert(
            "changes".into(),
            d.lines
                .iter()
                .map(|l| line_json(l, explained))
                .collect::<Vec<_>>()
                .into(),
        );
        let held: Vec<Json> = self
            .approvals
            .iter()
            .filter(|a| a.addr == d.addr.to_string())
            .map(|a| json!({"approval": a.reason, "site": a.site.as_ref().filter(|_| explained)}))
            .collect();
        if !held.is_empty() {
            m.insert("held".into(), held.into());
        }
        if explained {
            m.insert("site".into(), json!(d.site));
            m.insert("because".into(), json!(d.because));
        }
        if self.why == Why::Full {
            m.insert("why".into(), json!(d.why));
        }
        Json::Object(m)
    }
}

/// Why change `d` is planned ([`Report::explain`] at `Full`).
fn explanation(p: &tree::Printer, res: &EvalResult, d: &Deformation) -> Vec<Because> {
    let state = |text: &str| {
        vec![Because {
            kind: "state".into(),
            at: None,
            text: text.into(),
        }]
    };
    match d.kind {
        ActionKind::Delete => state("no statement derives it now; state has it"),
        ActionKind::DeleteDeposed => state("deposed by its replacement"),
        ActionKind::Noop | ActionKind::Create | ActionKind::Adopt => {
            p.want(&res.rules, &d.addr).unwrap_or_default()
        }
        ActionKind::Update
        | ActionKind::Drift
        | ActionKind::Pending
        | ActionKind::Replace { .. } => {
            let mut out: Vec<Because> = Vec::new();
            for l in &d.lines {
                for b in p
                    .attr(&res.rules, &res.facts, &d.addr, &l.path)
                    .unwrap_or_default()
                {
                    if !out.contains(&b) {
                        out.push(b);
                    }
                }
            }
            if out.is_empty() {
                out = p.want(&res.rules, &d.addr).unwrap_or_default();
            }
            out
        }
    }
}

fn line_json(l: &Line, explained: bool) -> Json {
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
            l.leaves
                .iter()
                .map(|x| line_json(x, explained))
                .collect::<Vec<_>>()
                .into(),
        );
    }
    if explained && let Some(s) = &l.site {
        m.insert("site".into(), json!(s));
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
        "witnesses": d.witnesses.iter().map(|w| json!({
            "rank": w.rank,
            "value": w.value.json(),
            "from": w.from,
        })).collect::<Vec<_>>(),
    })
}

fn write_line(
    rows: &mut Vec<Row>,
    kind: &ActionKind,
    l: &Line,
    indent: &str,
    style: Style,
    why: Why,
    right: Vec<String>,
) {
    let mut push = |plain: String, painted: String, right: Vec<String>| {
        rows.push(Row::new(&plain, painted).with(right))
    };
    let plain = |v: &Shown| v.said(why);
    let painted = |v: &Shown| style.said(v, why);
    match l.op {
        Op::Add | Op::Remove => {
            let (sign, paint, v) = if l.op == Op::Add {
                ("+", Paint::Create, &l.after)
            } else {
                ("-", Paint::Delete, &l.before)
            };
            let painted_sign = style.paint(paint, sign);
            if l.leaves.is_empty() {
                push(
                    format!("{indent}{sign} {} = {}", l.path, plain(v)),
                    format!("{indent}{painted_sign} {} = {}", l.path, painted(v)),
                    right,
                );
                return;
            }
            push(
                format!("{indent}{sign} {}", l.path),
                format!("{indent}{painted_sign} {}", l.path),
                right,
            );
            let inner = if l.op == Op::Add {
                ActionKind::Create
            } else {
                ActionKind::Delete
            };
            for x in &l.leaves {
                write_line(
                    rows,
                    &inner,
                    x,
                    &format!("{indent}    "),
                    style,
                    why,
                    vec![],
                );
            }
        }
        Op::Leaf => match kind {
            ActionKind::Create | ActionKind::Adopt => push(
                format!("{indent}{} = {}", l.path, plain(&l.after)),
                format!("{indent}{} = {}", l.path, painted(&l.after)),
                right,
            ),
            ActionKind::Delete | ActionKind::DeleteDeposed => push(
                format!("{indent}{} was {}", l.path, plain(&l.before)),
                format!("{indent}{} was {}", l.path, painted(&l.before)),
                right,
            ),
            ActionKind::Update
            | ActionKind::Drift
            | ActionKind::Pending
            | ActionKind::Replace { .. } => push(
                format!(
                    "{indent}{}: {} → {}",
                    l.path,
                    plain(&l.before),
                    plain(&l.after)
                ),
                format!(
                    "{indent}{}: {} → {}",
                    l.path,
                    painted(&l.before),
                    painted(&l.after)
                ),
                right,
            ),
            ActionKind::Noop => {}
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(t: &str, n: &str) -> Address {
        Address {
            typ: t.into(),
            name: n.into(),
        }
    }

    /// An extern's answer is its call, its inputs never read as a path
    /// (`127.0.0.1:22,ubuntu,/etc/k3s.yaml` split at its dots); a
    /// provider's extern by its column number.
    #[test]
    fn an_extern_label_is_its_call() {
        let l = crate::value::null_label("ssh.read", "127.0.0.1:22,ubuntu,/etc/k3s.yaml", "4");
        let call = "ssh.read(\"127.0.0.1:22\", \"ubuntu\", \"/etc/k3s.yaml\")";
        assert_eq!(label(&l), call);
        assert_eq!(attribute_label(&l), call);
        assert_eq!(printed_label(&crate::ir::label(&l)), call);
        assert_eq!(waited(&BTreeSet::from([l])), [format!("{call} not yet")]);
        let l = crate::value::null_label("aws.availability_zone", "available", "2");
        assert_eq!(label(&l), "aws.availability_zone(\"available\")");
        // A resource's attribute stays one.
        let l = crate::value::null_label("db.postgres", "d", "endpoint");
        assert_eq!(attribute_label(&l), "db.postgres d.endpoint");
    }

    /// R-111, R-112: an address is its type and its path, a copy's scope
    /// in front, a local name holding a dot one quoted segment; a
    /// reference is the path alone.
    #[test]
    fn an_address_prints_as_the_source_names_it() {
        assert_eq!(
            address(&a("ovh.ssh_key", "k3s.admin")),
            "ovh.ssh_key k3s.admin"
        );
        assert_eq!(
            address(&a("ovh.domain_record", r#"k3s."k8s-lab.vodik.xyz""#)),
            r#"ovh.domain_record k3s."k8s-lab.vodik.xyz""#
        );
        assert_eq!(address(&a("net.vpc", "main")), "net.vpc main");
        assert_eq!(address(&a("x.thing", "a b")), r#"x.thing "a b""#);
        assert_eq!(
            reference(&a("ovh.instance", "k3s.server"), "public_ip"),
            "k3s.server.public_ip"
        );
        assert_eq!(attribute(&a("input", ""), "db.days"), "input db.days");
        assert_eq!(
            label("ovh.instance/k3s.server#public_ip"),
            "k3s.server.public_ip"
        );
        assert_eq!(label("net.vpc/main#id"), "main");
        assert_eq!(attribute_label("net.vpc/main#id"), "net.vpc main");
        assert_eq!(
            address_text(r#"k8s.job["migrate-v${schema}"]"#),
            r#"k8s.job "migrate-v${schema}""#
        );
        assert_eq!(address_text("k8s.job[?]"), "k8s.job ?");
        assert_eq!(address_text(r#"app["blue"]"#), "app blue");
    }

    /// Past 60 characters a string elides its middle at the default
    /// level, and prints whole from `-v`.
    #[test]
    fn a_long_string_elides_its_middle_by_default() {
        let key = format!("ssh-ed25519 {} simon@framework", "A".repeat(68));
        let v = Shown::Value(Json::String(key.clone()));
        let line = v.said(Why::Line);
        assert_eq!(line.chars().count(), LONG + 2, "{line}");
        assert!(line.starts_with("\"ssh-ed25519 AAAA") && line.ends_with("AA simon@framework\""));
        assert!(line.contains('…'), "{line}");
        assert_eq!(v.said(Why::How), crate::partition::quote(&key));
        let short = Shown::Value(Json::String("b2-7".into()));
        assert_eq!(short.said(Why::Line), "\"b2-7\"");
    }

    /// No `?`: a value not known yet is the reference it is; a secret is
    /// `(sensitive)`, by its label from `-v`.
    #[test]
    fn an_unknown_is_its_reference_and_a_secret_is_sensitive() {
        let null = Shown::Null {
            label: r#"ovh.instance["k3s.server"].public_ip"#.into(),
            class: "computed".into(),
        };
        assert_eq!(null.said(Why::Line), "k3s.server.public_ip");
        assert_eq!(null.text(), r#"?ovh.instance["k3s.server"].public_ip"#);
        let secret = Shown::Sensitive(Some(r#"db.instance["main"].password"#.into()));
        assert_eq!(secret.said(Why::Line), "(sensitive)");
        assert_eq!(
            secret.said(Why::How),
            "(sensitive db.instance main.password)"
        );
        let r = Shown::Ref {
            addr: a("net.vpc", "main.vpc"),
            value: Json::String("vpc-1".into()),
        };
        assert_eq!(r.said(Why::Line), "main.vpc");
        assert_eq!(r.text(), r#"net.vpc["main.vpc"]"#);
    }

    /// Colour is a hint: the address bold in its kind's colour, the site
    /// column dim; plain, nothing.
    #[test]
    fn colour_follows_the_kind() {
        let c = Style { color: true };
        assert_eq!(
            c.address(&ActionKind::Create, "net.vpc main"),
            "\x1b[1;32mnet.vpc main\x1b[0m"
        );
        assert_eq!(
            c.address(&ActionKind::Delete, "net.vpc main"),
            "\x1b[1;31mnet.vpc main\x1b[0m"
        );
        assert_eq!(c.paint(Paint::Dim, "p.df:3"), "\x1b[2mp.df:3\x1b[0m");
        assert_eq!(c.paint(Paint::Because, "because"), "\x1b[36mbecause\x1b[0m");
        assert_eq!(Style::PLAIN.address(&ActionKind::Create, "x"), "x");
    }
}

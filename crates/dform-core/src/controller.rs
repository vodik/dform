//! Controller mode (DESIGN.org "Reactive inputs and controller mode"): the
//! second executor over the same evaluator. `dform controller` waits for an
//! input relation's source or the world to change, then runs what `apply`
//! runs (refresh, evaluate, plan, the policy pass, ticks) with this hook in
//! it. Nothing in the language changes: the plan is the reconciliation.
//!
//! What the hook adds to a run:
//!
//! * The event. Its memo (`<state dir>/controller.json`) holds the stamps
//!   of the sources and the world as the last run left them, and the world
//!   as that run accepted it (the baseline). A run compares the stamps to
//!   say what changed: `start` (no memo), `input NAME...`, `world`, or
//!   `resync` (nothing did; `--once` with no change).
//! * Drift as facts: every leaf where the world differs from the baseline is
//!   `drift(T, A, Path, Before, After)` (`Path` "" and `After` `absent` when
//!   the object is gone), so policy can decide about it.
//! * The gate, over the policy pass of every tick. A deformation of `T.A` is
//!   held when the policy derives `hold(T, A, Reason)` (a prod-style hold;
//!   the program writes when it is released, e.g. `not
//!   release_approved(...)` over an input relation), and, unless the event
//!   is an input change, when `T.A` has drift that is neither
//!   `auto_reconcile(T, A, Path)` for each drifted path nor `approve(T, A)`.
//!   Held drift stays in the baseline, so it is held again at every world
//!   event until an input change or an approval releases it.
//! * The log: one line per event and per tick, `HH:MM:SS` (UTC) first.

use crate::ast::{Atom, Span, Term};
use crate::ir::Address;
use crate::provider::{ActionKind, Plan};
use crate::value::Value;
use crate::watch::{self, Relation};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// `HH:MM:SS` of the wall clock, UTC.
pub fn clock() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// One line of the controller's log, on stdout.
pub fn log(msg: impl std::fmt::Display) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{} {msg}", clock());
    let _ = out.flush();
}

/// What a run of the controller answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Start,
    Input(Vec<String>),
    World,
    Resync,
}

/// What the controller remembers between runs, beside the stack's state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Memo {
    /// Per input relation, the stamp of its source.
    inputs: BTreeMap<String, String>,
    /// The world file's stamp.
    world: String,
    /// The world as the last run accepted it, per `state::key`.
    baseline: BTreeMap<String, Json>,
}

/// The controller's part of a run (see the module doc).
#[derive(Default)]
pub struct Hook {
    /// The stack's input relations, from the last run.
    pub relations: Vec<Relation>,
    world: Option<PathBuf>,
    memo_path: Option<PathBuf>,
    memo: Option<Memo>,
    /// The stamps of the sources as this run read them.
    input_stamps: BTreeMap<String, String>,
    pub event: Option<Event>,
    drift: Vec<Drift>,
    /// Addresses held by this run.
    held: BTreeSet<Address>,
}

/// A leaf where the world moved away from the baseline.
#[derive(Debug, Clone)]
pub struct Drift {
    pub addr: Address,
    pub path: String,
    pub before: Value,
    pub after: Value,
}

/// `path` relative to `root` when it is under it (an absolute path from the
/// registry included), else as it is.
fn relative_to<'a>(path: &'a Path, root: &Path) -> std::borrow::Cow<'a, Path> {
    let abs = |p: &Path| match p.as_os_str().is_empty() {
        true => std::fs::canonicalize(".").ok(),
        false => std::fs::canonicalize(p).ok(),
    };
    match (abs(path), abs(root)) {
        (Some(p), Some(r)) => match p.strip_prefix(&r) {
            Ok(rel) => std::borrow::Cow::Owned(rel.to_path_buf()),
            Err(_) => std::borrow::Cow::Borrowed(path),
        },
        _ => std::borrow::Cow::Borrowed(path),
    }
}

fn file_stamp(p: &Path) -> String {
    watch::stamp(&watch::Source::File(p.to_path_buf()))
}

impl Hook {
    /// The run has read its input relations: stamp them (before reading, so
    /// a change during the run is seen by the next one).
    pub fn inputs(&mut self, relations: &[Relation]) {
        self.relations = relations.to_vec();
        self.input_stamps = relations
            .iter()
            .map(|r| (r.pred.clone(), watch::stamp(&r.source)))
            .collect();
    }

    /// The run knows where the stack lives: load the memo, say what
    /// changed, and log the event (the world's path relative to `root`,
    /// the directory holding the state root, when it is under it).
    pub fn open(&mut self, state: &Path, world: &Path, root: &Path) -> Result<()> {
        let memo_path = state.with_file_name("controller.json");
        let memo: Option<Memo> = match std::fs::read(&memo_path) {
            Ok(b) => Some(
                serde_json::from_slice(&b)
                    .with_context(|| format!("parse {}", memo_path.display()))?,
            ),
            Err(_) => None,
        };
        self.world = Some(world.to_path_buf());
        self.memo_path = Some(memo_path);
        self.held.clear();
        let event = match &memo {
            None => Event::Start,
            Some(m) => {
                let changed: Vec<String> = self
                    .relations
                    .iter()
                    .filter(|r| m.inputs.get(&r.pred) != self.input_stamps.get(&r.pred))
                    .map(|r| r.pred.clone())
                    .collect();
                if !changed.is_empty() {
                    // One line per source: its relations changed together.
                    let mut by_source: BTreeMap<&watch::Source, Vec<&str>> = BTreeMap::new();
                    for r in self.relations.iter().filter(|r| changed.contains(&r.pred)) {
                        by_source.entry(&r.source).or_default().push(&r.pred);
                    }
                    for (source, preds) in by_source {
                        log(format_args!("input {} changed ({source})", preds.join(" ")));
                    }
                    Event::Input(changed)
                } else if m.world != file_stamp(world) {
                    Event::World
                } else {
                    Event::Resync
                }
            }
        };
        match &event {
            Event::Start => log("event start"),
            Event::Input(names) => log(format_args!("event input {}", names.join(" "))),
            Event::World => log(format_args!(
                "event world {} changed",
                relative_to(world, root).display()
            )),
            Event::Resync => log("event resync"),
        }
        self.memo = memo;
        self.event = Some(event);
        Ok(())
    }

    /// The world now against the baseline: the drift, as facts.
    pub fn drift_facts(&mut self, observed: &BTreeMap<Address, Json>) -> Vec<Atom> {
        self.drift = match &self.memo {
            Some(m) => drift(&m.baseline, observed),
            None => Vec::new(),
        };
        self.drift
            .iter()
            .map(|d| Atom {
                pred: "drift".into(),
                args: vec![
                    Term::Val(Value::Str(d.addr.typ.clone())),
                    Term::Val(Value::Str(d.addr.name.clone())),
                    Term::Val(Value::Str(d.path.clone())),
                    Term::Val(d.before.clone()),
                    Term::Val(d.after.clone()),
                ],
                record: None,
                span: Span::default(),
            })
            .collect()
    }

    /// The gate over tick `tick`'s plan and its policy pass: log the drift
    /// (at tick 1) and the plan's summary, the first line of `report`
    /// (nothing when it is undeformed: `finish` says so), then drop the
    /// held deformations and log why.
    pub fn gate(&mut self, tick: usize, plan: &mut Plan, facts: &BTreeSet<Atom>, report: &str) {
        let input_event = matches!(self.event, Some(Event::Input(_)));
        let has = |pred: &str, args: &[&str]| {
            facts.iter().any(|a| {
                a.pred == pred
                    && a.args.len() >= args.len()
                    && a.args.iter().zip(args).all(|(t, s)| str_of(t) == Some(s))
            })
        };
        if tick == 1 {
            for d in &self.drift {
                let (t, a) = (d.addr.typ.as_str(), d.addr.name.as_str());
                let verdict = if has("auto_reconcile", &[t, a, &d.path]) {
                    "auto_reconcile"
                } else if has("approve", &[t, a]) {
                    "approved"
                } else if input_event {
                    "reconciled with the input change"
                } else {
                    "held until approve or an input change"
                };
                log(format_args!(
                    "drift {t}.{a} {}: {} -> {} ({verdict})",
                    if d.path.is_empty() {
                        "(object)"
                    } else {
                        &d.path
                    },
                    crate::partition::fmt_value(&d.before),
                    crate::partition::fmt_value(&d.after),
                ));
            }
        }
        let first = report.lines().next().unwrap_or("");
        if !first.ends_with(" is undeformed") {
            log(format_args!("tick {tick}: {first}"));
        }
        let mut held = Vec::new();
        for act in &plan.actions {
            if matches!(act.kind, ActionKind::Noop) {
                continue;
            }
            let (t, a) = (act.addr.typ.as_str(), act.addr.name.as_str());
            let reasons: Vec<String> = facts
                .iter()
                .filter(|f| f.pred == "hold" && f.args.len() == 3)
                .filter(|f| str_of(&f.args[0]) == Some(t) && str_of(&f.args[1]) == Some(a))
                .map(|f| match &f.args[2] {
                    Term::Val(Value::Str(s)) => s.clone(),
                    other => format!("{other:?}"),
                })
                .collect();
            if let Some(r) = reasons.first() {
                held.push((act.addr.clone(), r.clone()));
                continue;
            }
            if input_event || has("approve", &[t, a]) {
                continue;
            }
            let unreconciled: Vec<&str> = self
                .drift
                .iter()
                .filter(|d| d.addr == act.addr && !has("auto_reconcile", &[t, a, &d.path]))
                .map(|d| d.path.as_str())
                .collect();
            if !unreconciled.is_empty() {
                held.push((
                    act.addr.clone(),
                    format!("drift at {} needs approval", unreconciled.join(", ")),
                ));
            }
        }
        for (addr, why) in &held {
            log(format_args!(
                "tick {tick}: proceed: held, {why}: {}.{}",
                addr.typ, addr.name
            ));
            self.held.insert(addr.clone());
        }
        plan.actions.retain(|a| !self.held.contains(&a.addr));
    }

    /// The run ends: `observed` is the world it leaves. Log whether the
    /// stack is undeformed and remember the stamps and the baseline (a held
    /// address keeps the one it had, so its drift is held again).
    pub fn finish(
        &mut self,
        stack: &str,
        undeformed: bool,
        observed: &BTreeMap<Address, Json>,
    ) -> Result<()> {
        if undeformed && self.held.is_empty() {
            log(format_args!("stack {stack} is undeformed"));
        } else if !self.held.is_empty() {
            log(format_args!(
                "stack {stack} is deformed: {} held",
                self.held
                    .iter()
                    .map(|a| format!("{}.{}", a.typ, a.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let old = self.memo.take().unwrap_or_default();
        let mut baseline: BTreeMap<String, Json> = observed
            .iter()
            .map(|(a, d)| (crate::state::key(a), d.clone()))
            .collect();
        for a in &self.held {
            let k = crate::state::key(a);
            match old.baseline.get(&k) {
                Some(d) => {
                    baseline.insert(k, d.clone());
                }
                None => {
                    baseline.remove(&k);
                }
            }
        }
        self.save(Memo {
            inputs: self.input_stamps.clone(),
            world: self.world.as_deref().map(file_stamp).unwrap_or_default(),
            baseline,
        })
    }

    /// The run failed: remember the stamps so the same change is not
    /// retried until something changes again; the baseline stays.
    pub fn failed(&mut self) -> Result<()> {
        let Some(old) = self.memo.take() else {
            return Ok(());
        };
        self.save(Memo {
            inputs: self.input_stamps.clone(),
            world: self.world.as_deref().map(file_stamp).unwrap_or_default(),
            baseline: old.baseline,
        })
    }

    fn save(&mut self, memo: Memo) -> Result<()> {
        let Some(path) = &self.memo_path else {
            return Ok(());
        };
        std::fs::write(path, serde_json::to_vec_pretty(&memo)?)
            .with_context(|| format!("write {}", path.display()))?;
        self.memo = Some(memo);
        Ok(())
    }

    /// Has a source or the world changed since the last run? (Before the
    /// first run: yes.)
    pub fn changed(&self) -> bool {
        let (Some(memo), Some(world)) = (&self.memo, &self.world) else {
            return true;
        };
        memo.world != file_stamp(world)
            || self
                .relations
                .iter()
                .any(|r| memo.inputs.get(&r.pred) != Some(&watch::stamp(&r.source)))
    }
}

fn str_of(t: &Term) -> Option<&str> {
    match t {
        Term::Val(Value::Str(s)) => Some(s),
        _ => None,
    }
}

/// Every leaf where `now` differs from `baseline`, per address both have;
/// an address the baseline has and the world no longer does is one drift
/// with path "".
fn drift(baseline: &BTreeMap<String, Json>, now: &BTreeMap<Address, Json>) -> Vec<Drift> {
    let mut out = Vec::new();
    for (k, before) in baseline {
        let Some(addr) = crate::state::parse_key(k) else {
            continue;
        };
        let Some(after) = now.get(&addr) else {
            out.push(Drift {
                addr,
                path: String::new(),
                before: Value::Str("present".into()),
                after: Value::Str("absent".into()),
            });
            continue;
        };
        let (mut b, mut a) = (BTreeMap::new(), BTreeMap::new());
        leaves(before, "", &mut b);
        leaves(after, "", &mut a);
        let paths: BTreeSet<&String> = b.keys().chain(a.keys()).collect();
        for p in paths {
            let (x, y) = (b.get(p), a.get(p));
            if x != y {
                let v = |j: Option<&Json>| match j {
                    Some(j) => json_value(j),
                    None => Value::Str("absent".into()),
                };
                out.push(Drift {
                    addr: addr.clone(),
                    path: p.clone(),
                    before: v(x),
                    after: v(y),
                });
            }
        }
    }
    out
}

/// A document's scalar leaves by dotted path, list elements by index.
fn leaves(j: &Json, at: &str, out: &mut BTreeMap<String, Json>) {
    match j {
        Json::Object(m) if !m.is_empty() => {
            for (k, v) in m {
                let p = if at.is_empty() {
                    k.clone()
                } else {
                    format!("{at}.{k}")
                };
                leaves(v, &p, out);
            }
        }
        Json::Array(xs) if !xs.is_empty() => {
            for (i, v) in xs.iter().enumerate() {
                leaves(v, &format!("{at}[{i}]"), out);
            }
        }
        _ => {
            out.insert(at.to_string(), j.clone());
        }
    }
}

fn json_value(j: &Json) -> Value {
    match j {
        Json::Null => Value::Str("null".into()),
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .unwrap_or(Value::Str(n.to_string())),
        Json::String(s) => Value::Str(s.clone()),
        Json::Array(xs) => Value::List(xs.iter().map(json_value).collect()),
        Json::Object(m) => Value::Obj(m.iter().map(|(k, v)| (k.clone(), json_value(v))).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drift_is_per_leaf_and_names_a_gone_object() {
        let addr = Address {
            typ: "k8s.deployment".into(),
            name: "web".into(),
        };
        let gone = Address {
            typ: "k8s.service".into(),
            name: "web".into(),
        };
        let baseline: BTreeMap<String, Json> = [
            (
                crate::state::key(&addr),
                json!({"spec": {"replicas": 3, "template": {"spec": {"containers": [{"name": "web", "image": "a"}]}}}}),
            ),
            (crate::state::key(&gone), json!({"spec": {}})),
        ]
        .into();
        let now: BTreeMap<Address, Json> = [(
            addr.clone(),
            json!({"spec": {"replicas": 5, "template": {"spec": {"containers": [{"name": "web", "image": "b"}]}}}}),
        )]
        .into();
        let got: Vec<(String, String, Value, Value)> = drift(&baseline, &now)
            .into_iter()
            .map(|d| (d.addr.typ, d.path, d.before, d.after))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "k8s.deployment".into(),
                    "spec.replicas".into(),
                    Value::Int(3),
                    Value::Int(5)
                ),
                (
                    "k8s.deployment".into(),
                    "spec.template.spec.containers[0].image".into(),
                    Value::Str("a".into()),
                    Value::Str("b".into())
                ),
                (
                    "k8s.service".into(),
                    "".into(),
                    Value::Str("present".into()),
                    Value::Str("absent".into())
                ),
            ]
        );
    }
}

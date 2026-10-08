//! Controller mode (DESIGN.org "Reactive inputs and controller mode"): the
//! second executor over the same evaluator. `dform controller run` waits for a
//! source it read (a table, a document, a program file) or the world to
//! change, then runs what `apply`
//! runs (refresh, evaluate, plan, the policy pass, ticks) with this hook in
//! it. Nothing in the language changes: the plan is the reconciliation.
//!
//! What the hook adds to a run:
//!
//! * The event. Its memo (`controller.json`, an object of the stack's
//!   store beside its state, so a bucket's too) holds the stamps
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
//!   release_approved(...)` over a table or a module of facts), and, unless the event
//!   is an input change, when `T.A` has drift that is neither
//!   `auto_reconcile(T, A, Path)` for each drifted path nor `approve(T, A)`.
//!   Held drift stays in the baseline, so it is held again at every world
//!   event until an input change or an approval releases it.
//! * Approvals (README "Approvals"): a deformation the policy pass says
//!   `requires_approval(r, Reason)` is held until a token for the plan's
//!   digest arrives, through the relation `approval/1` (the token's
//!   text) or as an object in the drop directory `approvals/` beside the
//!   state. While it is held the digest is published: a log line and
//!   `approval-pending.json` beside the state.
//! * The log: one line per event and per tick, `HH:MM:SS` (UTC) first.
//!   Events, holds and releases also go to the stack's audit log.

use crate::ast::{Atom, Span, Term};
use crate::ir::Address;
use crate::provider::{ActionKind, Plan};
use crate::spell;
use crate::store::{DROPS, MEMO, PENDING, Store};
use crate::value::Value;
use crate::watch::{self, Relation};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    /// A token arrived in the approvals drop directory.
    Approval,
    World,
    Resync,
}

/// What the controller remembers between runs, beside the stack's state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Memo {
    /// Per source (a table's relation, a program file's module), its stamp.
    inputs: BTreeMap<String, String>,
    /// The world file's stamp.
    world: String,
    /// The world as the last run accepted it, per `state::key`.
    baseline: BTreeMap<String, Json>,
    /// The approvals drop directory's stamp.
    #[serde(default)]
    drops: String,
    /// The sources the last run's tables read, by table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tables: Vec<(String, watch::Source)>,
}

/// The controller's part of a run (see the module doc).
#[derive(Default)]
pub struct Hook {
    /// The stack's program files, from the last run, and the sources its
    /// tables read.
    pub relations: Vec<Relation>,
    /// The relation `input p(..) from ..` declarations of this run.
    declared: Vec<Relation>,
    /// The sources the last run's tables read (`tables::Tables::sources`).
    tables: Vec<Relation>,
    world: Option<PathBuf>,
    /// The stack's store: the memo, the drop directory, the published
    /// digest.
    store: Option<Arc<dyn Store>>,
    /// The deployment of that store: the memo is written through it,
    /// fenced by its lease.
    deployment: Option<crate::store::Deployment>,
    /// The memo was kept in memory only (`save`): not yet written.
    unsaved: bool,
    memo: Option<Memo>,
    /// The stamps of the sources as this run read them.
    input_stamps: BTreeMap<String, String>,
    pub event: Option<Event>,
    drift: Vec<Drift>,
    /// Addresses held by this run.
    held: BTreeSet<Address>,
    /// The deployment's audit log.
    pub audit: Option<crate::audit::Log>,
    /// Whether this run published an approval's digest.
    published: bool,
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

/// The drop directory's objects, by name, with their contents.
fn drops(store: &dyn Store) -> Vec<(String, String)> {
    let prefix = format!("{DROPS}/");
    let mut out: Vec<(String, String)> = store
        .list(&prefix)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|k| {
            let o = store.get(&k).ok()??;
            let name = k.strip_prefix(&prefix)?.to_string();
            Some((name, String::from_utf8(o.bytes).ok()?))
        })
        .collect();
    out.sort();
    out
}

/// The drop directory's stamp: "" when it holds nothing.
fn drops_stamp(store: &dyn Store) -> String {
    use std::hash::{Hash, Hasher};
    let files = drops(store);
    if files.is_empty() {
        return String::new();
    }
    let mut h = std::hash::DefaultHasher::new();
    files.hash(&mut h);
    format!("{:016x}", h.finish())
}

impl Hook {
    /// The run has read its program files: stamp them (before reading, so
    /// a change during the run is seen by the next one).
    /// The last run's tables are stamped as their sources are now: a
    /// table whose file changed, or whose ref names another commit, is an
    /// input event.
    pub fn inputs(&mut self, relations: &[Relation]) {
        self.declared = relations.to_vec();
        self.relations = [relations, &self.tables].concat();
        self.input_stamps = stamps(&self.relations, |r| watch::stamp(&r.source));
    }

    fn table_sources(&self) -> Vec<(String, watch::Source)> {
        self.tables
            .iter()
            .map(|r| (r.pred.clone(), r.source.clone()))
            .collect()
    }

    /// The run has read its tables: the sources, each with its stamp as
    /// read (the digest of the file, the commit the ref named).
    pub fn tables(&mut self, read: &[(Relation, String)]) {
        self.tables = read.iter().map(|(r, _)| r.clone()).collect();
        self.relations = [&self.declared[..], &self.tables].concat();
        let now = stamps(&self.relations, |r| {
            match read
                .iter()
                .find(|(t, _)| t.pred == r.pred && t.source == r.source)
            {
                Some((_, s)) => s.clone(),
                None => watch::stamp(&r.source),
            }
        });
        self.input_stamps.extend(now);
    }

    /// The run knows where the stack lives, its store: load the memo, say
    /// what changed, and log the event (the world's path relative to
    /// `root`, the directory holding the state root, when it is under it).
    pub fn open(
        &mut self,
        deployment: &crate::store::Deployment,
        world: &Path,
        root: &Path,
    ) -> Result<()> {
        self.published = false;
        let store = deployment.store().clone();
        // A memo the last run could not write is newer than the stored one.
        let memo: Option<Memo> = match store.get(MEMO)? {
            _ if self.unsaved && self.memo.is_some() => self.memo.take(),
            Some(o) => Some(
                serde_json::from_slice(&o.bytes)
                    .with_context(|| format!("parse {}", store.locate(MEMO)))?,
            ),
            None => None,
        };
        // A controller that starts again (`--once`) knows its tables' last
        // sources from the memo.
        if let Some(m) = &memo
            && self.tables.is_empty()
            && !m.tables.is_empty()
        {
            self.tables = m
                .tables
                .iter()
                .map(|(pred, source)| Relation {
                    pred: pred.clone(),
                    arity: 0,
                    source: source.clone(),
                    span: Default::default(),
                })
                .collect();
            self.relations = [&self.declared[..], &self.tables].concat();
            let now = stamps(&self.tables, |r| watch::stamp(&r.source));
            self.input_stamps.extend(now);
        }
        self.world = Some(world.to_path_buf());
        self.held.clear();
        let event = match &memo {
            None => Event::Start,
            Some(m) => {
                let mut changed: Vec<String> = self
                    .relations
                    .iter()
                    .filter(|r| m.inputs.get(&r.pred) != self.input_stamps.get(&r.pred))
                    .map(|r| r.pred.clone())
                    .collect();
                // A table that read several sources is one relation.
                let mut seen = BTreeSet::new();
                changed.retain(|p| seen.insert(p.clone()));
                if !changed.is_empty() {
                    // One line per source: its relations changed together.
                    let mut by_source: BTreeMap<&watch::Source, Vec<&str>> = BTreeMap::new();
                    for r in self.relations.iter().filter(|r| changed.contains(&r.pred)) {
                        by_source.entry(&r.source).or_default().push(&r.pred);
                    }
                    for (source, preds) in by_source {
                        // A file by its path from the project, as the
                        // program names it.
                        let shown = match source {
                            watch::Source::File(p) => {
                                format!("file {}", relative_to(p, root).display())
                            }
                            s => s.to_string(),
                        };
                        log(format_args!("input {} changed ({shown})", preds.join(" ")));
                    }
                    Event::Input(changed)
                } else if m.drops != drops_stamp(store.as_ref()) {
                    Event::Approval
                } else if m.world != file_stamp(world) {
                    Event::World
                } else {
                    Event::Resync
                }
            }
        };
        let text = match &event {
            Event::Start => "event start".to_string(),
            Event::Input(names) => format!("event input {}", names.join(" ")),
            Event::Approval => format!(
                "event approval ({} changed)",
                relative_to(Path::new(&store.locate(DROPS)), root).display()
            ),
            Event::World => format!("event world {} changed", relative_to(world, root).display()),
            Event::Resync => "event resync".to_string(),
        };
        log(&text);
        if let Some(a) = &self.audit {
            a.append("controller", serde_json::json!({ "event": text }))?;
        }
        self.memo = memo;
        self.event = Some(event);
        self.store = Some(store);
        self.deployment = Some(deployment.clone());
        Ok(())
    }

    /// The tokens in the approvals drop directory.
    pub fn dropped_tokens(&self) -> Vec<String> {
        self.store
            .as_deref()
            .map(drops)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, t)| t)
            .collect()
    }

    /// After the gate of tick `tick`: the deformations `needs` names need an
    /// approval of the plan whose digest is `digest`. `approved` is the
    /// token that verified, if one did, and `refused` why the others did
    /// not (a token for another plan is left out). Unapproved, they are
    /// held and the digest is published; approved, the release is logged.
    pub fn approvals(
        &mut self,
        tick: usize,
        plan: &mut Plan,
        needs: &[(String, String)],
        digest: &str,
        approved: Option<&crate::approval::Verified>,
        refused: &[String],
    ) -> Result<()> {
        if needs.is_empty() {
            return Ok(());
        }
        for r in refused {
            log(format_args!("tick {tick}: approval refused: {r}"));
        }
        if let Some(v) = approved {
            log(format_args!(
                "tick {tick}: approved by {}: plan digest {digest}",
                v.statement.approver
            ));
            if let Some(a) = &self.audit {
                a.append(
                    "approval",
                    serde_json::json!({ "digest": digest, "attestation": v }),
                )?;
            }
            return Ok(());
        }
        // What the plan does to them, or did before the gate held them at
        // an earlier tick.
        let mut held = Vec::new();
        let candidates: BTreeSet<&Address> = plan
            .actions
            .iter()
            .filter(|a| !matches!(a.kind, ActionKind::Noop))
            .map(|a| &a.addr)
            .chain(&self.held)
            .collect();
        for (d, reason) in needs {
            for addr in &candidates {
                if addr.to_string() == *d {
                    log(format_args!(
                        "tick {tick}: proceed: held, needs approval ({reason}): {d}"
                    ));
                    held.push((*addr).clone());
                }
            }
        }
        if held.is_empty() {
            return Ok(());
        }
        let Some(dep) = self.deployment.clone() else {
            anyhow::bail!("internal: the controller published before it opened its store");
        };
        let doc = serde_json::json!({
            "digest": digest,
            "needs_approval": needs
                .iter()
                .map(|(d, r)| serde_json::json!({ "deformation": d, "reason": r }))
                .collect::<Vec<_>>(),
            "published": crate::approval::rfc3339(crate::approval::now()),
        });
        // Under the apply's lease, as the memo: a stale controller's digest
        // never replaces a newer one's.
        if !dep.put_fenced(
            PENDING,
            &serde_json::to_vec_pretty(&doc)?,
            "the approval digest",
        )? {
            anyhow::bail!(
                "the approval digest was not published: this run holds no lease on {}",
                dep.name()
            );
        }
        self.published = true;
        log(format_args!(
            "tick {tick}: approval needed: plan digest {digest} ({PENDING})"
        ));
        if let Some(a) = &self.audit {
            a.append(
                "controller",
                serde_json::json!({ "tick": tick, "held": needs
                    .iter()
                    .map(|(d, r)| serde_json::json!({ "deformation": d, "reason": r }))
                    .collect::<Vec<_>>(), "needs_approval": digest }),
            )?;
        }
        self.held.extend(held);
        plan.actions.retain(|a| !self.held.contains(&a.addr));
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
                    && a.args.iter().zip(args).all(|(t, s)| t.as_str() == Some(s))
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
                    "drift {}: {} -> {} ({verdict})",
                    if d.path.is_empty() {
                        format!("{} (object)", d.addr)
                    } else {
                        d.addr.attr(&d.path)
                    },
                    spell::value(&d.before),
                    spell::value(&d.after),
                ));
            }
        }
        let first = report.lines().next().unwrap_or("");
        if !first.ends_with(" is up to date") {
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
                .filter(|f| f.args[0].as_str() == Some(t) && f.args[1].as_str() == Some(a))
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
            log(format_args!("tick {tick}: proceed: held, {why}: {addr}"));
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
        let line = if undeformed && self.held.is_empty() {
            Some(format!("stack {stack} is up to date"))
        } else if !self.held.is_empty() {
            Some(format!(
                "stack {stack} has changes held: {}",
                self.held
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        } else {
            None
        };
        if let Some(line) = line {
            log(&line);
            if let Some(a) = &self.audit {
                a.append("controller", serde_json::json!({ "result": line }))?;
            }
        }
        // A digest published by an earlier run that holds nothing now.
        if !self.published
            && let Some(d) = &self.deployment
            && d.store().get(PENDING)?.is_some()
        {
            d.delete_fenced(PENDING, "the approval digest")?;
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
            drops: self.store.as_deref().map(drops_stamp).unwrap_or_default(),
            tables: self.table_sources(),
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
            drops: self.store.as_deref().map(drops_stamp).unwrap_or_default(),
            tables: self.table_sources(),
        })
    }

    /// Keep the memo, and write it under the run's lease
    /// (`Deployment::put_fenced`). After the apply's lease is gone (a
    /// failed run, in a store that fences) it is kept in memory, `unsaved`:
    /// the next run starts from it, not the stored one, and writes it once
    /// it holds the lease again ([`Hook::flush`]).
    fn save(&mut self, memo: Memo) -> Result<()> {
        let Some(dep) = &self.deployment else {
            return Ok(());
        };
        self.unsaved = !dep.put_fenced(
            MEMO,
            &serde_json::to_vec_pretty(&memo)?,
            "the controller's memo",
        )?;
        self.memo = Some(memo);
        Ok(())
    }

    /// The run holds the lease again: write a memo kept in memory only.
    pub fn flush(&mut self) -> Result<()> {
        if !self.unsaved {
            return Ok(());
        }
        let (Some(dep), Some(memo)) = (&self.deployment, &self.memo) else {
            return Ok(());
        };
        self.unsaved = !dep.put_fenced(
            MEMO,
            &serde_json::to_vec_pretty(memo)?,
            "the controller's memo",
        )?;
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
                .store
                .as_deref()
                .is_some_and(|s| memo.drops != drops_stamp(s))
            || stamps(&self.relations, |r| watch::stamp(&r.source))
                .iter()
                .any(|(p, s)| memo.inputs.get(p) != Some(s))
    }
}

/// Per relation, the stamps of its sources (a table may read several).
fn stamps(relations: &[Relation], stamp: impl Fn(&Relation) -> String) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, Vec<(&watch::Source, String)>> = BTreeMap::new();
    for r in relations {
        out.entry(r.pred.clone())
            .or_default()
            .push((&r.source, stamp(r)));
    }
    out.into_iter()
        .map(|(p, mut xs)| {
            xs.sort();
            xs.dedup();
            let s: Vec<String> = xs.into_iter().map(|(_, s)| s).collect();
            (p, s.join(","))
        })
        .collect()
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
                    Some(j) => crate::provider::json_to_value(j),
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
                leaves(v, &crate::ir::path_join(at, k), out);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The memo is written through the deployment's lease: a controller
    /// whose lease was taken over cannot overwrite the new holder's memo,
    /// and one whose apply has ended keeps it in memory only.
    #[test]
    fn a_stale_controllers_memo_is_refused_by_fencing() {
        use crate::store::{Deployment, LOCK, LeaseTimes, MemoryStore};
        let store = Arc::new(MemoryStore::new());
        let memo = |world: &str| {
            serde_json::to_vec(&Memo {
                world: world.into(),
                ..Memo::default()
            })
            .unwrap()
        };
        store
            .put(MEMO, &memo("first"), &crate::store::Cond::Any)
            .unwrap();
        let times = LeaseTimes {
            duration: std::time::Duration::from_secs(60),
            renewal: std::time::Duration::from_secs(15),
        };
        let a = Deployment::new(store.clone(), "app", times);
        a.load_state().unwrap();
        let ga = a.lock().unwrap();
        let mut hook = Hook::default();
        hook.open(&a, Path::new("world.json"), Path::new(""))
            .unwrap();
        // Another controller takes the lease over and writes its memo.
        store.break_lease(LOCK, "app").unwrap();
        let b = Deployment::new(store.clone(), "app", times);
        b.load_state().unwrap();
        let gb = b.lock().unwrap();
        assert!(b.put_fenced(MEMO, &memo("second"), "the memo").unwrap());
        let e = hook.failed().unwrap_err();
        assert!(
            format!("{e:#}").contains("the controller's memo was not written"),
            "{e:#}"
        );
        assert_eq!(store.get(MEMO).unwrap().unwrap().bytes, memo("second"));
        drop(ga);
        // A run that failed after its lease was released keeps its memo in
        // memory; the next run starts from it and writes it once it holds
        // the lease again.
        drop(gb);
        let mut hook = Hook::default();
        hook.open(&b, Path::new("world.json"), Path::new(""))
            .unwrap();
        hook.input_stamps.insert("p".into(), "third".into());
        hook.failed().unwrap();
        assert_eq!(store.get(MEMO).unwrap().unwrap().bytes, memo("second"));
        hook.open(&b, Path::new("world.json"), Path::new(""))
            .unwrap();
        assert_eq!(hook.memo.as_ref().unwrap().inputs["p"], "third");
        let gb = b.lock().unwrap();
        hook.flush().unwrap();
        let stored: Memo =
            serde_json::from_slice(&store.get(MEMO).unwrap().unwrap().bytes).unwrap();
        assert_eq!(stored.inputs["p"], "third");
        drop(gb);
    }

    /// The approval digest is published under the lease, as the memo is: a
    /// controller whose lease was taken over publishes nothing.
    #[test]
    fn a_stale_controllers_approval_digest_is_refused_by_fencing() {
        use crate::store::{Deployment, LOCK, LeaseTimes, MemoryStore};
        let store = Arc::new(MemoryStore::new());
        let times = LeaseTimes {
            duration: std::time::Duration::from_secs(60),
            renewal: std::time::Duration::from_secs(15),
        };
        let a = Deployment::new(store.clone(), "app", times);
        a.load_state().unwrap();
        let ga = a.lock().unwrap();
        let mut hook = Hook::default();
        hook.open(&a, Path::new("world.json"), Path::new(""))
            .unwrap();
        let addr = Address {
            typ: "net.vpc".into(),
            name: "main".into(),
        };
        let mut plan = Plan {
            actions: vec![crate::provider::Action {
                kind: ActionKind::Create,
                addr,
                changes: Vec::new(),
                on: BTreeSet::new(),
                kept: Vec::new(),
            }],
        };
        let needs = [(r#"net.vpc["main"]"#.to_string(), "every change".to_string())];
        store.break_lease(LOCK, "app").unwrap();
        let b = Deployment::new(store.clone(), "app", times);
        b.load_state().unwrap();
        let gb = b.lock().unwrap();
        let e = hook
            .approvals(1, &mut plan, &needs, "sha256:00", None, &[])
            .unwrap_err();
        assert!(
            format!("{e:#}").contains("the approval digest was not written"),
            "{e:#}"
        );
        assert!(store.get(PENDING).unwrap().is_none());
        drop((ga, gb));
    }

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

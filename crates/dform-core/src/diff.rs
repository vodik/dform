//! `dform diff --since REF` (R-15): what the applies since REF did, each
//! deformation with why it was planned, as the program was at that apply,
//! and which inputs and stated rows changed between the apply before REF
//! and now.
//!
//! The audit log holds no value (E DR-16), so it holds no explanation
//! either: each apply's program is evaluated again. Its `apply_start` entry
//! records the project's commit (and, when the tree was dirty, the tracked
//! files it had modified) and its `plan` entry the digests of the program
//! files and of the documents the run's tables read ([`documents`]); the
//! program is read at that commit (`git archive` into a scratch directory,
//! evaluated there by `dform __explain`) unless the program now is the
//! program then. Outside a repository the program now explains every
//! apply, and the report says so where a program file's or a document's
//! digest moved since.

use crate::ast::Term;
use crate::circuit::Leaf;
use crate::engine::EvalResult;
use crate::ir::Address;
use crate::query::Redactor;
use crate::report::Why;
use crate::report::tree::{Because, Printer, Site, cell_name};
use crate::value::Value;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What one evaluation says about the addresses `diff` asks for, every line
/// redacted: why each is wanted, each attribute's value and why, every
/// stated row and every input given.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub wants: BTreeMap<String, Vec<Because>>,
    pub attrs: BTreeMap<String, BTreeMap<String, Attr>>,
    /// A stated row's text -> where it is stated.
    pub rows: BTreeMap<String, String>,
    /// `--set k=v` and `--data k=v`, as given.
    pub inputs: BTreeSet<String>,
    /// Where each address is derived (R-79's `was FILE:LINE`).
    #[serde(default)]
    pub sites: BTreeMap<String, Site>,
    /// Every input and `let` cell (`input env`) and its value.
    #[serde(default)]
    pub cells: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Attr {
    pub value: String,
    pub why: Vec<Because>,
}

/// The snapshot of evaluation `res` for `addresses`.
pub fn snapshot(res: &EvalResult, redact: &Redactor, addresses: &[Address]) -> Snapshot {
    let p = Printer {
        circuit: &res.circuit,
        redact,
        all: false,
    };
    let mut s = Snapshot::default();
    let asked: BTreeSet<&Address> = addresses.iter().collect();
    for a in addresses {
        if let Some(w) = p.want(&res.rules, a) {
            s.wants.insert(a.to_string(), w);
        }
        if let Some(site) = p.want_site(&res.rules, a) {
            s.sites.insert(a.to_string(), site);
        }
    }
    for f in res.facts.iter().filter(|f| f.pred == "attr") {
        let [
            Term::Val(Value::Str(t)),
            Term::Val(Value::Str(n)),
            Term::Val(Value::Str(path)),
            Term::Val(v),
        ] = f.args.as_slice()
        else {
            continue;
        };
        if matches!(t.as_str(), "input" | "let") {
            s.cells.insert(cell_name(t, n, path), redact.surface(v));
        }
        let a = Address {
            typ: t.clone(),
            name: n.clone(),
        };
        if !asked.contains(&a) {
            continue;
        }
        let why = p.attr(&res.rules, &res.facts, &a, path).unwrap_or_default();
        s.attrs.entry(a.to_string()).or_default().insert(
            path.clone(),
            Attr {
                value: redact.surface(v),
                why,
            },
        );
    }
    for b in p.stated(&res.rules) {
        s.rows.insert(b.text, b.at.unwrap_or_default());
    }
    for l in res.circuit.leaves() {
        if let Leaf::Input { source } = l {
            s.inputs.insert(redact.text(source));
        }
    }
    s
}

/// One apply in the log: its `apply_start`, the `plan` entry it applied,
/// its actions and how it ended.
#[derive(Debug, Clone)]
pub struct Apply {
    pub seq: u64,
    pub time: String,
    pub who: String,
    pub commit: Option<String>,
    /// The tracked files modified at the commit when the apply ran: the
    /// tree was dirty.
    pub dirty: Vec<String>,
    pub plan: Option<Json>,
    pub actions: Vec<Action>,
    /// `ok`, `failed`, `declined`, or empty while it runs (or was killed).
    pub result: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub tick: u64,
    pub action: String,
    pub address: String,
    pub result: String,
}

/// The applies of a log's entries, in order.
pub fn applies(entries: &[Json]) -> Vec<Apply> {
    let s = |e: &Json, k: &str| e[k].as_str().unwrap_or_default().to_string();
    let mut out: Vec<Apply> = Vec::new();
    let mut plan = None;
    let mut open = false;
    for e in entries {
        match e["kind"].as_str() {
            Some("plan") => plan = Some(e.clone()),
            Some("apply_start") => {
                out.push(Apply {
                    seq: e["seq"].as_u64().unwrap_or_default(),
                    time: s(e, "time"),
                    who: s(e, "who"),
                    commit: e["commit"].as_str().map(str::to_string),
                    dirty: e["modified"]
                        .as_array()
                        .map(|fs| {
                            fs.iter()
                                .filter_map(|f| f.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    plan: plan.take(),
                    actions: Vec::new(),
                    result: String::new(),
                });
                open = true;
            }
            Some("action") if open => {
                if let Some(a) = out.last_mut() {
                    a.actions.push(Action {
                        tick: e["tick"].as_u64().unwrap_or_default(),
                        action: s(e, "action"),
                        address: s(e, "address"),
                        result: s(e, "result"),
                    });
                }
            }
            Some("apply_end") if open => {
                if let Some(a) = out.last_mut() {
                    a.result = s(e, "result");
                }
                open = false;
            }
            _ => {}
        }
    }
    out
}

/// The first of `applies` at or after `since`: a sequence number (a
/// number), a git commit an apply recorded (hex, a prefix of four or more),
/// or a time (RFC 3339, or a prefix of one: `2026-09-28`).
pub fn window(applies: &[Apply], since: &str) -> Result<usize> {
    let first =
        |keep: &dyn Fn(&Apply) -> bool| applies.iter().position(keep).unwrap_or(applies.len());
    if let Ok(n) = since.parse::<u64>() {
        return Ok(first(&|a| a.seq >= n));
    }
    let hex = since.len() >= 4 && since.bytes().all(|b| b.is_ascii_hexdigit());
    if hex {
        let at = since.to_ascii_lowercase();
        return match applies
            .iter()
            .position(|a| a.commit.as_deref().is_some_and(|c| c.starts_with(&at)))
        {
            Some(i) => Ok(i),
            None => bail!("diff --since {since}: no apply in the log was at a commit {since}"),
        };
    }
    if !since.starts_with(|c: char| c.is_ascii_digit()) {
        bail!(
            "diff --since {since}: expected a sequence number, a time (`2026-09-28`, RFC 3339) \
             or a git commit an apply recorded"
        );
    }
    Ok(first(&|a| a.time.as_str() >= since))
}

/// Which program explains an apply.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Program {
    /// The program as it is now.
    Now,
    /// The project at this git commit.
    Commit(String),
}

/// How `diff` evaluates a program at a commit: this executable, run in a
/// copy of the project at the commit, on the same deployment.
#[derive(Debug, Clone)]
pub struct Rerun {
    pub exe: PathBuf,
    /// The project root: the directory with `dform.toml`, else the
    /// program's own directory.
    pub top: PathBuf,
    /// The flags before the command (`dev --world W ...`), when this run
    /// has them.
    pub dev: Vec<String>,
    /// The program file relative to `top`, and the key values (`k=v`).
    pub target: Vec<String>,
    /// The stack's key inputs: the target names them, not `--set`.
    pub keys: Vec<String>,
}

/// The documents a run's tables read from files (`Tables::sources`), each
/// `{"path", "fnv64"}` of its bytes now, as the plan entry records them.
pub fn documents(read: &[(crate::watch::Relation, String)]) -> Vec<Json> {
    let paths: BTreeSet<&Path> = read
        .iter()
        .filter_map(|(r, _)| match &r.source {
            crate::watch::Source::File(p) => Some(p.as_path()),
            _ => None,
        })
        .collect();
    paths
        .into_iter()
        .map(|p| json!({ "path": p.display().to_string(), "fnv64": fnv64_of(p) }))
        .collect()
}

/// A file's digest as the plan entry records it; `missing` when it cannot
/// be read.
fn fnv64_of(p: &Path) -> String {
    std::fs::read(p)
        .map(|b| crate::zset::file::fnv64(&b))
        .unwrap_or_else(|_| "missing".into())
}

/// The documents `apply`'s plan entry recorded whose digest is not their
/// digest now.
fn documents_moved(apply: &Apply) -> Vec<String> {
    let Some(ds) = apply.plan.as_ref().and_then(|p| p["documents"].as_array()) else {
        return Vec::new();
    };
    ds.iter()
        .filter_map(|d| {
            let path = d["path"].as_str()?;
            (d["fnv64"].as_str() != Some(fnv64_of(Path::new(path)).as_str()))
                .then(|| path.to_string())
        })
        .collect()
}

/// The program that explains `apply`, and a note when it is not exactly
/// the program then. `files`: the program files now.
fn program_of(apply: &Apply, files: &[PathBuf], top: &Path) -> (Program, Option<String>) {
    let then: Option<Vec<String>> = apply.plan.as_ref().and_then(|p| {
        p["inputs"]["files"].as_array().map(|fs| {
            fs.iter()
                .map(|f| f["fnv64"].as_str().unwrap_or_default().to_string())
                .collect()
        })
    });
    let now: Option<Vec<String>> = files
        .iter()
        .map(|f| std::fs::read(f).ok().map(|b| crate::zset::file::fnv64(&b)))
        .collect();
    let same_now = then.is_some() && then == now;
    let Some(commit) = &apply.commit else {
        let moved = documents_moved(apply);
        return match (same_now, moved.is_empty()) {
            (true, true) => (Program::Now, None),
            (false, _) => (
                Program::Now,
                Some(
                    "not in a repository, and the program changed since this apply: explained \
                     by the program now"
                        .into(),
                ),
            ),
            (true, false) => (
                Program::Now,
                Some(format!(
                    "not in a repository, and {} changed since this apply: explained by the \
                     documents now",
                    moved.join(", ")
                )),
            ),
        };
    };
    let at_commit: Option<Vec<String>> = files
        .iter()
        .map(|f| {
            let dir = f.parent().filter(|d| !d.as_os_str().is_empty());
            let name = f.file_name()?.to_str()?;
            crate::git::show(dir.unwrap_or(Path::new(".")), commit, name)
                .ok()
                .map(|b| crate::zset::file::fnv64(&b))
        })
        .collect();
    let short = crate::report::short_id(commit);
    // The apply recorded its tree dirty: say so, by the files.
    if !apply.dirty.is_empty() {
        if same_now {
            return (Program::Now, None);
        }
        return (
            Program::Commit(commit.clone()),
            Some(format!(
                "the tree was dirty at this apply ({} modified): explained as committed at {short}",
                apply.dirty.join(", ")
            )),
        );
    }
    if then.is_none() || at_commit == then {
        // The commit is the program now when nothing under the project
        // moved since.
        let clean = crate::git::clean(top);
        if clean && crate::project::git_head(top).as_deref() == Some(commit.as_str()) {
            return (Program::Now, None);
        }
        return (Program::Commit(commit.clone()), None);
    }
    if same_now {
        return (Program::Now, None);
    }
    (
        Program::Commit(commit.clone()),
        Some(format!(
            "the program had changes not committed at this apply: explained as committed at {short}"
        )),
    )
}

/// A scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The snapshot of the program at `commit`, with the inputs `apply`'s plan
/// recorded: the project copied out of git and evaluated by `dform
/// __explain` there. A secret input is recorded only as its digest, so the
/// copy is evaluated without it.
fn at_commit(r: &Rerun, commit: &str, apply: &Apply, addresses: &[Address]) -> Result<Snapshot> {
    let dir = std::env::temp_dir().join(format!(
        "dform-diff-{}-{}",
        std::process::id(),
        crate::report::short_id(commit)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let scratch = Scratch(dir);
    // The project's tree at the commit, written out (no archive, no tar).
    crate::git::checkout(&r.top, commit, &scratch.0)?;
    explain_in(r, &scratch.0, apply, addresses)
}

/// The snapshot of the program in `dir` for `addresses`, evaluated by
/// `dform __explain` there with the inputs `apply`'s plan recorded.
fn explain_in(r: &Rerun, dir: &Path, apply: &Apply, addresses: &[Address]) -> Result<Snapshot> {
    let plan = apply.plan.as_ref();
    let recorded = |k: &str| -> Vec<String> {
        plan.and_then(|p| p["inputs"][k].as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    };
    let mut cmd = Command::new(&r.exe);
    cmd.arg("-C").arg(dir).args(&r.dev).arg("__explain");
    cmd.args(&r.target);
    for kv in recorded("set") {
        let k = kv.split_once('=').map_or(kv.as_str(), |(k, _)| k);
        if !r.keys.iter().any(|x| x == k) {
            cmd.arg("--set").arg(kv);
        }
    }
    for kv in recorded("data") {
        cmd.arg("--data").arg(kv);
    }
    for a in addresses {
        cmd.arg("--address").arg(a.to_string());
    }
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .context("run dform __explain")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "{}",
            err.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("it failed")
                .trim_start_matches("Error: ")
        );
    }
    serde_json::from_slice(&out.stdout).context("dform __explain: not a snapshot")
}

/// The snapshot of the program the last apply in `entries` ran, for
/// `addresses` (R-79's `because`): at the commit it recorded, or here
/// with the inputs it recorded. `None` when the log has no apply that
/// ended well, when that apply ran the program now with the inputs now
/// (`set`, `data`: there is nothing to compare), or when its program
/// cannot be read or evaluated.
pub fn last_apply(
    entries: &[Json],
    files: &[PathBuf],
    rerun: &Rerun,
    set: &[String],
    data: &[String],
    addresses: &[Address],
) -> Option<Snapshot> {
    let all = applies(entries);
    let last = all.iter().rev().find(|a| a.result == "ok")?;
    match program_of(last, files, &rerun.top).0 {
        Program::Commit(c) => at_commit(rerun, &c, last, addresses).ok(),
        Program::Now => {
            let recorded = |k: &str| -> BTreeSet<String> {
                last.plan
                    .as_ref()
                    .and_then(|p| p["inputs"][k].as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            };
            let given = |xs: &[String]| xs.iter().cloned().collect::<BTreeSet<_>>();
            if (recorded("set"), recorded("data")) == (given(set), given(data))
                || !documents_moved(last).is_empty()
            {
                return None;
            }
            explain_in(rerun, &rerun.top, last, addresses).ok()
        }
    }
}

/// The leaf change `kind` of `addr` turns on since the last apply (R-79),
/// `then` the snapshot of the last apply's program and `now` this plan's:
/// for a delete, the first leaf of the derivation the last apply had that
/// is false now (a row gone, an input changed); for an update or a
/// replace, the same of each changed attribute's winning derivation, else
/// its first leaf new since; for a create, the first leaf new since the
/// last apply. `None` when no leaf says.
pub fn because_since(
    kind: &str,
    addr: &str,
    paths: &[String],
    then: &Snapshot,
    now: &Snapshot,
) -> Option<String> {
    let gone = |bs: &[Because]| bs.iter().find_map(|b| moved(b, then, now, false));
    let new = |bs: &[Because]| bs.iter().find_map(|b| moved(b, now, then, true));
    match kind {
        "delete" => gone(then.wants.get(addr)?),
        "create" | "adopt" => new(now.wants.get(addr)?),
        _ => {
            // The attribute a change path is under (`tags` of `tags.team`).
            let under = |attrs: Option<&BTreeMap<String, Attr>>, path: &str| {
                attrs?
                    .iter()
                    .filter(|(k, _)| {
                        path == k.as_str()
                            || path
                                .strip_prefix(k.as_str())
                                .is_some_and(|r| r.starts_with(['.', '[']))
                    })
                    .max_by_key(|(k, _)| k.len())
                    .map(|(_, a)| a.why.clone())
            };
            paths.iter().find_map(|p| {
                under(then.attrs.get(addr), p)
                    .and_then(|w| gone(&w))
                    .or_else(|| under(now.attrs.get(addr), p).and_then(|w| new(&w)))
            })
        }
    }
}

/// Leaf `b` of snapshot `from`, when it does not hold in `to`: what
/// changed, said from `to`'s side when `gained` (a create's new leaf),
/// else from `from`'s (a delete's lost one).
fn moved(b: &Because, from: &Snapshot, to: &Snapshot, gained: bool) -> Option<String> {
    let (old, new) = if gained { (to, from) } else { (from, to) };
    match b.kind.as_str() {
        "fact" => {
            let text = b.text.split("   (").next().unwrap_or(&b.text);
            // A cell's value: `input env = "prod"`.
            if let Some((cell, v)) = text.split_once(" = ")
                && (cell.starts_with("input ") || cell.starts_with("let "))
            {
                let other = to.cells.get(cell)?;
                if other == v {
                    return None;
                }
                let (was, is) = if gained {
                    (other.as_str(), v)
                } else {
                    (v, other.as_str())
                };
                return Some(format!("{cell} is now {is} (was {was})"));
            }
            // A stated row: one both snapshots can say, stated by one only.
            if !from.rows.contains_key(text) || to.rows.contains_key(text) {
                return None;
            }
            let at = b.at.as_deref().unwrap_or_default();
            let file = at.rsplit_once(':').map_or(at, |(f, _)| f);
            let program = file.ends_with(".df");
            Some(match (gained, program) {
                (true, true) => format!("{at} now states {text}"),
                (true, false) => format!("{at} gained the row {text}"),
                (false, true) => format!("{file} no longer states {text}"),
                (false, false) => format!("{file} no longer has the row {text}"),
            })
        }
        "input" => {
            if to.inputs.contains(&b.text) {
                return None;
            }
            // A `--set`: by the input's value, however it is given now
            // (a key's value may come from the target, not a flag).
            if let Some((k, _)) = b
                .text
                .strip_prefix("--set ")
                .and_then(|kv| kv.split_once('='))
            {
                let cell = format!("input {k}");
                let (was, is) = (old.cells.get(&cell)?, new.cells.get(&cell)?);
                return (was != is).then(|| format!("{cell} is now {is} (was {was})"));
            }
            let key = b.text.split_once('=').map_or(b.text.as_str(), |(k, _)| k);
            let other = |s: &Snapshot| {
                s.inputs
                    .iter()
                    .find(|i| i.split_once('=').is_some_and(|(k, _)| k == key))
                    .cloned()
            };
            Some(match (other(old), other(new)) {
                (Some(was), Some(is)) => format!("{is} (was {was})"),
                (None, Some(is)) => format!("{is} is given now"),
                (Some(was), None) => format!("{was} is no longer given"),
                (None, None) => return None,
            })
        }
        _ => None,
    }
}

/// One apply as `diff` reports it.
#[derive(Debug, Clone)]
pub struct Explained {
    pub apply: Apply,
    pub program: Program,
    pub note: Option<String>,
    pub deformations: Vec<(Action, Vec<Because>)>,
}

/// What changed between the apply before the window and now.
#[derive(Debug, Clone, Default)]
pub struct Changed {
    /// (text, where), stated now and not then.
    pub added: Vec<(String, String)>,
    /// (text, where), stated then and not now.
    pub removed: Vec<(String, String)>,
    /// (input, before, after), `--set k` or `--data k`.
    pub inputs: Vec<(String, Option<String>, Option<String>)>,
}

#[derive(Debug, Clone)]
pub struct Diff {
    pub since: String,
    pub applies: Vec<Explained>,
    /// The apply before the window, what `changed` compares now with.
    pub before: Option<Apply>,
    pub changed: Option<Changed>,
}

/// The diff of a log's `entries` since `since`. `files`: the program
/// files now; `now`: the snapshot of the program now for some addresses.
pub fn diff(
    entries: &[Json],
    since: &str,
    files: &[PathBuf],
    rerun: &Rerun,
    now: &dyn Fn(&[Address]) -> Snapshot,
) -> Result<Diff> {
    let all = applies(entries);
    let first = window(&all, since)?;
    let addresses: Vec<Address> = all[first..]
        .iter()
        .flat_map(|a| &a.actions)
        .filter_map(|a| crate::ir::parse_resource_address(&a.address).ok())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let current = now(&addresses);
    // Every snapshot once: by program and the inputs its plan recorded.
    let mut snaps: BTreeMap<(Program, String), (Snapshot, Option<String>)> = BTreeMap::new();
    let mut snap = |a: &Apply| -> (Program, Snapshot, Option<String>) {
        let (program, note) = program_of(a, files, &rerun.top);
        let inputs = a
            .plan
            .as_ref()
            .map(|p| p["inputs"].to_string())
            .unwrap_or_default();
        let key = (program.clone(), inputs);
        let (s, failed) = snaps
            .entry(key)
            .or_insert_with(|| match &program {
                Program::Now => (current.clone(), None),
                Program::Commit(c) => match at_commit(rerun, c, a, &addresses) {
                    Ok(s) => (s, None),
                    Err(e) => (
                        current.clone(),
                        Some(format!(
                            "the program at {} could not be evaluated ({e:#}): explained by the \
                             program now",
                            crate::report::short_id(c)
                        )),
                    ),
                },
            })
            .clone();
        let program = if failed.is_some() {
            Program::Now
        } else {
            program
        };
        (program, s, failed.or(note))
    };
    let before = first.checked_sub(1).map(|i| all[i].clone());
    let mut prev = before.as_ref().map(|a| snap(a).1);
    let base = prev.clone();
    let mut out = Vec::new();
    for a in &all[first..] {
        let (program, s, note) = snap(a);
        let deformations = a
            .actions
            .iter()
            .filter(|x| x.action != "no-op")
            .map(|x| (x.clone(), because(x, &s, prev.as_ref())))
            .collect();
        out.push(Explained {
            apply: a.clone(),
            program,
            note,
            deformations,
        });
        prev = Some(s);
    }
    let changed = base.map(|b| changed(&b, &current));
    Ok(Diff {
        since: since.to_string(),
        applies: out,
        before,
        changed,
    })
}

fn state(text: &str) -> Because {
    Because {
        kind: "state".into(),
        at: None,
        text: text.into(),
    }
}

/// Why action `x` was planned, by snapshot `s` of its apply's program and
/// `prev` of the apply before it.
fn because(x: &Action, s: &Snapshot, prev: Option<&Snapshot>) -> Vec<Because> {
    let want = |s: &Snapshot| s.wants.get(&x.address).cloned();
    match x.action.as_str() {
        "delete" | "delete_deposed" => {
            let mut out = vec![state("no statement derives it at this apply")];
            if let Some(w) = prev.and_then(want) {
                out.push(state("the apply before derived it:"));
                out.extend(w);
            }
            out
        }
        "update" | "drift" | "pending" | "replace" => {
            let attrs = s.attrs.get(&x.address);
            let before = prev.and_then(|p| p.attrs.get(&x.address));
            let mut out: Vec<Because> = Vec::new();
            if let (Some(attrs), Some(before)) = (attrs, before) {
                for (path, a) in attrs {
                    if before.get(path).is_some_and(|b| b.value == a.value) {
                        continue;
                    }
                    for b in &a.why {
                        if !out.contains(b) {
                            out.push(b.clone());
                        }
                    }
                }
                // Nothing compares different: a secret is compared by its
                // label, and the world may have moved. Every attribute's.
                if out.is_empty() {
                    out.push(state(
                        "its values compare equal to the apply before's (a secret, or the world \
                         moved): all its attributes",
                    ));
                    // The attributes the program writes: a provider's
                    // computed one has no place in it.
                    let written = attrs
                        .values()
                        .filter(|a| a.why.iter().any(|b| b.at.is_some()));
                    for b in written.flat_map(|a| &a.why) {
                        if !out.contains(b) {
                            out.push(b.clone());
                        }
                    }
                }
            }
            if out.is_empty() {
                out = want(s).unwrap_or_default();
            }
            out
        }
        _ => want(s).unwrap_or_else(|| vec![state("no statement derives it now")]),
    }
}

/// The rows and inputs of `then` that `now` does not have, and the other
/// way.
fn changed(then: &Snapshot, now: &Snapshot) -> Changed {
    let rows = |a: &Snapshot, b: &Snapshot| -> Vec<(String, String)> {
        a.rows
            .iter()
            .filter(|(t, _)| !b.rows.contains_key(*t))
            .map(|(t, at)| (t.clone(), at.clone()))
            .collect()
    };
    let inputs = |s: &Snapshot| -> BTreeMap<String, String> {
        s.inputs
            .iter()
            .map(|i| match i.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (i.clone(), String::new()),
            })
            .collect()
    };
    let (a, b) = (inputs(then), inputs(now));
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    Changed {
        added: rows(now, then),
        removed: rows(then, now),
        inputs: keys
            .into_iter()
            .filter(|k| a.get(*k) != b.get(*k))
            .map(|k| (k.clone(), a.get(k).cloned(), b.get(k).cloned()))
            .collect(),
    }
}

fn marker(action: &str) -> &'static str {
    match action {
        "create" => "+",
        "adopt" => ">",
        "update" | "drift" | "pending" => "~",
        "replace" => "-/+",
        "delete" | "delete_deposed" => "-",
        _ => "=",
    }
}

fn apply_line(a: &Apply) -> String {
    let at = a
        .commit
        .as_deref()
        .map(|c| format!(" at {}", crate::report::short_id(c)))
        .unwrap_or_default();
    let result = if a.result.is_empty() {
        "running or interrupted"
    } else {
        &a.result
    };
    format!("apply {} {} by {}{at}: {result}", a.seq, a.time, a.who)
}

impl Diff {
    /// The diff as text, each change saying as much of why it was planned
    /// as `why` asks (R-111): `-q` the changes alone, the default and `-v`
    /// the first line of each change's derivation, `-vv` every line.
    pub fn text(&self, why: Why) -> String {
        let level = why;
        let mut out = String::new();
        if self.applies.is_empty() {
            out.push_str(&format!("no apply since {}\n", self.since));
        }
        for e in &self.applies {
            out.push_str(&apply_line(&e.apply));
            out.push('\n');
            if let Some(n) = &e.note {
                out.push_str(&format!("  note: {n}\n"));
            }
            if e.deformations.is_empty() {
                out.push_str("  no changes\n");
            }
            for (x, why) in &e.deformations {
                let failed = if x.result == "ok" {
                    String::new()
                } else {
                    format!("  ({})", x.result)
                };
                let addr = match level {
                    Why::None => x.address.clone(),
                    _ => crate::report::address_text(&x.address),
                };
                out.push_str(&format!("{} {addr}{failed}\n", marker(&x.action)));
                let shown = match level {
                    Why::None => 0,
                    Why::Line | Why::How => 1,
                    Why::Full => why.len(),
                };
                for b in why.iter().take(shown) {
                    out.push_str(&format!("  {}\n", b.line()));
                }
            }
        }
        match (&self.before, &self.changed) {
            (Some(b), Some(c)) => {
                out.push_str(&format!("changed since apply {} {}:\n", b.seq, b.time));
                if c.added.is_empty() && c.removed.is_empty() && c.inputs.is_empty() {
                    out.push_str("  no input or stated row\n");
                }
                for (t, at) in &c.added {
                    out.push_str(&format!("  + {at}  {t}\n"));
                }
                for (t, at) in &c.removed {
                    out.push_str(&format!("  - {at}  {t}\n"));
                }
                for (k, a, b) in &c.inputs {
                    let v = |x: &Option<String>| x.clone().unwrap_or_else(|| "(none)".into());
                    out.push_str(&format!("  {k}: {} -> {}\n", v(a), v(b)));
                }
            }
            _ => out.push_str(&format!("no apply before {} to compare with\n", self.since)),
        }
        out
    }

    pub fn json(&self) -> Json {
        let apply = |a: &Apply| {
            json!({
                "seq": a.seq,
                "time": a.time,
                "who": a.who,
                "commit": a.commit,
                "result": a.result,
            })
        };
        let row = |(t, at): &(String, String)| json!({ "at": at, "text": t });
        json!({
            "since": self.since,
            "applies": self.applies.iter().map(|e| {
                let mut j = apply(&e.apply);
                j["program"] = match &e.program {
                    Program::Now => json!("now"),
                    Program::Commit(c) => json!(c),
                };
                j["note"] = json!(e.note);
                j["changes"] = e.deformations.iter().map(|(x, why)| json!({
                    "action": x.action,
                    "address": x.address,
                    "tick": x.tick,
                    "result": x.result,
                    "why": why,
                })).collect();
                j
            }).collect::<Vec<_>>(),
            "changed": match (&self.before, &self.changed) {
                (Some(b), Some(c)) => json!({
                    "since": apply(b),
                    "added": c.added.iter().map(row).collect::<Vec<_>>(),
                    "removed": c.removed.iter().map(row).collect::<Vec<_>>(),
                    "inputs": c.inputs.iter().map(|(k, a, b)| json!({
                        "input": k,
                        "before": a,
                        "after": b,
                    })).collect::<Vec<_>>(),
                }),
                _ => Json::Null,
            },
        })
    }
}

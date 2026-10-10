//! `dform status [TARGET]` (R-203): each of a deployment's objects whose
//! provider answers Health for its type, as that provider judges it now,
//! one line each, then a summary. A separate question from the plan and
//! the apply, asked of the providers at no other time: nothing of it is
//! in the plan, the apply, state or the language. The program is
//! evaluated so its providers are configured as a plan configures them;
//! no object is planned or changed.
//!
//! The exit status is 0 when every object judged is healthy or suspended,
//! 1 otherwise (one progressing, degraded or unknown). An object of a
//! type no provider judges prints `-` and counts for neither. There is no
//! `--wait` or `--watch`: `status` is one look; controller mode serves the
//! same answer continuously.

use super::evaluated::Evaluated;
use super::{Cli, Dependency, Held, Outcome};
use crate::address::Address;
use crate::plugin::pb::HealthState;
use crate::report::table::{Cell, Table};
use crate::report::{self, Paint};
use anyhow::Result;
use serde_json::{Value as Json, json};

/// `dform status [TARGET] [--json]`.
#[derive(Debug, Clone)]
pub(super) struct Status {
    pub(super) json: bool,
}

/// The states, in the order the summary says them.
const STATES: [HealthState; 5] = [
    HealthState::Healthy,
    HealthState::Progressing,
    HealthState::Degraded,
    HealthState::Suspended,
    HealthState::Unknown,
];

/// A state as a word: `degraded`.
fn word(s: HealthState) -> String {
    s.as_str_name().to_ascii_lowercase()
}

/// How a state is painted.
fn paint(s: HealthState) -> Paint {
    match s {
        HealthState::Healthy => Paint::Create,
        HealthState::Progressing => Paint::Update,
        HealthState::Degraded => Paint::Error,
        HealthState::Suspended => Paint::Dim,
        HealthState::Unknown | HealthState::Unspecified => Paint::Null,
    }
}

/// One object: its address, its provider, and its health (`None`: its
/// type has none).
struct Object {
    addr: Address,
    /// Its name in the cloud, where dform gave it one (R-189).
    named: Option<String>,
    provider: String,
    health: Option<(HealthState, String)>,
}

/// A deployment's objects, judged.
struct Report {
    deployment: String,
    /// Whether it was ever applied.
    applied: bool,
    objects: Vec<Object>,
}

impl Report {
    fn of(run: &Evaluated) -> Result<Report> {
        let st = &run.ev.st;
        let mut judged = run.ev.evaluator.backend.health(st)?;
        let objects = st
            .resources
            .iter()
            .filter_map(|(k, e)| {
                let addr = crate::state::parse_key(k)?;
                let health = judged.remove(&addr).map(|h| {
                    let s = HealthState::try_from(h.state).unwrap_or(HealthState::Unknown);
                    let s = match s {
                        HealthState::Unspecified => HealthState::Unknown,
                        s => s,
                    };
                    (s, h.reason)
                });
                Some(Object {
                    addr,
                    named: e.name.clone(),
                    provider: e.provider.clone(),
                    health,
                })
            })
            .collect();
        Ok(Report {
            deployment: run.cx.deployment.clone(),
            applied: run.cx.dep.has_state()?,
            objects,
        })
    }

    /// How many objects are in state `s`.
    fn count(&self, s: HealthState) -> usize {
        self.objects
            .iter()
            .filter(|o| o.health.as_ref().is_some_and(|(h, _)| *h == s))
            .count()
    }

    fn without(&self) -> usize {
        self.objects.iter().filter(|o| o.health.is_none()).count()
    }

    /// Every object judged is healthy or suspended.
    fn settled(&self) -> bool {
        self.objects.iter().all(|o| {
            o.health
                .as_ref()
                .is_none_or(|(s, _)| matches!(s, HealthState::Healthy | HealthState::Suspended))
        })
    }

    /// `status: 12 healthy, 1 degraded, 3 without health`.
    fn summary(&self) -> String {
        if !self.applied {
            return "status: never applied, no objects".into();
        }
        let mut parts: Vec<String> = STATES
            .iter()
            .map(|&s| (self.count(s), word(s)))
            .filter(|(n, _)| *n > 0)
            .map(|(n, w)| format!("{n} {w}"))
            .collect();
        if self.without() > 0 {
            parts.push(format!("{} without health", self.without()));
        }
        match parts.is_empty() {
            true => "status: no objects".into(),
            false => format!("status: {}", parts.join(", ")),
        }
    }

    /// One line per object: its address (and the name dform gave it), its
    /// state, the reason.
    fn table(&self) -> Table {
        let mut t = Table::new(["resource", "health", "reason"]);
        for o in &self.objects {
            let (state, reason) = match &o.health {
                Some((s, r)) => (Cell::text(word(*s)).painted(paint(*s)), r.clone()),
                None => (Cell::text("-"), String::new()),
            };
            let at = match &o.named {
                Some(n) => format!("{}  named {n}", report::address(&o.addr)),
                None => report::address(&o.addr),
            };
            t.push(vec![Cell::text(at), state, Cell::text(reason)]);
        }
        t
    }

    /// What `--json` says: the deployment, each object, and the counts.
    fn json(&self) -> Json {
        let objects: Vec<Json> = self
            .objects
            .iter()
            .map(|o| {
                json!({
                    "address": report::address(&o.addr),
                    "type": o.addr.typ,
                    "name": o.addr.name,
                    "remote_name": o.named,
                    "provider": o.provider,
                    "health": o.health.as_ref().map(|(s, _)| word(*s)),
                    "reason": o.health.as_ref().map(|(_, r)| r),
                })
            })
            .collect();
        let mut counts = serde_json::Map::new();
        for s in STATES {
            counts.insert(word(s), json!(self.count(s)));
        }
        counts.insert("without_health".into(), json!(self.without()));
        json!({
            "deployment": self.deployment,
            "applied": self.applied,
            "settled": self.settled(),
            "objects": objects,
            "summary": counts,
        })
    }
}

impl Status {
    /// `dform status TARGET`: the lines and the summary, or `--json`, on
    /// stdout (or held, for the project's status).
    pub(super) fn run(&self, run: &Evaluated) -> Result<Outcome> {
        let r = Report::of(run)?;
        let held = &run.cx.cli.held;
        match self.json {
            true => held.print(&format!("{}\n", serde_json::to_string_pretty(&r.json())?)),
            false => {
                held.print(&r.table().pairs(&run.cx.cli.table));
                held.print(&format!("{}\n", r.summary()));
            }
        }
        Ok(match r.settled() {
            true => Outcome::Done,
            false => Outcome::Failed,
        })
    }
}

/// The project's status: each deployment the project module lists, in
/// apply order, headed `== NAME`; `--json` an array of each one's. The
/// worst outcome is the run's.
pub(super) fn matrix(
    cli: &Cli,
    order: &[Dependency],
    of: &dyn Fn(&Dependency, super::Cmd) -> Cli,
) -> Result<Outcome> {
    let super::Cmd::Status(s) = &cli.cmd else {
        anyhow::bail!("internal: a status");
    };
    let mut outcome = Outcome::Done;
    let mut docs = Vec::new();
    for d in order.iter().filter(|d| d.root) {
        let mut dep = of(d, cli.cmd.clone());
        dep.held = Held::new();
        let result = super::run(dep.clone(), None);
        let text = dep.held.take().text;
        if !s.json {
            super::matrix::head(cli, &d.name, "");
            print!("{text}");
        } else if let Ok(doc) = serde_json::from_str::<Json>(&text) {
            docs.push(doc);
        }
        match result {
            Ok(Outcome::Done) => {}
            Ok(o) => outcome = worst(outcome, o),
            Err(e) => {
                super::matrix::say(cli, &e);
                outcome = Outcome::Failed;
            }
        }
    }
    if s.json {
        println!("{}", serde_json::to_string_pretty(&docs)?);
    }
    Ok(outcome)
}

/// The worse of two outcomes: a failure, else the first that is not done.
fn worst(a: Outcome, b: Outcome) -> Outcome {
    match (a, b) {
        (Outcome::Done, b) => b,
        (a, _) => a,
    }
}

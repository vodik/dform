//! The executor under random chaos (a model test). A seed picks a random
//! program over the fake schema and a sequence of its versions (refs,
//! force_new cidrs, `prevent_destroy`, `create_before_destroy`, `moved`),
//! a starting world (empty, or with objects dform does not manage), and a
//! schedule: applies and plan-file applies, each with chaos knobs (fail,
//! timeout, crash, read-lag, mutate, latency, fresh-ids, `stop-after=N`),
//! `--parallel`, and the order the calls in flight answer in (the direct
//! backend's clock, or a seed); drift between a plan and its apply; the
//! program moving to its next version. Every run is `dform::cli` in this
//! process over the direct backend, the mock behind a recorder that sees
//! every call. Then one apply without chaos settles it.
//!
//! Invariants:
//!
//! * identity: after every Apply call that answers, the state on disk at
//!   the provider's next call holds what it answered (the executor
//!   persists per completion, not per tick);
//! * foreign: no Apply call names an object dform does not manage;
//! * overreach: an apply of a plan file touches only addresses the file
//!   lists (its deformations and a replace's dependents), and what chaos
//!   `mutate` drifted;
//! * plan-file: `apply PLAN` refuses at tick 1 exactly when a fresh plan's
//!   delta differs from the file's (`PlanFile::stale`);
//! * repeat-create: no Create meets an object already in the world;
//! * orphan: after every run, every object in the world dform made is in
//!   state (or deposed);
//! * settle: the last apply ends undeformed, or stopped on a deny.
//!
//! A Create or Replace that may have taken effect without dform hearing
//! its answer (chaos `timeout`, `crash`, or in flight when `stop-after`
//! stopped dform) leaves an object state does not map, only records as
//! uncertain, until the next apply asks the provider for what its
//! idempotency key made. That orphan is excused while state records it;
//! once the record is resolved the object must be in state or gone, and the
//! next apply creating it again is not excused (repeat-create).
//!
//! A failure prints the seed and the schedule minimized (delta debugging
//! over its steps, then over each step's knobs), and how to replay it:
//!
//!   DFORM_MODEL_SEED=N [DFORM_MODEL_SCHEDULE='...'] cargo test --test model
//!
//! `DFORM_MODEL_SEEDS=K` (default 300, about 30s in a debug build) runs
//! seeds 0..K; `DFORM_MODEL_START` offsets them (the nightly workflow runs
//! 10^4). `DFORM_MODEL_VERBOSE` prints every run's result and Apply calls.

mod common;

use dform::plugin::Launch;
use dform::plugin::backend::{Call, CallError, Provider, Reply, Ticket};
use dform::plugin::link::Link;
use dform::plugin::pb;
use dform::plugin::queue::{Order, Queue};
use dform::zset::file::PlanFile;
use serde_json::{Value as Json, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

// ---------------------------------------------------------------- random

/// splitmix64: the model's only source of randomness.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> Option<&'a T> {
        if xs.is_empty() {
            return None;
        }
        Some(&xs[self.below(xs.len())])
    }
}

// --------------------------------------------------------------- programs

const VPC: &str = "net.vpc";
const SUBNET: &str = "net.subnet";
const VM: &str = "compute.vm";
const DB: &str = "db.postgres";

#[derive(Debug, Clone)]
struct Res {
    typ: &'static str,
    name: String,
    /// A vpc's or a subnet's cidr, force_new.
    cidr: Option<u8>,
    tier: u8,
    /// A ref to an earlier resource: its address and the attribute read.
    to: Option<(&'static str, String, &'static str)>,
    prevent_destroy: bool,
    create_before_destroy: bool,
}

/// One version of the program.
#[derive(Debug, Clone, Default)]
struct Version {
    res: Vec<Res>,
    /// `moved(T, Old, New)`, for the renames since the last version.
    moved: Vec<(&'static str, String, String)>,
}

impl Version {
    fn text(&self) -> String {
        let mut out = String::from("edition 2026\n\n");
        for r in &self.res {
            let mut attrs = Vec::new();
            if let Some((typ, name, attr)) = &r.to {
                let key = match *attr {
                    "id" => "parent_id",
                    _ => "host",
                };
                attrs.push(format!("{key} = ref({typ}, \"{name}\", \"{attr}\")"));
            }
            match (r.typ, r.cidr) {
                (VPC, Some(c)) => attrs.push(format!("cidr = \"10.{c}.0.0/16\"")),
                (SUBNET, Some(c)) => attrs.push(format!("cidr = \"10.0.{c}.0/24\"")),
                _ => {}
            }
            attrs.push(format!("tier = \"t{}\"", r.tier));
            out.push_str(&format!(
                "resource {} {} {{ {} }}\n",
                r.typ,
                r.name,
                attrs.join(", ")
            ));
        }
        for r in &self.res {
            for (on, what) in [
                (r.prevent_destroy, "prevent_destroy"),
                (r.create_before_destroy, "create_before_destroy"),
            ] {
                if on {
                    out.push_str(&format!(
                        "lifecycle(\"{}\", \"{}\", \"{what}\")\n",
                        r.typ, r.name
                    ));
                }
            }
        }
        for (typ, old, new) in &self.moved {
            out.push_str(&format!("moved(\"{typ}\", \"{old}\", \"{new}\")\n"));
        }
        out
    }

    fn addrs(&self) -> Vec<String> {
        self.res
            .iter()
            .map(|r| format!("{}/{}", r.typ, r.name))
            .collect()
    }
}

struct Names(usize);

impl Names {
    fn fresh(&mut self) -> String {
        self.0 += 1;
        format!("r{}", self.0)
    }
}

fn new_res(rng: &mut Rng, names: &mut Names, earlier: &[Res]) -> Res {
    let typ = *rng.pick(&[VPC, VPC, SUBNET, SUBNET, VM, DB]).unwrap();
    let wants: &[(&str, &str)] = match typ {
        SUBNET => &[(VPC, "id")],
        VM => &[(SUBNET, "id"), (DB, "endpoint"), (VPC, "id")],
        DB => &[(VPC, "id")],
        _ => &[],
    };
    let targets: Vec<(&'static str, String, &'static str)> = earlier
        .iter()
        .filter_map(|e| {
            let (_, attr) = wants.iter().find(|(t, _)| *t == e.typ)?;
            let attr: &'static str = if *attr == "id" { "id" } else { "endpoint" };
            Some((e.typ, e.name.clone(), attr))
        })
        .collect();
    let to = match rng.chance(75) {
        true => rng.pick(&targets).cloned(),
        false => None,
    };
    Res {
        typ,
        name: names.fresh(),
        cidr: matches!(typ, VPC | SUBNET).then(|| rng.below(4) as u8),
        tier: rng.below(3) as u8,
        to,
        prevent_destroy: rng.chance(10),
        create_before_destroy: matches!(typ, VPC | SUBNET) && rng.chance(30),
    }
}

/// The next version: one to three edits of `v`.
fn next_version(rng: &mut Rng, names: &mut Names, v: &Version) -> Version {
    let mut res = v.res.clone();
    let mut moved = Vec::new();
    for _ in 0..1 + rng.below(3) {
        match rng.below(7) {
            0 => {
                let r = new_res(rng, names, &res);
                res.push(r);
            }
            1 if !res.is_empty() => {
                // A delete; what referenced it no longer does.
                let gone = res.remove(rng.below(res.len()));
                for r in &mut res {
                    if r.to
                        .as_ref()
                        .is_some_and(|(t, n, _)| *t == gone.typ && *n == gone.name)
                    {
                        r.to = None;
                    }
                }
            }
            2 => {
                // A force_new change: a replace.
                let idx: Vec<usize> = (0..res.len()).filter(|&i| res[i].cidr.is_some()).collect();
                if let Some(&i) = rng.pick(&idx) {
                    let c = res[i].cidr.unwrap();
                    res[i].cidr = Some((c + 1 + rng.below(3) as u8) % 4);
                }
            }
            3 if !res.is_empty() => {
                let i = rng.below(res.len());
                res[i].tier = (res[i].tier + 1) % 3;
            }
            4 if !res.is_empty() => {
                // A rename, with its `moved` fact.
                let i = rng.below(res.len());
                let (typ, old) = (res[i].typ, res[i].name.clone());
                let new = names.fresh();
                res[i].name = new.clone();
                for r in &mut res {
                    if let Some((t, n, _)) = &mut r.to
                        && *t == typ
                        && *n == old
                    {
                        *n = new.clone();
                    }
                }
                moved.push((typ, old, new));
            }
            5 if !res.is_empty() => {
                let i = rng.below(res.len());
                res[i].prevent_destroy = !res[i].prevent_destroy;
            }
            6 => {
                let idx: Vec<usize> = (0..res.len()).filter(|&i| res[i].cidr.is_some()).collect();
                if let Some(&i) = rng.pick(&idx) {
                    res[i].create_before_destroy = !res[i].create_before_destroy;
                }
            }
            _ => {}
        }
    }
    Version { res, moved }
}

/// Objects in the cloud that dform does not manage: the inventory's.
fn foreign(rng: &mut Rng) -> Vec<(&'static str, String)> {
    match rng.chance(40) {
        false => Vec::new(),
        true => (0..1 + rng.below(2))
            .map(|i| {
                (
                    *rng.pick(&[VPC, SUBNET, VM, DB]).unwrap(),
                    format!("foreign-{i}"),
                )
            })
            .collect(),
    }
}

// --------------------------------------------------------------- schedule

/// One step of a schedule.
#[derive(Debug, Clone, PartialEq)]
struct Step {
    kind: Kind,
    parallel: u8,
    /// The order calls in flight answer in: a seed, else the clock.
    order: Option<u64>,
    /// Drift between a plan and its apply (`plan-apply` only): the Kth
    /// managed object changed or removed in the world.
    perturb: Option<Perturb>,
    /// `--chaos` specs.
    chaos: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Apply,
    PlanApply,
    /// The program moves to its next version.
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Perturb {
    Drift(usize),
    Remove(usize),
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            Kind::Apply => "apply",
            Kind::PlanApply => "plan-apply",
            Kind::Next => return f.write_str("next"),
        };
        write!(f, "{kind} p={}", self.parallel)?;
        if let Some(s) = self.order {
            write!(f, " seed={s}")?;
        }
        match self.perturb {
            Some(Perturb::Drift(k)) => write!(f, " drift={k}")?,
            Some(Perturb::Remove(k)) => write!(f, " remove={k}")?,
            None => {}
        }
        for c in &self.chaos {
            write!(f, " {c}")?;
        }
        Ok(())
    }
}

struct Schedule(Vec<Step>);

impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let steps: Vec<String> = self.0.iter().map(|s| s.to_string()).collect();
        f.write_str(&steps.join("; "))
    }
}

/// The inverse of `Schedule`'s Display.
fn parse_schedule(s: &str) -> Vec<Step> {
    s.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut words = s.split_whitespace();
            let kind = match words.next() {
                Some("apply") => Kind::Apply,
                Some("plan-apply") => Kind::PlanApply,
                Some("next") => Kind::Next,
                other => panic!("schedule step {other:?}: expected apply, plan-apply or next"),
            };
            let mut step = Step {
                kind,
                parallel: 1,
                order: None,
                perturb: None,
                chaos: Vec::new(),
            };
            for w in words {
                let num = |v: &str| v.parse::<u64>().expect("a number");
                match w.split_once('=') {
                    Some(("p", v)) => step.parallel = num(v) as u8,
                    Some(("seed", v)) => step.order = Some(num(v)),
                    Some(("drift", v)) => step.perturb = Some(Perturb::Drift(num(v) as usize)),
                    Some(("remove", v)) => step.perturb = Some(Perturb::Remove(num(v) as usize)),
                    _ => step.chaos.push(w.to_string()),
                }
            }
            step
        })
        .collect()
}

fn knob(rng: &mut Rng, addrs: &[String]) -> Option<String> {
    let a = rng.pick(addrs)?.clone();
    Some(match rng.below(9) {
        0 => format!("fail={a}"),
        1 => format!("timeout={a}"),
        2 => format!("crash={a}"),
        // Within the retry budget (three Reads): past it an object is
        // taken as gone by design.
        3 => format!("read-lag={a}:{}", 1 + rng.below(2)),
        4 => format!("mutate={a}:tier=\"drift\""),
        5 => format!("latency={a}:{}", 10 * (1 + rng.below(5))),
        6 => "fresh-ids".to_string(),
        _ => format!("stop-after={}", 1 + rng.below(4)),
    })
}

/// An episode: the program's versions, the world it starts from, and the
/// schedule.
struct Episode {
    versions: Vec<Version>,
    foreign: Vec<(&'static str, String)>,
    steps: Vec<Step>,
}

fn episode(seed: u64) -> Episode {
    let mut rng = Rng(seed ^ 0x6d6f64656c);
    let mut names = Names(0);
    let mut first = Version::default();
    for _ in 0..1 + rng.below(4) {
        let r = new_res(&mut rng, &mut names, &first.res);
        first.res.push(r);
    }
    let foreign = foreign(&mut rng);
    let mut versions = vec![first];
    let mut steps = Vec::new();
    // Half the episodes start from a world the first version was applied to.
    if rng.chance(50) {
        steps.push(Step {
            kind: Kind::Apply,
            parallel: 1,
            order: None,
            perturb: None,
            chaos: Vec::new(),
        });
    }
    for _ in 0..3 + rng.below(6) {
        if rng.chance(25) {
            let v = next_version(&mut rng, &mut names, versions.last().unwrap());
            versions.push(v);
            steps.push(Step {
                kind: Kind::Next,
                parallel: 1,
                order: None,
                perturb: None,
                chaos: Vec::new(),
            });
            continue;
        }
        let addrs = versions.last().unwrap().addrs();
        let kind = match rng.chance(35) {
            true => Kind::PlanApply,
            false => Kind::Apply,
        };
        let perturb = match (kind, rng.below(4)) {
            (Kind::PlanApply, 0) => Some(Perturb::Drift(rng.below(8))),
            (Kind::PlanApply, 1) => Some(Perturb::Remove(rng.below(8))),
            _ => None,
        };
        let chaos = (0..rng.below(4))
            .filter_map(|_| knob(&mut rng, &addrs))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        steps.push(Step {
            kind,
            parallel: 1 + rng.below(4) as u8,
            order: rng.chance(50).then(|| rng.next() % 1000),
            perturb,
            chaos,
        });
    }
    Episode {
        versions,
        foreign,
        steps,
    }
}

// --------------------------------------------------------------- recorder

/// An Apply call as the recorder saw it.
#[derive(Debug, Clone)]
struct Req {
    op: pb::Op,
    typ: String,
    name: String,
    remote: String,
}

#[derive(Debug, Clone)]
enum Answer {
    Ok(String),
    Refused(String),
    MaybeApplied,
    Crashed,
}

#[derive(Default)]
struct Log {
    state: PathBuf,
    /// Apply calls in flight.
    applies: HashMap<Ticket, Req>,
    /// Every other call in flight (a blocking one: an Apply answer taken
    /// meanwhile waits in the link, out of the recorder's sight).
    blocking: HashSet<Ticket>,
    submitted: Vec<Req>,
    answered: Vec<(Req, Answer)>,
    end_ticks: usize,
    /// The last answered Apply call, to be found in state by the
    /// provider's next call.
    expect: Option<(Req, String)>,
    violations: Vec<(&'static str, String)>,
}

impl Log {
    fn check_expect(&mut self) {
        let Some((req, remote)) = self.expect.take() else {
            return;
        };
        let st: Json = std::fs::read(&self.state)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Json::Null);
        let key = format!("{}::{}", req.typ, req.name);
        let at = |section: &str| st[section][&key]["remote"].as_str().map(str::to_string);
        let ok = match req.op {
            pb::Op::Create | pb::Op::Replace | pb::Op::Adopt => {
                at("resources").as_deref() == Some(remote.as_str())
            }
            pb::Op::Update => at("resources").as_deref() == Some(req.remote.as_str()),
            pb::Op::Delete => {
                at("resources").as_deref() != Some(req.remote.as_str())
                    && at("deposed").as_deref() != Some(req.remote.as_str())
            }
            _ => true,
        };
        if !ok {
            self.violations.push((
                "identity",
                format!(
                    "{:?} {}/{} answered (remote {remote:?}), but the state on disk at the \
                     provider's next call has resources: {}, deposed: {}",
                    req.op, req.typ, req.name, st["resources"][&key], st["deposed"][&key]
                ),
            ));
        }
    }
}

struct Shared {
    order: Order,
    log: Log,
}

static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(|| {
    Mutex::new(Shared {
        order: Order::Clock,
        log: Log::default(),
    })
});

fn shared() -> MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The direct backend with a recorder between the executor and the mock.
struct Model;

static MODEL: Model = Model;

impl Launch for Model {
    fn mock(&self) -> anyhow::Result<Link> {
        let order = shared().order;
        Link::start(
            "the mock (model)",
            Box::new(Recorder(Queue::new(
                dform_mock::Mock::linked(),
                order,
                false,
            ))),
        )
    }

    fn plugin(&self, exe: &Path) -> anyhow::Result<Link> {
        anyhow::bail!("the model links only the mock, not {}", exe.display())
    }
}

struct Recorder(Queue<dform_mock::Mock>);

impl Provider for Recorder {
    fn submit(&mut self, call: Call) -> Ticket {
        let mut sh = shared();
        sh.log.check_expect();
        let req = match &call {
            Call::Apply(r) => Some(Req {
                op: r.op(),
                typ: r.r#type.clone(),
                name: r.name.clone(),
                remote: r.remote.clone(),
            }),
            _ => None,
        };
        let t = self.0.submit(call);
        match req {
            Some(r) if r.op == pb::Op::EndTick => {
                sh.log.end_ticks += 1;
                sh.log.blocking.insert(t);
            }
            Some(r) => {
                sh.log.submitted.push(r.clone());
                sh.log.applies.insert(t, r);
            }
            None => {
                sh.log.blocking.insert(t);
            }
        }
        t
    }

    fn next_completed(&mut self) -> (Ticket, Result<Reply, CallError>) {
        let mut sh = shared();
        sh.log.check_expect();
        let (t, r) = self.0.next_completed();
        let log = &mut sh.log;
        if log.blocking.remove(&t) {
            return (t, r);
        }
        let Some(req) = log.applies.remove(&t) else {
            return (t, r);
        };
        let answer = match &r {
            Ok(Reply::Apply(resp)) => Answer::Ok(resp.remote.clone()),
            Ok(_) => Answer::Refused("not an Apply reply".into()),
            Err(CallError::Refused(m)) => Answer::Refused(m.clone()),
            Err(CallError::MaybeApplied(_)) => Answer::MaybeApplied,
            Err(CallError::Crashed(_)) => Answer::Crashed,
        };
        // Taken while a blocking call waits, it waits in the link: the
        // executor has not seen it yet.
        if let Answer::Ok(remote) = &answer
            && log.blocking.is_empty()
        {
            log.expect = Some((req.clone(), remote.clone()));
        }
        log.answered.push((req, answer));
        (t, r)
    }

    fn is_dead(&mut self) -> bool {
        self.0.is_dead()
    }
}

// ----------------------------------------------------------------- runner

/// An invariant that failed: its name and what happened.
#[derive(Debug, Clone)]
struct Failure {
    invariant: &'static str,
    what: String,
}

struct Runner<'a> {
    dir: PathBuf,
    ep: &'a Episode,
    version: usize,
    foreign: BTreeSet<(String, String)>,
}

/// What one command line run did.
struct Ran {
    result: Result<(), String>,
    submitted: Vec<Req>,
    answered: Vec<(Req, Answer)>,
    end_ticks: usize,
}

fn read_json(p: &Path) -> Json {
    std::fs::read(p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Json::Null)
}

impl Runner<'_> {
    fn path(&self, f: &str) -> PathBuf {
        self.dir.join(f)
    }

    fn write_program(&self) {
        std::fs::write(self.path("p.df"), self.ep.versions[self.version].text()).unwrap();
    }

    /// `dform ARGS` in this process, recorded.
    fn run(&mut self, args: &[&str], order: Option<u64>) -> Result<Ran, Failure> {
        let p = self.path("p.df").display().to_string();
        let w = self.path("w.json").display().to_string();
        let mut argv = vec!["dform", "dev", "--world", &w];
        argv.extend_from_slice(args);
        // `apply PLAN.json` names its program; every other run names p.df.
        if !matches!(args, ["apply", f, ..] if f.ends_with(".json")) {
            argv.push(&p);
        }
        {
            let mut sh = shared();
            sh.order = order.map_or(Order::Clock, Order::Seed);
            sh.log = Log {
                state: self.path("w.state.json"),
                ..Log::default()
            };
        }
        let argv = common::yes(&argv);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dform::cli::run_in_process(&MODEL, argv)
        }));
        let mut sh = shared();
        sh.log.check_expect();
        let log = std::mem::take(&mut sh.log);
        drop(sh);
        let result = match result {
            Ok(r) => r.map_err(|e| format!("{e:#}")),
            Err(p) => {
                let msg = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                return Err(Failure {
                    invariant: "panic",
                    what: format!("dform {} panicked: {msg}", args.join(" ")),
                });
            }
        };
        if let Some((inv, what)) = log.violations.into_iter().next() {
            return Err(Failure {
                invariant: inv,
                what: format!("dform {}: {what}", args.join(" ")),
            });
        }
        if std::env::var_os("DFORM_MODEL_VERBOSE").is_some() {
            eprintln!(
                "dform {}: {:?}; {} applies submitted, {} answered: {:?}",
                args.join(" "),
                result,
                log.submitted.len(),
                log.answered.len(),
                log.answered
                    .iter()
                    .map(|(r, a)| format!("{:?} {}/{} {a:?}", r.op, r.typ, r.name))
                    .collect::<Vec<_>>()
            );
        }
        Ok(Ran {
            result,
            submitted: log.submitted,
            answered: log.answered,
            end_ticks: log.end_ticks,
        })
    }

    fn fail(invariant: &'static str, what: String) -> Result<(), Failure> {
        Err(Failure { invariant, what })
    }

    /// The per-run invariants: foreign, repeat-create, orphan.
    fn after(&mut self, ran: &Ran, what: &str) -> Result<(), Failure> {
        for r in &ran.submitted {
            let named = match r.op {
                pb::Op::Create => &r.name,
                _ => &r.remote,
            };
            if self.foreign.contains(&(r.typ.clone(), named.clone())) {
                return Self::fail(
                    "foreign",
                    format!(
                        "{what}: {:?} {}/{named}, an object dform does not manage",
                        r.op, r.typ
                    ),
                );
            }
        }
        for (r, a) in &ran.answered {
            if let Answer::Refused(m) = a
                && r.op == pb::Op::Create
                && m.contains("already exists in the world")
            {
                return Self::fail(
                    "repeat-create",
                    format!(
                        "{what}: Create {}/{} met the object already there: {m}",
                        r.typ, r.name
                    ),
                );
            }
        }
        self.orphans(what)
    }

    fn orphans(&self, what: &str) -> Result<(), Failure> {
        let world = read_json(&self.path("w.json"));
        let st = read_json(&self.path("w.state.json"));
        // The Creates and Replaces state records as uncertain: what they
        // made is not mapped until an apply resolves them.
        let uncertain: Vec<(&str, &str)> = st["uncertain"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, u)| u["op"] == "create" || u["op"].get("replace").is_some())
            .filter_map(|(k, _)| k.split_once("::"))
            .collect();
        // The object an uncertain Create or Replace at `typ/n` may have
        // made: `n`, or `n-N` with fresh ids.
        let uncertain = |typ: &str, remote: &str| {
            uncertain.iter().any(|&(t, n)| {
                t == typ
                    && (remote == n
                        || remote
                            .strip_prefix(n)
                            .and_then(|s| s.strip_prefix('-'))
                            .is_some_and(|s| s.parse::<u32>().is_ok()))
            })
        };
        let mut known: BTreeSet<(String, String)> = BTreeSet::new();
        for section in ["resources", "deposed"] {
            for (k, e) in st[section].as_object().into_iter().flatten() {
                let typ = k.split_once("::").map(|(t, _)| t).unwrap_or_default();
                known.insert((
                    typ.to_string(),
                    e["remote"].as_str().unwrap_or("").to_string(),
                ));
            }
        }
        for rr in world["resources"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(_, v)| v)
        {
            let typ = rr["typ"].as_str().unwrap_or("").to_string();
            let name = rr["name"].as_str().unwrap_or("").to_string();
            let id = (typ.clone(), name.clone());
            if !known.contains(&id) && !self.foreign.contains(&id) && !uncertain(&typ, &name) {
                return Self::fail(
                    "orphan",
                    format!(
                        "after {what}: {typ}/{name} is in the world, and neither in state nor \
                         recorded there as uncertain"
                    ),
                );
            }
        }
        Ok(())
    }

    fn perturb(&self, p: Perturb) {
        let path = self.path("w.json");
        let mut world = read_json(&path);
        let Some(objs) = world["resources"].as_object_mut() else {
            return;
        };
        let keys: Vec<String> = objs
            .iter()
            .filter(|(_, v)| {
                let id = (
                    v["typ"].as_str().unwrap_or("").to_string(),
                    v["name"].as_str().unwrap_or("").to_string(),
                );
                !self.foreign.contains(&id)
            })
            .map(|(k, _)| k.clone())
            .collect();
        let pick = |k: usize| keys.get(k % keys.len().max(1)).cloned();
        match p {
            Perturb::Drift(k) => {
                if let Some(key) = pick(k) {
                    objs[&key]["attrs"]["tier"] = json!("perturbed");
                }
            }
            Perturb::Remove(k) => {
                if let Some(key) = pick(k) {
                    objs.remove(&key);
                }
            }
        }
        std::fs::write(&path, serde_json::to_vec_pretty(&world).unwrap()).unwrap();
    }

    fn apply_args(step: &Step) -> Vec<String> {
        let mut args = vec!["apply".to_string()];
        for c in &step.chaos {
            args.push("--chaos".into());
            args.push(c.clone());
        }
        args.push("--parallel".into());
        args.push(step.parallel.max(1).to_string());
        args
    }

    fn step(&mut self, i: usize, step: &Step) -> Result<(), Failure> {
        let what = format!("step {i} ({step})");
        match step.kind {
            Kind::Next => {
                self.version = (self.version + 1).min(self.ep.versions.len() - 1);
                self.write_program();
                Ok(())
            }
            Kind::Apply => {
                let args = Self::apply_args(step);
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let ran = self.run(&args, step.order)?;
                self.after(&ran, &what)
            }
            Kind::PlanApply => self.plan_apply(&what, step),
        }
    }

    /// `plan --out`, drift, a fresh `plan --out` (the oracle), `apply
    /// PLAN`.
    fn plan_apply(&mut self, what: &str, step: &Step) -> Result<(), Failure> {
        let (plan, fresh) = (self.path("plan.json"), self.path("fresh.json"));
        let _ = std::fs::remove_file(&plan);
        let _ = std::fs::remove_file(&fresh);
        let ran = self.run(&["plan", "--out", &plan.display().to_string()], None)?;
        self.after(&ran, what)?;
        if ran.result.is_err() {
            return Ok(());
        }
        if let Some(p) = step.perturb {
            self.perturb(p);
        }
        let ran = self.run(&["plan", "--out", &fresh.display().to_string()], None)?;
        self.after(&ran, what)?;
        let saved = PlanFile::load(&plan).expect("plan --out wrote it");
        let stale_at_1 = match ran.result {
            Ok(()) => {
                let now = PlanFile::load(&fresh).expect("plan --out wrote it");
                Some(!saved.stale(&now.deformations, 1).is_empty())
            }
            Err(_) => None,
        };
        let mut args = Self::apply_args(step);
        args.insert(1, plan.display().to_string());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let ran = self.run(&args, step.order)?;
        self.after(&ran, what)?;
        // A refusal at tick 1 comes before the tick's Apply calls and its end.
        let refused_at_1 =
            ran.end_ticks == 0 && ran.result.as_ref().is_err_and(|e| e.contains("stale plan"));
        // Stopped before tick 1's check (a deny on the remaining actions of
        // an interrupted apply, a knob naming no resource): nothing to say.
        let before_tick_1 = ran.end_ticks == 0 && ran.result.is_err() && !refused_at_1;
        if let Some(stale) = stale_at_1
            && !before_tick_1
            && stale != refused_at_1
        {
            return Self::fail(
                "plan-file",
                format!(
                    "{what}: a fresh plan's delta {} the file's, and apply {} at tick 1 ({:?})",
                    if stale { "differs from" } else { "is" },
                    if refused_at_1 {
                        "refused"
                    } else {
                        "did not refuse"
                    },
                    ran.result
                ),
            );
        }
        let mut listed: BTreeSet<String> = BTreeSet::new();
        for d in &saved.deformations {
            listed.insert(format!("{}.{}", d.typ, d.name));
            listed.extend(d.dependents.iter().cloned());
        }
        for c in &step.chaos {
            if let Some(m) = c.strip_prefix("mutate=")
                && let Some((addr, _)) = m.split_once(':')
            {
                listed.insert(addr.replacen('/', ".", 1));
            }
        }
        for r in &ran.submitted {
            let at = format!("{}.{}", r.typ, r.name);
            if !listed.contains(&at) {
                return Self::fail(
                    "overreach",
                    format!(
                        "{what}: apply of the plan file called {:?} {at}, which it does not list",
                        r.op
                    ),
                );
            }
        }
        Ok(())
    }

    /// One apply without chaos: it ends undeformed, or stopped on a deny.
    fn settle(&mut self) -> Result<(), Failure> {
        let ran = self.run(&["apply"], None)?;
        self.after(&ran, "the settling apply")?;
        match &ran.result {
            Err(e) if e.contains("blocked by constraints") => return Ok(()),
            Err(e) => return Self::fail("settle", format!("the settling apply failed: {e}")),
            Ok(()) => {}
        }
        let out = self.path("settled.json");
        let ran = self.run(&["plan", "--out", &out.display().to_string()], None)?;
        if let Err(e) = &ran.result {
            return Self::fail(
                "settle",
                format!("the plan after the settling apply failed: {e}"),
            );
        }
        let f = PlanFile::load(&out).expect("plan --out wrote it");
        if !f.deformations.is_empty() {
            let left: Vec<String> = f
                .deformations
                .iter()
                .map(|d| format!("{} {}.{}", d.action, d.typ, d.name))
                .collect();
            return Self::fail(
                "settle",
                format!(
                    "the settling apply succeeded and left deformations: {}",
                    left.join(", ")
                ),
            );
        }
        Ok(())
    }
}

/// Run `seed`'s episode with `steps`: the first invariant that fails.
fn replay(seed: u64, steps: &[Step]) -> Result<(), Failure> {
    let ep = episode(seed);
    let s = common::Scratch::new(&format!("model-{seed}"));
    let foreign: BTreeSet<(String, String)> = ep
        .foreign
        .iter()
        .map(|(t, n)| (t.to_string(), n.clone()))
        .collect();
    if !foreign.is_empty() {
        let resources: serde_json::Map<String, Json> = foreign
            .iter()
            .map(|(t, n)| {
                (
                    format!("{t}::{n}"),
                    json!({"typ": t, "name": n, "attrs": {"tier": "theirs"},
                           "computed": {"id": format!("{t}:{n}")}}),
                )
            })
            .collect();
        std::fs::write(
            s.path("inventory.json"),
            serde_json::to_vec(&json!({ "resources": resources })).unwrap(),
        )
        .unwrap();
    }
    let mut r = Runner {
        dir: s.dir.clone(),
        ep: &ep,
        version: 0,
        foreign,
    };
    r.write_program();
    let out = (|| {
        for (i, step) in steps.iter().enumerate() {
            r.step(i, step)?;
        }
        r.settle()
    })();
    if out.is_ok() {
        let _ = std::fs::remove_dir_all(&s.dir);
    }
    out
}

/// Delta debugging over the steps (ddmin), then each step's knobs one at a
/// time, then `--parallel 1` and the clock: the smallest schedule that
/// still fails `invariant`.
fn minimize(seed: u64, steps: Vec<Step>, invariant: &str) -> Vec<Step> {
    let fails = |s: &[Step]| replay(seed, s).is_err_and(|f| f.invariant == invariant);
    let mut cur = steps;
    let mut n = 2;
    while cur.len() >= 2 {
        let chunk = cur.len().div_ceil(n);
        let mut reduced = false;
        for start in (0..cur.len()).step_by(chunk) {
            let mut without = cur.clone();
            without.drain(start..(start + chunk).min(cur.len()));
            if fails(&without) {
                cur = without;
                n = (n - 1).max(2);
                reduced = true;
                break;
            }
        }
        if !reduced {
            if n >= cur.len() {
                break;
            }
            n = (n * 2).min(cur.len());
        }
    }
    if cur.len() == 1 && fails(&[]) {
        cur.clear();
    }
    for i in 0..cur.len() {
        let mut k = 0;
        while k < cur[i].chaos.len() {
            let mut t = cur.clone();
            t[i].chaos.remove(k);
            if fails(&t) {
                cur = t;
            } else {
                k += 1;
            }
        }
        for simpler in [
            Step {
                parallel: 1,
                ..cur[i].clone()
            },
            Step {
                order: None,
                ..cur[i].clone()
            },
            Step {
                perturb: None,
                ..cur[i].clone()
            },
        ] {
            if simpler != cur[i] {
                let mut t = cur.clone();
                t[i] = simpler;
                if fails(&t) {
                    cur = t;
                }
            }
        }
    }
    cur
}

/// Every test in this file drives the command line in this process: one
/// at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn env_u64(k: &str) -> Option<u64> {
    std::env::var(k)
        .ok()
        .map(|v| v.parse().unwrap_or_else(|_| panic!("{k}: a number")))
}

/// Seeds `from..to`: the first that fails, minimized.
fn search(from: u64, to: u64) -> Option<(u64, Failure, Vec<Step>)> {
    for seed in from..to {
        let steps = episode(seed).steps;
        if let Err(f) = replay(seed, &steps) {
            let min = minimize(seed, steps, f.invariant);
            let f = replay(seed, &min).err().unwrap_or(f);
            return Some((seed, f, min));
        }
    }
    None
}

fn report(seed: u64, f: &Failure, steps: Vec<Step>) -> String {
    let ep = episode(seed);
    let schedule = Schedule(steps).to_string();
    format!(
        "seed {seed}: invariant {} failed: {}\n\
         minimized schedule: {schedule}\n\
         program versions:\n{}\
         foreign objects: {:?}\n\
         replay: DFORM_MODEL_SEED={seed} DFORM_MODEL_SCHEDULE='{schedule}' cargo test --test model",
        f.invariant,
        f.what,
        ep.versions
            .iter()
            .enumerate()
            .map(|(i, v)| format!("--- v{i}\n{}", v.text()))
            .collect::<String>(),
        ep.foreign,
    )
}

/// The model over seeds 0..300 (CI), `DFORM_MODEL_SEEDS` and
/// `DFORM_MODEL_START` for more; `DFORM_MODEL_SEED` (and
/// `DFORM_MODEL_SCHEDULE`) replays one.
#[test]
fn the_executor_under_random_chaos_never_orphans_or_overreaches() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(seed) = env_u64("DFORM_MODEL_SEED") {
        let steps = match std::env::var("DFORM_MODEL_SCHEDULE") {
            Ok(s) => parse_schedule(&s),
            Err(_) => episode(seed).steps,
        };
        if let Err(f) = replay(seed, &steps) {
            panic!("{}", report(seed, &f, steps));
        }
        return;
    }
    let from = env_u64("DFORM_MODEL_START").unwrap_or(0);
    let n = env_u64("DFORM_MODEL_SEEDS").unwrap_or(300);
    if let Some((seed, f, min)) = search(from, from + n) {
        panic!("{}", report(seed, &f, min));
    }
}

/// Reverting per-action persistence (the executor writes state once per
/// tick: `executor::hooks::PERSIST_PER_TICK`, a `test-hooks` build only)
/// is caught within the CI budget.
#[test]
fn persisting_once_per_tick_is_caught() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use std::sync::atomic::Ordering;
    dform::executor::hooks::PERSIST_PER_TICK.store(true, Ordering::SeqCst);
    let found = search(0, 10);
    dform::executor::hooks::PERSIST_PER_TICK.store(false, Ordering::SeqCst);
    let (seed, f, min) = found.expect("the model catches state written once per tick");
    assert_eq!(f.invariant, "identity", "{}", report(seed, &f, min));
}

/// An uncertain Create resolved as if the provider could not say what its
/// idempotency key made (`executor::hooks::CREATED_UNKNOWN`, the k8s
/// provider's lookup today) leaves the object it made unmapped once the
/// record is gone: an orphan, though no later apply creates it again (seed
/// 233: the next version drops the database).
#[test]
fn an_uncertain_create_resolved_to_nothing_is_an_orphan() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use std::sync::atomic::Ordering;
    let steps = parse_schedule(
        "next; apply p=3 seed=945 fresh-ids timeout=db.postgres/r1; next; plan-apply p=1",
    );
    dform::executor::hooks::CREATED_UNKNOWN.store(true, Ordering::SeqCst);
    let out = replay(233, &steps);
    dform::executor::hooks::CREATED_UNKNOWN.store(false, Ordering::SeqCst);
    let f = out.expect_err("the model catches the unmapped create");
    assert_eq!(f.invariant, "orphan", "{}", report(233, &f, steps.clone()));
    // Without the planted bug the same schedule settles.
    if let Err(f) = replay(233, &steps) {
        panic!("{}", report(233, &f, steps));
    }
}

/// WORK.org "Resume repeats a Create that may already have happened" (seed
/// 210): `stop-after` stops dform with a Create in flight that the direct
/// backend's clock has already run (as a process provider may have). The
/// resumed apply asks the provider for what the Create's idempotency key
/// made, finds it and maps it, and does not create it again.
#[test]
fn resume_does_not_repeat_a_create_that_was_in_flight() {
    replays(210, "apply p=2 stop-after=1");
}

/// The same after chaos `timeout` on a Create (seed 307): DEADLINE_EXCEEDED
/// may have taken effect.
#[test]
fn a_create_that_timed_out_is_not_created_again() {
    replays(307, "apply p=1 timeout=compute.vm/r2");
}

/// A Create at an address state still maps to an object that is gone (the
/// world lost it; `moved` gave the address its identity) times out: what
/// its key made is found although state maps the address (seed 732).
#[test]
fn a_timed_out_create_at_a_mapped_address_is_found() {
    replays(
        732,
        "apply p=1; next; plan-apply p=2 remove=1; apply p=4 seed=566 timeout=net.subnet/r2",
    );
}

fn replays(seed: u64, schedule: &str) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let steps = parse_schedule(schedule);
    if let Err(f) = replay(seed, &steps) {
        panic!("{}", report(seed, &f, steps));
    }
}

#[test]
fn schedules_print_and_parse_back() {
    for seed in 0..50 {
        let steps = episode(seed).steps;
        assert_eq!(parse_schedule(&Schedule(steps.clone()).to_string()), steps);
    }
}

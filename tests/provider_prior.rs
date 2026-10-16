//! What a provider's Read and Apply are told of an object it made, its
//! prior computed values, comes from state: an apply keeps what its
//! Apply answered, and the next plan's Read and the next apply's Update
//! carry it (docs/reference.md, "Providers are processes"). The command
//! line in this process over the mock linked in, behind a recorder of
//! the Reads and Applies it is sent; it is the only module of this test
//! binary that runs the command line in process (`run_in_process` holds
//! the process to one backend).

use crate::common::{self, Scratch};
use dform::plugin::Launch;
use dform::plugin::backend::{Call, CallError, Handler, Progress, Reply, Session};
use dform::plugin::link::Link;
use dform::plugin::pb;
use dform::plugin::queue::{Order, Queue};
use serde_json::{Value as Json, json};
use std::sync::Mutex;

/// The prior each Read and Apply of `compute.vm` was told, in order:
/// `("Read" | "Apply", prior)`.
static TOLD: Mutex<Vec<(&'static str, Option<Json>)>> = Mutex::new(Vec::new());

/// The mock, recording what each call of `compute.vm` is told.
struct Recording(dform_mock::Mock);

impl Handler for Recording {
    fn handle(&self, s: Session, call: Call, progress: Progress) -> Result<Reply, CallError> {
        let doc = |v: &Option<pb::Value>| {
            v.as_ref()
                .map(|v| dform::plugin::wire::from_doc(v).unwrap())
        };
        let told = match &call {
            Call::Read(r) if r.r#type == "compute.vm" => Some(("Read", doc(&r.prior))),
            Call::Apply(r) if r.r#type == "compute.vm" => Some(("Apply", doc(&r.prior))),
            _ => None,
        };
        TOLD.lock().unwrap().extend(told);
        self.0.handle(s, call, progress)
    }
}

struct Recorded;

impl Launch for Recorded {
    fn mock(&self) -> anyhow::Result<Link> {
        Link::start(
            "the mock (recorded)",
            Box::new(Queue::new(
                Recording(dform_mock::Mock::linked()),
                Order::Clock,
                false,
            )),
        )
    }

    fn plugin(
        &self,
        exe: &std::path::Path,
        _: &dform::plugin::host::Grants,
    ) -> anyhow::Result<Link> {
        anyhow::bail!("this test links only the mock, not {}", exe.display())
    }
}

static RECORDED: Recorded = Recorded;

/// `dform dev --world w.json ARGS p.df` in this process: what the vm's
/// calls were told.
fn run(s: &Scratch, args: &[&str]) -> Vec<(&'static str, Option<Json>)> {
    let (p, w) = (s.path("p.df"), s.path("w.json"));
    let mut argv = vec!["dform", "dev", "--world", w.to_str().unwrap()];
    argv.extend_from_slice(args);
    argv.push(p.to_str().unwrap());
    TOLD.lock().unwrap().clear();
    dform::cli::run_in_process(&RECORDED, common::yes(&argv)).unwrap();
    std::mem::take(&mut *TOLD.lock().unwrap())
}

/// What state keeps of the vm: its computed values.
fn kept(s: &Scratch) -> Json {
    s.json("w.state.json")["resources"]["compute.vm::app"]["computed"].clone()
}

/// A plan, an apply, a plan: the first plan reads nothing (state maps
/// nothing), the apply keeps what its Create answered, and the second
/// plan's Read is told it. An update's Apply is told it too.
#[test]
fn the_second_plan_reads_with_what_state_kept_of_the_apply() {
    let s = Scratch::project("provider-prior");
    s.write(
        "p.df",
        "use fake\nresource compute.vm app { labels = { team = \"a\" } }\n",
    );
    assert_eq!(run(&s, &["plan"]), []);
    let applied = run(&s, &["apply"]);
    assert_eq!(applied, [("Apply", None)]);
    let computed = kept(&s);
    assert_eq!(computed["id"], json!("compute.vm:app"), "{computed}");
    assert_eq!(run(&s, &["plan"]), [("Read", Some(computed.clone()))]);
    s.write(
        "p.df",
        "use fake\nresource compute.vm app { labels = { team = \"b\" } }\n",
    );
    let updated = run(&s, &["apply"]);
    assert_eq!(
        updated,
        [("Read", Some(computed.clone())), ("Apply", Some(computed))]
    );
}

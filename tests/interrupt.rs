//! Ctrl-C and SIGTERM ask dform to stop (R-137): no new Apply call, the
//! calls in flight awaited and their answers logged, then the run unwinds
//! (the lock released, the audit log's `apply_end` written, every
//! destructor run) and exits 128 + the signal. The unwind is checked in
//! this process through the flag the handler sets; two runs of the binary
//! check what only a process shows: the handler is installed and the exit
//! reaches the shell, and a run started with SIGINT ignored keeps
//! ignoring it.

mod common;
use common::Scratch;
use dform::plugin::Launch;
use dform::plugin::backend::{Call, CallError, Provider, Reply, Ticket};
use dform::plugin::link::Link;
use dform::plugin::pb;
use dform::plugin::queue::{Order, Queue};
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};

const PROG: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(main), cidr = "10.0.1.0/24" }
resource compute.vm app { subnet_id = ref(net.subnet, "a", "id") }
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("p.df", PROG);
    s
}

/// The mock linked in, asking to stop (as Ctrl-C would) when the subnet's
/// Apply is submitted: that call is in flight, the vm's not yet made.
struct StopAtSubnet;

impl Launch for StopAtSubnet {
    fn mock(&self) -> anyhow::Result<Link> {
        Link::start(
            "the mock (stops at the subnet)",
            Box::new(Stopper(Queue::new(
                dform_mock::Mock::linked(),
                Order::Clock,
                false,
            ))),
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

struct Stopper(Queue<dform_mock::Mock>);

impl Provider for Stopper {
    fn submit(&mut self, call: Call) -> Ticket {
        if let Call::Apply(r) = &call
            && r.r#type == "net.subnet"
            && r.op() == pb::Op::Create
        {
            dform::interrupt::request(libc::SIGINT);
        }
        self.0.submit(call)
    }

    fn next_completed(
        &mut self,
        events: &mut dyn FnMut(Ticket, pb::Event),
    ) -> (Ticket, Result<Reply, CallError>) {
        self.0.next_completed(events)
    }
}

static STOP_AT_SUBNET: StopAtSubnet = StopAtSubnet;

/// A stop asked for while the subnet's create is in flight: the create
/// is awaited and its answer kept, the vm is never started, and the run
/// unwinds: `apply_end` says the apply stopped, interrupted, and the lock
/// is released. The next apply resumes and creates the vm.
#[test]
fn an_interrupt_stops_after_the_calls_in_flight_and_unwinds() {
    let s = project("int-unwind");
    let (p, w) = (s.path("p.df"), s.path("w.json"));
    let argv = ["dform", "dev", "--world", w.to_str().unwrap(), "apply"];
    let apply = || {
        let argv = common::yes(&argv).into_iter().chain([p.clone().into()]);
        dform::cli::run_in_process(&STOP_AT_SUBNET, argv)
    };
    apply().unwrap();
    assert_eq!(common::identities(&s), ["net.subnet::a", "net.vpc::main"]);
    let end = s
        .read("w.state.audit.jsonl")
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .rfind(|e| e["kind"] == "apply_end")
        .unwrap();
    assert_eq!(
        (
            end["result"].as_str(),
            end["why"].as_str(),
            end["signal"].as_str()
        ),
        (Some("stopped"), Some("interrupted"), Some("SIGINT")),
        "{end}"
    );
    assert!(!s.path("w.state.lock").exists(), "the lock is released");

    // The stop withdrawn, the next apply makes what is left: the vm.
    dform::interrupt::request(0);
    apply().unwrap();
    assert_eq!(common::identities(&s).len(), 3);
    assert_eq!(
        dform::interrupt::requested(),
        None,
        "the subnet was not made again"
    );
}

/// `dform dev --world w.json --chaos delay=..:800 apply --yes p.df`, the
/// vpc's and the subnet's creates each answering in 0.8s, started with
/// SIGINT's disposition `disposition` (default here whatever the
/// harness's is: a suite run as a background job of a shell has it
/// ignored), its stdout read up to the plan's last line: the calls start
/// as the plan is printed, so a signal now lands in the vpc's. The rest
/// of stdout follows on the returned reader.
fn apply_at_plan(
    s: &Scratch,
    disposition: libc::sighandler_t,
) -> (Child, std::io::Lines<BufReader<std::process::ChildStdout>>) {
    let mock = [
        "--world",
        "w.json",
        "--chaos",
        "delay=net.vpc[\"main\"]:800",
        "--chaos",
        "delay=net.subnet[\"a\"]:800",
    ];
    let mut cmd = common::dform();
    cmd.args(common::yes(&common::on("p.df", &mock, &["apply"])))
        .current_dir(&s.dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: only `signal` between fork and exec: async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            libc::signal(libc::SIGINT, disposition);
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let planned = lines
        .by_ref()
        .map_while(Result::ok)
        .any(|l| l.starts_with("  + compute.vm app"));
    assert!(planned, "the plan was never printed");
    (child, lines)
}

/// SIGINT to `child`, then its exit status and stderr.
fn interrupt(
    child: Child,
    rest: std::io::Lines<BufReader<std::process::ChildStdout>>,
) -> (std::process::ExitStatus, String) {
    // SAFETY: a signal to the child this test spawned.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    // Read to its end: a closed stdout would fail the run's next print.
    rest.map_while(Result::ok).for_each(drop);
    let out = child.wait_with_output().unwrap();
    (
        out.status,
        String::from_utf8_lossy(&out.stderr).trim_end().to_string(),
    )
}

/// The binary installs the handler and its exit reaches the shell: one
/// SIGINT during a call, and the run ends 130 once the call is awaited,
/// the subnet never made.
#[test]
fn an_interrupt_exits_130() {
    let s = project("int-exit");
    let (child, rest) = apply_at_plan(&s, libc::SIG_DFL);
    let (status, stderr) = interrupt(child, rest);
    assert_eq!(status.code(), Some(130), "{stderr}");
    assert!(
        !common::identities(&s).contains(&"net.subnet::a".to_string()),
        "{stderr}"
    );
}

/// A run started with SIGINT ignored (`nohup`, a background job of a
/// shell) keeps ignoring it, as the shell asked: the apply runs to its
/// end.
#[test]
fn a_run_started_with_sigint_ignored_still_ignores_it() {
    let s = project("int-ignored");
    let (child, rest) = apply_at_plan(&s, libc::SIG_IGN);
    let (status, stderr) = interrupt(child, rest);
    assert!(status.success(), "{stderr}");
    assert_eq!(common::identities(&s).len(), 3);
}

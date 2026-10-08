//! R-130: what a provider says while an Apply runs (its progress events)
//! reaches apply's progress block: the status word beside the change, as
//! the provider says it, on every backend (the process's gRPC stream, the
//! direct and wire backends' linked mock). The mock says `made` when chaos
//! `delay` makes it answer late.

mod common;
use common::{BACKENDS, Run, Scratch};

const PROG: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(main), cidr = "10.0.1.0/24" }
"#;

/// `dform dev --world w.json --chaos C apply --yes p.df` on `backend`.
fn apply(s: &Scratch, backend: common::Backend, chaos: &str, env: &[(&str, &str)]) -> Run {
    let mut c = backend.command();
    c.args(common::on(
        "p.df",
        &["--world", "w.json", "--chaos", chaos],
        &["apply", "--yes"],
    ))
    .current_dir(s.path(""));
    for (k, v) in env {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

/// The delayed create's line says `made` once its provider said so, and
/// `made` when the create is done (R-206: the call's word, then its
/// time).
#[test]
fn a_status_the_provider_says_is_shown_beside_its_change() {
    for backend in BACKENDS {
        let s = Scratch::new(&format!("apply-events-{backend:?}"));
        s.write("p.df", PROG);
        let r = apply(&s, backend, "delay=net.subnet[\"a\"]:300", &[]).success();
        let lines: Vec<&str> = r
            .stderr
            .lines()
            .filter(|l| l.starts_with("  + net.subnet a  "))
            .collect();
        // Said as it came (it may beat once more), then done: each the
        // word, then its time.
        assert!(
            lines.len() >= 2
                && lines
                    .iter()
                    .all(|l| l.contains("  made ") && l.ends_with('s')),
            "{backend:?}\n{}",
            r.stderr
        );
        let vpc: Vec<&str> = r
            .stderr
            .lines()
            .filter(|l| l.starts_with("  + net.vpc main  "))
            .collect();
        assert!(
            vpc.len() == 1 && vpc[0].ends_with('s'),
            "{backend:?}\n{}",
            r.stderr
        );
    }
}

/// `DFORM_LOG=debug` logs each event with its message, which the block
/// does not show.
#[test]
fn an_event_s_message_goes_to_the_log() {
    let s = Scratch::new("apply-events-log");
    s.write("p.df", PROG);
    let r = apply(
        &s,
        common::Backend::Process,
        "delay=net.subnet[\"a\"]:100",
        &[("DFORM_LOG", "debug")],
    )
    .success();
    assert!(
        r.stderr
            .lines()
            .any(|l| l.ends_with("apply net.subnet a: made: answers 100ms late (chaos delay)")),
        "{}",
        r.stderr
    );
    let block: Vec<&str> = r
        .stderr
        .lines()
        .filter(|l| l.starts_with("  + net.subnet a  "))
        .collect();
    assert!(
        block.iter().all(|l| !l.contains("chaos delay")),
        "{}",
        r.stderr
    );
}

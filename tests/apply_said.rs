//! What an apply says on stdout around its blocks is a printer over
//! events (After R-206, the house rule: output is a printer over events):
//! a fixed list of [`Said`] prints the same every time; what it says on
//! stderr goes through the block's route. One test per line shape; which events a run says is the decision, read back in the
//! process tests (boundary_reask, apply_block, apply_tick2,
//! engine_phases) through `DFORM_TEST_SAID`.

use dform::address::Address;
use dform::report::Style;
use dform::said::{Question, Said, Teller};
use dform::zset::file::Difference;

fn text(quiet: bool, said: &[Said]) -> String {
    let t = Teller {
        quiet,
        style: Style::default(),
    };
    let mut out = Vec::new();
    for s in said {
        t.write(s, &mut out).unwrap();
    }
    String::from_utf8(out).unwrap()
}

fn plan(tick: usize, boundary: bool, waits: Option<&[&str]>) -> Said {
    Said::Plan {
        tick,
        boundary,
        waits: waits.map(|on| on.iter().map(|w| w.to_string()).collect()),
        text: "PLAN\n".into(),
    }
}

/// Tick 1's plan as the report prints it; a later tick's apart from the
/// block above it, one that only waits under its header, which says what
/// it waits on (the report gives it no section of its own).
#[test]
fn a_later_ticks_plan_stands_apart() {
    assert_eq!(text(false, &[plan(1, true, None)]), "PLAN\n");
    assert_eq!(text(false, &[plan(2, false, None)]), "\nPLAN\n");
    assert_eq!(
        text(
            false,
            &[plan(
                2,
                false,
                Some(&["db.postgres server.endpoint", "net.vpc main.id"])
            )]
        ),
        "\ntick 2  waits on db.postgres server.endpoint, net.vpc main.id\nPLAN\n"
    );
    assert_eq!(
        text(false, &[plan(2, false, Some(&[]))]),
        "\ntick 2  0 changes\nPLAN\n"
    );
}

/// `-q`: each tick's bare plan under `tick N:` when the apply has more
/// than one; a provider configured is not said.
#[test]
fn quiet_says_the_bare_plan_under_its_tick() {
    assert_eq!(text(true, &[plan(1, false, None)]), "PLAN\n");
    assert_eq!(text(true, &[plan(1, true, None)]), "tick 1:\nPLAN\n");
    assert_eq!(
        text(true, &[plan(2, false, Some(&["net.vpc main.id"]))]),
        "tick 2:\nPLAN\n"
    );
    let configured = Said::Configured {
        provider: "k8s".into(),
        after: 1,
        settings: vec!["kubeconfig = (sensitive)".into()],
    };
    assert_eq!(text(true, std::slice::from_ref(&configured)), "");
    assert_eq!(
        text(false, &[configured]),
        "provider k8s: configured after tick 1: kubeconfig = (sensitive)\n"
    );
}

/// The policies after a tick, apart from what is above them.
#[test]
fn the_policies_after_a_tick_stand_apart() {
    assert_eq!(
        text(
            false,
            &[Said::Policies {
                after: 1,
                text: "policy after tick 1   2 hold\n".into(),
            }]
        ),
        "\npolicy after tick 1   2 hold\n"
    );
}

/// A tick that differs from the plan shown: each difference under it.
#[test]
fn a_tick_that_differs_says_each_difference() {
    let vm = Address {
        typ: "compute.vm".into(),
        name: "app".into(),
    };
    assert_eq!(
        text(
            false,
            &[Said::Differs {
                tick: 2,
                differences: vec![Difference {
                    mark: '~',
                    addr: vm,
                    path: Some("rv".into()),
                    what: "rv = \"115\" → \"200\"".into(),
                }],
            }]
        ),
        "tick 2 differs from the plan shown:\n  ~ compute.vm app  rv = \"115\" → \"200\"\n"
    );
}

/// Each question as it is asked, its answer not echoed: tick 1's over
/// the plan, a later tick's on its header line, what the plan empties.
#[test]
fn each_question_is_asked_on_its_line() {
    let asked = |q: Question| text(false, &[Said::Asked(q), Said::Answered(true)]);
    assert_eq!(
        asked(Question::Plan {
            n: 2,
            destroy: false,
            deployment: "p".into(),
        }),
        "Apply these 2 changes to p? [y/N] "
    );
    assert_eq!(
        asked(Question::Plan {
            n: 1,
            destroy: true,
            deployment: "p".into(),
        }),
        "Destroy this object of p? [y/N] "
    );
    assert_eq!(
        asked(Question::Tick {
            tick: 2,
            n: 1,
            destroy: false,
        }),
        "tick 2  1 change   apply? [y/N] "
    );
    assert_eq!(
        asked(Question::Emptied {
            what: "the plan empties db.postgres main.tags".into(),
        }),
        "The plan empties db.postgres main.tags. Apply it anyway? [y/N] "
    );
}

/// The approval given, and what chaos did.
#[test]
fn an_approval_and_a_chaos_note_are_a_line_each() {
    assert_eq!(
        text(
            false,
            &[
                Said::Approved {
                    by: "alice@example.com".into(),
                    digest: "sha256:ab".into(),
                },
                Said::Chaos("net.vpc main answers 100ms late".into()),
            ]
        ),
        "approved by alice@example.com: plan digest sha256:ab\n\
         chaos: net.vpc main answers 100ms late\n"
    );
}

/// What refuses the apply and what a boundary warns of: the violations
/// of the tick's plan or of the boundary after a tick, a warning, the
/// world changed under a resumed apply's remaining actions.
#[test]
fn a_violation_and_a_warning_are_said_on_stderr() {
    let violations = |after| Said::Violations {
        after,
        lines: vec!["deny x  net.vpc main".into()],
    };
    assert_eq!(
        text(false, &[violations(None)]),
        "constraint violations:\n- deny x  net.vpc main\n"
    );
    assert_eq!(
        text(false, &[violations(Some(1))]),
        "constraint violations after tick 1:\n- deny x  net.vpc main\n"
    );
    assert_eq!(text(false, &[Said::Warning("w".into())]), "warning: w\n");
    assert_eq!(
        text(false, &[Said::ChangedUnder("~ net.vpc main\n".into())]),
        "the world changed under a remaining action:\n~ net.vpc main\n"
    );
    for s in [
        violations(None),
        Said::Warning("w".into()),
        Said::ChangedUnder("c".into()),
    ] {
        assert!(s.on_stderr(), "{s:?}");
    }
    assert!(!plan(1, false, None).on_stderr());
}

/// A line said on stderr goes through the block's route, a line at a
/// time: above a block drawn meanwhile, never past it.
#[test]
fn a_line_on_stderr_goes_through_the_blocks_route() {
    use std::sync::{Arc, Mutex};
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let to = lines.clone();
    dform::progress::route(Some(Box::new(move |l: &str| {
        to.lock().unwrap().push(l.to_string())
    })));
    let t = Teller {
        quiet: false,
        style: Style::default(),
    };
    t.say(Said::Violations {
        after: Some(2),
        lines: vec!["deny x  net.vpc main".into()],
    });
    t.say(Said::Warning("w".into()));
    dform::progress::route(None);
    assert_eq!(
        *lines.lock().unwrap(),
        [
            "constraint violations after tick 2:",
            "- deny x  net.vpc main",
            "warning: w"
        ]
    );
}

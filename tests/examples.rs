//! Every example plans and applies from its own project root: a copy of
//! `examples/NAME/`, and from inside it the commands its `README.md` lists:
//! plan, apply, and plan again, which finds the stack undeformed.
//!
//! A new example directory needs a case in `CASES`, and a test naming it.

mod common;
use common::{Scratch, copy_dir, repo};
use std::path::PathBuf;

/// What the test adds to each README apply: none yet. When apply asks for
/// confirmation, `"--yes"` (the README's commands stay interactive).
const APPLY_FLAGS: &[&str] = &[];

/// How one stack's apply ends.
enum Apply {
    /// It converges in this many ticks: the next plan is undeformed.
    Converges(usize),
    /// It stops after tick 1 on a deny (on purpose: the example shows
    /// one), with this in the error.
    Stops(&'static str),
}

/// A stack of an example: the plan and apply commands the README lists
/// (without `dform`), and how the apply ends.
struct Stack {
    plan: &'static [&'static str],
    apply: &'static [&'static str],
    ends: Apply,
}

/// An example project's stacks, run in order in one copy of it.
struct Case {
    name: &'static str,
    stacks: &'static [Stack],
}

const fn one(apply: &'static [&'static str], ends: Apply) -> Stack {
    Stack {
        plan: &["plan"],
        apply,
        ends,
    }
}

const CASES: &[Case] = &[
    Case {
        name: "adopt",
        stacks: &[one(&["apply"], Apply::Converges(1))],
    },
    Case {
        name: "advanced",
        stacks: &[one(&["apply"], Apply::Converges(1))],
    },
    // Keyed: apply names the deployment, the key's default.
    Case {
        name: "approvals",
        stacks: &[one(
            &["apply", "approvals.demo", "env=staging"],
            Apply::Converges(1),
        )],
    },
    Case {
        name: "aws",
        stacks: &[one(&["apply"], Apply::Converges(1))],
    },
    // Two stacks: each named. The workload after the bootstrap's three ticks.
    Case {
        name: "bootstrap",
        stacks: &[
            Stack {
                plan: &["plan", "renfry.bootstrap"],
                apply: &["apply", "renfry.bootstrap"],
                ends: Apply::Converges(3),
            },
            Stack {
                plan: &["plan", "renfry.workload"],
                apply: &["apply", "renfry.workload"],
                ends: Apply::Converges(1),
            },
        ],
    },
    // The migration Job, then the app's color, then the Service's selector.
    Case {
        name: "crud-api",
        stacks: &[one(&["apply"], Apply::Converges(3))],
    },
    // No resources: nothing to do.
    Case {
        name: "decl",
        stacks: &[one(&["apply"], Apply::Converges(0))],
    },
    Case {
        name: "demo",
        stacks: &[one(&["apply", "dform", "env=staging"], Apply::Converges(1))],
    },
    // gke_one_zone shows a deny at the boundary: its apply stops.
    Case {
        name: "gke",
        stacks: &[
            Stack {
                plan: &["plan", "gke_two_phase"],
                apply: &["apply", "gke_two_phase"],
                ends: Apply::Converges(2),
            },
            Stack {
                plan: &["plan", "gke_one_zone"],
                apply: &["apply", "gke_one_zone"],
                ends: Apply::Stops("cluster must be in at least two zones"),
            },
        ],
    },
    Case {
        name: "k8s",
        stacks: &[one(&["apply"], Apply::Converges(1))],
    },
    Case {
        name: "pngu",
        stacks: &[one(&["apply", "pngu", "env=dev"], Apply::Converges(1))],
    },
    // Three zones by default; `--set zones=2` stops (refine_types.rs).
    Case {
        name: "refine",
        stacks: &[one(&["apply"], Apply::Converges(2))],
    },
];

/// The example directories: every `examples/*/` with a `dform.toml`.
fn examples() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(repo().join("examples"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().join("dform.toml").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn every_example_has_a_case() {
    for name in examples() {
        assert!(
            CASES.iter().any(|c| c.name == name),
            "examples/{name} has no case: add examples/{name} to tests/examples.rs"
        );
    }
    for c in CASES {
        assert!(
            examples().contains(&c.name.to_string()),
            "tests/examples.rs has a case for examples/{}, which is not an example",
            c.name
        );
    }
}

/// The ticks an apply ran: none when it had nothing to do, else the last
/// `tick N:` it printed (a run of one tick prints none).
fn ticks(stdout: &str) -> usize {
    if stdout.contains("apply: nothing to do") {
        return 0;
    }
    stdout
        .lines()
        .filter_map(|l| l.strip_prefix("tick ")?.strip_suffix(':')?.parse().ok())
        .max()
        .unwrap_or(1)
}

/// The commands a README's code blocks list, their comments dropped.
fn commands(readme: &str) -> Vec<String> {
    let mut out = vec![];
    let mut code = false;
    for l in readme.lines() {
        if l.starts_with("```") {
            code = !code;
        } else if code {
            out.push(l.split(" # ").next().unwrap().trim().to_string());
        }
    }
    out
}

fn check(name: &str) {
    let case = CASES
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("add examples/{name} to tests/examples.rs"));
    let from = repo().join("examples").join(name);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("examples-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&from, &dir);
    let s = Scratch::adopt(dir);
    let readme = std::fs::read_to_string(from.join("README.md"))
        .unwrap_or_else(|e| panic!("examples/{name}/README.md: {e}"));
    let listed = commands(&readme);
    for st in case.stacks {
        // The README lists the commands this runs, from the project root.
        for args in [st.plan, st.apply] {
            let line = format!("dform {}", args.join(" "));
            assert!(
                listed.contains(&line),
                "examples/{name}/README.md does not list `{line}`:\n{readme}"
            );
        }
        let at = |what: &str| format!("examples/{name} {what}");
        let r = s.run(st.plan);
        assert!(r.ok, "{}\n{}{}", at(&st.plan.join(" ")), r.stdout, r.stderr);
        let r = s.run(&[st.apply, APPLY_FLAGS].concat());
        match st.ends {
            Apply::Converges(n) => {
                assert!(
                    r.ok,
                    "{}\n{}{}",
                    at(&st.apply.join(" ")),
                    r.stdout,
                    r.stderr
                );
                assert_eq!(ticks(&r.stdout), n, "{}\n{}", at("ticks"), r.stdout);
                let r = s.run(st.plan).success();
                assert!(
                    r.summary().ends_with(" is undeformed"),
                    "{}: the plan after apply is not undeformed\n{}{}",
                    at(&st.plan.join(" ")),
                    r.stdout,
                    r.stderr
                );
            }
            Apply::Stops(why) => {
                assert!(
                    !r.ok
                        && r.stderr.contains(why)
                        && r.stderr
                            .contains("apply stopped after tick 1: blocked by constraints"),
                    "{}: expected to stop on {why:?}\n{}{}",
                    at(&st.apply.join(" ")),
                    r.stdout,
                    r.stderr
                );
            }
        }
    }
}

#[test]
fn adopt() {
    check("adopt");
}

#[test]
fn advanced() {
    check("advanced");
}

#[test]
fn approvals() {
    check("approvals");
}

#[test]
fn aws() {
    check("aws");
}

#[test]
fn bootstrap() {
    check("bootstrap");
}

#[test]
fn crud_api() {
    check("crud-api");
}

#[test]
fn decl() {
    check("decl");
}

#[test]
fn demo() {
    check("demo");
}

#[test]
fn gke() {
    check("gke");
}

#[test]
fn k8s() {
    check("k8s");
}

#[test]
fn pngu() {
    check("pngu");
}

#[test]
fn refine() {
    check("refine");
}

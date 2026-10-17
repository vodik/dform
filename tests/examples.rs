//! Every example plans and applies from its own project root: a copy of
//! `examples/NAME/`, and from inside it the commands its `README.md` lists:
//! plan, apply, and plan again, which finds the stack undeformed. The
//! projects under tests/fixtures/ that were examples before the estates
//! do the same with the commands their case gives.
//!
//! A new example directory needs a case in `CASES`, and a test naming it.
//! The tour's stack file is also a walkthrough: every command its comments
//! tell the reader to run runs as written, in order, and prints what they
//! quote (`walkthrough`).

use crate::common;
use common::{Scratch, copy_dir, repo};

/// What the test adds to each README apply: apply asks for confirmation,
/// and the README's commands stay interactive.
const APPLY_FLAGS: &[&str] = &["--yes"];

/// How one stack's apply ends.
enum Apply {
    /// It completes: `--yes` applies every tick (R-122), and the plan
    /// after it is up to date.
    Completes,
    /// It stops after tick 1 on a deny (on purpose: the example shows
    /// one), with this in the error. It runs in a copy of its own.
    Stops(&'static str),
    /// It is not run: the mock cannot play what it needs (the case says
    /// what). The plan still runs.
    NotRun,
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
    // The migration Job, then the app's color, then the Service's selector.
    Case {
        name: "crud-api",
        stacks: &[one(&["apply"], Apply::Completes)],
    },
    Case {
        name: "gke-platform",
        stacks: &[one(&["apply", "gke_platform", "env=dev"], Apply::Completes)],
    },
    // The kubeconfig is read over SSH from the server: the mock's
    // instance answers none.
    Case {
        name: "k3s-platform",
        stacks: &[Stack {
            plan: &["plan"],
            apply: &[],
            ends: Apply::NotRun,
        }],
    },
    // project.df's deployments: staging and prod.
    Case {
        name: "shared-network",
        stacks: &[one(&["apply"], Apply::Completes)],
    },
    Case {
        name: "shop",
        stacks: &[one(&["apply"], Apply::Completes)],
    },
];

/// The projects under tests/fixtures/ that were examples, run the same
/// way.
const FIXTURES: &[Case] = &[
    Case {
        name: "adopt",
        stacks: &[one(&["apply"], Apply::Completes)],
    },
    Case {
        name: "advanced",
        stacks: &[one(&["apply"], Apply::Completes)],
    },
    // Keyed: apply names the deployment, the key's default.
    Case {
        name: "approvals",
        stacks: &[one(
            &["apply", "approvals", "env=staging"],
            Apply::Completes,
        )],
    },
    Case {
        name: "aws",
        stacks: &[one(&["apply"], Apply::Completes)],
    },
    // Two stacks: each named. The workload after the bootstrap's.
    Case {
        name: "bootstrap",
        stacks: &[
            Stack {
                plan: &["plan", "bootstrap"],
                apply: &["apply", "bootstrap"],
                ends: Apply::Completes,
            },
            Stack {
                plan: &["plan", "workload"],
                apply: &["apply", "workload"],
                ends: Apply::Completes,
            },
        ],
    },
    // The secrets and the controller's: the migration Job, then the
    // app's color, then the Service's selector.
    Case {
        name: "crud-api",
        stacks: &[one(&["apply"], Apply::Completes)],
    },
    // No resources: nothing to do.
    Case {
        name: "decl",
        stacks: &[one(&["apply"], Apply::Completes)],
    },
    // project.df's deployments: plan and apply with no target (R-114).
    Case {
        name: "demo",
        stacks: &[one(&["apply"], Apply::Completes)],
    },
    // Two zones by default; one shows the deny at the boundary.
    Case {
        name: "gke",
        stacks: &[
            one(&["apply"], Apply::Completes),
            one(
                &["apply", "--set", "zones=1"],
                Apply::Stops("cluster must be in at least two zones"),
            ),
        ],
    },
    Case {
        name: "k8s",
        stacks: &[one(&["apply"], Apply::Completes)],
    },
    // Three zones by default; two break the refinement on them.
    Case {
        name: "refine",
        stacks: &[
            one(&["apply"], Apply::Completes),
            one(
                &["apply", "--set", "zones=2"],
                Apply::Stops(
                    "gke_platform.zones: [\"us-east1-b\", \"us-east1-c\"] is outside the check zones.len >= 3",
                ),
            ),
        ],
    },
    // The policy named for the database's endpoint: named only once the
    // database is made, at tick 2.
    Case {
        name: "tour",
        stacks: &[one(&["apply"], Apply::Completes)],
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

/// Every provider an example's `dform.toml` names resolves from the
/// example's own directory exactly as a plan resolves it: `provider list`
/// names it and `provider schema NAME` succeeds. `provider schema` once
/// had a second lookup path of its own (it passed the bare project name
/// straight to the provider backend instead of the manifest's resolved
/// source); this guards against that path coming back.
#[test]
fn every_examples_provider_schema_resolves() {
    for name in examples() {
        let dir = repo().join("examples").join(&name);
        let manifest = dform_core::project::Manifest::load(
            &dir.join(dform_core::project::MANIFEST),
            env!("CARGO_PKG_VERSION"),
        )
        .unwrap_or_else(|e| panic!("examples/{name}/dform.toml: {e}"));
        if manifest.providers.is_empty() {
            continue;
        }
        let s = Scratch::in_target("examples", &format!("{name}-provider-schema"));
        copy_dir(&dir, &s.dir);
        let list = s.run(&["provider", "list"]).success();
        for provider in manifest.providers.keys() {
            assert!(
                list.stdout
                    .lines()
                    .any(|l| l.split_whitespace().next() == Some(provider.as_str())),
                "examples/{name}: provider list does not name {provider}:\n{}",
                list.stdout
            );
            let r = s.run(&["provider", "schema", provider]);
            assert!(
                r.ok,
                "examples/{name}: provider schema {provider} failed\n{}{}",
                r.stdout, r.stderr
            );
        }
    }
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
    let readme = std::fs::read_to_string(repo().join("examples").join(name).join("README.md"))
        .unwrap_or_else(|e| panic!("examples/{name}/README.md: {e}"));
    run(&format!("examples/{name}"), case, Some(&readme));
}

/// The fixture `name`'s case, its commands its own.
fn check_fixture(name: &str) {
    let case = FIXTURES.iter().find(|c| c.name == name).unwrap();
    run(&format!("tests/fixtures/{name}"), case, None);
}

/// Run `case` in copies of the project at `dir` (from the repository's
/// root); each command one its `readme` lists, when it has one.
fn run(dir: &str, case: &Case, readme: Option<&str>) {
    let from = repo().join(dir);
    let copy = || {
        let s = Scratch::in_target("examples", case.name);
        copy_dir(&from, &s.dir);
        s
    };
    let project = copy();
    let listed = readme.map(commands);
    for st in case.stacks {
        let fresh;
        let s = if matches!(st.ends, Apply::Stops(_)) {
            fresh = copy();
            &fresh
        } else {
            &project
        };
        // The README lists the commands this runs, from the project root.
        for args in [st.plan, st.apply].into_iter().filter(|a| !a.is_empty()) {
            let line = format!("dform {}", args.join(" "));
            if let (Some(listed), Some(readme)) = (&listed, readme) {
                assert!(
                    listed.contains(&line),
                    "{dir}/README.md does not list `{line}`:\n{readme}"
                );
            }
        }
        let at = |what: &str| format!("{dir} {what}");
        let r = s.run(st.plan);
        assert!(r.ok, "{}\n{}{}", at(&st.plan.join(" ")), r.stdout, r.stderr);
        if let Apply::NotRun = st.ends {
            continue;
        }
        let r = s.run(&[st.apply, APPLY_FLAGS].concat());
        match st.ends {
            Apply::Completes => {
                assert!(
                    r.ok,
                    "{}\n{}{}",
                    at(&st.apply.join(" ")),
                    r.stdout,
                    r.stderr
                );
                let r = s.run(st.plan).success();
                // A stack's, or a tree's every deployment (R-200).
                let tree = r.summary().starts_with("plan: 0 changes")
                    && r.stdout.lines().any(|l| l.starts_with("= "))
                    && !r
                        .stdout
                        .lines()
                        .any(|l| ["+ ", "~ ", "- "].iter().any(|m| l.starts_with(m)));
                assert!(
                    r.summary().ends_with(" is up to date") || tree,
                    "{}: the plan after apply is not undeformed\n{}{}",
                    at(&st.plan.join(" ")),
                    r.stdout,
                    r.stderr
                );
            }
            Apply::NotRun => unreachable!(),
            Apply::Stops(why) => {
                assert!(
                    !r.ok
                        && r.stderr.contains(why)
                        && r.stderr
                            .contains("; stopped after tick 1; ticks 1 to 1 were applied"),
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
fn crud_api() {
    check("crud-api");
}

#[test]
fn gke_platform() {
    check("gke-platform");
}

#[test]
fn k3s_platform() {
    check("k3s-platform");
}

#[test]
fn shared_network() {
    check("shared-network");
}

#[test]
fn shop() {
    check("shop");
}

#[test]
fn fixture_adopt() {
    check_fixture("adopt");
}

#[test]
fn fixture_advanced() {
    check_fixture("advanced");
}

#[test]
fn fixture_approvals() {
    check_fixture("approvals");
}

#[test]
fn fixture_aws() {
    check_fixture("aws");
}

#[test]
fn fixture_bootstrap() {
    check_fixture("bootstrap");
}

#[test]
fn fixture_crud_api() {
    check_fixture("crud-api");
}

#[test]
fn fixture_decl() {
    check_fixture("decl");
}

/// The demo's plan with no target is its project.df's: one tree, a
/// header line per environment with its plan nested under it (R-200).
#[test]
fn demo_plans_its_matrix() {
    let s = Scratch::in_target("examples", "demo-matrix");
    copy_dir(&repo().join("tests/fixtures/demo"), &s.dir);
    let r = s.run(&["plan"]).success();
    common::golden_file(
        &repo().join("tests/golden/dform/matrix.plan.txt"),
        &r.stdout,
        "examples",
    );
}

#[test]
fn fixture_demo() {
    check_fixture("demo");
}

#[test]
fn fixture_gke() {
    check_fixture("gke");
}

#[test]
fn fixture_k8s() {
    check_fixture("k8s");
}

#[test]
fn fixture_refine() {
    check_fixture("refine");
}

#[test]
fn fixture_tour() {
    check_fixture("tour");
    walkthrough("tests/fixtures/tour", "stacks/tour.df");
}

/// A command a stack file's comments tell the reader to run, and the lines
/// they quote from what it prints.
struct Step {
    line: String,
    args: Vec<String>,
    quoted: Vec<String>,
}

/// The steps of a walkthrough: each comment line `#   $ dform ARGS`, and
/// under it the `#   ` lines up to the next line that is not one.
fn steps(df: &str) -> Vec<Step> {
    let mut out: Vec<Step> = vec![];
    let mut open = false;
    for l in df.lines() {
        match l.strip_prefix("#   ") {
            Some(cmd) if cmd.starts_with("$ ") => {
                let line = cmd[2..].trim().to_string();
                let mut args = words(&line);
                assert_eq!(args.remove(0), "dform", "{line}");
                out.push(Step {
                    line,
                    args,
                    quoted: vec![],
                });
                open = true;
            }
            Some(quoted) if open => out.last_mut().unwrap().quoted.push(quoted.trim().into()),
            _ => open = false,
        }
    }
    out
}

/// A command line's words, as a shell splits one with only single quotes.
pub(crate) fn words(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut word = None::<String>;
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                word.get_or_insert_default();
            }
            ' ' if !quoted => out.extend(word.take()),
            c => word.get_or_insert_default().push(c),
        }
    }
    assert!(!quoted, "an unclosed quote: {line}");
    out.extend(word);
    out
}

/// Run a stack file's steps in order in a fresh copy of its project
/// `dir`: each succeeds and prints every line it quotes (one quoting an
/// `Error:` line fails and prints it).
fn walkthrough(dir: &str, file: &str) {
    let from = repo().join(dir);
    let name = from.file_name().unwrap().to_string_lossy().into_owned();
    let df = std::fs::read_to_string(from.join(file)).unwrap();
    let steps = steps(&df);
    assert!(!steps.is_empty(), "{dir}/{file} has no `#   $ dform` lines");
    let s = Scratch::in_target("examples", &format!("{name}-walkthrough"));
    copy_dir(&from, &s.dir);
    for step in &steps {
        let mut args = step.args.clone();
        // A reader answers each tick's question; `--yes` answers them all.
        if args[0] == "apply" {
            args.extend(APPLY_FLAGS.iter().map(|f| f.to_string()));
        }
        let r = s.run(&args);
        let out = format!("{}{}", r.stdout, r.stderr);
        let fails = step
            .quoted
            .iter()
            .any(|q| q.starts_with("error  ") || q.starts_with("refused  "));
        assert_eq!(!r.ok, fails, "{dir}/{file}: `{}`\n{out}", step.line);
        for q in &step.quoted {
            assert!(
                out.contains(q.as_str()),
                "{dir}/{file}: `{}` does not print `{q}`\n{out}",
                step.line
            );
        }
    }
}

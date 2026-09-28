//! Golden (snapshot) tests: pin `plan` and `strata` output per example
//! program. See tests/golden/README.md for the accept workflow.
//!
//! Cases: the example projects' stacks (examples/demo across envs,
//! examples/advanced, examples/pngu across envs, examples/decl,
//! examples/adopt with the discovery inventory, examples/k8s and
//! examples/aws and examples/gke's two-phase stack, examples/crud-api), and the eight
//! tests/fixtures/adversarial/*.df programs.
//! Each runs against a fresh (empty) world, and the demo additionally
//! against the tests/fixtures/world/ fixture. A case that errors (a rejected stratification, a blocked
//! constraint) still gets a snapshot: its stdout+stderr+exit status, pinned
//! like any other output.

mod common;
use common::{Backend, Scratch, repo};
use std::path::PathBuf;

struct Case {
    /// Subdirectory under tests/golden/.
    program: &'static str,
    /// Snapshot file stem under that subdirectory.
    case: &'static str,
    /// .df entry file, relative to the repo root.
    file: &'static str,
    /// `--provider` args (empty means the default `fake`).
    providers: &'static [&'static str],
    /// `--set` args, as `key=value`.
    sets: &'static [&'static str],
    /// The target's key values, as `key=value`.
    keys: &'static [&'static str],
    /// Copy tests/fixtures/world/dform.json (+ its .state.json) into the scratch
    /// dir and pass `--world` at it.
    world_fixture: bool,
    /// `--inventory PATH`, relative to the repo root.
    inventory: Option<&'static str>,
}

const fn case(program: &'static str, case: &'static str, file: &'static str) -> Case {
    Case {
        program,
        case,
        file,
        providers: &[],
        sets: &[],
        keys: &[],
        world_fixture: false,
        inventory: None,
    }
}

const CASES: &[Case] = &[
    case("dform", "staging", "examples/demo/stacks/dform.df"),
    Case {
        keys: &["env=prod"],
        ..case("dform", "prod", "examples/demo/stacks/dform.df")
    },
    Case {
        keys: &["env=dev"],
        ..case("dform", "dev", "examples/demo/stacks/dform.df")
    },
    Case {
        world_fixture: true,
        ..case("dform", "world", "examples/demo/stacks/dform.df")
    },
    case(
        "dform_advanced",
        "default",
        "examples/advanced/stacks/dform-advanced.df",
    ),
    Case {
        keys: &["env=dev"],
        ..case("pngu", "dev", "examples/pngu/stacks/pngu.df")
    },
    Case {
        keys: &["env=stg"],
        ..case("pngu", "stg", "examples/pngu/stacks/pngu.df")
    },
    Case {
        keys: &["env=prod"],
        ..case("pngu", "prod", "examples/pngu/stacks/pngu.df")
    },
    case("decl_demo", "default", "examples/decl/stacks/decl_demo.df"),
    Case {
        sets: &["env=prod"],
        inventory: Some("tests/fixtures/world/inventory.json"),
        ..case("adopt_demo", "prod", "examples/adopt/stacks/adopt_demo.df")
    },
    case(
        "adv2_rule3_coarse",
        "default",
        "tests/fixtures/adversarial/adv2_rule3_coarse.df",
    ),
    case(
        "adv3_pack_reads_other_path",
        "default",
        "tests/fixtures/adversarial/adv3_pack_reads_other_path.df",
    ),
    case(
        "adv3b_pack_reads_same_path",
        "default",
        "tests/fixtures/adversarial/adv3b_pack_reads_same_path.df",
    ),
    case(
        "adv4_variable_path_writer",
        "default",
        "tests/fixtures/adversarial/adv4_variable_path_writer.df",
    ),
    case(
        "adv4b_variable_path_writer_no_read",
        "default",
        "tests/fixtures/adversarial/adv4b_variable_path_writer_no_read.df",
    ),
    case(
        "adv7_mutual_recursion_attr",
        "default",
        "tests/fixtures/adversarial/adv7_mutual_recursion_attr.df",
    ),
    case(
        "adv7b_mutual_via_different_paths",
        "default",
        "tests/fixtures/adversarial/adv7b_mutual_via_different_paths.df",
    ),
    case(
        "adv9_default_tag_unless_present",
        "default",
        "tests/fixtures/adversarial/adv9_default_tag_unless_present.df",
    ),
    case(
        "gke_two_phase",
        "default",
        "examples/gke/stacks/gke_two_phase.df",
    ),
    // The provider is the program's `provider` statement.
    case("k8s_demo", "default", "examples/k8s/stacks/k8s_demo.df"),
    case("aws_demo", "default", "examples/aws/stacks/aws_demo.df"),
    // The providers are its project's (examples/crud-api/dform.toml).
    case(
        "crud_api",
        "default",
        "examples/crud-api/stacks/crud_api.df",
    ),
];

/// Strip the repo's absolute path so snapshots are portable across checkouts
/// and worktrees.
fn normalize(s: &str) -> String {
    let prefix = format!("{}/", repo().display());
    s.replace(&prefix, "")
        .replace(repo().to_str().unwrap(), ".")
}

/// One transcript: the exit status, then stdout, then stderr (each labeled
/// so a diff shows which stream changed).
fn transcript(status_ok: bool, stdout: &str, stderr: &str) -> String {
    let mut out = String::new();
    out.push_str(if status_ok {
        "exit: ok\n"
    } else {
        "exit: error\n"
    });
    out.push_str("-- stdout --\n");
    out.push_str(&normalize(stdout));
    if !stdout.ends_with('\n') && !stdout.is_empty() {
        out.push('\n');
    }
    out.push_str("-- stderr --\n");
    out.push_str(&normalize(stderr));
    if !stderr.ends_with('\n') && !stderr.is_empty() {
        out.push('\n');
    }
    out
}

fn golden_path(program: &str, case: &str, ext: &str) -> PathBuf {
    repo()
        .join("tests/golden")
        .join(program)
        .join(format!("{case}.{ext}.txt"))
}

/// Compare `got` against the golden file, or write it when `UPDATE_GOLDEN=1`.
#[track_caller]
fn check(program: &str, case: &str, ext: &str, got: &str) {
    let path = golden_path(program, case, ext);
    if std::env::var("UPDATE_GOLDEN").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden file {}: {e}\nrun `UPDATE_GOLDEN=1 cargo test --test golden` to accept it\n---\n{got}",
            path.display()
        )
    });
    assert_eq!(
        want,
        got,
        "\n{}/{} ({ext}) does not match the golden file ({})\nrun `UPDATE_GOLDEN=1 cargo test --test golden` to accept it if this is the intended change",
        program,
        case,
        path.display()
    );
}

fn run(s: &Scratch, args: &[&str]) -> (bool, String, String) {
    run_on(Backend::Process, s, args)
}

fn run_on(backend: Backend, s: &Scratch, args: &[&str]) -> (bool, String, String) {
    let r = s.run_on(backend, args);
    (r.ok, r.stdout, r.stderr)
}

#[test]
fn golden() {
    golden_on(Backend::Process, true);
}

/// Every case's plan with the mock linked in (the direct backend): the same
/// snapshot as over gRPC.
#[test]
fn golden_direct() {
    golden_on(Backend::Direct, false);
}

fn golden_on(backend: Backend, strata: bool) {
    let run = |s: &Scratch, args: &[&str]| run_on(backend, s, args);
    for c in CASES {
        let scratch = Scratch::new(&format!("golden-{}-{}", c.program, c.case));
        let file = repo().join(c.file);
        let file = file.to_str().unwrap();

        // -- plan --
        // The scratch directory is the working directory, so the project
        // and its dform.state/ are the scratch's: a developer's own state
        // never leaks into the snapshot.
        let mut plan_args: Vec<String> = vec!["dev".into()];
        for p in c.providers {
            plan_args.push("--provider".into());
            plan_args.push((*p).into());
        }
        for s in c.sets {
            plan_args.push("--set".into());
            plan_args.push((*s).into());
        }
        if let Some(inv) = c.inventory {
            plan_args.push("--inventory".into());
            plan_args.push(repo().join(inv).to_str().unwrap().into());
        }
        if c.world_fixture {
            std::fs::copy(
                repo().join("tests/fixtures/world/dform.json"),
                scratch.path("dform.json"),
            )
            .unwrap();
            std::fs::copy(
                repo().join("tests/fixtures/world/dform.state.json"),
                scratch.path("dform.state.json"),
            )
            .unwrap();
            plan_args.push("--world".into());
            plan_args.push("dform.json".into());
        }
        plan_args.push("plan".into());
        plan_args.push(file.into());
        plan_args.extend(c.keys.iter().map(|k| k.to_string()));
        let plan_args: Vec<&str> = plan_args.iter().map(String::as_str).collect();
        let (ok, out, err) = run(&scratch, &plan_args);
        check(c.program, c.case, "plan", &transcript(ok, &out, &err));
        if !strata {
            continue;
        }

        // -- strata -- (no --world/--inventory: strata reads neither; the
        // provider's schema expands the prelude, as in plan)
        let mut strata_args: Vec<String> = vec!["dev".into()];
        for p in c.providers {
            strata_args.push("--provider".into());
            strata_args.push((*p).into());
        }
        for s in c.sets {
            strata_args.push("--set".into());
            strata_args.push((*s).into());
        }
        strata_args.push("strata".into());
        strata_args.push(file.into());
        strata_args.extend(c.keys.iter().map(|k| k.to_string()));
        let strata_args: Vec<&str> = strata_args.iter().map(String::as_str).collect();
        let (ok, out, err) = run(
            &Scratch::new(&format!("golden-strata-{}-{}", c.program, c.case)),
            &strata_args,
        );
        check(c.program, c.case, "strata", &transcript(ok, &out, &err));
    }
}

/// `plan --json` for C's two-phase GKE stack against the gke mock: every
/// section at once (definite, pending, a pending group, an undetermined
/// policy, the apply order), nulls with their class, a secret redacted.
#[test]
fn golden_gke_plan_json() {
    let scratch = Scratch::new("golden-gke-json");
    let file = repo().join("examples/gke/stacks/gke_two_phase.df");
    let (ok, out, err) = run(
        &scratch,
        &[
            "dev",
            "--world",
            "w.json",
            "plan",
            "--json",
            file.to_str().unwrap(),
        ],
    );
    check(
        "gke_two_phase",
        "gke",
        "plan-json",
        &transcript(ok, &out, &err),
    );
}

/// The k8s plan pinned inline in tests/k8s.rs before this ticket now lives
/// here; keep this thin sanity check that the file exists and looks like a
/// plan, so a future rename of the golden layout is caught immediately.
#[test]
fn k8s_demo_golden_plan_is_present() {
    let p = golden_path("k8s_demo", "default", "plan");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    assert!(text.contains("plan:"), "{text}");
}

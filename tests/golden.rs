//! Golden (snapshot) tests: pin `plan` and `strata` output per example
//! program. See tests/golden/README.md for the accept workflow.
//!
//! Cases: the five example programs (dform.df across envs, dform-advanced.df,
//! pngu.df across envs, examples/decl_demo.df, examples/adopt_demo.df with
//! the discovery inventory), the nine examples/adversarial/*.df programs, and
//! examples/k8s_demo.df / examples/aws_demo.df. Each runs against a fresh
//! (empty) world, and dform.df additionally against the examples/world/
//! fixture. A case that errors (a rejected stratification, a blocked
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
    /// Copy examples/world/dform.json (+ its .state.json) into the scratch
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
        world_fixture: false,
        inventory: None,
    }
}

const CASES: &[Case] = &[
    case("dform", "staging", "dform.df"),
    Case {
        sets: &["env=prod"],
        ..case("dform", "prod", "dform.df")
    },
    Case {
        sets: &["env=dev"],
        ..case("dform", "dev", "dform.df")
    },
    Case {
        world_fixture: true,
        ..case("dform", "world", "dform.df")
    },
    case("dform_advanced", "default", "dform-advanced.df"),
    Case {
        sets: &["env=dev"],
        ..case("pngu", "dev", "pngu.df")
    },
    Case {
        sets: &["env=stg"],
        ..case("pngu", "stg", "pngu.df")
    },
    Case {
        sets: &["env=prod"],
        ..case("pngu", "prod", "pngu.df")
    },
    case("decl_demo", "default", "examples/decl_demo.df"),
    Case {
        sets: &["env=prod"],
        inventory: Some("examples/world/inventory.json"),
        ..case("adopt_demo", "prod", "examples/adopt_demo.df")
    },
    case(
        "adv2_rule3_coarse",
        "default",
        "examples/adversarial/adv2_rule3_coarse.df",
    ),
    case(
        "adv3_pack_reads_other_path",
        "default",
        "examples/adversarial/adv3_pack_reads_other_path.df",
    ),
    case(
        "adv3b_pack_reads_same_path",
        "default",
        "examples/adversarial/adv3b_pack_reads_same_path.df",
    ),
    case(
        "adv4_variable_path_writer",
        "default",
        "examples/adversarial/adv4_variable_path_writer.df",
    ),
    case(
        "adv4b_variable_path_writer_no_read",
        "default",
        "examples/adversarial/adv4b_variable_path_writer_no_read.df",
    ),
    case(
        "adv7_mutual_recursion_attr",
        "default",
        "examples/adversarial/adv7_mutual_recursion_attr.df",
    ),
    case(
        "adv7b_mutual_via_different_paths",
        "default",
        "examples/adversarial/adv7b_mutual_via_different_paths.df",
    ),
    case(
        "adv9_default_tag_unless_present",
        "default",
        "examples/adversarial/adv9_default_tag_unless_present.df",
    ),
    case(
        "gke_two_phase",
        "default",
        "examples/adversarial/gke_two_phase.df",
    ),
    // The provider is the program's `provider` statement.
    case("k8s_demo", "default", "examples/k8s_demo.df"),
    case("aws_demo", "default", "examples/aws_demo.df"),
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
        // --root pins state inside the scratch dir: without it a program's
        // state lives beside the program, and a developer's own .dform/ in
        // the repo would leak into the snapshot.
        let root = scratch.path(".").to_str().unwrap().to_string();
        let mut plan_args: Vec<String> =
            vec!["--root".into(), root.clone(), "--file".into(), file.into()];
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
                repo().join("examples/world/dform.json"),
                scratch.path("dform.json"),
            )
            .unwrap();
            std::fs::copy(
                repo().join("examples/world/dform.state.json"),
                scratch.path("dform.state.json"),
            )
            .unwrap();
            plan_args.push("--world".into());
            plan_args.push("dform.json".into());
        }
        plan_args.push("plan".into());
        let plan_args: Vec<&str> = plan_args.iter().map(String::as_str).collect();
        let (ok, out, err) = run(&scratch, &plan_args);
        check(c.program, c.case, "plan", &transcript(ok, &out, &err));
        if !strata {
            continue;
        }

        // -- strata -- (no --world/--inventory: strata reads neither; the
        // provider's schema expands the prelude, as in plan)
        let mut strata_args: Vec<String> =
            vec!["--root".into(), root, "--file".into(), file.into()];
        for p in c.providers {
            strata_args.push("--provider".into());
            strata_args.push((*p).into());
        }
        for s in c.sets {
            strata_args.push("--set".into());
            strata_args.push((*s).into());
        }
        strata_args.push("strata".into());
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
    let file = repo().join("examples/adversarial/gke_two_phase.df");
    let (ok, out, err) = run(
        &scratch,
        &[
            "--file",
            file.to_str().unwrap(),
            "--world",
            "w.json",
            "plan",
            "--json",
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

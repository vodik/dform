//! Stacks and providers' `use`s: a stack is named after its file, which
//! scopes its state; its backend (dform.toml) holds the state and a lock,
//! other stacks read its outputs, and a provider's `use` picks the mock's schema.

mod common;
use common::{Scratch, copy_dir, repo};
use std::process::{Command, Stdio};

const NET: &str = r#"
use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
output vpc_cidr = "10.0.0.0/16"
output vpc_id = ref(net.vpc, "main", "id")
"#;

#[test]
fn the_file_names_the_stack_and_scopes_the_state() {
    let s = Scratch::project("lang-stack-name");
    s.write("edge.df", NET);
    s.run(&["apply", "edge.df"]).success();
    assert!(s.path("dform.state/edge/state.json").exists());
    let r = s.run(&["plan", "edge.df"]).success();
    assert_eq!(r.summary(), "stack edge is up to date", "{}", r.stdout);
}

#[test]
fn a_local_backend_holds_the_state() {
    let s = Scratch::project("lang-stack-backend");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[stacks.p]\nbackend = 'local(\"state/x\")'\n",
    );
    s.write(
        "p.df",
        "\nuse fake\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n",
    );
    s.run(&["apply", "p.df"]).success();
    assert!(s.path("state/x/state.json").exists());
    assert!(s.path("state/x/remote.json").exists());
    assert!(
        !s.path("state/x/state.lock").exists(),
        "the lock is released"
    );
}

/// `local(DIR)` is relative to the project root (the directory holding
/// `dform.toml`, and `dform.state/`), found up from the working directory,
/// and so is `handover --to local(DIR)`.
#[test]
fn a_local_backend_is_relative_to_the_project_root() {
    let s = Scratch::project("lang-stack-backend-root");
    s.write(
        "infra/dform.toml",
        "[project]\nedition = \"2026\"\n\n[stacks.p]\nbackend = 'local(\"state/x\")'\n",
    );
    s.write(
        "infra/stacks/p.df",
        "\nuse fake\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n",
    );
    s.run(&["-C", "infra", "apply", "p"]).success();
    assert!(s.path("infra/state/x/state.json").exists());
    assert!(!s.path("state").exists());
    // From a subdirectory of the project: the same root.
    let r = s.run_in("infra/stacks", &["plan", "p.df"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    assert!(!s.path("infra/stacks/state").exists());

    s.write("infra/stacks/net.df", NET);
    s.run(&["-C", "infra", "apply", "net"]).success();
    s.run(&[
        "-C",
        "infra",
        "stack",
        "handover",
        "net",
        "--to",
        "local(\"moved\")",
    ])
    .success();
    assert!(s.path("infra/moved/state.json").exists());
    assert!(!s.path("moved").exists());
    let r = s.run(&["-C", "infra", "plan", "net"]).success();
    assert_eq!(r.summary(), "stack net is up to date", "{}", r.stdout);
}

/// A backend dform.toml names is checked where it names it.
#[test]
fn a_backend_is_checked() {
    let s = Scratch::project("lang-stack-backend-check");
    s.write("p.df", "\nuse fake\n");
    for (backend, error) in [
        ("gcs(\"b\")", "unknown backend"),
        ("s3(\"b\")", "takes a bucket, a prefix"),
        (
            "s3(\"b\", \"p\", {endpont: \"http://x\"})",
            "backend s3 has no option endpont",
        ),
    ] {
        s.write(
            "dform.toml",
            &format!("[project]\nedition = \"2026\"\n\n[stacks.p]\nbackend = '{backend}'\n"),
        );
        let r = s.run(&["plan", "p.df"]).failure();
        assert!(
            r.stderr.contains("dform.toml: [stacks.p] backend") && r.stderr.contains(error),
            "{backend}: {}",
            r.stderr
        );
    }
}

/// Two processes: the first apply holds the stack's lock (a test hook
/// keeps it there until a file appears); the second fails cleanly naming
/// the holder; once the first is done the second goes through.
#[test]
fn a_second_concurrent_apply_fails_cleanly() {
    let s = Scratch::project("lang-stack-lock");
    s.write("net.df", NET);
    let first = common::dform()
        .args(["apply", "--yes", "net.df"])
        .current_dir(&s.dir)
        .env("DFORM_TEST_HOLD_LOCK", s.path("release"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lock = s.path("dform.state/net/state.lock");
    for _ in 0..1000 {
        if lock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(lock.exists(), "the first apply took the lock");
    let r = s.run(&["apply", "net.df"]).failure();
    // A held lock is its own exit status (R-147).
    assert_eq!(r.code, Some(6), "{}", r.stderr);
    assert!(
        r.stderr.contains(&format!(
            "stack net is locked by another apply (pid {})",
            first.id()
        )),
        "{}",
        r.stderr
    );
    s.write("release", "");
    let out = first.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!lock.exists());
    let r = s.run(&["apply", "net.df"]).success();
    assert!(r.stdout.ends_with("is up to date\n"), "{}", r.stdout);
}

/// A lock left by an apply that is gone (killed) is taken over: no one
/// holds its `flock`.
#[test]
fn a_stale_lock_is_taken_over() {
    let s = Scratch::project("lang-stack-stale");
    s.write("net.df", NET);
    let mut dead = Command::new("true").spawn().unwrap();
    let pid = dead.id();
    dead.wait().unwrap();
    s.write("dform.state/net/state.lock", &format!("{pid}\n"));
    let r = s.run(&["apply", "net.df"]).success();
    assert!(
        r.stderr.contains(&format!(
            "taking over the lock of pid {pid}, which no longer holds it"
        )),
        "{}",
        r.stderr
    );
}

/// An applied stack's outputs, computed values included once they exist,
/// are what every other stack reads of its deployment (`use stacks.net as
/// network`, `network.vpc_cidr`).
#[test]
fn another_stack_reads_the_outputs() {
    let s = Scratch::project("lang-stack-outputs");
    s.write("stacks/net.df", NET);
    s.write(
        "app.df",
        r#"
use fake
use stacks.net as network
resource net.subnet a {
  cidr = network.vpc_cidr
  vpc_id = network.vpc_id
}
"#,
    );
    // Before net is applied, a plan of app plans net first (R-200): the
    // output its plan knows flows, its computed one waits on its apply
    // (R-121).
    let r = s.run(&["plan", "app.df"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 create); 1 create after stacks.net is applied",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("    waits on  stack stacks.net\n"),
        "{}",
        r.stdout
    );
    s.run(&["apply", "net"]).success();
    let r = s.run(&["plan", "--why=none", "app.df"]).success();
    assert!(
        r.stdout.contains(
            "+ net.subnet[\"a\"]\n  cidr = \"10.0.0.0/16\"\n  vpc_id = \"net.vpc:main\"\n"
        ),
        "{}",
        r.stdout
    );
}

/// The registry and the stacks' state live at the project root
/// (`dform.state/`), found from the working directory: stacks that read each
/// other's outputs share them from anywhere in the project. `-C DIR` runs
/// as if from DIR; the registry records absolute state paths.
#[test]
fn the_registry_is_the_projects() {
    let s = Scratch::project("lang-stack-root");
    s.write("infra/dform.toml", "[project]\nedition = \"2026\"\n");
    s.write("infra/stacks/net.df", NET);
    let app = r#"
use fake
use stacks.net as network
resource net.subnet a {
  cidr = network.vpc_cidr
  vpc_id = network.vpc_id
}
"#;
    s.write("infra/stacks/app.df", app);
    s.run(&["-C", "infra", "apply", "net"]).success();
    assert!(s.path("infra/dform.state/stacks.json").exists());
    assert!(!s.path("dform.state").exists());
    let registry: serde_json::Value =
        serde_json::from_str(&s.read("infra/dform.state/stacks.json")).unwrap();
    let state = registry["net"].as_str().unwrap();
    assert!(std::path::Path::new(state).is_absolute(), "{registry}");

    let want = "+ net.subnet[\"a\"]\n  cidr = \"10.0.0.0/16\"\n";
    let r = s
        .run_in("infra/stacks", &["plan", "--why=none", "app"])
        .success();
    assert!(r.stdout.contains(want), "{}", r.stdout);

    // Another project sees none of it, its own net not applied; -C runs in
    // this one.
    s.write("stacks/net.df", NET);
    s.write("elsewhere/app.df", app);
    let r = s.run_in("elsewhere", &["plan", "app.df"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 create); 1 create after stacks.net is applied",
        "{}",
        r.stdout
    );
    let r = s
        .run_in(
            "elsewhere",
            &["-C", "../infra", "plan", "--why=none", "app"],
        )
        .success();
    assert!(r.stdout.contains(want), "{}", r.stdout);
}

/// A provider's `use` picks the mock's schema; `--provider` overrides it.
#[test]
fn the_provider_statement_selects_the_schema() {
    let s = Scratch::project("lang-stack-provider");
    s.write(
        "mine/schema.df",
        "type_provider(x.thing, \"fakecloud\")\ntype_attr(x.thing, \"size\", \"int\", [\"required\"])\n",
    );
    s.write(
        "p.df",
        "\nuse mine { source = \"mine\" }\nresource x.thing a {}\n",
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(r.stderr.contains("size"), "{}", r.stderr);
    s.write("other.df", "type_provider(x.thing, \"fakecloud\")\n");
    let r = s
        .run(&["dev", "--provider", "./other.df", "plan", "p.df"])
        .success();
    assert!(r.stdout.contains("+ x.thing a"), "{}", r.stdout);
}

/// A resource's type is declared by the schema of the provider that
/// applies it: pngu on `use fake` (which declares only the demo
/// types) is refused at plan, naming the resource, the provider block and
/// the schema that declares the type, not handed to the fake at apply. On
/// `use gke` it plans and applies.
#[test]
fn a_type_the_provider_does_not_declare_is_a_plan_error() {
    let s = Scratch::new("lang-stack-undeclared");
    copy_dir(&repo().join("examples/pngu"), &s.dir);
    let src = s.read("stacks/pngu.df");
    assert!(src.contains("use gke {"), "{src}");
    s.write("stacks/pngu.df", &src.replace("use gke {", "use fake {"));
    let r = s.run(&["plan", "pngu"]).failure();
    assert!(
        r.stderr
            .contains("provider fake does not declare google.compute_subnetwork; declared by: gke"),
        "{}",
        r.stderr
    );
    // At the resource, and at the provider block.
    assert!(
        r.stderr
            .contains("resource google.compute_subnetwork gke_subnet {"),
        "{}",
        r.stderr
    );
    // The provider block is labeled; nothing was planned.
    assert!(r.stderr.contains("use fake {"), "{}", r.stderr);
    assert!(r.stderr.contains("─ provider fake\n"), "{}", r.stderr);
    assert_eq!(r.stdout, "deployment: stacks.pngu[env=dev]\n");
    // A type no known schema declares says so.
    s.write(
        "stacks/pngu.df",
        &src.replace(
            "resource alert_metrics_pack pngu",
            "resource mystery_pack pngu",
        ),
    );
    let r = s.run(&["plan", "pngu"]).failure();
    assert!(
        r.stderr.contains(
            "provider gke does not declare mystery_pack; no known provider schema declares it"
        ),
        "{}",
        r.stderr
    );

    s.write("stacks/pngu.df", &src);
    s.run(&["apply", "pngu"]).success();
    let r = s.run(&["plan", "pngu"]).success();
    assert!(r.stdout.contains("is up to date"), "{}", r.stdout);
}

/// A program that names no provider starts none: every command that
/// evaluates against providers fails naming the fix, and `dev --provider`
/// still runs it (R-26).
#[test]
fn a_program_with_no_provider_starts_none() {
    let s = Scratch::project("lang-stack-no-provider");
    s.write(
        "p.df",
        "\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n",
    );
    for args in [
        &["plan", "p.df"][..],
        &["apply", "p.df"],
        &["query", "want(T, A)", "p.df"],
        &["why", "want(T, A)", "p.df"],
        &["test", "p.df"],
    ] {
        let r = s.run(args).failure();
        assert!(
            r.stderr.contains(
                "the program names no provider: add `use NAME` (dform.toml names its \
                 source) or run under `dev --provider`"
            ),
            "{args:?}: {}",
            r.stderr
        );
    }
    let r = s
        .run(&["dev", "--provider", "fake", "plan", "p.df"])
        .success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 create) over 1 tick",
        "{}",
        r.stdout
    );
    s.write(
        "p.df",
        "\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nuse fake\n",
    );
    s.run(&["plan", "p.df"]).success();
}

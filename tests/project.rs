//! Projects and targets (docs/layout.md, README "Targets"): discovery finds
//! the stacks under the project root, a target names one by name, by file,
//! or as a deployment with its key, and the manifest's providers, defaults
//! and discovery settings apply.

mod common;
use common::Scratch;

const APP: &str = r#"edition 2026
stack app[env] {}
input env: enum("staging", "prod") = "staging"
resource net.vpc main {
  cidr = "10.0.0.0/16"
  tags = { env: env }
}
"#;

const NET: &str = r#"edition 2026
stack net {}
resource net.vpc shared {
  cidr = "10.9.0.0/16"
}
"#;

/// A project with two stacks: `app[env]` and `net`.
fn project(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("dform.toml", "[project]\nname = \"t\"\n");
    s.write("stacks/app.df", APP);
    s.write("stacks/net.df", NET);
    s
}

#[test]
fn a_target_is_a_name_a_file_or_a_deployment() {
    let s = project("target-forms");
    let by_name = s.run(&["plan", "net"]).success();
    assert_eq!(by_name.summary(), "plan: 1 deformation (1 create)");
    let by_file = s.run(&["plan", "stacks/net.df"]).success();
    assert_eq!(by_file.stdout, by_name.stdout);

    // The key: in brackets, or after the name; the default otherwise.
    let bracket = s.run(&["plan", "app[env=prod]"]).success();
    assert!(
        bracket.stdout.contains("tags.env = \"prod\""),
        "{}",
        bracket.stdout
    );
    let after = s.run(&["plan", "app", "env=prod"]).success();
    assert_eq!(after.stdout, bracket.stdout);
    let default = s.run(&["plan", "app"]).success();
    assert!(
        default.stdout.contains("tags.env = \"staging\""),
        "{}",
        default.stdout
    );

    // From a subdirectory, the project is found up the tree, and the one
    // stack under the working directory is the target.
    s.write("stacks/net/extra.txt", "");
    let r = s.run_in("stacks/net", &["plan", "net"]).success();
    assert_eq!(r.stdout, by_name.stdout);
    let r = s.run_in("stacks", &["plan", "net"]).success();
    assert_eq!(r.stdout, by_name.stdout);

    // Apply names every key value.
    let r = s.run(&["apply", "app"]).failure();
    assert!(
        r.stderr
            .contains("apply names its deployment: stack app is keyed by env"),
        "{}",
        r.stderr
    );
    s.run(&["apply", "app[env=prod]"]).success();
    assert!(s.path("dform.state/app/env=prod/state.json").exists());
    assert!(!s.path("dform.state/app/env=staging").exists());
}

#[test]
fn no_target_is_the_one_stack_here_else_a_listing() {
    let s = project("target-none");
    let r = s.run(&["plan"]).failure();
    assert!(
        r.stderr
            .contains("2 stacks under the current directory; name one:")
            && r.stderr.contains("app[env]  stacks/app.df")
            && r.stderr.contains("net  stacks/net.df"),
        "{}",
        r.stderr
    );
    s.write("net-only/dform.toml", "");
    s.write("net-only/stacks/net.df", NET);
    let r = s.run_in("net-only", &["plan"]).success();
    assert_eq!(r.summary(), "plan: 1 deformation (1 create)");
    // A directory with its own dform.toml is another project: the outer
    // one does not see its stacks.
    let r = s.run(&["plan", "net"]).success();
    assert_eq!(r.summary(), "plan: 1 deformation (1 create)");

    let r = s.run(&["plan", "nope"]).failure();
    assert!(
        r.stderr.contains("no stack nope in the project at")
            && r.stderr.contains("the project's stacks:"),
        "{}",
        r.stderr
    );
    let empty = Scratch::new("target-empty");
    empty.write("dform.toml", "");
    let r = empty.run(&["plan"]).failure();
    assert!(
        r.stderr.contains("no stack under") && r.stderr.contains("`stack` statement"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_key_value_is_the_targets_never_set() {
    let s = project("target-keys");
    let r = s.run(&["plan", "app", "--set", "env=prod"]).failure();
    assert!(
        r.stderr.contains(
            "--set env=prod: env is stack app's key; name the deployment in the target: \
             `dform plan app env=prod`"
        ),
        "{}",
        r.stderr
    );
    let r = s.run(&["plan", "app", "region=x"]).failure();
    assert!(
        r.stderr
            .contains("region is not a key of stack app (its key: env)"),
        "{}",
        r.stderr
    );
    let r = s.run(&["plan", "net", "env=prod"]).failure();
    assert!(r.stderr.contains("stack net has no key"), "{}", r.stderr);
}

#[test]
fn stack_names_are_unique_in_a_project() {
    let s = project("target-dup");
    s.write("stacks/net2.df", NET);
    let r = s.run(&["plan", "net"]).failure();
    assert!(
        r.stderr.contains(
            "stack net is stated by 2 files; a stack name is unique in its project: \
             stacks/net.df, stacks/net2.df"
        ),
        "{}",
        r.stderr
    );
    // A path still names one file, but the project's layout is checked.
    let r = s.run(&["plan", "stacks/app.df"]).failure();
    assert!(
        r.stderr.contains("stack net is stated by 2 files"),
        "{}",
        r.stderr
    );
}

#[test]
fn the_layout_lints() {
    let s = project("target-lints");
    // A module with a stack statement is an error.
    s.write("modules/m.df", "edition 2026\nstack m {}\n");
    let r = s.run(&["plan", "net"]).failure();
    assert!(
        r.stderr
            .contains("modules/m.df: a module file has a `stack` statement"),
        "{}",
        r.stderr
    );
    std::fs::remove_file(s.path("modules/m.df")).unwrap();
    // A .df outside the layout is a warning.
    s.write("loose.df", "edition 2026\n");
    let r = s.run(&["plan", "net"]).success();
    assert!(
        r.stderr
            .contains("warning: loose.df is outside the project layout"),
        "{}",
        r.stderr
    );
    // [discovery] exclude: not walked, not linted.
    s.write(
        "dform.toml",
        "[project]\nname = \"t\"\n[discovery]\nexclude = [\"loose.df\", \"scratch/**\"]\n",
    );
    s.write("scratch/net.df", NET);
    let r = s.run(&["plan", "net"]).success();
    assert!(!r.stderr.contains("warning"), "{}", r.stderr);
}

#[test]
fn a_program_imports_modules_never_a_stack() {
    let s = project("target-import-stack");
    s.write(
        "stacks/both.df",
        "edition 2026\nstack both {}\nimport \"stacks/net.df\"\n",
    );
    let r = s.run(&["plan", "both"]).failure();
    assert!(
        r.stderr.contains(
            "import \"stacks/net.df\": stacks/net.df is a stack (it has a `stack` statement); \
             a program imports modules, never another stack"
        ),
        "{}",
        r.stderr
    );
    // Imports resolve from the project root, `../` from the file.
    s.write(
        "modules/tags.df",
        "edition 2026\nresource net.vpc extra { cidr = \"10.1.0.0/16\" }\n",
    );
    for import in ["modules/tags.df", "../modules/tags.df"] {
        s.write(
            "stacks/both.df",
            &format!("edition 2026\nstack both {{}}\nimport \"{import}\"\n"),
        );
        let r = s.run(&["plan", "both"]).success();
        assert!(
            r.stdout.contains("+ net.vpc.extra"),
            "{import}: {}",
            r.stdout
        );
    }
}

#[test]
fn dash_c_runs_from_a_directory() {
    let s = project("target-dash-c");
    s.write("elsewhere/x", "");
    let r = s
        .run_in("elsewhere", &["-C", "..", "apply", "net"])
        .success();
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    assert!(s.path("dform.state/net/state.json").exists());
    assert!(!s.path("elsewhere/dform.state").exists());
}

#[test]
fn the_manifest_names_providers_and_defaults() {
    let s = Scratch::new("manifest");
    s.write(
        "dform.toml",
        r#"[project]
name = "m"
dform = ">=0.1"

[providers]
cloud = { source = "providers/cloud.df", version = "2.1" }

[defaults]
backend = 'local("state/{stack}")'
unknowns = "strict"
"#,
    );
    s.write(
        "providers/cloud.df",
        "type_provider(x.thing, \"fakecloud\")\ntype_attr(x.thing, \"size\", \"int\", [\"required\"])\n",
    );
    s.write(
        "stacks/p.df",
        r#"edition 2026
stack p {}
provider cloud {}
resource x.thing a { size = 1 }
pinned(n, c) if project_provider(n, c)
default(k, v) if project_default(k, v)
"#,
    );
    // The provider is the manifest's source: its schema knows x.thing.
    let r = s.run(&["apply", "p"]).success();
    assert!(r.stdout.contains("+ x.thing.a"), "{}", r.stdout);
    // The default backend, under the project root.
    assert!(s.path("state/p/state.json").exists());
    // A plan file's program takes its project's manifest too.
    s.write(
        "stacks/p.df",
        &s.read("stacks/p.df").replace("size = 1", "size = 2"),
    );
    s.run(&["plan", "p", "--out", "plan.json"]).success();
    let r = s.run(&["apply", "plan.json"]).success();
    assert!(r.stdout.contains("size: 1 -> 2"), "{}", r.stdout);
    // The manifest as facts.
    let r = s.run(&["query", "pinned(N, C)", "p"]).success();
    assert!(r.stdout.contains(r#""cloud"  "^2.1""#), "{}", r.stdout);
    let r = s.run(&["query", "default(K, V)", "p"]).success();
    assert!(r.stdout.contains(r#""unknowns"  "strict""#), "{}", r.stdout);

    // A project for another dform is refused.
    s.write("dform.toml", "[project]\ndform = \">=9\"\n");
    let r = s.run(&["plan", "p"]).failure();
    assert!(
        r.stderr.contains("requires dform >=9; this is dform 0.1.0"),
        "{}",
        r.stderr
    );
    // Nothing per deployment.
    s.write("dform.toml", "[inputs]\nenv = \"prod\"\n");
    let r = s.run(&["plan", "p"]).failure();
    assert!(r.stderr.contains("unknown field `inputs`"), "{}", r.stderr);
}

/// `unknowns` from the manifest makes a stack strict unless the stack
/// statement says otherwise.
#[test]
fn a_default_yields_to_the_stack_statement() {
    let s = Scratch::new("manifest-unknowns");
    s.write("dform.toml", "[defaults]\nunknowns = \"strict\"\n");
    let two_phase = |stack: &str| {
        format!(
            "edition 2026\n{stack}\nresource db.postgres main {{ size = 1 }}\n\
             resource compute.vm app {{\n  for want(\"db.postgres\", \"main\"), \
             e = ref(db.postgres, \"main\", \"endpoint\"), e != \"\"\n  size = 1\n}}\n"
        )
    };
    s.write("stacks/p.df", &two_phase("stack p {}"));
    let strict = s.run(&["plan", "p"]).failure();
    assert!(strict.stderr.contains("strict"), "{}", strict.stderr);
    s.write(
        "stacks/p.df",
        &two_phase("stack p { unknowns = \"permissive\" }"),
    );
    s.run(&["plan", "p"]).success();
}

#[test]
fn stack_list_shows_deployments_and_their_last_apply() {
    let s = project("stack-list");
    s.run(&["apply", "app", "env=prod"]).success();
    s.run(&["plan", "net", "--out", "net.json"]).success();
    let r = s.run(&["stack", "list"]).success();
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert_eq!(lines[0], "app[env]  stacks/app.df", "{}", r.stdout);
    assert!(
        lines[1].starts_with("  app[env=prod]: last apply 20") && lines[1].ends_with(": ok"),
        "{}",
        r.stdout
    );
    assert_eq!(lines[2], "net  stacks/net.df", "{}", r.stdout);
    assert!(
        lines[3].starts_with("  net: never applied; plan pending: net.json (sha256:"),
        "{}",
        r.stdout
    );
}

#[test]
fn state_show_mv_and_unlock() {
    let s = project("state-cmds");
    s.run(&["apply", "net"]).success();
    let r = s.run(&["state", "show", "net"]).success();
    assert!(
        r.stdout.contains("  net.vpc/shared  fakecloud shared"),
        "{}",
        r.stdout
    );
    s.run(&["state", "mv", "net.vpc/shared", "net.vpc/moved", "net"])
        .success();
    let r = s.run(&["state", "show", "net"]).success();
    assert!(r.stdout.contains("  net.vpc/moved  "), "{}", r.stdout);
    let r = s
        .run(&["state", "mv", "net.vpc/shared", "net.vpc/x", "net"])
        .failure();
    assert!(
        r.stderr
            .contains("state mv: stack net has no object at net.vpc/shared"),
        "{}",
        r.stderr
    );

    // A lock whose holder is gone is removed; a running holder's is not.
    let lock = s.path("dform.state/net/state.lock");
    std::fs::write(&lock, "999999999\n").unwrap();
    let r = s.run(&["stack", "unlock", "net"]).success();
    assert!(r.stdout.contains("stack net unlocked"), "{}", r.stdout);
    assert!(!lock.exists());
    std::fs::write(&lock, format!("{}\n", std::process::id())).unwrap();
    let r = s.run(&["stack", "unlock", "net"]).failure();
    assert!(
        r.stderr.contains("is locked by a running apply"),
        "{}",
        r.stderr
    );
    assert!(lock.exists());
}

#[test]
fn provider_schema_prints_the_facts() {
    let s = Scratch::new("provider-schema");
    let r = s.run(&["provider", "schema", "fake"]).success();
    assert!(
        r.stdout
            .contains(r#"type_attr("net.vpc", "id", "string", ["computed", "id"])"#),
        "{}",
        r.stdout
    );
}

#[test]
fn completion_lists_stacks_keys_and_deployments() {
    let s = project("complete");
    for shell in ["zsh", "bash", "fish"] {
        let r = s.run(&["completions", shell]).success();
        assert!(
            r.stdout.contains("dform __complete"),
            "{shell}: {}",
            r.stdout
        );
    }
    let r = s.run(&["__complete", "plan"]).success();
    assert_eq!(r.stdout, "app\nnet\n");
    let r = s.run(&["__complete", "plan", "app"]).success();
    assert_eq!(r.stdout, "env=prod\nenv=staging\n");
    s.run(&["apply", "app", "env=prod"]).success();
    let r = s.run(&["__complete", "plan"]).success();
    assert_eq!(r.stdout, "app\napp[env=prod]\nnet\n");
    let r = s.run(&["__complete", "stack"]).success();
    assert_eq!(r.stdout, "handover\nlist\nrekey\nunlock\n");
}

#[test]
fn fmt_with_no_path_formats_the_project() {
    let s = project("fmt-project");
    s.write(
        "stacks/net.df",
        "edition 2026\nstack net {}\nresource net.vpc shared {cidr=\"10.9.0.0/16\"}\n",
    );
    let r = s.run(&["fmt", "--check"]).failure();
    assert_eq!(r.stdout, "stacks/net.df\n");
    s.run(&["fmt"]).success();
    s.run(&["fmt", "--check"]).success();
}

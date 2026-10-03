//! Projects and targets (docs/layout.md, README "Targets"): discovery finds
//! the stacks under the project root, a target names one by name, by file,
//! or as a deployment with its key, and the manifest's providers, defaults
//! and discovery settings apply.

mod common;
use common::Scratch;
use std::path::PathBuf;

const APP: &str = r#"edition 2026
key env: enum("staging", "prod") = "staging"
provider fake
resource net.vpc main {
  cidr = "10.0.0.0/16"
  tags = { env }
}
"#;

const NET: &str = r#"edition 2026
provider fake
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

    // Apply, as plan does, takes the key's default when the target names
    // none, and says so first.
    s.run(&["apply", "app[env=prod]"]).success();
    assert!(s.path("dform.state/app/env=prod/state.json").exists());
    assert!(!s.path("dform.state/app/env=staging").exists());
    let r = s.run(&["apply", "app"]).success();
    assert!(
        r.stdout
            .starts_with("deployment: app[env=staging] (env from its default)\n"),
        "{}",
        r.stdout
    );
    assert!(s.path("dform.state/app/env=staging/state.json").exists());
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
        r.stderr.contains("no stack under") && r.stderr.contains("a file under stacks/"),
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

/// A stack is named after its file; with no stacks/ directory the root's
/// files are the stacks, and a `[stacks.NAME]` no file is is an error.
#[test]
fn a_stack_is_a_file_named_after_itself() {
    let s = Scratch::new("target-files");
    s.write("dform.toml", "");
    s.write("shop.df", NET);
    s.write("modules/m.df", "edition 2026\n");
    let r = s.run(&["plan", "shop"]).success();
    assert_eq!(r.summary(), "plan: 1 deformation (1 create)");
    let r = s.run(&["stack", "list"]).success();
    assert_eq!(
        r.stdout,
        "stack  file     result\nshop   shop.df  no deployment has state\n",
    );
    // With a stacks/ directory, only its files are stacks.
    s.write("stacks/net.df", NET);
    let r = s.run(&["plan", "shop"]).failure();
    assert!(
        r.stderr.contains("no stack shop in the project"),
        "{}",
        r.stderr
    );
    s.run(&["plan", "net"]).success();
    // A table for a stack no file is.
    s.write("dform.toml", "[stacks.nope]\nisolated = true\n");
    let r = s.run(&["plan", "net"]).failure();
    assert!(
        r.stderr
            .contains("dform.toml: [stacks.nope] names no stack: there is no stacks/nope.df"),
        "{}",
        r.stderr
    );
    // The table's settings are a closed list.
    s.write("dform.toml", "[stacks.net]\nbackends = 'local(\"x\")'\n");
    let r = s.run(&["plan", "net"]).failure();
    assert!(
        r.stderr
            .contains("unknown field `backends`, expected one of `backend`"),
        "{}",
        r.stderr
    );
}

#[test]
fn the_layout_lints() {
    let s = project("target-lints");
    // A module that is not a stack with a key is an error: a key is its
    // stack's.
    s.write("modules/m.df", "edition 2026\nkey env: string\n");
    let r = s.run(&["plan", "net"]).failure();
    assert!(
        r.stderr
            .contains("modules/m.df: a file that is not a stack has a `key`"),
        "{}",
        r.stderr
    );
    std::fs::remove_file(s.path("modules/m.df")).unwrap();
    // Every other .df is a module, wherever it is (R-65).
    s.write("loose.df", "edition 2026\n");
    let r = s.run(&["plan", "net"]).success();
    assert!(!r.stderr.contains("warning"), "{}", r.stderr);
    // [discovery] exclude: not walked, not linted.
    s.write(
        "dform.toml",
        "[project]\nname = \"t\"\n[discovery]\nexclude = [\"loose.df\", \"scratch/**\"]\n",
    );
    s.write("scratch/net.df", NET);
    let r = s.run(&["plan", "net"]).success();
    assert!(!r.stderr.contains("warning"), "{}", r.stderr);
}

/// A stack is a module the tool uses: a program `use`s it to read its
/// deployments and never instances it; any other module, by its path from
/// the root, whatever file names it (R-65).
#[test]
fn a_program_uses_a_stack_and_never_instances_it() {
    let s = project("target-use-stack");
    s.write(
        "stacks/both.df",
        "edition 2026\ncomponent c {\n  input n: int\n}\ninstance stacks.net x\nprovider fake\n",
    );
    let r = s.run(&["plan", "both"]).failure();
    assert!(
        r.stderr
            .contains("stacks.net is deployed by the tool; `use` it"),
        "{}",
        r.stderr
    );
    s.write(
        "modules/tags.df",
        "edition 2026\nresource net.vpc extra { cidr = \"10.1.0.0/16\" }\n",
    );
    s.write(
        "stacks/both.df",
        "edition 2026\nuse modules.tags\nprovider fake\n",
    );
    let r = s.run(&["plan", "both"]).success();
    assert!(
        r.stdout.contains("+ net.vpc[\"tags::extra\"]"),
        "{}",
        r.stdout
    );
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
audit_sink = "true"
"#,
    );
    s.write(
        "providers/cloud.df",
        "type_provider(x.thing, \"fakecloud\")\ntype_attr(x.thing, \"size\", \"int\", [\"required\"])\n",
    );
    s.write(
        "stacks/p.df",
        r#"edition 2026
provider cloud
resource x.thing a { size = 1 }
pinned(n, c) where project_provider(n, c)
default(k, v) where project_default(k, v)
"#,
    );
    // The provider is the manifest's source: its schema knows x.thing.
    let r = s.run(&["apply", "p"]).success();
    assert!(r.stdout.contains("+ x.thing[\"a\"]"), "{}", r.stdout);
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
    assert!(r.stdout.contains(r#""audit_sink"  "true""#), "{}", r.stdout);

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

/// `[stacks.NAME]` overrides `[defaults]`: the defaults' backend holds
/// a stack's state unless its own table says otherwise, and policy reads
/// both.
#[test]
fn a_stack_table_overrides_the_defaults() {
    let s = Scratch::new("manifest-defaults");
    s.write(
        "dform.toml",
        "[defaults]\nbackend = 'local(\"a/{stack}\")'\n",
    );
    s.write(
        "stacks/p.df",
        "edition 2026\nprovider fake\nresource db.postgres main { size = 1 }\n\
         stacked(n, k, v) where project_stack(n, k, v)\n",
    );
    s.run(&["apply", "p", "--yes"]).success();
    assert!(s.path("a/p/state.json").exists());
    s.write(
        "dform.toml",
        "[defaults]\nbackend = 'local(\"a/{stack}\")'\n\n[stacks.p]\nbackend = 'local(\"b\")'\n",
    );
    s.run(&["apply", "p", "--yes"]).success();
    assert!(s.path("b/state.json").exists());
    let r = s.run(&["query", "stacked(N, K, V)", "p"]).success();
    assert!(
        r.stdout.contains(r#""p"  "backend"  "local(\"b\")""#),
        "{}",
        r.stdout
    );
}

/// A backend may name the key: each deployment's state is where it says.
#[test]
fn a_backend_names_the_key() {
    let s = project("backend-key");
    s.write(
        "dform.toml",
        "[stacks.app]\nbackend = 'local(\"state/{stack}-{env}\")'\n",
    );
    s.run(&["apply", "app", "env=prod"]).success();
    assert!(s.path("state/app-prod/state.json").exists());
    let r = s.run(&["plan", "app", "env=prod"]).success();
    assert_eq!(r.summary(), "stack app is undeformed", "{}", r.stdout);
}

#[test]
fn stack_list_shows_deployments_and_their_last_apply() {
    let s = project("stack-list");
    s.run(&["apply", "app", "env=prod"]).success();
    s.run(&["plan", "net", "--out", "net.json"]).success();
    let r = s.run(&["stack", "list"]).success();
    // One row per deployment: its stack, file, last apply and pending
    // plan; the commit column is left out, no row has one.
    let lines: Vec<&str> = r.stdout.lines().collect();
    let cells = |l: &str| -> Vec<String> {
        l.split("  ")
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect()
    };
    assert_eq!(
        cells(lines[0]),
        [
            "stack",
            "file",
            "deployment",
            "applied",
            "by",
            "result",
            "pending"
        ],
        "{}",
        r.stdout
    );
    let app = cells(lines[1]);
    assert_eq!(
        app[..3],
        ["app[env]", "stacks/app.df", "app[env=prod]"],
        "{}",
        r.stdout
    );
    assert!(app[3].starts_with("20") && app[5] == "ok", "{}", r.stdout);
    let net = cells(lines[2]);
    assert_eq!(
        net[..4],
        ["net", "stacks/net.df", "net", "never"],
        "{}",
        r.stdout
    );
    assert!(net[4].starts_with("net.json (sha256:"), "{}", r.stdout);
    assert_eq!(lines.len(), 3, "{}", r.stdout);
}

#[test]
fn state_show_mv_and_unlock() {
    let s = project("state-cmds");
    s.run(&["apply", "net"]).success();
    let r = s.run(&["state", "show", "net"]).success();
    assert!(
        r.stdout.contains(
            "address            provider   remote\nnet.vpc[\"shared\"]  fakecloud  shared\n"
        ),
        "{}",
        r.stdout
    );
    s.run(&[
        "state",
        "mv",
        r#"net.vpc["shared"]"#,
        r#"net.vpc["moved"]"#,
        "net",
    ])
    .success();
    let r = s.run(&["state", "show", "net"]).success();
    assert!(r.stdout.contains("\nnet.vpc[\"moved\"]  "), "{}", r.stdout);
    // An address as plan prints it names one object; the old spelling is
    // refused.
    let one = r#"net.vpc["moved"]"#;
    let r = s.run(&["state", "show", "net", "--address", one]).success();
    assert_eq!(
        r.stdout,
        "address           provider   remote\nnet.vpc[\"moved\"]  fakecloud  shared\n"
    );
    let r = s
        .run(&["state", "show", "net", "--address", r#"net.vpc["shared"]"#])
        .failure();
    assert!(
        r.stderr
            .contains("stack net has no object at net.vpc[\"shared\"]"),
        "{}",
        r.stderr
    );
    s.run(&["state", "mv", "net.vpc/moved", "net.vpc/x", "net"])
        .failure();
    let r = s
        .run(&[
            "state",
            "mv",
            r#"net.vpc["shared"]"#,
            r#"net.vpc["x"]"#,
            "net",
        ])
        .failure();
    assert!(
        r.stderr
            .contains("state mv: stack net has no object at net.vpc[\"shared\"]"),
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
        "edition 2026\nprovider fake\nresource net.vpc shared {cidr=\"10.9.0.0/16\"}\n",
    );
    let r = s.run(&["fmt", "--check"]).failure();
    assert_eq!(r.stdout, "stacks/net.df\n");
    s.run(&["fmt"]).success();
    s.run(&["fmt", "--check"]).success();
}

/// Outside a project a program plans, with no state; nothing that reads or
/// writes state runs, and nothing is written. A directory inside a git
/// repository (here, under the build's directory) is no project either.
#[test]
fn outside_a_project_only_what_writes_no_state_runs() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("outside-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s = Scratch::adopt(dir);
    s.write("net.df", NET);
    let r = s.run(&["plan", "net.df"]).success();
    assert_eq!(r.summary(), "plan: 1 deformation (1 create)");
    for args in [
        &["apply", "net.df"][..],
        &["plan", "net.df", "--out", "plan.json"],
        &["controller", "run", "net.df", "--once"],
        &["log", "net.df"],
        &["state", "show", "net.df"],
        &["stack", "list"],
        &["stack", "handover", "net", "--to", "local(\"x\")"],
    ] {
        let r = s.run(args).failure();
        assert!(
            r.stderr.contains("not in a project: no dform.toml above")
                && r.stderr.contains("run `dform init`"),
            "{args:?}: {}",
            r.stderr
        );
    }
    let r = s.run(&["plan", "net"]).failure();
    assert!(
        r.stderr.contains("stack net: not in a project")
            && r.stderr.contains("name a program file"),
        "{}",
        r.stderr
    );
    assert!(!s.path("dform.state").exists());
    assert!(!s.path("plan.json").exists());
    // A world fixture's run keeps its state beside the world file.
    s.run(&["dev", "--world", "w.json", "apply", "net.df"])
        .success();
    assert!(s.path("w.state.json").exists());
    assert!(!s.path("dform.state").exists());
    assert!(!common::repo().join("dform.state").exists());
}

/// The repository's root has no dform.toml: no project.
#[test]
fn the_repository_root_is_not_a_project() {
    let out = common::dform()
        .arg("plan")
        .current_dir(common::repo())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not in a project"), "{err}");
}

#[test]
fn init_makes_a_project() {
    let s = Scratch::new("init");
    s.write(".gitignore", "target/\n");
    s.write("stacks/net.df", NET);
    let r = s.run(&["init", "shop"]).success();
    assert!(r.stdout.contains("wrote"), "{}", r.stdout);
    assert!(s.read("dform.toml").contains("name = \"shop\""));
    assert_eq!(s.read(".gitignore"), "target/\ndform.state/\n");
    s.run(&["apply", "net"]).success();
    assert!(s.path("dform.state/net/state.json").exists());
    let r = s.run(&["init"]).failure();
    assert!(r.stderr.contains("is a project already"), "{}", r.stderr);
}

/// Every path a program states resolves from the project root, whichever
/// file states it.
#[test]
fn program_paths_resolve_from_the_root() {
    let s = project("root-paths");
    s.write("data/peers.csv", "name\na\nb\n");
    s.write("data/note.txt", "hello");
    s.write("data/tags.df", "edition 2026\n\ntag(\"x\")\n");
    s.write(
        "providers/cloud/schema.df",
        "type_provider(x.thing, \"fakecloud\")\n",
    );
    s.write(
        "stacks/paths.df",
        r#"edition 2026
input peer from csv("data/peers.csv")
use data.tags
decl peer(name: string)
provider cloud { source = "providers/cloud" }
provider file
note(v) where v = file.text["data/note.txt"]
resource x.thing "${n}" {
  label = "${n}-${g}-${v}"
} where peer(name: n), tags.tag(g), note(v)
"#,
    );
    let r = s.run(&["plan", "paths"]).success();
    assert!(r.stdout.contains("label = \"a-x-hello\""), "{}", r.stdout);
    assert!(r.stdout.contains("label = \"b-x-hello\""), "{}", r.stdout);
}

/// A Scratch can never own, and so never delete, a directory outside the
/// test roots: the repository's own directory is refused.
#[test]
#[should_panic(expected = "not under a test root")]
fn a_scratch_refuses_a_directory_outside_the_test_roots() {
    let _ = Scratch::adopt(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
}

//! A provider the program configures holds what it serves until its
//! settings are known (R-45, R-110), whether the program writes its
//! `use k8s { .. }` block or the core `provider_config("k8s", ..)` the block
//! lowers to, and whatever link serves the types: a provider process of its
//! own (the fake run as a plugin beside the k8s mock) or one mock playing
//! both schemas (`use fake` and `use k8s`, the mock's one link, configured
//! by neither name). A setting a tick makes (the server's endpoint) holds
//! the cluster's objects for the tick after it; a setting that reads a row
//! nothing makes holds them under `later`.
//!
//! Through the command line on every backend, and through the engine's API
//! (`deployment::load`, `locate`, `evaluate`) over the linked mock.

mod common;
use common::{BACKENDS, Scratch};

const USE: &str = r#"
use fake
use k8s { kubeconfig = server.endpoint }
resource db.postgres server { name = "server" }
resource k8s.namespace ns { metadata.name = "app" }
"#;

const CORE: &str = r#"
use fake
use k8s
provider_config("k8s", { kubeconfig: server.endpoint })
resource db.postgres server { name = "server" }
resource k8s.namespace ns { metadata.name = "app" }
"#;

/// A setting that reads a row no rule and no tick makes.
const ABSENT: &str = r#"
use fake
use k8s { kubeconfig = db.postgres["absent"].endpoint }
resource db.postgres server { name = "server" }
resource k8s.namespace ns { metadata.name = "app" }
"#;

const TICK2_USE: &str = "\
tick 2  1 change
  waits on  provider k8s  kubeconfig = server.endpoint
  + k8s.namespace ns    p.df:5
";

const TICK2_CORE: &str = "\
tick 2  1 change
  waits on  provider k8s  kubeconfig
  + k8s.namespace ns    p.df:6
";

const LATER: &str = "\
later
  waits on  provider k8s  kubeconfig = db.postgres[\"absent\"].endpoint
  + k8s.namespace ns    p.df:5
";

/// `dform dev --world w.json plan p.df` of `prog` over `backend`, its
/// attributes left out, the fake
/// played by the mock's one link, or (`plugin`) by a process of its own.
fn plan(backend: common::Backend, prog: &str, plugin: bool) -> String {
    let s = Scratch::new("use-block-settings");
    let prog = match plugin {
        true => {
            std::fs::create_dir_all(s.path("prov")).unwrap();
            std::os::unix::fs::symlink(
                common::exe("dform-provider-fake"),
                s.path("prov/dform-provider-fake"),
            )
            .unwrap();
            prog.replacen("use fake\n", "use fake { source = \"prov\" }\n", 1)
        }
        false => prog.to_string(),
    };
    s.write("p.df", &prog);
    let out = s
        .run_on(backend, &["dev", "--world", "w.json", "plan", "p.df"])
        .success()
        .stdout;
    // The headers and the waits, not each object's attributes.
    out.lines()
        .filter(|l| !l.starts_with("      "))
        .map(|l| format!("{l}\n"))
        .collect()
}

#[test]
fn an_open_setting_holds_the_providers_objects_on_every_path() {
    for backend in BACKENDS {
        // A plugin executable needs the process backend.
        let plugins: &[bool] = match backend {
            common::Backend::Process => &[false, true],
            _ => &[false],
        };
        for &plugin in plugins {
            for (prog, tick2) in [(USE, TICK2_USE), (CORE, TICK2_CORE)] {
                let out = plan(backend, prog, plugin);
                assert!(
                    out.starts_with(
                        "plan: 2 changes (2 create) over 2 ticks\n\ntick 1  1 change\n"
                    ),
                    "{backend:?} plugin={plugin}\n{out}"
                );
                assert!(out.contains(tick2), "{backend:?} plugin={plugin}\n{out}");
            }
            let out = plan(backend, ABSENT, plugin);
            assert!(
                out.starts_with(
                    "plan: 1 change (1 create) over 1 tick; 1 create waiting on provider k8s  \
                     kubeconfig = db.postgres[\"absent\"].endpoint\n"
                ),
                "{backend:?} plugin={plugin}\n{out}"
            );
            assert!(out.ends_with(LATER), "{backend:?} plugin={plugin}\n{out}");
        }
    }
}

/// What the engine's API plans for `prog` over the linked mock: the
/// namespace's wait, as the report says it.
fn engine_wait(prog: &str) -> Option<Vec<String>> {
    use dform::deployment::{self, Notes, Options, Selection, Target};
    let s = Scratch::new("use-block-settings-engine");
    let file = s.write("p.df", prog);
    let read = |p: &std::path::Path| std::fs::read_to_string(p);
    let mut notes = Notes::default();
    let target = Target {
        files: vec![file],
        ..Default::default()
    };
    let loaded = deployment::load(&target, env!("CARGO_PKG_VERSION"), &read, &mut notes).unwrap();
    let no_s3 = |spec: &dform::store::S3Spec| -> anyhow::Result<_> {
        anyhow::bail!("no bucket here: {spec}")
    };
    let located = loaded
        .locate(
            &Selection {
                root: s.path("dform.state"),
                world: Some(s.path("w.json")),
                ..Default::default()
            },
            &no_s3,
            &mut notes,
        )
        .unwrap();
    let launch = dform_mock::Linked::direct();
    let opts = Options {
        policy: true,
        ..Options::new(&launch)
    };
    let ev = located.evaluate(Vec::new(), &opts, &mut notes).unwrap();
    let planned = ev.policy.unwrap().unwrap();
    let ns = planned
        .plan
        .actions
        .iter()
        .find(|a| a.addr.typ == "k8s.namespace")
        .unwrap();
    dform::report::waits_on(ns, &planned.sections)
}

#[test]
fn the_engine_api_holds_them_as_the_command_line_does() {
    let held = |w: &str| Some(vec![w.to_string()]);
    assert_eq!(
        engine_wait(USE),
        held("provider k8s  kubeconfig = server.endpoint")
    );
    assert_eq!(engine_wait(CORE), held("provider k8s  kubeconfig"));
    assert_eq!(
        engine_wait(ABSENT),
        held("provider k8s  kubeconfig = db.postgres[\"absent\"].endpoint")
    );
}

/// A provider declares the settings its `use` block gives it, each
/// sensitive or not (`provider_setting` in a mock's schema, `settings` in a
/// plugin's handshake): a secret goes only to a sensitive one, E0304
/// elsewhere, the help naming those it may go to. A provider declaring
/// none takes any.
fn settings_plan(s: &Scratch, block: &str, plugin: bool) -> common::Run {
    let block = match plugin {
        true => block.replacen("use k8s {", "use k8s { source = \"./providers/k8s\",", 1),
        false => block.to_string(),
    };
    s.write(
        "p.df",
        &format!(
            "input pw: secret(string) = \"opensesame\"\n{block}\n\
             resource k8s.namespace n {{ metadata.name = \"a\" }}\n"
        ),
    );
    if plugin && !s.path("providers/k8s").exists() {
        std::fs::create_dir_all(s.path("providers/k8s")).unwrap();
        std::os::unix::fs::symlink(
            common::exe("dform-provider-k8s"),
            s.path("providers/k8s/dform-provider-k8s"),
        )
        .unwrap();
    }
    let out = common::dform()
        .args(["dev", "--world", "w.json", "plan", "p.df"])
        .current_dir(&s.dir)
        .env("DFORM_K8S_OFFLINE", "1")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT")
        .output()
        .unwrap();
    common::Run::from(out)
}

#[test]
fn a_secret_goes_only_to_a_setting_declared_sensitive() {
    let help = "provider k8s's sensitive settings: ";
    for plugin in [false, true] {
        let s = Scratch::new("use-block-sensitive");
        let r = settings_plan(&s, "use k8s { host = pw }", plugin).failure();
        assert!(
            r.stderr.contains(
                "E0304: a secret reaches provider k8s's setting host, not declared sensitive"
            ) && r.stderr.contains(help)
                && r.stderr.contains("kubeconfig")
                && r.stderr.contains("token"),
            "plugin={plugin}\n{}",
            r.stderr
        );
        let r = settings_plan(&s, "use k8s { cluster_name = pw }", plugin).failure();
        assert!(
            r.stderr.contains(
                "E0304: a secret reaches provider k8s's setting cluster_name, which it does not declare"
            ),
            "plugin={plugin}\n{}",
            r.stderr
        );
        // Where the provider keeps it: no E0304 (the plugin, offline,
        // may refuse the kubeconfig's text at Configure).
        let r = settings_plan(&s, "use k8s { kubeconfig = pw, host = \"h\" }", plugin);
        assert!(!r.stderr.contains("E0304"), "plugin={plugin}\n{}", r.stderr);
    }
    // The fake cloud declares no setting: it takes any.
    let s = Scratch::new("use-block-undeclared");
    s.write(
        "p.df",
        "input pw: secret(string) = \"opensesame\"\nuse fake { region = pw }\n\
         resource net.vpc v { cidr = \"10.0.0.0/16\" }\n",
    );
    let r = s.run(&["dev", "--world", "w.json", "plan", "p.df"]);
    assert!(!r.stderr.contains("E0304"), "{}", r.stderr);
}

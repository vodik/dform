//! The provider plugin protocol at the CLI: the mock is a separate process,
//! a provider that dies during an Apply is a failed action naming the
//! resource and the next apply resumes, a `provider` source directory
//! holding an executable is that plugin, and `dform provider check` is the
//! conformance suite, which the mock passes on every backend (a process
//! over gRPC, linked in, linked in across prost).

mod common;
use common::{BACKENDS, Backend, Scratch};
use dform::plugin::link::Link;
use dform::plugin::{Config, Launch, Providers};

const PROG: &str = r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
resource compute.vm app { subnet_id = ref(net.subnet, "a", "id") }
"#;

fn fake() -> String {
    common::exe("dform-provider-fake")
}

fn dform(s: &Scratch, args: &[&str]) -> common::Run {
    s.run(&[&["--file", "p.df", "--world", "w.json"][..], args].concat())
}

fn identities(s: &Scratch) -> Vec<String> {
    let st: serde_json::Value = serde_json::from_str(&s.read("w.state.json")).unwrap();
    st["resources"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

/// The provider process exits as it is called to Apply the third action:
/// dform reports that action failed, naming it, keeps the two identities
/// before it, and the next apply finishes the rest.
#[test]
fn a_provider_crash_mid_apply_fails_the_action_and_resume_finishes() {
    let s = Scratch::new("protocol-crash");
    s.write("p.df", PROG);
    let r = dform(&s, &["apply", "--chaos", "crash=compute.vm/app"]).failure();
    assert!(
        r.stderr.contains(
            "apply compute.vm/app: the provider fakecloud exited during the call \
             (exit status: 137)"
        ),
        "{}",
        r.stderr
    );
    assert_eq!(identities(&s), ["net.subnet::a", "net.vpc::main"]);
    let r = dform(&s, &["apply"]).success();
    assert!(
        r.stdout
            .contains("resuming the apply interrupted at tick 1; remaining: compute.vm.app"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    let r = dform(&s, &["plan"]).success();
    assert_eq!(r.summary(), "stack p is undeformed", "{}", r.stdout);
}

/// `provider NAME { source = "DIR" }` where DIR holds an executable
/// `dform-provider*`: that executable is the provider (here the mock
/// itself, which plays the `fake` schema when given none).
#[test]
fn a_source_directory_holding_an_executable_is_that_plugin() {
    let s = Scratch::new("protocol-plugin-dir");
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(fake(), s.path("prov/dform-provider-fake")).unwrap();
    s.write(
        "p.df",
        "edition 2026\n\nprovider fake { source = \"prov\" }\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n",
    );
    let r = dform(&s, &["plan"]).success();
    assert!(r.stdout.contains("+ net.vpc.main"), "{}", r.stdout);
}

#[test]
fn the_mock_conforms() {
    let s = Scratch::new("protocol-check");
    let exe = fake();
    let runs = BACKENDS
        .into_iter()
        .map(|b| (b, "fake"))
        .chain([(Backend::Process, exe.as_str())]);
    let mut reports = Vec::new();
    for (backend, path) in runs {
        let r = s.run_on(backend, &["provider", "check", path]).success();
        assert!(!r.stdout.contains("FAIL"), "{backend:?}: {}", r.stdout);
        assert!(
            r.stdout
                .contains("ok    Apply refuses an action whose assertion fails"),
            "{backend:?}: {}",
            r.stdout
        );
        assert!(
            r.stdout.ends_with("conforms\n"),
            "{backend:?}: {}",
            r.stdout
        );
        reports.push(r.stdout.replace(path, "PATH"));
    }
    // The same cases, the same report.
    assert!(reports.windows(2).all(|w| w[0] == w[1]), "{reports:#?}");
}

/// The backends a test links: the process one, the direct and wire ones.
fn launches() -> [(Backend, Box<dyn Launch>); 3] {
    [
        (Backend::Process, Box::new(dform_grpc::client::Process)),
        (Backend::Direct, Box::new(dform_mock::Linked::direct())),
        (Backend::Wire, Box::new(dform_mock::Linked::wire())),
    ]
}

/// An executable that does not speak the protocol is a deviation, not a
/// hang or a crash of dform.
#[test]
fn a_provider_without_a_handshake_deviates() {
    let s = Scratch::new("protocol-check-bad");
    let script = s.write("dform-provider-bad", "#!/bin/sh\necho hello\n");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let r = s
        .run(&["provider", "check", "./dform-provider-bad"])
        .failure();
    assert!(
        r.stdout.contains("FAIL  Handshake") && r.stdout.contains("dform-provider|1|ADDRESS"),
        "{}",
        r.stdout
    );
    assert!(r.stderr.contains("1 of 1 checks deviate"), "{}", r.stderr);
}

/// The type of each `type_attr` row of a Schema answer, and how many
/// `type_provider` rows it holds.
fn schema_rows(
    conn: &mut Link,
    types: Option<&[&str]>,
) -> (std::collections::BTreeSet<String>, usize) {
    use dform::plugin::{pb, wire};
    let req = pb::SchemaRequest {
        types: types.map(|t| pb::TypeFilter {
            names: t.iter().map(|s| s.to_string()).collect(),
        }),
    };
    let resp: pb::SchemaResponse = conn.call(req).unwrap();
    let facts: Vec<_> = resp
        .facts
        .iter()
        .map(|f| wire::from_fact(f).unwrap())
        .collect();
    let typ = |a: &dform::ast::Atom| dform::partition::fmt_term(&a.args[0]);
    (
        facts
            .iter()
            .filter(|a| a.pred == "type_attr")
            .map(typ)
            .collect(),
        facts.iter().filter(|a| a.pred == "type_provider").count(),
    )
}

/// A Schema request naming types gets their rows (and those of the types
/// they alias), and every type_provider row; none names all of them.
#[test]
fn the_mock_answers_the_schema_of_the_types_asked_for() {
    for (_, launch) in launches() {
        the_mock_answers_the_schema_asked_for(launch.mock().unwrap());
    }
}

fn the_mock_answers_the_schema_asked_for(mut conn: Link) {
    let s = Scratch::new("protocol-schema-scope");
    let config = serde_json::json!({
        "schemas": ["fake", "k8s"],
        "world": s.path("w.json").display().to_string(),
    });
    let config = Some(dform::plugin::wire::doc(&config));
    let _: dform::plugin::pb::ConfigureResponse = conn
        .call(dform::plugin::pb::ConfigureRequest { config })
        .unwrap();
    let (all, providers) = schema_rows(&mut conn, None);
    assert!(all.len() > 10, "{all:?}");
    let (some, scoped_providers) = schema_rows(&mut conn, Some(&["net.vpc", "k8s.deployment"]));
    assert_eq!(
        some.into_iter().collect::<Vec<_>>(),
        ["\"k8s.deployment\"", "\"net.vpc\""],
    );
    assert_eq!(scoped_providers, providers);
}

/// A run that knows the types it names asks the providers for their
/// schema only: the rest is not loaded, and its catalog cannot be asked
/// for more.
#[test]
fn a_scoped_run_loads_the_schema_of_its_types() {
    for (_, launch) in launches() {
        a_scoped_run_loads_the_schema_of_its_types_on(&*launch);
    }
}

fn a_scoped_run_loads_the_schema_of_its_types_on(launch: &dyn Launch) {
    let s = Scratch::new("protocol-schema-load");
    let backend = Providers::start_deferred(
        launch,
        &[],
        &Config {
            world: s.path("w.json"),
            inventory: s.path("inv.json"),
            chaos: vec![],
        },
    )
    .unwrap();
    let named: std::collections::BTreeSet<String> = ["net.vpc".to_string()].into();
    backend.load_schema(Some(&named)).unwrap();
    let schema = backend.schema();
    assert!(schema.attr("net.vpc", "cidr").is_some());
    assert!(schema.attr("net.subnet", "cidr").is_none());
    assert!(schema.provider_of.contains_key("net.subnet"));
    assert!(backend.catalog(Some(&named)).is_ok());
    assert!(backend.catalog(None).is_err());
}

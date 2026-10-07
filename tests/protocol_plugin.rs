//! The provider plugin protocol at the CLI: the mock is a separate process,
//! a provider that dies during an Apply is a failed action naming the
//! resource and the next apply resumes, a provider's `source` directory
//! holding an executable is that plugin, and `dform provider check` is the
//! conformance suite, which the mock passes on every backend (a process
//! over gRPC, linked in, linked in across prost).

mod common;
use common::{BACKENDS, Backend, Scratch, identities, mock};
use dform::plugin::link::Link;
use dform::plugin::{Config, Launch, Providers};

const PROG: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
resource compute.vm app { subnet_id = ref(net.subnet, "a", "id") }
use fake
"#;

fn fake() -> String {
    common::exe("dform-provider-fake")
}

/// The provider process exits as it is called to Apply the third action:
/// dform reports that action failed, naming it, keeps the two identities
/// before it, and the next apply finishes the rest. (Its exit status is the
/// native process's: a component's wasip2 exit carries none, and
/// tests/host_wasm.rs has its own case.)
#[test]
fn a_provider_crash_mid_apply_fails_the_action_and_resume_finishes() {
    let s = Scratch::new("protocol-crash");
    s.write("p.df", PROG);
    let r = mock(&s, &["apply", "--chaos", "crash=compute.vm[\"app\"]"]).failure();
    assert!(
        r.stderr.contains(
            "apply compute.vm app: the provider fakecloud exited during the call \
             (exit status: 137)"
        ),
        "{}",
        r.stderr
    );
    assert_eq!(identities(&s), ["net.subnet::a", "net.vpc::main"]);
    let r = mock(&s, &["apply"]).success();
    assert!(
        r.stdout
            .contains("\ntick 1  1 remaining, resumed\n  + compute.vm app  "),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("apply: complete"), "{}", r.stdout);
    let r = mock(&s, &["plan"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}

/// The mock is dform itself (`dform __provider fake`), never an executable
/// found beside it that may be of another build: here a `dform` whose
/// neighbouring `dform-provider-fake` is `false`. `DFORM_PROVIDER_FAKE`
/// still names another executable to run instead.
#[test]
fn the_mock_is_dform_itself_not_the_executable_beside_it() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("selfspawn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s = Scratch::adopt(dir);
    // Linked, not copied: the same filesystem as the build, and nothing
    // written just before its exec (ETXTBSY).
    std::fs::hard_link(env!("CARGO_BIN_EXE_dform"), s.path("dform")).unwrap();
    std::os::unix::fs::symlink("/bin/false", s.path("dform-provider-fake")).unwrap();
    s.write("dform.toml", "[project]\nedition = \"2026\"\n");
    s.write("p.df", PROG);
    let run = |fake: Option<&str>| {
        let mut c = std::process::Command::new(s.path("dform"));
        c.env_remove("DFORM_PROVIDER_FAKE");
        if let Some(f) = fake {
            c.env("DFORM_PROVIDER_FAKE", f);
        }
        let args = common::on("p.df", &["--world", "w.json"], &["plan"]);
        common::Run::from(c.args(args).current_dir(&s.dir).output().unwrap())
    };
    let r = run(None).success();
    assert!(r.stdout.contains("+ net.vpc main"), "{}", r.stdout);
    let stale = s.path("dform-provider-fake");
    let r = run(Some(stale.to_str().unwrap())).failure();
    assert!(
        r.stderr
            .contains("dform-provider-fake exited before its handshake"),
        "{}",
        r.stderr
    );
    // The command is dform's, not the user's.
    let help = std::process::Command::new(s.path("dform"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("__provider"));
}

/// `use NAME { source = "DIR" }` where DIR holds an executable
/// `dform-provider*`: that executable is the provider (here the mock
/// itself, which plays the `fake` schema when given none).
#[test]
fn a_source_directory_holding_an_executable_is_that_plugin() {
    let s = Scratch::new("protocol-plugin-dir");
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(fake(), s.path("prov/dform-provider-fake")).unwrap();
    s.write(
        "p.df",
        "\n\nuse fake { source = \"prov\" }\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\n",
    );
    let r = mock(&s, &["plan"]).success();
    assert!(r.stdout.contains("+ net.vpc main"), "{}", r.stdout);
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
        for line in [
            "ok    Schema's types are named under the providers that serve them",
            "ok    Apply refuses an action whose assertion fails",
            "ok    Apply CREATE again with the same idempotency key answers the object it made",
            "ok    Query provider.created answers what an idempotency key made",
            "ok    Query provider.created answers nothing for a key that made nothing",
        ] {
            assert!(r.stdout.contains(line), "{backend:?}: {line}\n{}", r.stdout);
        }
        assert!(
            r.stdout.ends_with("conforms\n"),
            "{backend:?}: {}",
            r.stdout
        );
        // The executable is hosted natively, the host's service beside it,
        // and declares what it uses (its Manifest service, R-13b).
        if path == exe {
            for line in [
                "host  native: imports: host none",
                "host  native: imports: beyond the host wasi:filesystem",
                "host  native: granted: none",
            ] {
                assert!(r.stdout.contains(line), "{line}\n{}", r.stdout);
            }
        }
        // The same cases; how each is hosted (`host` lines) is its own.
        let cases: Vec<&str> = r
            .stdout
            .lines()
            .filter(|l| !l.starts_with("host  "))
            .collect();
        reports.push(cases.join("\n").replace(path, "PATH"));
    }
    // The same cases, the same report.
    assert!(reports.windows(2).all(|w| w[0] == w[1]), "{reports:#?}");
}

/// The backends a test links: the process one, the direct and wire ones.
fn launches() -> [(Backend, Box<dyn Launch>); 3] {
    // The process backend spawns the mock provider: in a test binary,
    // its own executable, started as the CLI starts it (`dform-host`'s
    // launcher: the component when `DFORM_PROVIDER_FAKE` names one).
    [
        (
            Backend::Process,
            Box::new(dform_host::Launcher::Mock(fake().into())),
        ),
        (Backend::Direct, Box::new(dform_mock::Linked::direct())),
        (Backend::Wire, Box::new(dform_mock::Linked::wire())),
    ]
}

/// An executable that does not speak the protocol is a deviation, not a
/// hang or a crash of dform.
#[test]
fn a_provider_without_a_handshake_deviates() {
    let s = Scratch::new("protocol-check-bad");
    // An executable already on disk (echo prints an empty line), linked
    // rather than written: a script written just before its exec races
    // other tests' forks (ETXTBSY).
    std::os::unix::fs::symlink("/bin/echo", s.path("dform-provider-bad")).unwrap();
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
            cache: None,
            ..Default::default()
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

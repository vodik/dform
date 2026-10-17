//! The wasm host (R-13b, the `wasm` feature): the fake provider built as a
//! component conforms as its native build does, a run plans, applies and
//! resumes through it, a component's grantable imports are refused
//! without their grant and admitted with it, and one call of a component
//! keeps several host calls in flight (the async ABI). The OVH and
//! Kubernetes providers, built as wasm providers, run against their fake
//! APIs and trace as their native builds do.
//!
//! The components are built apart (a test never runs cargo):
//!
//!   cargo xtask components
//!   cargo test --features wasm --test main -- host_wasm::
//!
//! Every other test that drives the mock runs through the wasm host when
//! `DFORM_PROVIDER_FAKE` names the component (the CLI's mock is then the
//! component): `DFORM_PROVIDER_FAKE=$PWD/target/wasm32-wasip3/release/dform_provider_fake.wasm
//! cargo test --features wasm --test chaos`.

#![cfg(feature = "wasm")]

use crate::common;
use common::{Scratch, identities};

const PROG: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
resource compute.vm app { subnet_id = ref(net.subnet, "a", "id") }
use fake
"#;

/// The fake provider as a component, built beside the workspace's target.
fn component() -> String {
    built(
        "dform_provider_fake.wasm",
        "cargo build -p dform-provider-fake --release --target wasm32-wasip3 --features component",
    )
}

/// The component `file` under the wasip3 target's directory, release or
/// debug, else a panic naming the `build` command.
fn built(file: &str, build: &str) -> String {
    let target = std::path::Path::new(env!("CARGO_BIN_EXE_dform"))
        .parent()
        .and_then(|d| d.parent())
        .unwrap()
        .to_path_buf();
    for profile in ["release", "debug"] {
        let p = target.join(format!("wasm32-wasip3/{profile}/{file}"));
        if p.exists() {
            return p.display().to_string();
        }
    }
    panic!("the component {file} is not built: {build}");
}

/// `dform ARGS` with the mock as the component, its compiled code cached
/// under the build's directory for tests.
fn on_component<S: AsRef<std::ffi::OsStr>>(s: &Scratch, args: &[S]) -> common::Run {
    common::Run::from(component_dform(s, args).output().unwrap())
}

/// The command [`on_component`] runs.
fn component_dform<S: AsRef<std::ffi::OsStr>>(s: &Scratch, args: &[S]) -> std::process::Command {
    let mut c = common::dform();
    c.args(common::yes(args))
        .env("DFORM_PROVIDER_FAKE", component())
        .env("XDG_CACHE_HOME", wasm_cache())
        .current_dir(&s.dir);
    c
}

/// `dform dev --world w.json ARGS` on `p.df`, the mock the component.
fn mock(s: &Scratch, args: &[&str]) -> common::Run {
    on_component(s, &common::on("p.df", &["--world", "w.json"], args))
}

/// One cache for the test binary's runs and its own process (in-process
/// links, `dform_host::wasm::compile_cache`): the component is compiled
/// once, never into the user's cache.
fn wasm_cache() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("host-wasm-cache")
}

#[test]
fn the_build_says_the_wasm_host_is_in() {
    let s = Scratch::new("host-wasm-version");
    let r = s.run(&["version"]).success();
    assert!(r.stdout.contains("wasm host in"), "{}", r.stdout);
}

/// The same conformance report on the component as on the native
/// executable, but for how each is hosted.
#[test]
fn the_fake_component_conforms_as_its_native_build_does() {
    let s = Scratch::new("host-wasm-check");
    let wasm = on_component(&s, &["provider", "check", "fake"]).success();
    assert!(wasm.stdout.ends_with("conforms\n"), "{}", wasm.stdout);
    for line in [
        "host  wasm: imports: host none",
        "host  wasm: imports: beyond the host wasi:filesystem",
        "host  wasm: granted: wasi:filesystem",
    ] {
        assert!(wasm.stdout.contains(line), "{line}\n{}", wasm.stdout);
    }
    let exe = common::exe("dform-provider-fake");
    let native = s.run(&["provider", "check", &exe]).success();
    let cases = |out: &str, path: &str| -> String {
        out.lines()
            .filter(|l| !l.starts_with("host  "))
            .collect::<Vec<_>>()
            .join("\n")
            .replace(path, "PATH")
    };
    assert_eq!(
        cases(&wasm.stdout, "fake"),
        cases(&native.stdout, &exe),
        "the two hosts' reports differ"
    );
}

/// Plan, apply and plan again, up to date, with the mock a component.
#[test]
fn a_run_plans_and_applies_through_the_component() {
    let s = Scratch::new("host-wasm-apply");
    s.write("p.df", PROG);
    let r = mock(&s, &["plan"]).success();
    assert!(r.stdout.contains("+ net.vpc main"), "{}", r.stdout);
    mock(&s, &["apply"]).success();
    let r = mock(&s, &["plan"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}

/// The component's Apply answers a stream of events and a future of its
/// result (the component model's async ABI, R-130): what the mock says
/// under chaos `delay` reaches dform as it is written, as the native
/// build's does (tests/apply_events.rs: `DFORM_LOG=debug` logs each event).
#[test]
fn an_apply_s_events_come_through_the_component_s_stream() {
    let s = Scratch::new("host-wasm-events");
    s.write("p.df", PROG);
    let args = common::on(
        "p.df",
        &[
            "--world",
            "w.json",
            "--chaos",
            "delay=net.subnet[\"a\"]:200",
        ],
        &["apply", "--yes"],
    );
    let out = component_dform(&s, &args)
        .env("DFORM_LOG", "debug")
        .output()
        .unwrap();
    let r = common::Run::from(out).success();
    assert!(
        r.stderr
            .lines()
            .any(|l| l.ends_with("apply net.subnet a: made: answers 200ms late (chaos delay)")),
        "{}",
        r.stderr
    );
}

/// The component exits as it is called to Apply the third action: the
/// action fails naming it, its exit status the native process's (WASI
/// 0.3's exit carries its code), the two before it are kept, and the next
/// apply finishes, as with the native process (tests/protocol_plugin.rs).
#[test]
fn a_component_crash_mid_apply_fails_the_action_and_resume_finishes() {
    let s = Scratch::new("host-wasm-crash");
    s.write("p.df", PROG);
    let r = mock(&s, &["apply", "--chaos", "crash=compute.vm[\"app\"]"]).failure();
    assert!(
        r.stderr.contains(
            "! apply compute.vm app: the provider died; the change may have taken effect\n    \
             the provider fakecloud exited during the call (exit status: 137)"
        ),
        "{}",
        r.stderr
    );
    assert_eq!(identities(&s), ["net.subnet::a", "net.vpc::main"]);
    let r = mock(&s, &["apply"]).success();
    assert!(!r.stdout.contains("apply: complete"), "{}", r.stdout);
}

/// A component that imports `wasi:sockets` is refused without the grant,
/// naming the provider and the package, and admitted with it.
#[test]
fn wasi_sockets_needs_its_grant() {
    use dform::plugin::host::Grants;
    let s = Scratch::new("host-wasm-sockets");
    // A component importing the network interface and exporting nothing:
    // admitted, it fails later, for want of the provider export.
    let wasm = wat::parse_str(
        r#"(component
             (import "wasi:sockets/types@0.3.0" (instance
               (export "tcp-socket" (type (sub resource))))))"#,
    )
    .unwrap();
    std::fs::write(s.path("net.wasm"), wasm).unwrap();
    let path = s.path("net.wasm");
    dform_host::wasm::compile_cache(wasm_cache());
    let launch = dform_host::Launcher::Cli;
    let start = |grants: &Grants| {
        dform::plugin::Launch::plugin(&launch, &path, grants)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default()
    };
    let refused = start(&Grants::none("net"));
    assert!(
        refused.contains("provider net imports wasi:sockets and is not granted it"),
        "{refused}"
    );
    assert!(refused.contains("allow = [\"wasi:sockets\"]"), "{refused}");
    let mut granted = Grants::none("net");
    granted.allow.insert("wasi:sockets".into());
    let admitted = start(&granted);
    assert!(!admitted.contains("not granted"), "{admitted}");
    assert!(admitted.contains("instantiate"), "{admitted}");
}

/// The SDK's probe (`examples/fetch_at_once`), linked in this process
/// under `grants`, its compiled code in the tests' cache.
fn probe() -> dform::plugin::link::Link {
    use dform::plugin::host::Grants;
    let probe = built(
        "examples/fetch_at_once.wasm",
        "cargo build -p dform-sdk --example fetch_at_once --release --target wasm32-wasip3 \
         --features component",
    );
    // It imports no `wasi:filesystem`, so it needs no grant.
    dform_host::wasm::compile_cache(wasm_cache());
    dform_host::wasm::link(std::path::Path::new(&probe), Grants::none("fetch-at-once")).unwrap()
}

/// Two Reads of the probe's `typ` at once, each fetching one page of a
/// server that answers only while two requests are open together: each
/// found when it was answered 200, so both when neither waited for the
/// other.
fn two_reads_at_once(typ: &str) -> [bool; 2] {
    use dform::plugin::backend::Session;
    use dform::plugin::pb;
    let pairs = common::answers_in_pairs(std::time::Duration::from_secs(3));
    let mut link = probe();
    let tickets = ["a", "b"].map(|page| {
        link.submit(
            Session::NONE,
            pb::ReadRequest {
                r#type: typ.into(),
                remote: format!("http://{pairs}/{page}"),
                name: page.into(),
                ..Default::default()
            },
        )
    });
    tickets.map(|t| {
        let answer = link.wait(t).unwrap();
        let read: pb::ReadResponse = link.expect("Read", answer).unwrap();
        read.found
    })
}

/// A component's call that awaits the host lets the instance answer its
/// other calls meanwhile: two Reads, each awaiting one request, have
/// both requests open together.
#[test]
fn a_component_answers_its_other_calls_while_one_awaits_the_host() {
    assert_eq!(two_reads_at_once("web.one"), [true, true]);
}

/// Two calls of a component contend on the SDK's lock
/// (`dform_sdk::sync::Mutex`, as the OVH client's list asked once): the
/// second awaits it while the first holds it across a request, and is
/// woken when the first lets it go, a task of the instance waking
/// another (wit-bindgen's `inter-task-wakeup`); both are answered.
#[test]
fn a_components_call_awaits_a_lock_another_call_holds() {
    use dform::http::fake::Server;
    use dform::plugin::backend::Session;
    use dform::plugin::pb;
    let server = Server::http1();
    server.answer(|_| (200, Vec::new(), Vec::new()));
    server.slow(std::time::Duration::from_millis(200));
    let mut link = probe();
    let tickets = ["a", "b"].map(|page| {
        link.submit(
            Session::NONE,
            pb::ReadRequest {
                r#type: "web.locked".into(),
                remote: server.url(&format!("/{page}")),
                name: page.into(),
                ..Default::default()
            },
        )
    });
    let found = tickets.map(|t| {
        let answer = link.wait(t).unwrap();
        let read: pb::ReadResponse = link.expect("Read", answer).unwrap();
        read.found
    });
    assert_eq!(found, [true, true]);
}

/// One Read of a component sends two requests at once
/// (`dform_sdk::Http::request_all`): under the async ABI each `send` is a
/// subtask, the component going on while the host waits, with no thread
/// in it. The server answers only while two requests are open together,
/// so the Read finds it only if neither waited for the other: one at a
/// time, the first waits out its 3 s alone and is answered 503.
#[test]
fn a_components_read_keeps_two_requests_in_flight() {
    use dform::plugin::backend::Session;
    use dform::plugin::pb;
    let pairs = common::answers_in_pairs(std::time::Duration::from_secs(3));
    let mut link = probe();
    let from = std::time::Instant::now();
    let read: pb::ReadResponse = link
        .call(
            Session::NONE,
            pb::ReadRequest {
                r#type: "web.page".into(),
                remote: format!("http://{pairs}"),
                name: "both".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        read.found,
        "both requests open at once ({:?})",
        from.elapsed()
    );
}

/// A synchronous handler waiting on the host on its own task
/// (`dform_sdk::block_on`) holds the instance: its other calls wait for
/// it (wasmtime 49, wit-bindgen 0.61). The first Read is answered alone
/// after the server's 3 s; the second is sent only then. What
/// docs/providers.md says of `--parallel` for a component.
#[test]
fn a_component_blocked_on_the_host_holds_its_instance() {
    assert_eq!(two_reads_at_once("web.blocked"), [false, true]);
}

/// The OVH provider built as a component (`cargo xtask components`): a
/// Read of an instance against the fake OVH API (native, in this
/// process) through the host's HTTP, configured with the program's token
/// (the host holds it; a component reads no environment and no file). The
/// instance, its ports and the project's networks are asked together, so
/// the server has several requests in flight from the one call.
#[test]
fn the_ovh_component_reads_an_instance_through_the_host() {
    use dform::plugin::backend::Session;
    use dform::plugin::host::Grants;
    use dform::plugin::{pb, wire};
    use dform_provider_ovh::fake::{self, Server};
    let ovh = built(
        "dform_provider_ovh.wasm",
        "cargo build -p dform-provider-ovh --release --target wasm32-wasip3 --features component",
    );
    let server = Server::start();
    let vm = server.add_instance("vm", "ca-east-tor");
    dform_host::wasm::compile_cache(wasm_cache());
    // dform-core's language front-end, which parses the provider's schema,
    // links `wasi:filesystem` (it reads a source's file for its columns):
    // granted, until the crate split keeps it out of a provider.
    let mut grants = Grants::none("ovh");
    grants.allow.insert("wasi:filesystem".into());
    let mut link = dform_host::wasm::link(std::path::Path::new(&ovh), grants).unwrap();
    let settings = serde_json::json!({"settings": {
        "endpoint": server.endpoint,
        "project": fake::PROJECT,
        "token": server.access_token(),
    }});
    let configured = link.configure(Session::NONE, wire::doc(&settings)).unwrap();
    assert_eq!(configured.account.as_deref(), Some(fake::PROJECT));
    server.slow(std::time::Duration::from_millis(50));
    let read: pb::ReadResponse = link
        .call(
            Session(configured.session),
            pb::ReadRequest {
                r#type: "ovh.instance".into(),
                remote: vm,
                name: "vm".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(read.found);
    let attrs = wire::from_doc(read.attrs.as_ref().unwrap()).unwrap();
    assert_eq!(attrs["name"], "vm");
    assert_eq!(
        server.most_at_once(),
        3,
        "the instance, its ports and the networks in flight together"
    );
}

/// The OVH provider's trace as a component is its native build's: a plan
/// of the same program against the fake OVH API has, under each of its
/// `provider.call` spans, the same `io.call` requests (each named with
/// its bucket) and `ratelimit.wait` waits (each with its session): the
/// host finds the call a component's request is made for by the task of
/// the export it is made in, as a native provider names it.
#[test]
fn the_ovh_component_traces_as_its_native_build_does() {
    use dform_provider_ovh::fake::{self, Server};
    let wasm = built(
        "dform_provider_ovh.wasm",
        "cargo build -p dform-provider-ovh --release --target wasm32-wasip3 --features component",
    );
    let server = Server::start();
    let program = format!(
        r#"
use ovh {{ endpoint = "{}", project = "{}", token = "{}" }}

resource ovh.ssh_key admin {{ name = "lab-admin", public_key = "ssh-ed25519 AAAAC3Nz lab" }}

resource ovh.instance server {{
  name = "lab-server"
  region = "ca-east-tor"
  flavor = "b2-7"
  image = "Ubuntu 24.04"
  ssh_key = admin
}}
"#,
        server.endpoint,
        fake::DESCRIPTION,
        server.access_token()
    );
    let traced = |name: &str, provider: &str| {
        let s = Scratch::project(name);
        s.write(
            "dform.toml",
            &format!(
                "[project]\nedition = \"2026\"\n\n[providers]\novh = {{ path = \"{provider}\", \
                 allow = [\"wasi:filesystem\"] }}\n"
            ),
        );
        s.write("main.df", &program);
        let out = common::dform()
            .args(["plan", "main.df", "--trace", "run.jsonl"])
            .current_dir(&s.dir)
            .env("HOME", &s.dir)
            .env("XDG_CONFIG_HOME", s.path("config"))
            .env("XDG_CACHE_HOME", wasm_cache())
            .env("DFORM_OVH_RESOLVER", "127.0.0.1:9")
            .env_remove("OVH_ENDPOINT")
            .env_remove("OVH_APPLICATION_KEY")
            .env_remove("DFORM_TRACE")
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .output()
            .unwrap();
        common::Run::from(out).success();
        under_calls(&common::spans(&s.path("run.jsonl")))
    };
    let native = traced("ovh-trace-native", &common::exe("dform-provider-ovh"));
    let component = traced("ovh-trace-wasm", &wasm);
    assert!(
        native
            .keys()
            .any(|(call, child)| call == "ovh Configure" && child.starts_with("ovh GET")),
        "{native:#?}"
    );
    assert_eq!(component, native);
}

/// The Kubernetes provider built as a wasm provider (`cargo xtask
/// components`) against the fake API server (tests/k8s_api.rs, native in
/// this process), through the host's HTTP with the kubeconfig the host
/// holds for it (a wasm provider reads no environment and no file): its
/// Configure asks the index, then the documents, several in flight
/// together; a Read GETs the object; a Plan is a dry run, a server-side
/// apply that persists nothing.
#[test]
fn the_k8s_component_configures_reads_and_plans_through_the_host() {
    use crate::k8s_api::{Api, kubeconfig};
    use dform::plugin::backend::Session;
    use dform::plugin::host::Grants;
    use dform::plugin::{pb, wire};
    let k8s = k8s_component();
    let (api, url) = Api::start();
    let s = Scratch::project("k8s-wasm");
    let text = std::fs::read_to_string(kubeconfig(&s, &url)).unwrap();
    dform_host::wasm::compile_cache(wasm_cache());
    let mut grants = Grants::none("k8s");
    grants.allow.insert("wasi:filesystem".into());
    let mut link = dform_host::wasm::link(std::path::Path::new(&k8s), grants).unwrap();
    // The program's cluster (`use k8s { kubeconfig }`), its kubeconfig
    // held by the host and the provider configured with its name; its
    // kinds beyond the static schema derived and kept for the deployment.
    let config = serde_json::json!({
        "settings": {"kubeconfig": text},
        "cache": s.path("cache"),
        "stack": "shop",
    });
    let configured = link.configure(Session::NONE, wire::doc(&config)).unwrap();
    assert_eq!(configured.account.as_deref(), Some(url.as_str()));
    let asked = api.count("GET /openapi/v3/", &[]);
    assert!(asked > 2, "{asked} documents");
    assert!(
        api.most_documents_at_once() >= 2,
        "the server answered {} of {asked} documents at once",
        api.most_documents_at_once()
    );
    let session = Session(configured.session);
    let read: pb::ReadResponse = link
        .call(
            session,
            pb::ReadRequest {
                r#type: "k8s.core.v1.namespace".into(),
                remote: "default".into(),
                name: "default".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(read.found);
    let computed = wire::from_doc(read.computed.as_ref().unwrap()).unwrap();
    assert_eq!(computed["metadata"]["uid"], "uid-default", "{computed}");
    let desired = serde_json::json!({"metadata": {"name": "app", "namespace": "default"},
                                     "data": {"mode": "a"}});
    let plan: pb::PlanResponse = link
        .call(
            session,
            pb::PlanRequest {
                r#type: "k8s.core.v1.config_map".into(),
                name: "app".into(),
                desired: Some(wire::doc(&desired)),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!plan.changes.is_empty());
    assert_eq!(
        api.count(
            "PATCH /api/v1/namespaces/default/configmaps/app?",
            &["dryRun=All", "fieldManager=dform"]
        ),
        1
    );
    assert!(
        api.get("/api/v1/namespaces/default/configmaps/app")
            .is_none(),
        "a dry run persists nothing"
    );
}

/// The Kubernetes provider's trace as a wasm provider is its native
/// build's: tests/fixtures/k8s's demo planned against the fake API server
/// (from no cache, so each Configure fetches every document) has, under
/// each `provider.call` span, the same `io.call` requests (the index and
/// the documents under the Configure, the dry runs under the Plans, each
/// with its bucket) and its `ratelimit.wait` waits (the documents queued
/// behind the window's first).
#[test]
fn the_k8s_component_traces_as_its_native_build_does() {
    use crate::k8s_api::{Api, kubeconfig};
    // One server: each run's cache is its own, so each fetches every
    // document, and a plan's dry runs persist nothing.
    let (_api, url) = Api::start();
    let traced = |name: &str, provider: &str| {
        let s = k8s_demo(name, provider);
        plan_demo(&s, &kubeconfig(&s, &url));
        under_calls(&common::spans(&s.path("run.jsonl")))
    };
    let native = traced("k8s-trace-native", &common::exe("dform-provider-k8s"));
    let component = traced("k8s-trace-wasm", &k8s_component());
    for call in ["k8s Configure", "k8s Plan"] {
        assert!(
            native.keys().any(|(c, _)| c == call),
            "requests under {call}: {native:#?}"
        );
    }
    assert!(
        native
            .keys()
            .any(|(_, child)| child.ends_with("waits on its window")),
        "the documents wait on the window: {native:#?}"
    );
    assert_eq!(component, native);
}

/// The Kubernetes provider as a wasm provider keeps what it fetched and
/// derived in the run's cache directory, as its native build does: the
/// next plan asks the cluster's index, and no document again.
#[test]
fn the_k8s_component_keeps_its_cache_across_runs() {
    use crate::k8s_api::{Api, kubeconfig};
    let (api, url) = Api::start();
    let s = k8s_demo("k8s-wasm-cache", &k8s_component());
    let kc = kubeconfig(&s, &url);
    plan_demo(&s, &kc);
    let fetched = api.count("GET /openapi/v3/", &[]);
    assert!(fetched > 2, "{fetched} documents");
    for file in ["k8s-openapi.json", "k8s-schema.json"] {
        let cached = s.path(&format!("dform.state/cache/{file}"));
        assert!(cached.exists(), "{}", cached.display());
    }
    plan_demo(&s, &kc);
    assert_eq!(
        api.count("GET /openapi/v3/", &[]),
        fetched,
        "no document again"
    );
}

/// A project of tests/fixtures/k8s's demo whose dform.toml names the
/// Kubernetes provider at `provider` (the executable or the wasm
/// provider), granted the environment's kubeconfig and, for a wasm
/// provider, the filesystem (its schema's parse, its cache).
fn k8s_demo(name: &str, provider: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers.k8s]\npath = \"{provider}\"\n\
             allow = [\"wasi:filesystem\"]\ncredentials = [\"kubeconfig:environment\"]\n"
        ),
    );
    let demo = common::repo().join("tests/fixtures/k8s/stacks/k8s_demo.df");
    s.write("k8s_demo.df", &std::fs::read_to_string(demo).unwrap());
    s
}

/// `dform plan` of [`k8s_demo`]'s project against the cluster the
/// kubeconfig `kc` names, traced to run.jsonl.
fn plan_demo(s: &Scratch, kc: &str) -> common::Run {
    let out = common::dform()
        .args(common::on(
            "k8s_demo.df",
            &[],
            &["plan", "--trace", "run.jsonl"],
        ))
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"))
        .env("XDG_CACHE_HOME", wasm_cache())
        .env("KUBECONFIG", kc)
        .env_remove("DFORM_K8S_OFFLINE")
        .env_remove("KUBERNETES_SERVICE_HOST")
        .env_remove("KUBERNETES_SERVICE_PORT")
        .env_remove("DFORM_TRACE")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .output()
        .unwrap();
    common::Run::from(out).success()
}

/// The Kubernetes provider as a wasm provider, built beside the
/// workspace's target.
fn k8s_component() -> String {
    built(
        "dform_provider_k8s.wasm",
        "cargo build -p dform-provider-k8s --release --target wasm32-wasip3 --features component",
    )
}

/// Each `io.call` span of a trace by its parent's name (the call it is
/// made for) and its own, and each `ratelimit.wait` span by the call of
/// the request that waited, counted: which of a call's requests sent
/// together wait is the order they reach the host in. Each request's
/// bucket and each wait's session filled.
fn under_calls(spans: &[serde_json::Value]) -> std::collections::BTreeMap<(String, String), usize> {
    let by_id: std::collections::BTreeMap<&str, &serde_json::Value> = spans
        .iter()
        .map(|sp| (sp["spanId"].as_str().unwrap(), sp))
        .collect();
    let parent = |sp: &serde_json::Value| {
        sp["parentSpanId"]
            .as_str()
            .and_then(|p| by_id.get(p).copied())
    };
    let is_request = |sp: &serde_json::Value| common::attr(sp, "kind").as_deref() == Some("http");
    let mut out = std::collections::BTreeMap::new();
    for sp in spans {
        let name = sp["name"].as_str().unwrap_or_default();
        let request = is_request(sp);
        let wait = name.ends_with("waits on its window");
        if !request && !wait {
            continue;
        }
        if request {
            assert!(common::attr(sp, "bucket").is_some(), "{sp}");
        }
        if wait {
            assert_ne!(common::attr(sp, "session").as_deref(), Some("0"), "{sp}");
        }
        // A wait under the request that waited is by that request's call.
        let under = match parent(sp) {
            Some(p) if wait && is_request(p) => parent(p),
            p => p,
        };
        let call = under.and_then(|p| p["name"].as_str()).unwrap_or("(none)");
        *out.entry((call.to_string(), name.to_string())).or_insert(0) += 1;
    }
    out
}

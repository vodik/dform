//! The wasm host (R-13b, the `wasm` feature): the fake provider built as a
//! component conforms as its native build does, a run plans, applies and
//! resumes through it, a component's grantable imports are refused
//! without their grant and admitted with it, and one call of a component
//! keeps several host calls in flight (the async ABI).
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

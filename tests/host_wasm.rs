//! The wasm host (R-13b, the `wasm` feature): the fake provider built as a
//! component conforms as its native build does, a run plans, applies and
//! resumes through it, and a component's grantable imports are refused
//! without their grant and admitted with it.
//!
//! The component is built apart (a test never runs cargo):
//!
//!   cargo build -p dform-provider-fake --release --target wasm32-wasip2 --features component
//!   cargo test --features wasm --test host_wasm
//!
//! Every other test that drives the mock runs through the wasm host when
//! `DFORM_PROVIDER_FAKE` names the component (the CLI's mock is then the
//! component): `DFORM_PROVIDER_FAKE=$PWD/target/wasm32-wasip2/release/dform_provider_fake.wasm
//! cargo test --features wasm --test chaos`.

#![cfg(feature = "wasm")]

mod common;
use common::{Scratch, identities};

const PROG: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
resource compute.vm app { subnet_id = ref(net.subnet, "a", "id") }
use fake
"#;

/// The fake provider as a component, built beside the workspace's target.
fn component() -> String {
    let target = std::path::Path::new(env!("CARGO_BIN_EXE_dform"))
        .parent()
        .and_then(|d| d.parent())
        .unwrap()
        .to_path_buf();
    for profile in ["release", "debug"] {
        let p = target.join(format!("wasm32-wasip2/{profile}/dform_provider_fake.wasm"));
        if p.exists() {
            return p.display().to_string();
        }
    }
    panic!(
        "the fake provider's component is not built: cargo build -p dform-provider-fake --release \
         --target wasm32-wasip2 --features component"
    );
}

/// `dform ARGS` with the mock as the component, its compiled code cached
/// under the build's directory for tests.
fn on_component<S: AsRef<std::ffi::OsStr>>(s: &Scratch, args: &[S]) -> common::Run {
    let out = common::dform()
        .args(common::yes(args))
        .env("DFORM_PROVIDER_FAKE", component())
        .env("XDG_CACHE_HOME", wasm_cache())
        .current_dir(&s.dir)
        .output()
        .unwrap();
    common::Run::from(out)
}

/// `dform dev --world w.json ARGS` on `p.df`, the mock the component.
fn mock(s: &Scratch, args: &[&str]) -> common::Run {
    on_component(s, &common::on("p.df", &["--world", "w.json"], args))
}

/// One cache for the test binary's runs: the component is compiled once.
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
/// under chaos `delay` is shown beside the change, as the native build's
/// is (tests/apply_events.rs).
#[test]
fn an_apply_s_events_come_through_the_component_s_stream() {
    let s = Scratch::new("host-wasm-events");
    s.write("p.df", PROG);
    let r = mock(
        &s,
        &["apply", "--chaos", "delay=net.subnet[\"a\"]:200", "--yes"],
    )
    .success();
    let lines: Vec<&str> = r
        .stderr
        .lines()
        .filter(|l| l.starts_with("  + net.subnet a  "))
        .collect();
    assert!(
        !lines.is_empty() && lines.iter().all(|l| l.ends_with("s  made")),
        "{}",
        r.stderr
    );
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
}

/// The component exits as it is called to Apply the third action: the
/// action fails naming it, the two before it are kept, and the next apply
/// finishes, as with the native process (tests/protocol_plugin.rs).
#[test]
fn a_component_crash_mid_apply_fails_the_action_and_resume_finishes() {
    let s = Scratch::new("host-wasm-crash");
    s.write("p.df", PROG);
    let r = mock(&s, &["apply", "--chaos", "crash=compute.vm[\"app\"]"]).failure();
    assert!(
        r.stderr
            .contains("apply compute.vm app: the provider")
            // wasip2's exit carries only failure, not its code.
            && r.stderr.contains("exited during the call (exit status: 1)"),
        "{}",
        r.stderr
    );
    assert_eq!(identities(&s), ["net.subnet::a", "net.vpc::main"]);
    let r = mock(&s, &["apply"]).success();
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
}

/// A component that imports `wasi:sockets` is refused without the grant,
/// naming the provider and the package, and admitted with it.
#[test]
fn wasi_sockets_needs_its_grant() {
    use dform::plugin::host::{Grants, register};
    let s = Scratch::new("host-wasm-sockets");
    // A component importing the network interface and exporting nothing:
    // admitted, it fails later, for want of the provider export.
    let wasm = wat::parse_str(
        r#"(component
             (import "wasi:sockets/network@0.2.12" (instance
               (export "network" (type (sub resource))))))"#,
    )
    .unwrap();
    std::fs::write(s.path("net.wasm"), wasm).unwrap();
    let path = s.path("net.wasm");
    let spec = path.display().to_string();
    let launch = dform_host::Launcher::Cli;
    let start = || {
        dform::plugin::Launch::plugin(&launch, &path)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default()
    };
    register([(spec.clone(), Grants::none("net"))]);
    let refused = start();
    assert!(
        refused.contains("provider net imports wasi:sockets and is not granted it"),
        "{refused}"
    );
    assert!(refused.contains("allow = [\"wasi:sockets\"]"), "{refused}");
    let mut granted = Grants::none("net");
    granted.allow.insert("wasi:sockets".into());
    register([(spec, granted)]);
    let admitted = start();
    assert!(!admitted.contains("not granted"), "{admitted}");
    assert!(admitted.contains("instantiate"), "{admitted}");
}

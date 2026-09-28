//! The provider plugin protocol at the CLI: the mock is a separate process,
//! a provider that dies during an Apply is a failed action naming the
//! resource and the next apply resumes, a `provider` source directory
//! holding an executable is that plugin, and `dform provider check` is the
//! conformance suite.

mod common;
use common::Scratch;

const PROG: &str = r#"edition 2026.

resource net.vpc main { cidr = "10.0.0.0/16" }.
resource net.subnet a { vpc_id = ref(net.vpc, main, id), cidr = "10.0.1.0/24" }.
resource compute.vm app { subnet_id = ref(net.subnet, a, id) }.
"#;

const FAKE: &str = env!("CARGO_BIN_EXE_dform-provider-fake");

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
    std::os::unix::fs::symlink(FAKE, s.path("prov/dform-provider-fake")).unwrap();
    s.write(
        "p.df",
        "edition 2026.\n\nprovider fake { source = \"prov\" }.\n\
         resource net.vpc main { cidr = \"10.0.0.0/16\" }.\n",
    );
    let r = dform(&s, &["plan"]).success();
    assert!(r.stdout.contains("+ net.vpc.main"), "{}", r.stdout);
}

#[test]
fn the_mock_conforms() {
    let s = Scratch::new("protocol-check");
    let r = s.run(&["provider", "check", FAKE]).success();
    assert!(!r.stdout.contains("FAIL"), "{}", r.stdout);
    assert!(
        r.stdout
            .contains("ok    Apply refuses an action whose assertion fails"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.ends_with("conforms\n"), "{}", r.stdout);
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

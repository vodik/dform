//! Persist identity after every action: a failure or a crash at action N
//! leaves the N-1 identities before it in state.

mod common;
use common::Scratch;

const PROG: &str = r#"
resource net.vpc main { cidr = "10.0.0.0/16" }.
resource net.subnet a { vpc_id = ref(net.vpc, main, id), cidr = "10.0.1.0/24" }.
resource compute.vm app { subnet_id = ref(net.subnet, a, id) }.
"#;

fn stack(name: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("p.df", PROG);
    s
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

/// `fail=` at the third action: the first two are in state.
#[test]
fn a_failure_at_action_n_leaves_n_minus_one_identities() {
    let s = stack("persist-fail");
    dform(&s, &["apply", "--chaos", "fail=compute.vm/app"]).failure();
    assert_eq!(identities(&s), ["net.subnet::a", "net.vpc::main"]);
}

/// The process dies as it calls Apply for the third action: nothing after
/// that point runs, so only what was written per action survives.
#[test]
fn a_crash_at_action_n_leaves_n_minus_one_identities() {
    let s = stack("persist-crash");
    let r = dform(&s, &["apply", "--chaos", "crash=compute.vm/app"]).failure();
    assert!(
        r.stderr
            .contains("chaos: crash during apply compute.vm/app"),
        "{}",
        r.stderr
    );
    assert_eq!(identities(&s), ["net.subnet::a", "net.vpc::main"]);
    // The next plan sees two resources through identity and creates one.
    let r = dform(&s, &["plan"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 deformation (1 create)",
        "{}",
        r.stdout
    );
}

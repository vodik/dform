//! Resume a half-applied plan: `apply` on a stack an apply left partial
//! finishes the remaining actions, or reports what changed underneath them
//! and stops.

mod common;
use common::Scratch;

const PROG: &str = r#"edition 2026.

resource net.vpc main { cidr = "10.0.0.0/16" }.
resource net.subnet a { vpc_id = ref(net.vpc, main, id), cidr = "10.0.1.0/24" }.
resource compute.vm app { subnet_id = ref(net.subnet, a, id) }.
"#;

fn dform(s: &Scratch, args: &[&str]) -> common::Run {
    s.run(&[&["--file", "p.df", "--world", "w.json"][..], args].concat())
}

fn state(s: &Scratch) -> serde_json::Value {
    serde_json::from_str(&s.read("w.state.json")).unwrap()
}

#[test]
fn apply_after_a_crash_finishes_the_remaining_actions() {
    let s = Scratch::new("resume-crash");
    s.write("p.df", PROG);
    dform(&s, &["apply", "--chaos", "crash=compute.vm/app"]).failure();
    let st = state(&s);
    assert_eq!(
        st["in_flight"]["remaining"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["compute.vm::app"],
        "{st}"
    );
    let r = dform(&s, &["apply"]).success();
    assert_eq!(
        r.stdout,
        "resuming the apply interrupted at tick 1; remaining: compute.vm.app\n\
         plan: 1 deformation (1 create)\ndefinite:\n\
         + compute.vm.app\n  subnet_id = \"net.subnet:a\"\n\
         apply order: tick 1 [compute.vm.app]\n\
         apply: complete\n"
    );
    let st = state(&s);
    assert!(st.get("in_flight").is_none(), "{st}");
    assert_eq!(st["resources"].as_object().unwrap().len(), 3);
    let r = dform(&s, &["plan"]).success();
    assert!(
        r.stdout.ends_with("stack p is undeformed\n"),
        "{}",
        r.stdout
    );
}

/// The first apply updates the vpc and fails on the subnet; after that tick
/// the world changes the subnet under the remaining update. The next apply
/// prints the change and stops before any Apply call; the one after that
/// plans against the world as it now is.
#[test]
fn apply_stops_when_the_world_changed_under_a_remaining_action() {
    let s = Scratch::new("resume-changed");
    s.write("p.df", PROG);
    dform(&s, &["apply"]).success();
    s.write("p.df", &PROG.replace("\" }", "\", tier = \"web\" }"));
    dform(
        &s,
        &[
            "apply",
            "--chaos",
            "fail=net.subnet/a",
            "--chaos",
            r#"mutate=net.subnet/a:tags.owner="someone""#,
        ],
    )
    .failure();
    let before = s.read("w.json");
    let r = dform(&s, &["apply"]).failure();
    assert!(
        r.stderr.contains(
            "the world changed under a remaining action:\n~ net.subnet.a\n  tags.owner: <none> -> \"someone\"\n"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("apply stopped: the world changed under 1 remaining actions"),
        "{}",
        r.stderr
    );
    assert_eq!(s.read("w.json"), before, "no Apply call was made");
    assert!(state(&s).get("in_flight").is_none());
    let r = dform(&s, &["apply"]).success();
    assert!(
        r.stdout.contains(
            "~ net.subnet.a\n  tags.owner: \"someone\" -> <none>\n  tier: <none> -> \"web\"\n"
        ),
        "{}",
        r.stdout
    );
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
}

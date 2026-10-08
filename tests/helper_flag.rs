//! The relations the compiler writes beside a program's (a `not { .. }`
//! body, an aggregate's fold) behave as the program's own: one whose body
//! reads what waits waits too. On the mock.

mod common;
use common::{Run, Scratch};

/// A box whose computed `status` is not known until it is created.
const BOX: &str = r#"
type_provider(x.box, "boxcloud")
type_attr(x.box, "id", "string", ["computed", "id"])
type_attr(x.box, "status", "object", ["computed"])
"#;

/// The policy's denies over `x.box`: `not has` of a field of the computed
/// object, a `not { }` over it, a count, and a `not { }` of a relation
/// that waits on it.
const BOX_POLICY: &str = r#"
deny "not ready" { box: r } where r in x.box, not has r.status.ready
deny "odd" { box: r } where r in x.box, not { r.status.phase == "x" }
ready(r) where r in x.box, r.status.ready == true
deny "few" { n: n } where n = count(r), ready(r), n < 1
deny "none ready" { box: r } where r in x.box, not { ready(r) }
"#;

/// `p.df` holding `src`, beside the box's schema and `policy.df`.
fn boxes(name: &str, src: &str) -> Scratch {
    let s = Scratch::new(name);
    s.write("s.df", BOX);
    s.write("policy.df", BOX_POLICY);
    s.write("p.df", src);
    s
}

fn dev(s: &Scratch, args: &[&str]) -> Run {
    let mut all = vec!["dev", "--world", "w.json", "--provider", "s.df"];
    all.extend_from_slice(args);
    all.push("p.df");
    s.run(&all)
}

/// A `not { }` whose body reads a relation that waits on a value not
/// known yet waits with it, at the top and in a module alike: `ready(b)`
/// may hold once `b.status` is known, so "none ready" is undetermined, not
/// a violation: the plan is no refusal and `why` says what it waits on.
#[test]
fn a_negation_of_what_waits_waits() {
    let top = format!("resource x.box b {{ size = 1 }}\n{BOX_POLICY}");
    for (name, src) in [
        ("helper-waits-top", top.as_str()),
        (
            "helper-waits-module",
            "use policy\nresource x.box b { size = 1 }\n",
        ),
    ] {
        let s = boxes(name, src);
        dev(&s, &["plan"]).success();
        let r = dev(&s, &["why", "deny \"none ready\""]).success();
        assert!(
            r.stdout
                .starts_with("deny \"none ready\": undetermined, waits on b.status\n"),
            "{name}\n{}",
            r.stdout
        );
    }
}

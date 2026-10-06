//! A write-only attribute (R-106, `type_attr(.., ["write_only"])`): the API
//! takes it and never answers it, as OVH's an instance's user data. State
//! keeps the digest of what was last applied beside the resource, never
//! the value; Plan compares the program's value with it: the same is no
//! change, another replaces or updates as the schema says.

mod common;
use common::{Scratch, repo};

fn project(user_data: &str, token: &str) -> Scratch {
    let s = Scratch::project("write-only");
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_attr(compute.vm, \"user_data\", \"string\", [\"write_only\", \"force_new\"])\n\
               type_attr(compute.vm, \"token\", \"string\", [\"write_only\", \"sensitive\"])\n"),
    );
    program(&s, user_data, token);
    s
}

fn program(s: &Scratch, user_data: &str, token: &str) {
    s.write(
        "stacks/p.df",
        &format!(
            "provider fake\nresource compute.vm a {{\n  name = \"a\"\n  user_data = \"{user_data}\"\n  token = \"{token}\"\n}}\n"
        ),
    );
}

#[test]
fn a_write_only_attribute_is_compared_with_the_digest_state_keeps() {
    let s = project("#cloud-config one", "t1");
    s.run(&["apply", "p"]).success();
    // State keeps a digest of each, beside the resource; never the value.
    let state = s.read("dform.state/p/state.json");
    let st: serde_json::Value = serde_json::from_str(&state).unwrap();
    let written = &st["resources"]["compute.vm::a"]["written"];
    for p in ["user_data", "token"] {
        assert!(
            written[p].as_str().is_some_and(|d| d.contains("sha256:")),
            "{state}"
        );
    }
    assert!(
        !state.contains("cloud-config one") && !state.contains("t1"),
        "{state}"
    );

    // The API never answers them; the same values are no change.
    let r = s.run(&["plan", "p"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);

    // Another user data replaces (force_new); another token updates.
    program(&s, "#cloud-config two", "t1");
    let r = s.run(&["plan", "p"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 replace) over 1 tick",
        "{}",
        r.stdout
    );
    program(&s, "#cloud-config one", "t2");
    let r = s.run(&["plan", "p"]).success();
    assert_eq!(
        r.summary(),
        "plan: 1 change (1 update) over 1 tick",
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("token"), "{}", r.stdout);

    // Applied, the new digest is kept, and the plan is clean again.
    s.run(&["apply", "p"]).success();
    let r = s.run(&["plan", "p"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}

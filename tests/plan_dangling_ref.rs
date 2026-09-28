//! A `ref(T, A, P)` to an address no rule wants is a deny, not a silently
//! missing field (ticket "A ref to an address nothing wants drops the field
//! silently").

mod common;
use common::{Scratch, repo};

const MSG: &str = "ref to an address no rule wants";

#[test]
fn a_ref_to_an_unwanted_address_is_denied() {
    let s = Scratch::new("dangling-ref");
    s.write(
        "p.df",
        r#"
resource net.vpc main { cidr = "10.0.0.0/16" }.
resource net.subnet a { cidr = "10.0.1.0/24", vpc_id = ref(net.vpc, other, id) }.
"#,
    );
    let r = s.run(&["--file", "p.df", "plan"]).failure();
    assert!(r.stderr.contains(MSG), "{}", r.stderr);
    for want in [
        r#""type":"net.vpc""#,
        r#""addr":"other""#,
        r#""path":"id""#,
        r#""from":"net.subnet.a""#,
    ] {
        assert!(r.stderr.contains(want), "{want}: {}", r.stderr);
    }
    assert!(r.stderr.contains("blocked by constraints"), "{}", r.stderr);
}

#[test]
fn a_ref_to_a_wanted_address_is_not_denied() {
    let s = Scratch::new("dangling-ref-ok");
    s.write(
        "p.df",
        r#"
resource net.vpc main { cidr = "10.0.0.0/16" }.
resource net.subnet a { cidr = "10.0.1.0/24", vpc_id = ref(net.vpc, main, id) }.
"#,
    );
    let r = s.run(&["--file", "p.df", "plan"]).success();
    assert!(!r.stderr.contains(MSG), "{}", r.stderr);
}

/// The case the ticket names: dform.df in stg peers two VPCs no rule wants.
#[test]
fn dform_df_in_stg_is_blocked() {
    let s = Scratch::new("dangling-ref-stg");
    let file = repo().join("dform.df");
    let r = s
        .run(&[
            "--file",
            file.to_str().unwrap(),
            "--set",
            "env=stg",
            "--world",
            "w.json",
            "plan",
        ])
        .failure();
    assert!(r.stderr.contains(MSG), "{}", r.stderr);
    assert!(
        r.stderr
            .contains(r#""from":"net.vpc_peering.peer-main-peer""#),
        "{}",
        r.stderr
    );
}

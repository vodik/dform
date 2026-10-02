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
        r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { cidr = "10.0.1.0/24", vpc_id = ref(net.vpc, "other", "id") }
provider fake
"#,
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(r.stderr.contains(MSG), "{}", r.stderr);
    for want in [
        r#""type":"net.vpc""#,
        r#""addr":"other""#,
        r#""path":"id""#,
        r#""from":"net.subnet.a""#,
        r#""at":"p.df:4:47""#,
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
        r#"edition 2026

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { cidr = "10.0.1.0/24", vpc_id = ref(net.vpc, "main", "id") }
provider fake
"#,
    );
    let r = s.run(&["plan", "p.df"]).success();
    assert!(!r.stderr.contains(MSG), "{}", r.stderr);
}

/// The case the ticket names: dform.df in stg planned a peering between two
/// VPCs no rule wanted. The demo now guards the peering on both VPCs existing,
/// so the guarded program is not blocked; the unguarded shape is kept here.
#[test]
fn a_peering_between_unwanted_vpcs_is_blocked() {
    let s = Scratch::new("dangling-ref-peering");
    s.write(
        "p.df",
        r#"edition 2026

input_env("stg")
resource net.vpc main {
  cidr = "10.0.0.0/16"
} where input_env("prod")
resource net.vpc peer {
  cidr = "10.1.0.0/16"
} where input_env("prod")
resource net.vpc_peering peer_main_peer {
  requester_vpc_id = ref(net.vpc, "main", "id")
  accepter_vpc_id = ref(net.vpc, "peer", "id")
}
provider fake
"#,
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(r.stderr.contains(MSG), "{}", r.stderr);
    assert!(
        r.stderr
            .contains(r#""from":"net.vpc_peering.peer_main_peer""#),
        "{}",
        r.stderr
    );
}

/// The demo itself plans in every environment once the refs are guarded.
#[test]
fn dform_df_plans_in_every_env() {
    for env in ["dev", "staging", "prod"] {
        let s = Scratch::new(&format!("dangling-ref-{env}"));
        let file = repo().join("examples/demo/stacks/dform.df");
        let r = s
            .run(&[
                "dev",
                "--world",
                "w.json",
                "plan",
                file.to_str().unwrap(),
                &format!("env={env}"),
            ])
            .success();
        assert!(!r.stderr.contains(MSG), "{env}: {}", r.stderr);
    }
}

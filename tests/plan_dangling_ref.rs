//! A `ref(T, A, P)` to an address no rule wants is an error at the read's
//! site (R-194), not a silently missing field (ticket "A ref to an address
//! nothing wants drops the field silently") nor a deny listed after a plan
//! that shows the resource without it.

mod common;
use common::{Scratch, repo};

const MSG: &str = "answered nothing";

#[test]
fn a_ref_to_an_unwanted_address_is_an_error_at_the_read() {
    let s = Scratch::new("dangling-ref");
    s.write(
        "p.df",
        r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { cidr = "10.0.1.0/24", vpc_id = ref(net.vpc, "other", "id") }
use fake
"#,
    );
    let r = s.run(&["plan", "p.df"]).failure();
    // Where the read is, what it reads, the attribute it leaves without a
    // value, and why (R-119's form); no plan is printed.
    assert!(
        r.stderr.starts_with(
            "error  net.vpc other.id answered nothing, so net.subnet a.vpc_id has no \
             value: nothing derives net.vpc other\n  p.df:4  "
        ),
        "{}",
        r.stderr
    );
    assert!(r.stdout.is_empty(), "{}", r.stdout);
}

#[test]
fn a_ref_to_a_wanted_address_is_not_denied() {
    let s = Scratch::new("dangling-ref-ok");
    s.write(
        "p.df",
        r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { cidr = "10.0.1.0/24", vpc_id = ref(net.vpc, "main", "id") }
use fake
"#,
    );
    let r = s.run(&["plan", "p.df"]).success();
    assert!(!r.stderr.contains(MSG), "{}", r.stderr);
}

/// The case the ticket names: dform.df in stg planned a peering between two
/// VPCs no rule wanted. The demo now guards the peering on both VPCs existing,
/// so the guarded program is not refused; the unguarded shape is kept here.
#[test]
fn a_peering_between_unwanted_vpcs_is_refused() {
    let s = Scratch::new("dangling-ref-peering");
    s.write(
        "p.df",
        r#"

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
use fake
"#,
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains(
            "error  net.vpc main.id answered nothing, so net.vpc_peering \
             peer_main_peer.requester_vpc_id has no value: nothing derives net.vpc main\n"
        ),
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

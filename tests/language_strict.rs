//! `stack x { unknowns = strict }`: a plan that needs a phase boundary (a
//! stuck derivation, a pending group, a deformation held on a null) is
//! refused, saying why; fresh nulls still flow.

mod common;
use common::{Scratch, repo};

fn gke(strict: bool) -> Scratch {
    let s = Scratch::new("lang-strict-gke");
    let src =
        std::fs::read_to_string(repo().join("examples/adversarial/gke_two_phase.df")).unwrap();
    let stack = if strict {
        "stack gke { unknowns = strict }."
    } else {
        "stack gke {}."
    };
    s.write(
        "g.df",
        &src.replacen(
            "provider gke {}.",
            &format!("provider gke {{}}.\n{stack}"),
            1,
        ),
    );
    s
}

#[test]
fn a_two_phase_plan_is_refused_saying_why() {
    let s = gke(true);
    let r = s.run(&["--file", "g.df", "plan"]).failure();
    assert!(
        r.stdout.contains(
            "refused: stack gke is strict (unknowns = strict) and this plan needs a phase boundary:\n\
             - pending group want(\"gke_nodepool\", _) x unknown, on ?gke_cluster/pngu#zones"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("- k8s.namespace.pngu waits on ?gke_cluster/pngu#ca_certificate ?gke_cluster/pngu#endpoint"),
        "{}",
        r.stdout
    );
    assert!(
        r.stderr.contains("plan refused: stack gke is strict"),
        "{}",
        r.stderr
    );
    let j = s.run(&["--file", "g.df", "plan", "--json"]).failure();
    assert!(j.stdout.contains("\"refused\": ["), "{}", j.stdout);

    // apply refuses before any Apply call.
    let r = s.run(&["--file", "g.df", "apply"]).failure();
    assert!(r.stderr.contains("apply refused at tick 1"), "{}", r.stderr);
    assert!(
        !s.path(".dform/gke/remote.json").exists()
            || !s.read(".dform/gke/remote.json").contains("gke_cluster")
    );

    // Permissive, the same program plans its two phases.
    let s = gke(false);
    s.run(&["--file", "g.df", "plan"]).success();
}

/// A single-phase plan whose documents carry fresh nulls (ids known after
/// apply) is not refused.
#[test]
fn fresh_nulls_still_flow() {
    let s = Scratch::new("lang-strict-fresh");
    s.write(
        "p.df",
        "edition 2026.\nstack p { unknowns = strict }.\nresource net.vpc main { cidr = \"10.0.0.0/16\" }.\n\
         resource net.subnet a { vpc_id = ref(net.vpc, main, .id), cidr = \"10.0.1.0/24\" }.\n",
    );
    let r = s.run(&["--file", "p.df", "plan"]).success();
    assert!(
        r.stdout.contains("vpc_id = ?net.vpc/main#id"),
        "{}",
        r.stdout
    );
    s.run(&["--file", "p.df", "apply"]).success();
}

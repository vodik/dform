//! `stack x { unknowns = strict }`: a plan that needs a phase boundary (a
//! stuck derivation, a pending group, a deformation held on a null) is
//! refused, saying why; fresh nulls still flow.

mod common;
use common::{Scratch, repo};

fn gke(strict: bool) -> Scratch {
    let s = Scratch::project("lang-strict-gke");
    let src = std::fs::read_to_string(repo().join("examples/gke/stacks/gke_two_phase.df")).unwrap();
    let stack = if strict {
        "stack gke { unknowns = \"strict\" }"
    } else {
        "stack gke {}"
    };
    s.write("g.df", &src.replacen("stack gke_two_phase {}", stack, 1));
    s
}

#[test]
fn a_two_phase_plan_is_refused_saying_why() {
    let s = gke(true);
    let r = s.run(&["plan", "g.df"]).failure();
    // The plan prints, then the generated deny refuses it, one violation
    // per stuck instance with its rule, head pattern and nulls.
    assert!(
        r.stdout
            .contains("pending groups:\n? gke_nodepool[?] x unknown"),
        "{}",
        r.stdout
    );
    assert!(
        r.stderr.contains(
            "- strict: unresolved value at plan time ctx={\"head\":\"want(\\\"gke_nodepool\\\", _)\",\
             \"nulls\":[\"gke_cluster/pngu#zones\"],\"rule\":"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("Error: blocked by constraints"),
        "{}",
        r.stderr
    );
    // It is a deny: why explains it.
    let w = s
        .run(&[
            "why",
            "deny(\"strict: unresolved value at plan time\", C)",
            "g.df",
        ])
        .success();
    assert!(w.stdout.contains("stuck("), "{}", w.stdout);
    let j = s.run(&["plan", "--json", "g.df"]).failure();
    assert!(
        j.stderr.contains("strict: unresolved value at plan time"),
        "{}",
        j.stderr
    );

    // apply refuses before any Apply call.
    let r = s.run(&["apply", "g.df"]).failure();
    assert!(
        r.stderr.contains("strict: unresolved value at plan time"),
        "{}",
        r.stderr
    );
    assert!(
        !s.path("dform.state/gke/remote.json").exists()
            || !s
                .read("dform.state/gke/remote.json")
                .contains("gke_cluster")
    );

    // Permissive, the same program plans its two phases.
    let s = gke(false);
    s.run(&["plan", "g.df"]).success();
}

/// A single-phase plan whose documents carry fresh nulls (ids known after
/// apply) is not refused.
#[test]
fn fresh_nulls_still_flow() {
    let s = Scratch::project("lang-strict-fresh");
    s.write(
        "p.df",
        "edition 2026\nstack p { unknowns = \"strict\" }\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nresource net.subnet a { vpc_id = ref(net.vpc, \"main\", \"id\"), cidr = \"10.0.1.0/24\" }\n",
    );
    let r = s.run(&["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("vpc_id = ?net.vpc[\"main\"].id"),
        "{}",
        r.stdout
    );
    s.run(&["apply", "p.df"]).success();
}

/// A strict stack whose resource rule reads a helper with a stuck
/// instance: the helper's instance is stuck/4, the resource rule a pending
/// group that may derive after the boundary. Each is its own deny, and
/// `allow_stuck(HeadPattern).` relaxes each per key.
#[test]
fn a_pending_group_is_refused_and_allow_stuck_relaxes_per_key() {
    let s = Scratch::project("lang-strict-allow");
    let program = "edition 2026\nstack p { unknowns = \"strict\" }\nresource db.postgres a {}\nup(d) if e = db.postgres[d].endpoint, e != \"\"\nresource net.subnet s {\n  if up(\"a\")\n  cidr = \"10.0.1.0/24\"\n}\n";
    s.write("p.df", program);
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("- strict: unresolved value at plan time ctx={\"head\":\"up(\\\"a\\\")\""),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains(
            "- strict: a pending group at plan time ctx={\"head\":\"want(\\\"net.subnet\\\", \\\"s\\\")\",\
             \"nulls\":[\"db.postgres/a#endpoint\"]"
        ),
        "{}",
        r.stderr
    );

    // Relaxing the helper's key leaves the pending group refused.
    s.write(
        "p.df",
        &format!("{program}allow_stuck(\"up(\\\"a\\\")\")\n"),
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(!r.stderr.contains("unresolved value"), "{}", r.stderr);
    assert!(
        r.stderr.contains("strict: a pending group at plan time"),
        "{}",
        r.stderr
    );

    // Both keys relaxed: the two-phase plan is allowed.
    s.write(
        "p.df",
        &format!(
            "{program}allow_stuck(\"up(\\\"a\\\")\")\n\
             allow_stuck(\"want(\\\"net.subnet\\\", \\\"s\\\")\")\n"
        ),
    );
    let r = s.run(&["plan", "p.df"]).success();
    assert!(
        r.stdout
            .contains("pending groups:\n? net.subnet[\"s\"] x unknown"),
        "{}",
        r.stdout
    );

    // allow_stuck is facts only.
    s.write(
        "p.df",
        &format!("{program}allow_stuck(h) if stuck(_, h, _, _)\n"),
    );
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains("allow_stuck must be a fact, not a rule"),
        "{}",
        r.stderr
    );
}

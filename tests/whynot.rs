//! R-80: `dform why-not ADDR`, the rule that could have derived a thing
//! and the first condition of it that failed, with the nearest rows that
//! would have passed; and nothing invented for what no rule mentions.

mod common;
use common::{Scratch, repo};

/// The README's program, on the fake cloud: a subnet in every zone the
/// table says is available.
const SHOP: &str = r#"use fake

resource net.vpc main {
  cidr = "10.0.0.0/16"
}

decl availability_zone(state: string, name: string, n: int)
availability_zone("available", "us-east-1a", 0)
availability_zone("available", "us-east-1b", 1)
availability_zone("impaired", "us-east-1c", 2)

#| One private subnet in every available zone.
resource net.subnet "private-${availability_zone}" {
  vpc = main
  cidr = inet.subnet(main.cidr, 8, n)
  zone = availability_zone
} where availability_zone("available", availability_zone, n)

set s.tags = { tier: "edge" } where s in net.subnet, s.zone == "us-east-1c"
"#;

fn shop(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/shop.df", SHOP);
    s
}

#[track_caller]
fn golden(name: &str, got: &str) {
    let path = repo()
        .join("tests/golden/inspection")
        .join(format!("{name}.txt"));
    common::golden_file(&path, got, "whynot");
}

/// The README's example: the zone is in the table, but not available. The
/// rule is named with its statement, the literal that failed with what
/// the address bound in it, and the nearest rows, without the column the
/// literal states.
#[test]
fn a_missing_subnet_names_the_zone_row_it_lacks() {
    let s = shop("whynot-zone");
    let r = s
        .run(&["why-not", r#"net.subnet["private-us-east-1c"]"#, "shop"])
        .success();
    assert_eq!(
        r.stdout,
        "net.subnet[\"private-us-east-1c\"]: no rule derives it\n  \
         stacks/shop.df:13  resource net.subnet \"private-${availability_zone}\" { .. } where \
         availability_zone(\"available\", availability_zone, n)\n    \
         availability_zone(\"available\", \"us-east-1c\", n): no row\n    \
         nearest: (\"us-east-1a\", 0), (\"us-east-1b\", 1)\n"
    );
    golden("whynot_readme", &r.stdout);
    // A row with constants, the same way; no column is stated, so the
    // rows print whole.
    let r = s
        .run(&[
            "why-not",
            r#"availability_zone("available", "us-east-1c", 2)"#,
            "shop",
        ])
        .success();
    assert_eq!(
        r.stdout,
        "no rule derives availability_zone(\"available\", \"us-east-1c\", 2): no row of \
         availability_zone is it\n  nearest: (\"impaired\", \"us-east-1c\", 2), (\"available\", \
         \"us-east-1a\", 0), (\"available\", \"us-east-1b\", 1)\n"
    );
    // What is derived is `why`'s.
    let r = s
        .run(&["why-not", r#"net.subnet["private-us-east-1a"]"#, "shop"])
        .success();
    assert!(
        r.stdout.contains("it is derived; `dform why"),
        "{}",
        r.stdout
    );
}

/// A copy under a guard: the copy is not made, and why is its guard,
/// with the value it compared.
#[test]
fn a_guarded_instance_names_the_guard_that_did_not_hold() {
    let s = Scratch::new("whynot-guard");
    let file = repo().join("examples/demo/stacks/dform.df");
    let r = s
        .run(
            &common::on(
                file.to_str().unwrap(),
                &["--world", "w.json"],
                &["why-not", r#"net.vpc["peer.vpc"]"#],
            )
            .into_iter()
            .chain(["env=dev".to_string()])
            .collect::<Vec<_>>(),
        )
        .success();
    let out = r.stdout.replace(&format!("{}/", repo().display()), "");
    assert_eq!(
        out,
        "net.vpc[\"peer.vpc\"]: no rule derives it\n  \
         examples/demo/network.df:19  resource net.vpc vpc { .. }   (instance network.vpc peer)\n    \
         instance network.vpc peer: not made\n      \
         examples/demo/stacks/dform.df:54  instance network.vpc peer { .. } where env != \"dev\"\n        \
         env != \"dev\": false, with env = \"dev\"\n"
    );
    golden("whynot_guard", &out);
}

/// An attribute a `set` writes only where a condition holds: the
/// condition, and the value the attribute it read has.
#[test]
fn an_attribute_names_the_condition_and_the_value_there() {
    let s = shop("whynot-attr");
    let r = s
        .run(&[
            "why-not",
            r#"net.subnet["private-us-east-1a"].tags.tier"#,
            "shop",
        ])
        .success();
    assert_eq!(
        r.stdout,
        "net.subnet[\"private-us-east-1a\"].tags.tier: no rule derives it\n  \
         stacks/shop.df:19  set s.tags = { tier: \"edge\" } where s in net.subnet, s.zone == \
         \"us-east-1c\"\n    \
         net.subnet[\"private-us-east-1a\"].zone = \"us-east-1c\": no row\n    \
         nearest: net.subnet[\"private-us-east-1a\"].zone = \"us-east-1a\"\n"
    );
    golden("whynot_attribute", &r.stdout);
    // An attribute of a resource that is not derived: the resource.
    let r = s
        .run(&[
            "why-not",
            r#"net.subnet["private-us-east-1c"].cidr"#,
            "shop",
        ])
        .success();
    assert!(
        r.stdout.starts_with(
            "net.subnet[\"private-us-east-1c\"].cidr: net.subnet[\"private-us-east-1c\"] is not \
             derived\n"
        ) && r.stdout.contains("nearest: (\"us-east-1a\", 0)"),
        "{}",
        r.stdout
    );
}

/// What no rule mentions gets no invented reason: one line, and nothing
/// under it.
#[test]
fn a_never_mentioned_address_says_so_and_stops() {
    let s = shop("whynot-never");
    for (addr, want) in [
        (
            r#"aws.subnet["private-us-east-1c"]"#,
            "no rule derives aws.subnet[\"private-us-east-1c\"]: no resource aws.subnet is \
             named like it\n",
        ),
        (
            r#"net.subnet["public-us-east-1a"]"#,
            "no rule derives net.subnet[\"public-us-east-1a\"]: no resource net.subnet is \
             named like it\n",
        ),
        (
            r#"net.vpc["main"].tags"#,
            "no rule derives net.vpc[\"main\"].tags: no statement sets it\n",
        ),
    ] {
        let r = s.run(&["why-not", addr, "shop"]).success();
        assert_eq!(r.stdout, want);
    }
    let r = s.run(&["why-not", "not an address", "shop"]).failure();
    assert!(
        r.stderr.contains("why-not: expected an address"),
        "{}",
        r.stderr
    );
}

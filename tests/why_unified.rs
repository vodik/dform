//! One `why` (R-150): what exists gets its chain; what does not, why not
//! (R-80: the rule that could have derived it and the first condition of
//! it that failed, with the nearest rows that would have passed; nothing
//! invented for what no rule mentions, but the nearest name); a resource
//! `later` holds, both; `why 'deny "MESSAGE"'`, whether the deny holds and
//! if not which clause failed on what. `why-not` is gone.

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
    common::golden_file(&path, got, "why_unified");
}

/// The README's example: the zone is in the table, but not available. The
/// rule is named with its statement, the literal that failed with what
/// the address bound in it, and the nearest rows, without the column the
/// literal states.
#[test]
fn a_missing_subnet_names_the_zone_row_it_lacks() {
    let s = shop("whynot-zone");
    let r = s
        .run(&["why", r#"net.subnet["private-us-east-1c"]"#, "shop"])
        .success();
    assert_eq!(
        r.stdout,
        "net.subnet private-us-east-1c: no rule derives it\n  \
         stacks/shop.df:13  resource net.subnet \"private-${availability_zone}\" { .. } where \
         availability_zone(\"available\", availability_zone, n)\n    \
         availability_zone(\"available\", \"us-east-1c\", n): no row\n    \
         nearest: (\"us-east-1a\", 0), (\"us-east-1b\", 1)\n"
    );
    golden("why_absent_readme", &r.stdout);
    // A row with constants, the same way; no column is stated, so the
    // rows print whole.
    let r = s
        .run(&[
            "why",
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
    // What is derived: its chain.
    let r = s
        .run(&["why", r#"net.subnet["private-us-east-1a"]"#, "shop"])
        .success();
    assert!(
        r.stdout
            .starts_with("net.subnet private-us-east-1a  stacks/shop.df:13"),
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
                &["why", r#"net.vpc["peer.vpc"]"#],
            )
            .into_iter()
            .chain(["env=dev".to_string()])
            .collect::<Vec<_>>(),
        )
        .success();
    let out = r.stdout.replace(&format!("{}/", repo().display()), "");
    assert_eq!(
        out,
        "net.vpc peer.vpc: no rule derives it\n  \
         examples/demo/network.df:19  resource net.vpc vpc { .. }   (resource network.vpc peer)\n    \
         resource network.vpc peer: not made\n      \
         examples/demo/stacks/dform.df:54  resource network.vpc peer { .. } where env != \"dev\"\n        \
         env != \"dev\": false, with env = \"dev\"\n"
    );
    golden("why_absent_guard", &out);
}

/// An attribute a `set` writes only where a condition holds: the
/// condition, and the value the attribute it read has.
#[test]
fn an_attribute_names_the_condition_and_the_value_there() {
    let s = shop("whynot-attr");
    let r = s
        .run(&[
            "why",
            r#"net.subnet["private-us-east-1a"].tags.tier"#,
            "shop",
        ])
        .success();
    assert_eq!(
        r.stdout,
        "net.subnet private-us-east-1a.tags.tier: no rule derives it\n  \
         stacks/shop.df:19  set s.tags = { tier: \"edge\" } where s in net.subnet, s.zone == \
         \"us-east-1c\"\n    \
         net.subnet private-us-east-1a.zone = \"us-east-1c\": no row\n    \
         nearest: net.subnet private-us-east-1a.zone = \"us-east-1a\"\n"
    );
    golden("why_absent_attribute", &r.stdout);
    // An attribute of a resource that is not derived: the resource.
    let r = s
        .run(&["why", r#"net.subnet["private-us-east-1c"].cidr"#, "shop"])
        .success();
    assert!(
        r.stdout.starts_with(
            "net.subnet private-us-east-1c.cidr: net.subnet private-us-east-1c is not \
             derived\n"
        ) && r.stdout.contains("nearest: (\"us-east-1a\", 0)"),
        "{}",
        r.stdout
    );
}

/// What no rule mentions gets no invented reason: one line, and the
/// nearest address the program derives when one is near.
#[test]
fn a_never_mentioned_address_says_so_and_stops() {
    let s = shop("whynot-never");
    for (addr, want) in [
        (
            r#"aws.subnet["private-us-east-1c"]"#,
            "no rule derives aws.subnet private-us-east-1c: no resource aws.subnet is \
             named like it\n  nearest: net.subnet private-us-east-1a\n",
        ),
        (
            r#"net.subnet["public-us-east-1a"]"#,
            "no rule derives net.subnet public-us-east-1a: no resource net.subnet is \
             named like it\n  nearest: net.subnet private-us-east-1a\n",
        ),
        (
            r#"compute.vm["far-from-everything"]"#,
            "no rule derives compute.vm far-from-everything: no resource compute.vm is \
             named like it\n",
        ),
        (
            r#"net.vpc["main"].tags"#,
            "no rule derives net.vpc main.tags: no statement sets it\n",
        ),
    ] {
        let r = s.run(&["why", addr, "shop"]).success();
        assert_eq!(r.stdout, want);
    }
    let r = s.run(&["why", "not an address", "shop"]).failure();
    assert!(
        r.stderr.contains("why: expected an address"),
        "{}",
        r.stderr
    );
}

fn provider_scratch(name: &str, kubeconfig: &str) -> Scratch {
    let s = Scratch::new(name);
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s.write(
        "p.df",
        &format!(
            r#"
use fake {{ source = "prov" }}
resource db.postgres server {{ name = "server" }}
let kc = str.format("kc@%s", {kubeconfig})
use k8s {{ kubeconfig = kc }}
resource k8s.namespace ns {{ metadata.name = "app" }}
"#
        ),
    );
    s
}

/// A resource the plan puts in tick 2 (its provider configured from
/// what tick 1 makes): its chain, and the tick it runs in with what it
/// waits on, as the plan's tick says it (After R-156).
#[test]
fn a_held_resource_gets_its_chain_and_its_tick() {
    let s = provider_scratch("why-tick2", "server.endpoint");
    let r = common::mock(&s, &["why", "k8s.namespace ns"]).success();
    assert!(
        r.stdout.starts_with("k8s.namespace ns  p.df:6\n")
            && r.stdout.contains("\n  metadata.name = \"app\" ")
            && r.stdout
                .ends_with("\ntick 2  waits on  provider k8s  kubeconfig = kc\n"),
        "{}",
        r.stdout
    );
    // Applied, the provider is configured: the chain alone.
    common::mock(&s, &["apply"]).success();
    let r = common::mock(&s, &["why", "k8s.namespace ns"]).success();
    assert!(!r.stdout.contains("waits on"), "{}", r.stdout);
}

/// What no tick of the plan makes (a read that has not answered) is
/// `later`'s.
#[test]
fn a_resource_no_tick_makes_is_later() {
    let s = provider_scratch("why-later", "io.read(\"ssh://ubuntu@127.0.0.1:1/kc\")");
    let r = common::mock(&s, &["why", "k8s.namespace ns"]).success();
    assert!(
        r.stdout
            .ends_with("\nlater  waits on  provider k8s  kubeconfig = kc\n"),
        "{}",
        r.stdout
    );
}

/// A resource rule the plan holds as a group: why not, and its tick,
/// `tick 2+`: the first it can run in, what it is stuck on first.
#[test]
fn a_group_says_its_tick_as_a_lower_bound() {
    let s = Scratch::new("why-group-tick");
    s.write(
        "p.df",
        r#"
resource db.postgres orders { size = 1 }
resource iam.policy "connect-${host}" {
  statements = [{ action: "db.connect", resource: host }]
} where pg in db.postgres, host = pg.endpoint
use fake
"#,
    );
    let r = common::mock(&s, &["why", "iam.policy \"connect-${host}\""]).success();
    assert!(
        r.stdout.ends_with("\ntick 2+  waits on  orders.endpoint\n"),
        "{}",
        r.stdout
    );
}

const DENIES: &str = r#"use fake
resource net.vpc main { cidr = "10.0.0.0/16" }
deny "no vpcs" { v } where v in net.vpc
deny "no subnets" where _ in net.subnet
"#;

/// `why 'deny "MESSAGE"'`: a deny that holds, with what made it; one that
/// does not, with the clause that failed; a message no deny says, the
/// nearest that one does.
#[test]
fn a_deny_by_its_message_says_whether_it_holds() {
    let s = Scratch::new("why-deny");
    s.write("p.df", DENIES);
    let r = common::mock(&s, &["why", r#"deny "no vpcs""#]).success();
    assert_eq!(
        r.stdout,
        "deny \"no vpcs\": holds\n\
         deny \"no vpcs\" {v: \"main\"}\n  \
         p.df:3  deny \"no vpcs\" { v } where v in net.vpc\n  \
         with v = net.vpc main\n  \
         └─ net.vpc main   p.df:2\n"
    );
    let r = common::mock(&s, &["why", r#"deny "no subnets""#]).success();
    assert!(
        r.stdout.starts_with(
            "deny \"no subnets\": does not hold\n  \
             p.df:4  deny \"no subnets\" where _ in net.subnet\n    \
             want(\"net.subnet\", _): no row\n"
        ),
        "{}",
        r.stdout
    );
    let r = common::mock(&s, &["why", r#"deny "no subnet""#]).success();
    assert_eq!(
        r.stdout,
        "deny \"no subnet\": no deny says it\n  nearest: deny \"no subnets\"\n"
    );
}

/// `why-not` is gone: the parser's usage error names `why`.
#[test]
fn why_not_is_gone_and_names_why() {
    let s = shop("why-not-gone");
    let r = s
        .run(&["why-not", r#"net.subnet["private-us-east-1c"]"#, "shop"])
        .failure();
    assert_eq!(r.code, Some(2), "{}", r.stderr);
    assert!(
        r.stderr.contains("a similar subcommand exists: 'why'"),
        "{}",
        r.stderr
    );
}

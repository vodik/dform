//! The policy block carries every deny the plan's evaluation carries
//! (R-214): a `use`d module's (a policy pack) and a component copy's, as
//! the stack's own (R-200): one line each, at the site in the file it is
//! written in, a waiting one with its cell and tick, counted in the
//! headline; `-v` lists the holding ones; `--json` nests them. A copy's
//! `x in T` ranges over its own resources, so its line counts only
//! those. On the mock.

mod common;
use common::Scratch;

const STACK: &str = r#"
use fake
use policy
resource net.vpc a { cidr = "10.0.0.0/16" }
resource net.vpc c { cidr = "10.2.0.0/8" }
resource db.postgres db { name = "db" }
"#;

/// A policy pack: one deny fails, one holds, one reads a computed cell.
const POLICY: &str = r#"
deny "a network is no bigger than a /16" { vpc: v } where v in net.vpc, v.cidr == "10.2.0.0/8"
deny "networks are named" where v in net.vpc, v.name == "unnamed"
deny "the database has no endpoint yet" where d in db.postgres, d.endpoint == "nowhere"
"#;

fn pack(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("p.df", STACK);
    s.write("policy.df", POLICY);
    s
}

/// The block's lines, from its header to its end, each with its columns
/// one `|` apart (their widths are the printer's, tests/policy_block.rs).
fn block(stdout: &str) -> String {
    let lines: Vec<String> = stdout
        .lines()
        .skip_while(|l| !l.starts_with("policy  "))
        .take_while(|l| !l.is_empty())
        .map(|l| {
            let indent = l.len() - l.trim_start().len();
            let cols: Vec<&str> = l
                .trim()
                .split("  ")
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect();
            format!("{}{}", " ".repeat(indent), cols.join("|"))
        })
        .collect();
    lines.join("\n")
}

#[test]
fn a_used_modules_denies_are_in_the_block_and_the_headline() {
    let s = pack("policy-module");
    let r = s.run(&["plan", "p.df"]);
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert_eq!(
        r.summary(),
        "plan: 3 changes (3 create) over 1 tick; policy: 1 hold · 1 fails · 1 undetermined",
        "{}",
        r.stdout
    );
    assert_eq!(
        block(&r.stdout),
        "policy|1 hold · 1 fails · 1 undetermined\n  \
         fails|a network is no bigger than a /16|policy.df:2|1 hold · 1 fails\n    \
         net.vpc c|vpc = \"c\"\n    \
         1 hold\n  \
         undetermined|the database has no endpoint yet|policy.df:4|1 undetermined\n    \
         db.postgres db|until endpoint is known (tick 2)",
        "{}",
        r.stdout
    );
}

#[test]
fn verbose_lists_a_used_modules_holding_deny() {
    let s = pack("policy-module-v");
    let r = s.run(&["plan", "p.df", "-v"]);
    let b = block(&r.stdout);
    assert!(
        b.contains("  holds|networks are named|policy.df:3|2 hold\n    net.vpc a|holds"),
        "{}",
        r.stdout
    );
}

#[test]
fn json_carries_a_used_modules_denies() {
    let s = pack("policy-module-json");
    let r = s.run(&["plan", "p.df", "--json"]);
    let j: serde_json::Value = serde_json::from_str(&r.stdout).expect(&r.stdout);
    let lines: Vec<(&str, &str, &str)> = j["policy"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["mark"].as_str().unwrap(),
                p["text"].as_str().unwrap(),
                p["at"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        lines,
        [
            ("fails", "a network is no bigger than a /16", "policy.df:2"),
            (
                "undetermined",
                "the database has no endpoint yet",
                "policy.df:4"
            ),
            ("holds", "networks are named", "policy.df:3"),
        ],
        "{j:#}"
    );
    assert_eq!(
        j["policy"][1]["undetermined"][0]["of"], "db.postgres db",
        "{j:#}"
    );
}

/// A module the stack uses whose component it copies twice: the copy's
/// deny ranges over the copy's own database, never the stack's.
const K3S: &str = r#"
component node {
  input index: int
  resource db.postgres db { name = "db-${index}" }
  deny "a node's database has no endpoint yet" where d in db.postgres, d.endpoint == "nowhere"
  deny "a node's database is named" where d in db.postgres, d.name == ""
}
resource node "agent-${i}" { index = i } where i in [0, 1]
"#;

#[test]
fn a_copys_denies_range_over_its_own_resources() {
    let s = Scratch::project("policy-module-copy");
    s.write("k3s.df", K3S);
    s.write(
        "p.df",
        "use fake\nuse k3s\nresource db.postgres main { name = \"main\" }\n",
    );
    let r = s.run(&["plan", "p.df", "-v"]).success();
    assert_eq!(
        r.summary(),
        "plan: 3 changes (3 create) over 1 tick; policy: 1 hold · 1 undetermined",
        "{}",
        r.stdout
    );
    assert_eq!(
        block(&r.stdout),
        "policy|1 hold · 1 undetermined\n  \
         undetermined|a node's database has no endpoint yet|k3s.df:5|2 undetermined\n    \
         db.postgres k3s.agent-0.db|until endpoint is known (tick 2)\n    \
         db.postgres k3s.agent-1.db|until endpoint is known (tick 2)\n  \
         holds|a node's database is named|k3s.df:6|2 hold\n    \
         db.postgres k3s.agent-0.db|holds\n    \
         db.postgres k3s.agent-1.db|holds",
        "{}",
        r.stdout
    );
}

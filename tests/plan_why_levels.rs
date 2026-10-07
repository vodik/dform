//! R-79, R-111: the plan's ladder on the demo and crud-api. The default
//! (`--why=line`) is the plan grouped by tick, each change's site and each
//! value written outside its block's; `-v` (`--why=how`) says how on the
//! same lines; `-vv` (`--why=full`, `--why` alone) adds under each
//! attribute the chain of expressions its value passed through (R-122); `-q` (`--why=none`) is the bare diff as it was laid
//! out before (tests/golden/*.plan-bare.txt pins it byte for byte).

mod common;
use common::{Scratch, repo};

fn plan(s: &Scratch, example: &str, why: &[&str]) -> String {
    let dir = repo().join("examples").join(example);
    let mut args = vec!["-C", dir.to_str().unwrap(), "plan"];
    args.extend_from_slice(why);
    s.run(&args).success().stdout
}

/// A line without its site column and its value (a secret's label and a
/// long string say more from `-v`, as does the deployment line, where its
/// key came from): the text before the first two spaces after its indent,
/// and before ` = ` or ` (`.
fn left(l: &str) -> String {
    let body = l.trim_start();
    let indent = &l[..l.len() - body.len()];
    let text = body.split("  ").next().unwrap_or_default();
    let text = match text.strip_prefix("deployment: ") {
        Some(_) => text.split(" (").next().unwrap_or_default(),
        None => text,
    };
    // A secret inside a laid-out value says its label from `-v` too.
    let text = text.split("(sensitive").next().unwrap_or_default();
    format!("{indent}{}", text.split(" = ").next().unwrap_or_default())
}

fn levels(example: &str) -> (String, String) {
    let s = Scratch::new(&format!("why-levels-{example}"));
    let line = plan(&s, example, &[]);
    assert_eq!(plan(&s, example, &["--why=line"]), line);
    assert!(line.contains("\ntick 1  "), "{line}");
    assert!(!line.contains("\napply: "), "{line}");
    // No `?`, no bindings, no ranks at the default level.
    assert!(
        !line.contains(" = ?") && !line.contains("  with ") && !line.contains(" @"),
        "{line}"
    );

    // `-v` says how on the same lines.
    let how = plan(&s, example, &["-v"]);
    assert_eq!(plan(&s, example, &["--why=how"]), how);
    assert_eq!(
        how.lines().map(left).collect::<Vec<_>>(),
        line.lines().map(left).collect::<Vec<_>>(),
        "{how}"
    );
    assert_ne!(how, line);

    // `-vv` adds each value's chain under its attribute, nothing else: no
    // line for the resource as a whole (R-122).
    let full = plan(&s, example, &["-vv"]);
    assert_eq!(plan(&s, example, &["--why=full"]), full);
    assert_eq!(plan(&s, example, &["--why"]), full);
    let derivation = |l: &str| {
        let t = l.trim_start();
        t.starts_with("= ") || t.starts_with("over ")
    };
    // Its changes are `-v`'s, each value leaf by leaf (R-124): a value
    // one write made is one laid-out line below `-vv`.
    let changes = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| {
                let t = l.trim_start();
                l.len() == t.len()
                    || ["+ ", "~ ", "- ", "± ", "tick "]
                        .iter()
                        .any(|m| t.starts_with(m))
            })
            .map(left)
            .collect()
    };
    assert_eq!(changes(&full), changes(&how), "{full}");
    assert!(
        !full
            .lines()
            .any(|l| !derivation(l) && (l.ends_with(" = {") || l.ends_with(" = ["))),
        "{full}"
    );
    assert!(
        full.lines().any(|l| l.trim_start().starts_with("= ")),
        "{full}"
    );
    assert!(
        !full.lines().any(|l| l.trim_start().starts_with("by ")),
        "{full}"
    );
    assert!(full.lines().count() > line.lines().count(), "{full}");

    // `-q`: the bare diff, no tick, no site.
    let none = plan(&s, example, &["-q"]);
    assert_eq!(plan(&s, example, &["--why=none"]), none);
    assert!(none.contains("\ndefinite:\n+ "), "{none}");
    assert!(
        !none.contains("\ntick 1  ") && !none.contains(".df:") && !none.contains("\napply: "),
        "{none}"
    );
    (line, how)
}

#[test]
fn the_demo_plans_at_each_level() {
    let (line, how) = levels("demo");
    // A copy's resources under it, each by its full address; a value set
    // outside its block by where; a reference by its address.
    assert!(
        line.contains(
            "  + network.vpc main\n    + net.vpc main.vpc                          network.df:19\n        \
             cidr = \"10.50.0.0/16\"                   stacks/dform.df:10\n        \
             tags = { env: \"staging\", component: \"network\" }\n"
        ) && line.contains("        vpc = main.vpc\n"),
        "{line}"
    );
    // `-v`: the bindings and the expressions.
    assert!(
        how.contains("    + net.subnet main.private-us-test-1a        network.df:24  with z = \"us-test-1a\"\n        \
             cidr = \"10.50.0.0/20\"                   inet.subnet(vpc.cidr, 4, zone_index[z])\n"),
        "{how}"
    );
}

#[test]
fn crud_api_plans_at_each_level() {
    let (line, how) = levels("crud-api");
    // A secret is `(sensitive)`, by its label from `-v`; a value another
    // resource of the tick computes is the expression that reads it.
    assert!(
        line.contains("      password = (sensitive)\n")
            && line.contains("        PGHOST: db.private_ip_address,\n"),
        "{line}"
    );
    assert!(
        how.contains("      password = (sensitive random.password(\"crud-api-db\"))\n"),
        "{how}"
    );
    // A long string inside a laid-out value is whole (R-124); one on its
    // own line elides its middle by default, whole from `-v`.
    let digest = "sha256:9f2c4d0e8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d";
    assert!(
        line.contains(&format!(
            "          image: \"gcr.io/shop/crud-api@{digest}\",\n"
        )),
        "{line}"
    );
    let full = plan(&Scratch::new("why-levels-crud-vv"), "crud-api", &["-vv"]);
    assert!(
        full.contains(".image = \"gcr.io/shop/crud-api@sha256:9f2c4d0e8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d\"\n"),
        "{full}"
    );
}

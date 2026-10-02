//! Doc comments (docs/grammar.md "Doc comments"): `#|` lines directly above
//! an item lower to `doc(Kind, Name, Key, Value)` facts a policy can read
//! and require; `dform doc` renders a project's as Markdown.

mod common;
use common::{Scratch, repo};
use dform::ast::{Stmt, Term};
use dform::value::Value;

/// Every `doc` fact of the program at `entry`, as its four strings.
fn docs(s: &Scratch, entry: &str) -> Vec<[String; 4]> {
    let program = dform::loader::load_program(&[s.path(entry)]).unwrap_or_else(|e| panic!("{e:#}"));
    program
        .statements
        .iter()
        .filter_map(|st| match st {
            Stmt::Fact(a) if a.pred == "doc" => Some(a.args.clone()),
            _ => None,
        })
        .map(|args| {
            args.iter()
                .map(|t| match t {
                    Term::Val(Value::Str(s)) => s.clone(),
                    t => panic!("{t:?}"),
                })
                .collect::<Vec<_>>()
                .try_into()
                .unwrap()
        })
        .collect()
}

fn row(k: &str, n: &str, key: &str, v: &str) -> [String; 4] {
    [k, n, key, v].map(String::from)
}

#[test]
fn doc_comments_lower_to_doc_facts() {
    let s = Scratch::new("docs-facts");
    s.write(
        "lib.df",
        "edition 2026\n\
         #| Shared names.\n\
         type zone = enum(\"a\", \"b\")\n\
         ",
    );
    s.write(
        "p.df",
        "edition 2026\n\
         import \"lib.df\"\n\
         #| A network.\n\
         #| owner: net-team\n\
         #| since: 2026.1\n\
         module network {\n\
         \x20 #| Its range.\n\
         \x20 input cidr: string\n\
         \x20 # a plain comment is no doc\n\
         \x20 output id: string = cidr\n\
         }\n\
         #| deprecated: read zones/1\n\
         zone_of(\"a\")\n\
         \n\
         #| Not above anything: a blank line follows.\n\
         \n\
         #| The first VM.\n\
         resource compute.vm one {}\nprovider fake\n",
    );
    assert_eq!(
        docs(&s, "p.df"),
        vec![
            row("alias", "zone", "description", "Shared names."),
            row("module", "network", "description", "A network."),
            row("module", "network", "owner", "net-team"),
            row("module", "network", "since", "2026.1"),
            row("input", "network.cidr", "description", "Its range."),
            row("rule", "zone_of", "deprecated", "read zones/1"),
            row(
                "resource",
                "compute.vm[\"one\"]",
                "description",
                "The first VM."
            ),
        ]
    );
}

/// A policy reads `doc/4`: here it requires an owner of every documented
/// module, and warns of a deprecated item.
#[test]
fn a_policy_can_require_docs() {
    let s = Scratch::new("docs-policy");
    s.write(
        "p.df",
        "edition 2026\n\
         #| Has an owner.\n\
         #| owner: a-team\n\
         module owned {}\n\
         #| Has none.\n\
         module orphan {}\n\
         #| deprecated: use owned\n\
         module old {}\n\
         policy docs {\n\
         \x20 deny \"a module has no owner\" { module: m } where doc(\"module\", m, \"description\", _), not doc(\"module\", m, \"owner\", _)\n\
         \x20 warn \"deprecated\" { item: n, why } where doc(_, n, \"deprecated\", why)\n\
         }\n\
         use docs\nprovider fake\n",
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "p.df"])
        .failure();
    let out = format!("{}{}", r.stdout, r.stderr);
    assert!(
        out.contains("- a module has no owner ctx={\"module\":\"orphan\"}"),
        "{out}"
    );
    assert!(!out.contains("\"owned\""), "{out}");
    assert!(
        out.contains("warning: deprecated ctx={\"item\":\"old\",\"why\":\"use owned\"}"),
        "{out}"
    );
}

/// `dform doc` on examples/demo renders every documented item of every
/// file, and `dform doc STACK` its program's.
#[test]
fn dform_doc_renders_every_documented_item() {
    let demo = repo().join("examples/demo");
    let out = common::dform()
        .args(["-C", demo.to_str().unwrap(), "doc"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let md = String::from_utf8(out.stdout).unwrap();
    assert!(md.starts_with("# demo\n"), "{md}");
    let mut documented = 0;
    for f in [
        "stacks/dform.df",
        "modules/network.df",
        "modules/database.df",
        "modules/kubernetes.df",
        "modules/iam.df",
        "modules/stdlib_net.df",
        "policies/baseline.df",
    ] {
        let text = std::fs::read_to_string(demo.join(f)).unwrap();
        let tree = dform::syntax::parser::parse(&text).syntax();
        for d in dform::syntax::doc::collect(&tree) {
            documented += 1;
            let heading = format!("### {} `{}`\n", d.kind, d.name);
            assert!(md.contains(&heading), "{heading} in\n{md}");
        }
        assert!(md.contains(&format!("\n## {f}\n")), "{f} in\n{md}");
    }
    assert!(documented >= 20, "{documented} documented items");
    assert!(md.contains("### input `env`\n\n```dform\nkey env: environment = \"staging\"\n```\n\nThe deployment's environment"), "{md}");
    assert!(md.contains("- **owner**: platform\n"), "{md}");

    let out = common::dform()
        .args(["-C", demo.to_str().unwrap(), "doc", "dform", "env=prod"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let md = String::from_utf8(out.stdout).unwrap();
    assert!(md.contains("\n## modules/network.df\n"), "{md}");
    assert!(md.contains("### module `network`"), "{md}");
}

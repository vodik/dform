//! `dform why`: the derivation tree read from the provenance circuit.

mod common;
mod inspection_common;
use common::{Scratch, repo};
use inspection_common::{dform, golden};

/// The ticket's Play line: ask why a tag exists, see the statement that
/// added it (the baseline policy pack's `set r.tags`), as written, at its
/// file and line, and not the module's own tags.
#[test]
fn why_a_tag_exists() {
    let out = dform(
        "examples/demo/stacks/dform.df env=prod",
        &[
            "why",
            r#"attr(net.vpc, "main::vpc", "tags.team", "platform")"#,
        ],
    );
    assert!(
        out.starts_with(
            "net.vpc[\"main::vpc\"].tags = {component: \"network\", env: \"prod\", \
             team: \"platform\"}\n  merged from 2 contributions\n  ├─ {team: \"platform\"}\n"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "examples/demo/baseline.df:12  set r.tags = { team: \"platform\" } where r in \
             resource   (use baseline)\n"
        ),
        "{out}"
    );
    assert!(out.contains("with r = net.vpc[\"main::vpc\"]\n"), "{out}");
    assert!(out.contains("... 1 other contribution (--all)"), "{out}");
    assert!(!out.contains(":-") && !out.contains("Σattr"), "{out}");
    golden("why_dform_prod_tag", &out);
}

/// `--core` prints the tree in the core's spelling: the lowered rule by
/// its id, core variables, the aggregate by its marker.
#[test]
fn why_core_prints_the_lowered_rules() {
    let out = dform(
        "examples/demo/stacks/dform.df env=prod",
        &[
            "why",
            "--core",
            r#"attr(net.vpc, "main::vpc", "tags.team", "platform")"#,
        ],
    );
    assert!(
        out.contains(r#"arg(Type, R, "tags", {team: "platform"}, "normal") :- want(Type, R)"#),
        "{out}"
    );
    assert!(out.contains("by Σattr: attribute aggregate"), "{out}");
    assert!(out.contains("[rank normal, owner r"), "{out}");
}

/// H-16: an address as plan prints it is a `why` and a `query` argument.
/// `T["A"]` explains the want, `T["A"].path` the attribute; any other
/// spelling of an address is refused.
#[test]
fn why_and_query_take_an_address_as_plan_prints_it() {
    let at = "examples/demo/stacks/dform.df env=prod";
    let want = dform(at, &["why", r#"net.vpc["main::vpc"]"#]);
    assert!(
        want.starts_with(
            "net.vpc[\"main::vpc\"]\n  examples/demo/network.df:15  resource \
             net.vpc vpc { .. }   (instance network.vpc main)\n"
        ),
        "{want}"
    );
    let tag = dform(at, &["why", r#"net.vpc["main::vpc"].tags.team"#]);
    assert!(
        tag.contains(r#"set r.tags = { team: "platform" } where r in resource"#),
        "{tag}"
    );
    let cidr = dform(at, &["query", r#"net.vpc["main::vpc"].cidr"#]);
    assert!(cidr.contains("10.20.0.0/16"), "{cidr}");
    let all = dform(at, &["query", r#"net.vpc["main::vpc"]"#]);
    assert!(
        all.contains("\"tags\"") && all.contains("\"cidr\""),
        "{all}"
    );
    let s = Scratch::new("why-old-address");
    common::copy_dir(&repo().join("examples/demo"), &s.dir);
    let r = s
        .run(&["why", "net.vpc/main::vpc", "dform", "env=prod"])
        .failure();
    assert!(r.stderr.contains("cannot parse"), "{}", r.stderr);
}

/// Every contribution of an aggregate, with its rank and the statement
/// that made it; bindings by the source's names; a value given on the
/// command line as its flag.
#[test]
fn why_an_attribute_shows_every_contribution() {
    let out = dform(
        "examples/demo/stacks/dform.df env=prod",
        &["why", r#"attr(net.vpc, "main::vpc", "tags", X)"#],
    );
    assert!(out.contains("  merged from 2 contributions\n"), "{out}");
    assert!(
        out.contains(
            "examples/demo/network.df:17  resource net.vpc vpc { .. tags = { env, \
             component: \"network\" } }   (instance network.vpc main)\n"
        ),
        "{out}"
    );
    assert!(out.contains("with env = \"prod\"\n"), "{out}");
    // The input's default is a contribution at its rank, stated where the
    // input is declared; --set's wins.
    assert!(
        out.contains("├─ \"staging\" @default   examples/demo/stacks/dform.df:11\n"),
        "{out}"
    );
    assert!(out.contains("└─ --set env=prod\n"), "{out}");
    assert!(out.contains("(see above)"), "{out}");
    // The cells merged on the way: the tags, the stack input --set gives,
    // the instance's input, and the stack input the settings give (R-38).
    assert_eq!(out.matches("merged from").count(), 4, "{out}");
}

/// The tour's route: the resource statement as written, the clause's
/// variables as bound, the interpolated name and the read it joins
/// through, and under it the recursion that found the path.
#[test]
fn why_a_route_shows_the_statements_that_fired() {
    let out = dform(
        "examples/tour/stacks/tour.df",
        &["why", r#"net.route["blue-to-green"]"#],
    );
    let start = "net.route[\"blue-to-green\"]
  examples/tour/stacks/tour.df:302  resource net.route \"${a}-to-${b}\" { .. } where reaches(a, b), a != b, network_of(b, v), dest = net.vpc[v].cidr
  with a = \"blue\", b = \"green\", v = \"green::vpc\", dest = 10.2.0.0/16
       \"${a}-to-${b}\" = \"blue-to-green\"
       net.vpc[v].cidr = 10.2.0.0/16
  ├─ reaches(\"blue\", \"green\")
       examples/tour/stacks/tour.df:300  reaches(a, c) where reaches(a, b), link(b, c)
";
    let got: String = out
        .lines()
        .take(7)
        .map(|l| format!("{}\n", l.replace('│', " ")))
        .collect();
    assert_eq!(got.replace("  │ ", "    "), start, "{out}");
    assert!(
        out.contains("├─ spoke(\"green\")   examples/tour/stacks/tour.df:267\n"),
        "{out}"
    );
    assert!(out.contains("network[t].vpc = \"green::vpc\"\n"), "{out}");
    golden("why_tour_route", &out);
}

/// A settings read: the entry that reads `database.backup_days`, its
/// value, and the input's layers, each where it is written (R-38).
#[test]
fn why_a_settings_read_shows_the_read() {
    let out = dform(
        "examples/tour/stacks/tour.df env=prod",
        &["why", r#"db.postgres["orders"].backup_days"#],
    );
    assert!(
        out.contains(
            "examples/tour/stacks/tour.df:156  resource db.postgres orders { .. backup_days = \
             database.backup_days .. }\n"
        ),
        "{out}"
    );
    assert!(out.contains("with database.backup_days = 14\n"), "{out}");
    // The input's layers (R-38): its default, and the settings block that
    // holds in prod, where it is written.
    assert!(
        out.contains("{backup_days: 1} @default   examples/tour/stacks/tour.df:29\n"),
        "{out}"
    );
    assert!(
        out.contains(
            "examples/tour/stacks/tour.df:148  settings { database.backup_days = 14 .. } where \
             env == \"prod\"\n"
        ),
        "{out}"
    );
    golden("why_tour_settings_read", &out);
}

#[test]
fn why_prints_one_alternative_unless_all() {
    let s = Scratch::new("why-alts");
    s.write(
        "p.df",
        "edition 2026\np(1)\nq(1)\nr(x) where p(x)\nr(x) where q(x)\ns(x) where r(x), not t(x)\nt(2) where p(2)\nprovider fake\n",
    );
    let why = |extra: &[&str]| {
        let mut a = vec!["dev", "--world", "w.json", "why"];
        a.extend(extra);
        a.push("p.df");
        s.run(&a).success().stdout
    };
    let one = why(&["s(1)"]);
    assert!(one.contains("... 1 more alternative (--all)"), "{one}");
    assert!(one.contains("not t(1)   (absent)"), "{one}");
    let all = why(&["s(X)", "--all"]);
    assert!(
        all.contains("alternative 1 of 2:") && all.contains("alternative 2 of 2:"),
        "{all}"
    );
    assert!(
        all.contains("p(1)   p.df:2\n") && all.contains("q(1)   p.df:3\n"),
        "{all}"
    );

    let none = s
        .run(&["dev", "--world", "w.json", "why", "s(7)", "p.df"])
        .failure();
    assert!(
        none.stderr.contains("no fact matches s(7)"),
        "{}",
        none.stderr
    );
}

/// A variable matches every fact; each gets its own tree.
#[test]
fn why_with_a_variable_prints_each_match() {
    let s = Scratch::new("why-vars");
    s.write(
        "p.df",
        "edition 2026\np(1)\np(2)\nq(x) where p(x)\nprovider fake\n",
    );
    let out = s
        .run(&["dev", "--world", "w.json", "why", "q(N)", "p.df"])
        .success()
        .stdout;
    assert!(
        out.starts_with("q(1)\n") && out.contains("\n\nq(2)\n"),
        "{out}"
    );
}

/// The never-prints claim holds for why: the tree over a secret prints its
/// label, also where a rule forwards it into another predicate.
#[test]
fn why_never_prints_a_labeled_secret() {
    let s = Scratch::new("why-secret");
    s.write(
        "p.df",
        r#"edition 2026
resource leaky.vault v { password = "VAULT-SECRET-DO-NOT-PRINT" }
           copy(p) where p = v.password
provider fake
"#,
    );
    let schema = repo().join("tests/fixtures/providers/leaky/schema.df");
    let out = s
        .run(&[
            "dev",
            "--provider",
            schema.to_str().unwrap(),
            "--world",
            "w.json",
            "why",
            "copy(X)",
            "p.df",
        ])
        .success()
        .stdout;
    assert!(!out.contains("VAULT-SECRET"), "{out}");
    assert!(
        out.contains("(sensitive leaky.vault[\"v\"].password)"),
        "{out}"
    );
}

/// The deformation the planner hands back for the policy pass is the
/// plan's, not the world's: `why` labels it `plan`, and a refreshed world
/// fact keeps `world (refresh)`.
#[test]
fn why_labels_planner_facts_as_the_plan() {
    let s = Scratch::new("why-plan-leaf");
    s.write(
        "p.df",
        "edition 2026\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nprovider fake\n",
    );
    s.run(&["dev", "--world", "w.json", "apply", "p.df"])
        .success();
    s.write(
        "p.df",
        "edition 2026\nlifecycle(net.vpc[\"main\"], \"prevent_destroy\")\nseen(a) where identity(net.vpc, a, _)\nprovider fake\n",
    );
    let why = |q: &str| {
        s.run(&["dev", "--world", "w.json", "why", q, "p.df"])
            .success()
            .stdout
    };
    let out = why("deny(M)");
    let line = out
        .lines()
        .find(|l| l.contains("─ deformation(\"delete\""))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(line.ends_with("   plan"), "{out}");
    assert!(!out.contains("world (refresh)"), "{out}");
    let out = why("seen(A)");
    assert!(out.contains("   world (refresh)"), "{out}");
}

/// Facts injected at an apply tick (a boundary, a resume) are labelled
/// with it, and the resume stop is a deny derived from them.
#[test]
fn why_labels_facts_injected_at_a_tick() {
    use dform::ast::{Atom, Lit, Term};
    use dform::value::Value;
    let program =
        dform::zset::with_policy_rules(dform::parser::parse_program("edition 2026\n").unwrap())
            .unwrap();
    let s = |x: &str| Term::Val(Value::Str(x.into()));
    let a = Term::Val(Value::Ref {
        typ: "net.subnet".into(),
        name: "a".into(),
        attr: String::new(),
    });
    let fact = |pred: &str, args: Vec<Term>| Atom {
        pred: pred.into(),
        args,
        record: None,
        span: Default::default(),
    };
    let facts = [
        fact("deformation", vec![s("remaining"), a.clone(), s("d1")]),
        fact("world_digest", vec![a, s("d2")]),
    ];
    let (res, denies) = dform::engine::eval_at(&program, &facts, Some(3)).unwrap();
    assert_eq!(
        denies,
        ["the world changed under a remaining action: net.subnet[\"a\"]"]
    );
    let schema = dform::schema::Schema::default();
    let redact = dform::query::Redactor::new(&res.facts, &schema);
    let printer = dform::why::Printer {
        circuit: &res.circuit,
        redact: &redact,
        all: false,
    };
    let dform::query::Query::Body { body, .. } = dform::query::parse("deny(M)").unwrap() else {
        unreachable!()
    };
    let [Lit::Pos(pat)] = body.as_slice() else {
        unreachable!()
    };
    let (deny, _) = dform::why::find(pat, &res.facts).unwrap().remove(0);
    let id = res
        .circuit
        .fact_id(&dform::engine::circuit_fact(&deny))
        .unwrap();
    let out = printer.tree(id, None);
    assert_eq!(out.matches("   plan (tick 3)\n").count(), 2, "{out}");
}

/// A clause in braces prints on the statement's line, its literals
/// joined; a read in it shows the value the firing found.
#[test]
fn why_prints_a_braced_clause_on_one_line() {
    let out = dform(
        "examples/demo/stacks/dform.df env=prod",
        &["why", r#"attr("iam.policy", P, "statements", S)"#],
    );
    assert!(
        out.contains(
            "examples/demo/baseline.df:17  set p.statements = [{ action: \"org.read\", \
             resource: \"org\" }] where env == \"prod\", p in iam.policy, p.name == \"app\"   \
             (use baseline)\n"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "│    with p = iam.policy[\"identity::app_policy\"]\n  │         p.name = \"app\"\n"
        ),
        "{out}"
    );
}

/// R-15: `plan --why` prints under each deformation the statement that
/// derived it and one line per leaf of that derivation: the facts, the
/// table rows, the inputs it rests on.
#[test]
fn plan_why_explains_each_deformation() {
    let out = dform("examples/tour/stacks/tour.df env=prod", &["plan", "--why"]);
    assert!(
        out.contains(
            "+ net.subnet[\"private-us-test-1a\"]
  cidr = \"10.0.1.0/24\"
  tags.team = \"shop\"
  visibility = \"private\"
  vpc = ?net.vpc[\"main\"]
  zone = \"us-test-1a\"
  by examples/tour/stacks/tour.df:112  resource net.subnet \"private-${z}\" { .. } where zone(z, n)
  because examples/tour/stacks/tour.df:109  zone(\"us-test-1a\", 1)
  because examples/tour/stacks/tour.df:49  net.vpc[\"main\"].cidr = 10.0.0.0/16
"
        ),
        "{out}"
    );
    // A value given on the command line is its flag.
    assert!(out.contains("  because --set env=prod\n"), "{out}");
    // Without --why the plan is as it was.
    let plain = dform("examples/tour/stacks/tour.df env=prod", &["plan"]);
    assert!(
        !plain.contains("because") && !plain.contains("  by "),
        "{plain}"
    );
    golden("why_tour_prod_plan", &out);
}

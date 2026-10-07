//! `dform why`: a value's chain (R-122), and with `--tree` the
//! derivation tree read from the provenance circuit.

mod common;
mod inspection_common;
use common::{Scratch, repo};
use inspection_common::{dform, golden};

/// A value's chain (R-122): one `= EXPRESSION   SITE` step per expression
/// its value passed through, following only what each reads, to the
/// literal; what it beat after. A literal is its place on its line.
#[test]
fn why_a_value_prints_its_chain() {
    let at = "examples/tour/stacks/tour.df env=prod";
    let out = dform(at, &["why", "orders.backup_days"]);
    assert_eq!(
        out,
        "db.postgres orders.backup_days = 14
  = database.backup_days  stacks/tour.df:145
  = 14                    stacks/tour.df:139
  over 1 @default         stacks/tour.df:27
"
    );
    let out = dform(
        "examples/demo/stacks/dform.df env=prod",
        &["why", "main.vpc.tags.team"],
    );
    assert_eq!(
        out,
        "net.vpc main.vpc.tags.team = \"platform\"  baseline.df:10\n"
    );
    // A resource: its header, and each attribute's chain; never the
    // bindings of the whole rule, nor how many contributions merged.
    let out = dform(at, &["why", "orders"]);
    assert!(
        out.starts_with("db.postgres orders  stacks/tour.df:143\n"),
        "{out}"
    );
    assert!(
        out.contains("\n  backup_days = 14\n    = database.backup_days  "),
        "{out}"
    );
    assert!(!out.contains("merged from"), "{out}");
}

/// The ticket's Play line: ask why a tag exists, see the statement that
/// added it (the baseline policy pack's `set r.tags`), as written, at its
/// file and line, and not the module's own tags.
#[test]
fn why_a_tag_exists() {
    let out = dform(
        "examples/demo/stacks/dform.df env=prod",
        &[
            "why",
            "--tree",
            r#"attr(net.vpc, "main.vpc", "tags.team", "platform")"#,
        ],
    );
    assert!(
        out.starts_with(
            "net.vpc main.vpc.tags = {component: \"network\", env: \"prod\", \
             team: \"platform\"}\n  merged from 2 contributions\n  ├─ {team: \"platform\"}\n"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "examples/demo/baseline.df:10  set r.tags = { team: \"platform\" } where r in \
             resource   (use baseline)\n"
        ),
        "{out}"
    );
    assert!(out.contains("with r = net.vpc main.vpc\n"), "{out}");
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
            r#"attr(net.vpc, "main.vpc", "tags.team", "platform")"#,
        ],
    );
    assert!(
        out.contains(r#"arg(Type, R, "tags", {team: "platform"}, "normal") :- want(Type, R)"#),
        "{out}"
    );
    assert!(out.contains("by Σattr: attribute aggregate"), "{out}");
    assert!(out.contains("[rank normal, owner r"), "{out}");
}

/// R-112: a resource's address is its path, so `why` takes it as the plan
/// prints it after the type, with the type or without: `main.vpc` (the
/// copy main's vpc), `net.vpc main.vpc`, and `T["main.vpc"]` explain one
/// want.
#[test]
fn why_takes_a_resource_by_its_path() {
    let at = "examples/demo/stacks/dform.df env=prod";
    let by_address = dform(at, &["why", r#"net.vpc["main.vpc"]"#]);
    for path in ["main.vpc", "net.vpc main.vpc"] {
        let out = dform(at, &["why", path]);
        assert_eq!(out, by_address, "{path}");
    }
    assert!(
        by_address.starts_with("net.vpc main.vpc  network.df:19\n"),
        "{by_address}"
    );
}

/// H-16: an address as plan prints it is a `why` and a `query` argument.
/// `T["A"]` explains the want, `T["A"].path` the attribute; any other
/// spelling of an address is refused.
#[test]
fn why_and_query_take_an_address_as_plan_prints_it() {
    let at = "examples/demo/stacks/dform.df env=prod";
    let want = dform(at, &["why", "--tree", r#"net.vpc["main.vpc"]"#]);
    assert!(
        want.starts_with(
            "net.vpc main.vpc\n  examples/demo/network.df:19  resource \
             net.vpc vpc { .. }   (resource network.vpc main)\n"
        ),
        "{want}"
    );
    let tag = dform(at, &["why", "--tree", r#"net.vpc["main.vpc"].tags.team"#]);
    assert!(
        tag.contains(r#"set r.tags = { team: "platform" } where r in resource"#),
        "{tag}"
    );
    let cidr = dform(at, &["query", r#"net.vpc["main.vpc"].cidr"#]);
    assert!(cidr.contains("10.20.0.0/16"), "{cidr}");
    // The address as the plan prints it now, or its path alone (R-111).
    for arg in ["net.vpc main.vpc", "main.vpc"] {
        let want = dform(at, &["why", "--tree", arg]);
        assert!(
            want.starts_with("net.vpc main.vpc\n  examples/demo/network.df:19  "),
            "{arg}: {want}"
        );
    }
    let tag = dform(at, &["why", "--tree", "main.vpc.tags.team"]);
    assert!(
        tag.contains(r#"set r.tags = { team: "platform" } where r in resource"#),
        "{tag}"
    );
    let all = dform(at, &["query", r#"net.vpc["main.vpc"]"#]);
    assert!(
        all.contains("\"tags\"") && all.contains("\"cidr\""),
        "{all}"
    );
    let s = Scratch::new("why-old-address");
    common::copy_dir(&repo().join("examples/demo"), &s.dir);
    let r = s
        .run(&["why", "net.vpc/main/vpc", "dform", "env=prod"])
        .failure();
    assert!(r.stderr.contains("not `/` (R-112)"), "{}", r.stderr);
    // The old scope separators are refused naming the path (R-112).
    for (cmd, old) in [
        ("why", r#"net.vpc["main::vpc"].cidr"#),
        ("query", r#"net.vpc["main/vpc"].cidr"#),
        ("why", "net.vpc main/vpc"),
    ] {
        let r = s.run(&[cmd, old, "dform", "env=prod"]).failure();
        assert!(
            r.stderr.contains(
                "an address is a path, its scope separated by `.`, not `/` (R-112): \"main.vpc\""
            ),
            "{cmd} {old}: {}",
            r.stderr
        );
    }
}

/// Every contribution of an aggregate, with its rank and the statement
/// that made it; bindings by the source's names; a value given on the
/// command line as its flag.
#[test]
fn why_an_attribute_shows_every_contribution() {
    let out = dform(
        "examples/demo/stacks/dform.df env=prod",
        &["why", "--tree", r#"attr(net.vpc, "main.vpc", "tags", X)"#],
    );
    assert!(out.contains("  merged from 2 contributions\n"), "{out}");
    assert!(
        out.contains(
            "examples/demo/network.df:19  resource net.vpc vpc { .. tags = { env, \
             component: \"network\" } }   (resource network.vpc main)\n"
        ),
        "{out}"
    );
    assert!(out.contains("with env = \"prod\"\n"), "{out}");
    // The input's default is a contribution at its rank, stated where the
    // input is declared; --set's wins.
    assert!(
        out.contains("├─ \"staging\" @default   examples/demo/stacks/dform.df:9\n"),
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
        &["why", "--tree", r#"net.route["blue-to-green"]"#],
    );
    let start = "net.route blue-to-green
  examples/tour/stacks/tour.df:283  resource net.route \"${a}-to-${b}\" { .. } where reaches(a, b), a != b, network_of(b, v), dest = net.vpc[v].cidr
  with a = \"blue\", b = \"green\", v = \"green.vpc\", dest = 10.2.0.0/16
       \"${a}-to-${b}\" = \"blue-to-green\"
       net.vpc[v].cidr = 10.2.0.0/16
  ├─ reaches(\"blue\", \"green\")
       examples/tour/stacks/tour.df:281  reaches(a, c) where reaches(a, b), link(b, c)
";
    let got: String = out
        .lines()
        .take(7)
        .map(|l| format!("{}\n", l.replace('│', " ")))
        .collect();
    assert_eq!(got.replace("  │ ", "    "), start, "{out}");
    assert!(
        out.contains("├─ spoke(\"green\")   examples/tour/stacks/tour.df:248\n"),
        "{out}"
    );
    assert!(out.contains("network[t].vpc = \"green.vpc\"\n"), "{out}");
    golden("why_tour_route", &out);
}

/// A settings read: the entry that reads `database.backup_days`, its
/// value, and the input's layers, each where it is written (R-38).
#[test]
fn why_a_settings_read_shows_the_read() {
    let out = dform(
        "examples/tour/stacks/tour.df env=prod",
        &["why", "--tree", r#"db.postgres["orders"].backup_days"#],
    );
    assert!(
        out.contains(
            "examples/tour/stacks/tour.df:145  resource db.postgres orders { .. backup_days = \
             database.backup_days .. }\n"
        ),
        "{out}"
    );
    assert!(out.contains("with database.backup_days = 14\n"), "{out}");
    // The input's layers (R-38): its default, and the `set` block that
    // holds in prod, where it is written.
    assert!(
        out.contains("{backup_days: 1} @default   examples/tour/stacks/tour.df:27\n"),
        "{out}"
    );
    assert!(
        out.contains(
            "examples/tour/stacks/tour.df:139  set { database.backup_days = 14 .. } where \
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
        "\np(1)\nq(1)\nr(x) where p(x)\nr(x) where q(x)\ns(x) where r(x), not t(x)\nt(2) where p(2)\nuse fake\n",
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

    // What is not derived: why not (R-150).
    let none = why(&["s(7)"]);
    assert!(
        none.starts_with("s(7): no rule derives it\n  p.df:6  s(x) where r(x), not t(x)\n"),
        "{none}"
    );
}

/// A variable matches every fact; each gets its own tree.
#[test]
fn why_with_a_variable_prints_each_match() {
    let s = Scratch::new("why-vars");
    s.write("p.df", "\np(1)\np(2)\nq(x) where p(x)\nuse fake\n");
    let out = s
        .run(&["dev", "--world", "w.json", "why", "q(N)", "p.df"])
        .success()
        .stdout;
    assert!(
        out.starts_with("decl q(x: int)\nq(1)\n") && out.contains("\n\nq(2)\n"),
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
        r#"
resource leaky.vault v { password = "VAULT-SECRET-DO-NOT-PRINT" }
           copy(p) where p = v.password
use fake
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
    assert!(out.contains("(sensitive leaky.vault v.password)"), "{out}");
}

/// The deformation the planner hands back for the policy pass is the
/// plan's, not the world's: `why` labels it `plan`, and a refreshed world
/// fact keeps `world (refresh)`.
#[test]
fn why_labels_planner_facts_as_the_plan() {
    let s = Scratch::new("why-plan-leaf");
    s.write(
        "p.df",
        "\nresource net.vpc main { cidr = \"10.0.0.0/16\" }\nuse fake\n",
    );
    s.run(&["dev", "--world", "w.json", "apply", "p.df"])
        .success();
    s.write(
        "p.df",
        "\nlifecycle(net.vpc[\"main\"], \"prevent_destroy\")\nseen(a) where identity(net.vpc, a, _)\nuse fake\n",
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
        dform::zset::with_policy_rules(dform::parser::parse_program("\n").unwrap()).unwrap();
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
    let printer = dform::report::tree::Printer {
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
    let (deny, _) = dform::report::tree::find(pat, &res.facts)
        .unwrap()
        .remove(0);
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
        &["why", "--tree", r#"attr("iam.policy", P, "statements", S)"#],
    );
    assert!(
        out.contains(
            "examples/demo/baseline.df:15  set p.statements = [{ action: \"org.read\", \
             resource: \"org\" }] where env == \"prod\", p in iam.policy, p.name == \"app\"   \
             (use baseline)\n"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "│    with p = iam.policy identity.app_policy\n  │         p.name = \"app\"\n"
        ),
        "{out}"
    );
}

/// R-15, R-122: `plan --why` prints under each attribute the chain of
/// expressions its value passed through, each where it is written, the
/// clause's binding where the expression reads it; nothing for the
/// resource as a whole.
#[test]
fn plan_why_explains_each_deformation() {
    let out = dform("examples/tour/stacks/tour.df env=prod", &["plan", "--why"]);
    assert!(
        out.contains(
            "  + net.subnet private-us-test-1a          stacks/tour.df:104  with z = \"us-test-1a\", n = 1
      cidr = \"10.0.1.0/24\"                 inet.subnet(main.cidr, 8, n)
        = inet.subnet(main.cidr, 8, n)     stacks/tour.df:106  with n = 1
      tags.team = \"shop\"                   stacks/tour.df:170
      visibility = \"private\"
      vpc = main
      zone = \"us-test-1a\"
        = z                                stacks/tour.df:107
"
        ),
        "{out}"
    );
    // A value written for a copy's input: the input, then where it is given.
    assert!(
        out.contains(
            "        cidr = \"10.1.0.0/16\"               input blue.cidr = 10.1.0.0/16   \
             stacks/tour.df:227\n          = cidr                           stacks/tour.df:218\n"
        ),
        "{out}"
    );
    assert!(!out.contains("  by ") && !out.contains("because "), "{out}");
    // At the default level, a line per change and attribute, no chain.
    let plain = dform("examples/tour/stacks/tour.df env=prod", &["plan"]);
    assert!(!plain.contains("\n        = "), "{plain}");
    golden("why_tour_prod_plan", &out);
}

/// `dform why ARGS` over a world fixture in `s`.
fn why_in(s: &Scratch, file: &str, args: &[&str]) -> String {
    let mut all = vec!["dev", "--world", "w.json", "why"];
    all.extend_from_slice(args);
    all.push(file);
    s.run(&all).success().stdout
}

/// A rule the compiler wrote (the lifecycle rule) prints by its name and
/// description at `dform`, with its bindings, not its core text at
/// `<input>:N`.
#[test]
fn why_names_a_rule_the_compiler_wrote() {
    let s = Scratch::new("why-policy-rule");
    let p = |cidr: &str| {
        format!(
            "\nuse fake\nresource net.vpc main {{\n  cidr = \"{cidr}\"\n}}\n\
             lifecycle(main, \"prevent_destroy\") where main in net.vpc\n"
        )
    };
    s.write("p.df", &p("10.0.0.0/16"));
    s.run(&["dev", "--world", "w.json", "apply", "p.df"])
        .success();
    s.write("p.df", &p("10.1.0.0/16"));
    let out = why_in(&s, "p.df", &["deny(M)"]);
    assert!(
        out.contains(
            "\n  dform  the lifecycle rule prevent_destroy, against a replace\n  with m = \
             \"lifecycle prevent_destroy: the plan would replace net.vpc[\\\"main\\\"]\", r = \
             net.vpc main\n"
        ),
        "{out}"
    );
    assert!(
        !out.contains("<input>") && !out.contains("deny(m)"),
        "{out}"
    );
}

/// A schema refinement is a check on the value, not one of the
/// contributions it is merged from.
#[test]
fn why_prints_a_refinement_as_a_check() {
    let s = Scratch::new("why-refine");
    s.write(
        "p.df",
        "\nuse fake\nresource db.postgres main {\n  size = 1\n  backup_days = 7\n}\n",
    );
    let out = why_in(&s, "p.df", &["--tree", "db.postgres[\"main\"].backup_days"]);
    assert_eq!(
        out,
        "db.postgres main.backup_days = 7\n  merged from 1 contribution\n  ├─ 7   p.df:5\n  \
         └─ check range(1, 35)   provider schema\n"
    );
}

/// A rule whose body reads no relation prints its statement and bindings,
/// not a bare `by rN`.
#[test]
fn why_prints_the_statement_of_a_rule_that_reads_nothing() {
    let s = Scratch::new("why-no-reads");
    s.write("p.df", "\nuse fake\nys(n) where n = 1 + 2\n");
    let out = why_in(&s, "p.df", &["ys(N)"]);
    assert_eq!(
        out,
        "ys(3)\n  p.df:3  ys(n) where n = 1 + 2\n  with n = 3\n"
    );
}

/// `plan --why` follows a copy's input to where the copy is given it,
/// `vpc_net = inet(cidrs.main)` at its `resource network.vpc main`, and
/// prints no core fact (`instance_of(..)`, R-65).
#[test]
fn plan_why_follows_a_copys_input() {
    let out = dform("examples/demo/stacks/dform.df", &["plan", "--why"]);
    assert!(
        out.contains("          = vpc_net ") && out.contains("          = inet(cidrs.main) "),
        "{out}"
    );
    assert!(!out.contains("instance_of("), "{out}");
}

/// `why` of a relation's facts prints its signature first, its columns as
/// declared or inferred (R-34), as `decl` writes them.
#[test]
fn why_prints_a_relations_signature() {
    let s = Scratch::new("why-signature");
    s.write(
        "p.df",
        "\nuse fake\naz(\"us-test-1a\", 1)\naz(\"us-test-1b\", 2)\n",
    );
    let out = why_in(&s, "p.df", &["az(Z, I)"]);
    assert!(
        out.starts_with("decl az(string, int)\naz(\"us-test-1a\", 1)   p.df:3\n"),
        "{out}"
    );
    assert_eq!(out.matches("decl az").count(), 1, "{out}");
}

/// An interpolated reference prints as its address in `why`'s computed
/// terms, as the evaluation formats it (R-42), typed by `in` or not.
#[test]
fn why_interpolates_a_reference_as_its_address() {
    let s = Scratch::new("why-ref-interp");
    s.write(
        "p.df",
        "\nuse fake\nresource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n}\n\
         msg(m) where r in net.vpc, m = \"vpc ${r}\"\n",
    );
    let out = why_in(&s, "p.df", &["msg(M)"]);
    assert!(
        out.contains("       \"vpc ${r}\" = \"vpc net.vpc[\\\"main\\\"]\"\n"),
        "{out}"
    );
}

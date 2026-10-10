use super::*;
use crate::ast::{Stmt, str_term};
use crate::circuit::Leaf;

fn run(src: &str) -> Result<(EvalResult, Vec<String>)> {
    let program = crate::parser::parse_program(src)?;
    eval(&program, &[])
}

fn facts_of(r: &EvalResult, pred: &str) -> Vec<String> {
    r.facts
        .iter()
        .filter(|a| a.pred == pred)
        .map(spell::atom)
        .collect()
}

/// DESIGN.org "Aggregates are not stratified": a consumer of an
/// aggregate used to see every partial result mid-fixpoint.
#[test]
fn aggregate_consumer_sees_one_complete_result() {
    let (r, _) = run("decl n(a) mixed\n             n(1)\n             n(2) where n(1)\n             n(3) where n(2)\n             all(r) where r = collect_set(x), n(x)\n             snap(l) where all(l)")
        .unwrap();
    assert_eq!(facts_of(&r, "snap"), vec!["snap([1, 2, 3])".to_string()]);
}

/// `sum` folds every body match of a group, as `count` counts them:
/// two matches of the same size are both summed. An empty group
/// derives nothing, for `count` as for `sum`.
#[test]
fn sum_folds_every_match_per_group() {
    let (r, violations) = run(r#"size("a", "x", 3)
             size("b", "x", 3)
             size("c", "y", 4)
             total(g, r) where r = sum(n), size(_, g, n)
             all(r) where r = sum(n), size(_, _, n)
             sizes(r) where r = count(n), size(_, _, n)
             big(r) where r = sum(n), size(_, _, n), n > 9
             many(r) where r = count(n), size(_, _, n), n > 9"#)
    .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    assert_eq!(
        facts_of(&r, "total"),
        [r#"total("x", 6)"#, r#"total("y", 4)"#]
    );
    assert_eq!(facts_of(&r, "all"), ["all(10)"]);
    assert_eq!(facts_of(&r, "sizes"), ["sizes(3)"]);
    assert!(facts_of(&r, "big").is_empty());
    assert!(facts_of(&r, "many").is_empty());
}

/// `min` and `max` over ints and over strings, per group.
#[test]
fn min_and_max_order_ints_and_strings() {
    let (r, violations) = run(r#"decl v(g, x: any)
             v("a", 3)
             v("a", 1)
             v("a", 12)
             v("b", "q")
             v("b", "p")
             lo(g, r) where r = min(x), v(g, x)
             hi(g, r) where r = max(x), v(g, x)"#)
    .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    assert_eq!(facts_of(&r, "lo"), [r#"lo("a", 1)"#, r#"lo("b", "p")"#]);
    assert_eq!(facts_of(&r, "hi"), [r#"hi("a", 12)"#, r#"hi("b", "q")"#]);
}

/// A group `sum` has a non-int in, or `min`/`max` a mix of ints and
/// strings or another kind, derives a deny naming the group and the
/// value, and not its head; the other groups derive theirs.
#[test]
fn an_ill_kinded_group_is_a_deny() {
    let (r, violations) = run(r#"decl v(g, x: any)
             v("a", 1)
             v("a", "p")
             v("b", true)
             v("c", 2)
             s(g, r) where r = sum(x), v(g, x)
             m(g, r) where r = max(x), v(g, x)"#)
    .unwrap();
    assert_eq!(facts_of(&r, "s"), [r#"s("c", 2)"#]);
    assert_eq!(facts_of(&r, "m"), [r#"m("c", 2)"#]);
    let has = |m: &str| violations.iter().any(|v| v.starts_with(m));
    assert!(
        has(r#"s("a", _): sum() over "p", which is not an int"#),
        "{violations:?}"
    );
    assert!(
        has(r#"s("b", _): sum() over true, which is not an int"#),
        "{violations:?}"
    );
    assert!(
        has(r#"m("a", _): max() over "p" and 1, an int and a string"#),
        "{violations:?}"
    );
    assert!(
        has(r#"m("b", _): max() over true, which is neither an int nor a string"#),
        "{violations:?}"
    );
    let (_, violations) = run(&format!(
        "v({})\n v(1)\n s(r) where r = sum(x), v(x)",
        i64::MAX
    ))
    .unwrap();
    assert!(
        violations
            .iter()
            .any(|v| v.starts_with("s(_): sum() overflows")),
        "{violations:?}"
    );
}

/// `sum` of a value known not to be an int, `min`/`max` of one neither
/// an int nor a string, is a compile error at the call.
#[test]
fn a_statically_ill_kinded_aggregate_is_an_error() {
    for (src, want) in [
        (
            r#"s(r) where r = sum("a"), b(x)"#,
            "`sum` aggregates ints, not a string",
        ),
        (
            r#"s(r) where r = sum("p-${x}"), b(x)"#,
            "`sum` aggregates ints, not a string",
        ),
        (
            r#"s(r) where r = min([x]), b(x)"#,
            "`min` aggregates ints or strings, not a list",
        ),
        (
            r#"s(r) where r = max(true), b(x)"#,
            "`max` aggregates ints or strings, not a bool",
        ),
    ] {
        let err = run(&format!("b(1)\n{src}")).unwrap_err();
        assert!(format!("{err:#}").contains(want), "{src}: {err:#}");
    }
    run("b(1)\ns(r) where r = sum(x), b(x)\nt(r) where r = max(\"p-${x}\"), b(x)").unwrap();
}

/// Rule 2: the aggregated value of `sum`, `min` and `max` is a content
/// position. A group with a null in, open or fresh, is stuck and derives
/// nothing (a fresh null's order is content too); the others derive.
#[test]
fn sum_min_max_over_a_null_is_stuck() {
    let null = |class, label: &str| Value::Null {
        label: label.into(),
        class,
        ty: "int".into(),
    };
    let fact = |g: &str, v: Value| Atom {
        pred: "size".into(),
        args: vec![str_term(g), Term::Val(v)],
        record: None,
        span: Default::default(),
    };
    let extra = [
        fact("a", null(crate::value::NullClass::Open, "t/a#size")),
        fact("a", Value::Int(1)),
        fact("b", null(crate::value::NullClass::Fresh, "t/b#size")),
        fact("c", Value::Int(2)),
    ];
    let (r, violations) = run_with(
            "decl size(a, b)\n             total(g, r) where r = sum(n), size(g, n)\n             lo(g, r) where r = min(n), size(g, n)\n             hi(g, r) where r = max(n), size(g, n)\n             all(r) where r = sum(n), size(_, n)",
            &extra,
        )
        .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    assert_eq!(facts_of(&r, "total"), [r#"total("c", 2)"#]);
    assert_eq!(facts_of(&r, "lo"), [r#"lo("c", 2)"#]);
    assert_eq!(facts_of(&r, "hi"), [r#"hi("c", 2)"#]);
    assert!(facts_of(&r, "all").is_empty());
    for (pred, name) in [
        ("total", "sum"),
        ("lo", "min"),
        ("hi", "max"),
        ("all", "sum"),
    ] {
        assert!(
            r.stuck
                .iter()
                .any(|s| s.head.pred == pred && s.reason == format!("{name} over a null")),
            "{pred}: {:?}",
            r.stuck
        );
    }
}

/// A wildcard in a negated atom matches anything: `not has k` is
/// `not k(_)` (docs/grammar.md), `not p(x, _)` asks for no row with `x`
/// first.
#[test]
fn a_wildcard_in_a_negation_matches_anything() {
    let (r, _) = run("p(1, 2)\n             k(0) where p(9, 9)\n             none(1) where not k(_)\n             lonely(x) where x in [1, 3], not p(x, _)\n             let active = \"x\" where p(9, 9)\n             let next = \"blue\" where not has active")
        .unwrap();
    assert_eq!(facts_of(&r, "none"), vec!["none(1)".to_string()]);
    assert_eq!(facts_of(&r, "lonely"), vec!["lonely(3)".to_string()]);
    assert_eq!(facts_of(&r, "next"), vec!["next(\"blue\")".to_string()]);
}

/// `has x.f` of a value without `f` does not hold (and `not has` does):
/// a walk to a missing path is no value, not an error.
#[test]
fn a_walk_to_a_missing_path_is_no_value() {
    let (r, _) = run("p({a: {b: 1}})
             deep(x) where p(x), has x.a.b
             open(x) where p(x), not has x.a.c
             shallow(x) where p(x), has x.c")
    .unwrap();
    assert_eq!(facts_of(&r, "deep").len(), 1);
    assert_eq!(facts_of(&r, "open").len(), 1);
    assert!(facts_of(&r, "shallow").is_empty());
    let (r, _) = run("p({a: {b: [1]}})\n             elem(e) where p(x), e in x.a.b\n             none(e) where p(x), e in x.a.c")
        .unwrap();
    assert_eq!(facts_of(&r, "elem"), vec!["elem(1)".to_string()]);
    assert!(facts_of(&r, "none").is_empty());
}

/// A cycle through negation is a compile error naming the cycle with
/// the text of every rule on it.
#[test]
fn negative_cycle_is_an_error_with_rule_text() {
    let err = run("q(1)
             p(x) where q(x), not r(x)
             r(x) where p(x)")
    .unwrap_err()
    .to_string();
    assert!(err.contains("negative cycle"), "{err}");
    assert!(err.contains("p(X) :- q(X), not r(X)"), "{err}");
}

/// Rules run in the stratum of their head's partition node: a `want`
/// of one type may negate, or aggregate over, `want` of another type.
#[test]
fn want_is_partitioned_by_type() {
    let (r, _) = run("want(\"net.subnet\", \"a\")
             want(\"net.subnet\", \"b\")
             subnets(r) where r = collect_set(s), want(\"net.subnet\", s)
             want(\"db.postgres\", \"db\") where subnets(l), member(l, \"a\"), not want(\"net.subnet\", \"c\")")
        .unwrap();
    assert!(facts_of(&r, "want").contains(&"want(\"db.postgres\", \"db\")".to_string()));
}

fn input(k: &str, v: Value) -> Atom {
    Atom {
        pred: "input".into(),
        args: vec![str_term(k), Term::Val(v)],
        record: None,
        span: Default::default(),
    }
}

/// Two rules set one attribute to different values: no attr fact, an
/// attr_conflict, and a deny naming the resource, the path and both
/// contributing rules with where each is written.
#[test]
fn conflicting_contributions_derive_a_deny_naming_every_witness() {
    let (r, violations) = run("resource net.vpc main { cidr = \"10.0.0.0/16\" }
             arg(net.vpc, \"main\", \"cidr\", \"10.1.0.0/16\") where want(net.vpc, \"main\")")
    .unwrap();
    assert!(
        facts_of(&r, "attr").iter().all(|a| !a.contains("cidr")),
        "{:?}",
        facts_of(&r, "attr")
    );
    assert_eq!(facts_of(&r, "attr_conflict").len(), 1);
    assert_eq!(violations.len(), 1, "{violations:?}");
    let v = &violations[0];
    let (msg, ctx) = v.split_once(" ctx=").unwrap();
    assert_eq!(msg, "conflicting attribute contributions");
    let ctx: serde_json::Value = serde_json::from_str(ctx).unwrap();
    assert_eq!(ctx["type"], "net.vpc");
    assert_eq!(ctx["addr"], "main");
    assert_eq!(ctx["path"], "cidr");
    let from: Vec<String> = ctx["witnesses"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|w| {
            w["from"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f.as_str().unwrap().to_string())
        })
        .collect();
    assert_eq!(
            from,
            vec![
                "arg(\"net.vpc\", \"main\", \"cidr\", \"10.0.0.0/16\", \"normal\") (at <input>:1:25)".to_string(),
                "arg(\"net.vpc\", \"main\", \"cidr\", \"10.1.0.0/16\", \"normal\") :- want(\"net.vpc\", \"main\") (at <input>:2:14)".to_string(),
            ]
        );
}

/// Readers see the collapsed value, never a raw contribution: an input
/// and an output are the same aggregate on pseudo-types, and a `set`'s
/// block's contribution wins over the declaration's default (R-38).
#[test]
fn input_and_output_readers_read_the_collapsed_value() {
    let (r, violations) = run("input days: int = 3
             input audit: bool = false
             set days = 14 where audit == false
             component network {\n output ids: list(string) = [\"a\", \"b\"]\n }
             resource network main {}
             got(d) where d = days
             ids(l) where output(\"main\", \"ids\", l)
             deny \"no audit\" where not audit")
    .unwrap();
    assert_eq!(facts_of(&r, "got"), vec!["got(14)".to_string()]);
    assert_eq!(facts_of(&r, "ids"), vec!["ids([\"a\", \"b\"])".to_string()]);
    assert_eq!(violations, vec!["no audit".to_string()]);
}

/// E §2.5 path normalization: `tags.team` contributes `{team: V}` to
/// `tags`, which is a Map, so it meets the block's other tags per leaf.
#[test]
fn dotted_path_contributes_to_its_top_level_attribute() {
    let (r, _) = run("resource net.vpc main { tags = { env: \"dev\" } }
             arg(net.vpc, \"main\", \"tags.team\", \"platform\") where want(net.vpc, \"main\")")
    .unwrap();
    assert_eq!(
        facts_of(&r, "attr"),
        vec![
            "attr(\"net.vpc\", \"main\", \"tags\", {env: \"dev\", team: \"platform\"})".to_string()
        ]
    );
}

/// Ranks in the core form: the winning rank decides; two disagreeing
/// defaults under a normal value are a warning, not an error (F DR-9).
#[test]
fn highest_rank_wins_and_a_shadowed_disagreement_warns() {
    let (r, violations) = run("want(net.vpc, \"main\")
             arg(net.vpc, \"main\", \"cidr\", \"10.0.0.0/16\", \"default\")
             arg(net.vpc, \"main\", \"cidr\", \"10.9.0.0/16\", \"default\")
             arg(net.vpc, \"main\", \"cidr\", \"10.1.0.0/16\")")
    .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    assert_eq!(
        facts_of(&r, "attr"),
        vec!["attr(\"net.vpc\", \"main\", \"cidr\", \"10.1.0.0/16\")".to_string()]
    );
    assert_eq!(r.warnings.len(), 1);
    assert!(
        r.warnings[0].starts_with("attr_shadowed"),
        "{:?}",
        r.warnings
    );
}

/// A null contribution (a computed attribute at plan time) is carried
/// through the aggregate as a value.
#[test]
fn a_null_contribution_is_carried_through_attr() {
    let null = Value::Null {
        label: "net.vpc/main#id".into(),
        class: crate::value::NullClass::Fresh,
        ty: "string".into(),
    };
    let program = crate::parser::parse_program(
        "want(\"net.subnet\", \"a\")
             arg(\"net.subnet\", \"a\", \"vpc_id\", v) where input(\"vpc\", v)
             seen(v) where arg(\"net.subnet\", \"a\", \"vpc_id\", v)",
    )
    .unwrap();
    let (r, violations) = eval(&program, &[input("vpc", null.clone())]).unwrap();
    assert!(violations.is_empty());
    assert_eq!(
        facts_of(&r, "seen"),
        vec![format!("seen({})", spell::value(&null))]
    );
}

/// DR-1 acceptance: shuffling statement order yields identical attr/4.
#[test]
fn statement_order_does_not_change_attr() {
    fn shuffle(stmts: &mut [Stmt], seed: &mut u64) {
        for i in (1..stmts.len()).rev() {
            *seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            stmts.swap(i, (*seed >> 33) as usize % (i + 1));
        }
        for s in stmts.iter_mut() {
            if let Stmt::Module(c) = s {
                shuffle(&mut c.body, seed);
            }
        }
    }
    let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let program =
        crate::loader::load_program(&[root.join("examples/demo/stacks/dform.df")]).unwrap();
    for env in ["staging", "prod"] {
        let extra = [input("env", Value::Str(env.into()))];
        // The settings document is a table: answered.
        let attrs = |p: &Program| {
            let (r, _) = eval_tables(p, &extra).unwrap();
            facts_of(&r, "attr")
        };
        let base = attrs(&program);
        assert!(base.len() > 40, "{}", base.len());
        let mut seed = 7u64;
        for _ in 0..8 {
            let mut p = program.clone();
            shuffle(&mut p.statements, &mut seed);
            assert_eq!(attrs(&p), base, "env={env}");
        }
    }
}

/// E §2.4 syntax: `@default` / `@override` after a value, after a
/// `resource` header for every leaf without its own, and after a
/// `set` block's (R-38).
#[test]
fn ranks_in_blocks() {
    let (r, violations) = run("input days: int = 1\n             input zones: list(string)\n             resource net.vpc main @default {\n               cidr = \"10.0.0.0/16\"\n               tags = { env: \"dev\", team: \"net\" }\n               public = true @override\n             }\n             resource net.vpc main {\n               cidr = \"10.1.0.0/16\"\n               tags = { team: \"platform\" }\n               public = false\n             }\n             set {\n               days = 3\n               zones = [\"a\"]\n             } @default where on(1)\n             set { days = 14 } where on(1)\n             on(1)\n             got(d, z) where d = days, z = zones")
        .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    assert_eq!(
        facts_of(&r, "attr")
            .into_iter()
            .filter(|a| a.contains("net.vpc"))
            .collect::<Vec<_>>(),
        vec![
            "attr(\"net.vpc\", \"main\", \"cidr\", \"10.1.0.0/16\")".to_string(),
            "attr(\"net.vpc\", \"main\", \"public\", true)".to_string(),
            "attr(\"net.vpc\", \"main\", \"tags\", {env: \"dev\", team: \"platform\"})".to_string(),
        ]
    );
    assert_eq!(facts_of(&r, "got"), vec!["got(14, [\"a\"])".to_string()]);
}

/// `eval`, with the tables the program reads (`crate::tables`):
/// dform.df's `set from` document. Any other extern has no answer, as
/// under `eval`.
fn eval_tables(program: &Program, extra: &[Atom]) -> Result<(EvalResult, Vec<String>)> {
    let lowered = crate::transform::lower(program)?;
    let tables = crate::tables::Tables::default();
    let externs = crate::externs::Externs::new(&lowered.program, &lowered.extern_fns, |f, ins| {
        tables.answer(f, ins).unwrap_or(Ok(Vec::new()))
    });
    externs.eval(program, extra)
}

/// dform.df's inputs are their defaults, a block for the modules', and
/// each environment's document, `set from yaml.decode(io.read("config/dform/${env}.yaml"))`
/// (R-38). Every environment compiles to exactly the resources the
/// `set` blocks the documents say would.
#[test]
fn dform_df_set_from_matches_the_blocks() {
    let root = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let src = std::fs::read_to_string(root.join("examples/demo/stacks/dform.df")).unwrap();
    let from = "set from yaml.decode(io.read(\"config/dform/${env}.yaml\"))\n";
    assert!(src.contains(from));
    let blocks = "set {
              cidrs.main = \"10.20.0.0/16\"
              cidrs.peer = \"10.21.0.0/16\"
              database.backup_days = 14
              database.multi_az = true
              kubernetes.private_api = true
              kubernetes.nodepool_min = 3
              kubernetes.nodepool_max = 10
              baseline.audit.sinks = [\"s3\", \"cloudwatch\"]
            } where env == \"prod\"
            set { cidrs.main = \"10.90.0.0/16\" } where env == \"dev\"
            ";
    let dir = std::env::temp_dir().join(format!("dform-settings-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("dform.df");
    std::fs::write(&old, src.replace(from, blocks)).unwrap();
    // The modules and components it names by path, beside it.
    for f in std::fs::read_dir(root.join("examples/demo")).unwrap() {
        let f = f.unwrap().path();
        if f.extension().is_some_and(|e| e == "df") {
            std::fs::copy(&f, dir.join(f.file_name().unwrap())).unwrap();
        }
    }
    let resources = |path: &std::path::Path, env: Option<&str>| {
        let program = crate::loader::load_program(&[path.to_path_buf()]).unwrap();
        let extra: Vec<Atom> = env
            .map(|e| input("env", Value::Str(e.into())))
            .into_iter()
            .collect();
        let (r, violations) = eval_tables(&program, &extra).unwrap();
        let docs: Vec<String> = crate::ir::compile_resources(
            r.facts.iter().cloned(),
            &crate::schema::Schema::default(),
        )
        .unwrap()
        .iter()
        .map(|r| format!("{} {} {}", r.addr.typ, r.addr.name, spell::value(&r.attrs)))
        .collect();
        (docs, violations, r.warnings)
    };
    for env in [None, Some("staging"), Some("prod"), Some("dev")] {
        let new = resources(&root.join("examples/demo/stacks/dform.df"), env);
        assert!(!new.0.is_empty());
        assert_eq!(new, resources(&old, env), "env={env:?}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A ranked Set is three shelves: a policy pack's `@default` set is
/// replaced wholesale by a normal one, and same-shelf sets union.
#[test]
fn a_default_set_is_replaced_not_unioned() {
    let (r, violations) = run("type_lattice(net.vpc, \"sgs\", \"set\")\n             resource net.vpc a { sgs = [\"base\"] }\n             resource net.vpc b { }\n             arg(t, n, \"sgs\", [\"default_sg\", \"ssh\"], \"default\") where want(t, n)\n             arg(t, n, \"sgs\", [\"audit\"]) where want(t, n), n = \"a\"")
        .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    assert_eq!(
        facts_of(&r, "attr"),
        vec![
            "attr(\"net.vpc\", \"a\", \"sgs\", [\"audit\", \"base\"])".to_string(),
            "attr(\"net.vpc\", \"b\", \"sgs\", [\"default_sg\", \"ssh\"])".to_string(),
        ]
    );
}

/// DESIGN.org "Unknown predicates are silently empty": a misspelled
/// predicate is a compile error naming it and the rule.
#[test]
fn undefined_predicate_is_an_error() {
    let err = run("env(\"prod\")\n             resource net.vpc main {\n               cidr = \"10.0.0.0/16\"\n             } where envv(\"prod\")")
        .unwrap_err()
        .to_string();
    assert!(err.contains("undefined predicate envv/1"), "{err}");
    // The relation it is nearest; said once though the resource
    // lowers to several rules that read it; no rule in core form.
    assert!(
        err.contains("help: `env` is a relation of the program; else define `envv`"),
        "{err}"
    );
    assert_eq!(err.matches("undefined predicate").count(), 1, "{err}");
    assert!(!err.contains(":-"), "{err}");
}

/// `decl p/N` declares a provider-fed predicate; provider-injected
/// predicates are defined with no rows.
#[test]
fn extern_and_provider_predicates_are_defined() {
    let (r, _) = run("decl allowed(a)\n             want(net.vpc, \"a\")\n             lonely(n) where want(net.vpc, n), not allowed(n), not cloud_exists(net.vpc, n)")
        .unwrap();
    assert_eq!(facts_of(&r, "lonely"), vec!["lonely(\"a\")".to_string()]);
}

/// DESIGN.org "Silent string-to-int coercion": arithmetic takes
/// integers; conversions are explicit builtins.
#[test]
fn coercion_is_explicit() {
    // A column of strings in arithmetic is a compile error (R-34).
    let err = run("s(\"10\")\nn(x) where s(s), x = s + 1")
        .unwrap_err()
        .to_string();
    assert!(err.contains("`s + 1`: `s` is string"), "{err}");
    let err = run("n(x) where x = int.trunc(\"abc\") + 1")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("`int.trunc`'s argument `f` is float, not the string \"abc\""),
        "{err}"
    );
    let (r, _) = run("let ten = \"10\"
             let n: int = ten
             explicit(x) where x = n + 1
             text(t) where t = \"${14}\"
             sizes(a, b, c) where xs = [\"x\", \"y\"], s = \"héllo\", o = {k: 1}, a = xs.len, b = s.len, c = o.len
             cases(l, u) where l = str.lower(\"AbC\"), u = str.upper(\"AbC\")
             parts(p) where p = str.split(\"a,b,c\", \",\")
             joined(j) where j = list.join([\"a\", 1, true], \"-\")")
        .unwrap();
    assert_eq!(facts_of(&r, "explicit"), vec!["explicit(11)".to_string()]);
    assert_eq!(facts_of(&r, "text"), vec!["text(\"14\")".to_string()]);
    assert_eq!(facts_of(&r, "sizes"), vec!["sizes(2, 5, 1)".to_string()]);
    assert_eq!(
        facts_of(&r, "cases"),
        vec!["cases(\"abc\", \"ABC\")".to_string()]
    );
    assert_eq!(
        facts_of(&r, "parts"),
        vec!["parts([\"a\", \"b\", \"c\"])".to_string()]
    );
    assert_eq!(
        facts_of(&r, "joined"),
        vec!["joined(\"a-1-true\")".to_string()]
    );
}

/// A builtin over a null is a content position (Rule 2): the instance
/// is recorded as stuck instead of deriving.
#[test]
fn a_builtin_over_a_null_is_stuck() {
    let (r, _) = run_with(
        "want(net.vpc, \"a\")
             id_len(n) where want(net.vpc, a), i = ref(net.vpc, a, \"id\"), n = i.len",
        &crate::schema::fake().facts,
    )
    .unwrap();
    assert!(r.facts.iter().all(|a| a.pred != "id_len"));
    assert!(
        r.stuck
            .iter()
            .any(|s| s.reason == "builtin `.len` over a null"),
        "{:?}",
        r.stuck
    );
}

/// DESIGN.org "Fixpoint iteration cap is arbitrary": a derivation chain
/// deeper than the old 200-iteration cap converges.
#[test]
fn a_300_deep_chain_converges() {
    let (r, _) = run("decl n(a) mixed\n             n(0)\n             n(y) where n(x), x < 300, y = x + 1\n             deepest(x) where n(x), x >= 300")
        .unwrap();
    assert_eq!(facts_of(&r, "n").len(), 301);
    assert_eq!(facts_of(&r, "deepest"), vec!["deepest(300)".to_string()]);
}

/// Dotted paths under one attribute meet in nested maps: two fields of
/// `spec.template` do not conflict on `template`.
#[test]
fn nested_dotted_paths_merge_recursively() {
    let (r, violations) = run("resource k8s.deployment web {
               spec.replicas = 3,
               spec.template.metadata.labels = {app: \"web\"},
               spec.template.spec.containers = [{name: \"web\"}]
             }")
    .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    assert_eq!(
            facts_of(&r, "attr"),
            vec![
                "attr(\"k8s.deployment\", \"web\", \"spec\", {replicas: 3, template: {metadata: {labels: {app: \"web\"}}, spec: {containers: [{name: \"web\"}]}}})"
                    .to_string()
            ]
        );
}

/// A record atom in a resource body is rewritten to positional form like
/// any other body, so the resource is derived (pngu.df's peerings).
#[test]
fn a_record_atom_in_a_resource_body_matches() {
    let (r, _) = run("decl peering(env: symbol, name: symbol)\n             peering( env: \"prod\", name: \"legacy\" )\n             resource net.peering \"${name}\" {\n               env = env\n             } where peering( env: env, name: name )")
        .unwrap();
    assert_eq!(
        facts_of(&r, "attr"),
        vec!["attr(\"net.peering\", \"legacy\", \"env\", \"prod\")".to_string()]
    );
}

fn run_with(src: &str, extra: &[Atom]) -> Result<(EvalResult, Vec<String>)> {
    let program = crate::parser::parse_program(src)?;
    eval(&program, extra)
}

fn schema_facts(src: &str) -> Vec<Atom> {
    crate::schema::Schema::parse(src, "test").unwrap().facts
}

/// E §2.5 / F14: one null per (want, computed path), at rank normal for
/// `computed` and `@default` for `optional_computed`; a program's value
/// for an Optional+Computed path wins, and a ref reads the collapsed cell.
#[test]
fn optional_computed_mints_a_default_null() {
    let schema = schema_facts(
        "type_provider(\"vm\", \"mock\")
             type_attr(\"vm\", \"id\", \"string\", [\"computed\", \"id\"])
             type_attr(\"vm\", \"zone\", \"string\", [\"optional_computed\"])",
    );
    let (r, violations) = run_with(
            "resource vm a { size = 1 }
             resource vm b { zone = \"z1\" }
             resource vm c { peer_zone = ref(\"vm\", \"a\", \"zone\"), other_zone = ref(\"vm\", \"b\", \"zone\"), a_id = ref(\"vm\", \"a\", \"id\") }",
            &schema,
        )
        .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    let attrs = facts_of(&r, "attr");
    for want in [
        "attr(\"vm\", \"a\", \"zone\", ?vm/a#zone:Open)",
        "attr(\"vm\", \"a\", \"id\", ?vm/a#id:Fresh)",
        "attr(\"vm\", \"b\", \"zone\", \"z1\")",
        "attr(\"vm\", \"c\", \"peer_zone\", ?vm/a#zone:Open)",
        "attr(\"vm\", \"c\", \"other_zone\", \"z1\")",
        "attr(\"vm\", \"c\", \"a_id\", ?vm/a#id:Fresh)",
    ] {
        assert!(
            attrs.contains(&want.to_string()),
            "{want} not in {attrs:#?}"
        );
    }
    // The user's value beat the @default null without a conflict or a
    // shadowed warning (the null is alone on its shelf).
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    let docs = crate::ir::compile_resources(
        r.facts.iter().cloned(),
        &crate::schema::Schema::from_facts(&schema).unwrap(),
    )
    .unwrap();
    // assemble drops computed paths and a zone the provider will pick.
    let doc = |n: &str| {
        let d = docs.iter().find(|d| d.addr.name == n).unwrap();
        spell::value(&d.attrs)
    };
    assert_eq!(doc("a"), "{size: 1}");
    assert_eq!(doc("b"), "{zone: \"z1\"}");
}

/// The mock Kubernetes schema: `metadata.name` is Optional+Computed, so
/// a Deployment without a name carries `?k8s.deployment/api#metadata.name`
/// and a Service that names one reads it.
#[test]
fn k8s_metadata_name_is_a_default_null_until_set() {
    let schema = crate::schema::load_provider("k8s").unwrap().facts;
    let (r, _) = run_with(
            "resource k8s.deployment api { metadata.namespace = \"shop\" }
             resource k8s.deployment web { metadata.name = \"web\" }
             resource k8s.service api { spec.selector.app = ref(k8s.deployment, \"api\", \"metadata.name\"),
                                        spec.selector.web = ref(k8s.deployment, \"web\", \"metadata.name\") }",
            &schema,
        )
        .unwrap();
    let attrs = facts_of(&r, "attr");
    let get = |addr: &str, path: &str| {
        attrs
            .iter()
            .find(|a| {
                a.starts_with(&format!(
                    "attr(\"{}\", \"{}\", \"{path}\"",
                    addr.split(' ').next().unwrap(),
                    addr.split(' ').nth(1).unwrap()
                ))
            })
            .cloned()
            .unwrap_or_default()
    };
    assert!(
        get("k8s.deployment api", "metadata")
            .contains("name: ?k8s.deployment/api#metadata.name:Fresh")
    );
    assert!(get("k8s.deployment web", "metadata").contains("name: \"web\""));
    let svc = get("k8s.service api", "spec");
    assert!(
        svc.contains("app: ?k8s.deployment/api#metadata.name:Fresh"),
        "{svc}"
    );
    assert!(svc.contains("web: \"web\""), "{svc}");
}

/// aws-mock's Optional+Computed attributes, the Terraform shape.
#[test]
fn aws_optional_computed_is_the_programs_when_set() {
    let schema = crate::schema::load_provider("aws-mock").unwrap().facts;
    let (r, violations) = run_with(
        "resource aws.vpc main { cidr_block = \"10.0.0.0/16\" }
             resource aws.security_group web { vpc_id = ref(\"aws.vpc\", \"main\", \"id\") }",
        &schema,
    )
    .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    let attrs = facts_of(&r, "attr");
    assert!(
        attrs.contains(&"attr(\"aws.vpc\", \"main\", \"cidr_block\", \"10.0.0.0/16\")".to_string()),
        "{attrs:#?}"
    );
    assert!(
        attrs.contains(
            &"attr(\"aws.security_group\", \"web\", \"name\", ?aws.security_group/web#name:Open)"
                .to_string()
        ),
        "{attrs:#?}"
    );
    assert!(
        attrs.contains(
            &"attr(\"aws.security_group\", \"web\", \"vpc_id\", ?aws.vpc/main#id:Fresh)"
                .to_string()
        ),
        "{attrs:#?}"
    );
}

/// A plain `computed` path is the provider's: writing it is a compile
/// error naming the resource and the path.
#[test]
fn writing_a_computed_path_is_an_error() {
    let schema = schema_facts(
        "type_provider(\"vm\", \"mock\")
             type_attr(\"vm\", \"id\", \"string\", [\"computed\", \"id\"])
             type_attr(\"vm\", \"meta.uid\", \"string\", [\"computed\", \"id\"])",
    );
    let err = run_with("resource vm a { id = \"x\" }", &schema)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("resource vm[\"a\"]: attribute id is computed"),
        "{err}"
    );
    let err = run_with("resource vm a { meta.uid = \"x\" }", &schema)
        .unwrap_err()
        .to_string();
    assert!(err.contains("attribute meta.uid is computed"), "{err}");
}

/// Round 0 (E Rule 4): a resource the world has resolves its computed
/// attributes through the identity mapping; a secret never does.
#[test]
fn round_zero_resolves_through_identity() {
    let mut extra = schema_facts(
        "type_provider(\"vm\", \"mock\")
             type_attr(\"vm\", \"id\", \"string\", [\"computed\", \"id\"])
             type_attr(\"vm\", \"pw\", \"string\", [\"computed\", \"sensitive\"])",
    );
    let s = |x: &str| Term::Val(Value::Str(x.into()));
    extra.push(Atom {
        pred: "identity".into(),
        args: vec![s("vm"), s("a"), s("remote-a")],
        record: None,
        span: Default::default(),
    });
    extra.push(Atom {
        pred: "world_attr".into(),
        args: vec![s("vm"), s("remote-a"), s("id"), s("vm-123")],
        record: None,
        span: Default::default(),
    });
    let (r, _) = run_with(
            "resource vm a { size = 1 }
             resource vm b { peer = ref(\"vm\", \"a\", \"id\"), secret = ref(\"vm\", \"a\", \"pw\") }",
            &extra,
        )
        .unwrap();
    let attrs = facts_of(&r, "attr");
    assert!(
        attrs.contains(&"attr(\"vm\", \"b\", \"peer\", \"vm-123\")".to_string()),
        "{attrs:#?}"
    );
    assert!(
        attrs.contains(&"attr(\"vm\", \"b\", \"secret\", ?vm/a#pw:Secret)".to_string()),
        "{attrs:#?}"
    );
    assert!(
        attrs.contains(&"attr(\"vm\", \"b\", \"id\", ?vm/b#id:Fresh)".to_string()),
        "{attrs:#?}"
    );
}

fn repo_file(rel: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../..")).join(rel)
}

/// Evaluate a repository program against a provider schema, and its
/// sections (E §2.7) on an empty world.
fn run_file(
    rel: &str,
    schema: &crate::schema::Schema,
    extra: &[Atom],
) -> (EvalResult, Vec<String>, stuck::Sections) {
    let program = crate::loader::load_program(&[repo_file(rel)]).unwrap();
    let mut extra = extra.to_vec();
    extra.extend(schema.facts.clone());
    let (r, violations) = eval_tables(&program, &extra).unwrap();
    let docs = crate::ir::compile_resources(r.facts.iter().cloned(), schema)
        .unwrap()
        .into_iter()
        .map(|d| ((d.addr.typ, d.addr.name), d.attrs))
        .collect();
    let sections = stuck::sections(&r.stuck, &r.may_derive, &r.facts, &docs, schema);
    (r, violations, sections)
}

/// Orchestrator regression (the mock-provider hand-back): `format` over a
/// ref to a computed attribute used to plan the constant
/// "ref(net.vpc,v,id)-x". It is a content position: the rule is stuck,
/// recorded as `stuck/4`, and derives nothing.
#[test]
fn format_over_a_computed_ref_is_stuck() {
    let (r, violations) = run_with(
        "resource net.vpc v { cidr = \"10.0.0.0/16\" }
             resource net.subnet s { name = str.format(\"%s-x\", ref(net.vpc, \"v\", \"id\")) }",
        &crate::schema::fake().facts,
    )
    .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    assert!(
        !facts_of(&r, "attr").iter().any(|a| a.contains("\"name\"")),
        "{:?}",
        facts_of(&r, "attr")
    );
    let stuck = facts_of(&r, "stuck");
    assert_eq!(stuck.len(), 1, "{stuck:?}");
    assert!(
        stuck[0].contains("arg(\\\"net.subnet\\\", \\\"s\\\", \\\"name\\\", _, \\\"normal\\\")")
            && stuck[0].contains("[\"net.vpc/v#id\"]"),
        "{stuck:?}"
    );
    assert_eq!(r.stuck[0].reason, "builtin str.format() over a null");
}

/// E §7.1 / F 4.1: every ref in dform.df is to a fresh `id` and is
/// forwarded, so nothing is stuck and all 13 resources are definite.
/// (Was sim.rs's dform_df_under_nulls_is_single_phase_...)
#[test]
fn dform_df_under_nulls_is_single_phase() {
    let (r, violations, s) = run_file(
        "examples/demo/stacks/dform.df",
        &crate::schema::fake(),
        &[input("env", Value::Str("prod".into()))],
    );
    assert!(violations.is_empty(), "{violations:?}");
    assert!(r.stuck.is_empty(), "{:?}", r.stuck);
    assert_eq!(facts_of(&r, "want").len(), 13);
    assert!(s.pending.is_empty() && s.pending_groups.is_empty() && s.undetermined.is_empty());
}

/// The other example programs under nulls: single-phase, nothing stuck.
#[test]
fn other_examples_have_no_stuck_instances() {
    let prod = [input("env", Value::Str("prod".into()))];
    let cases: [(&str, crate::schema::Schema, &[Atom]); 4] = [
        (
            "examples/advanced/stacks/dform-advanced.df",
            crate::schema::fake(),
            &prod,
        ),
        ("examples/pngu/stacks/pngu.df", crate::schema::gke(), &prod),
        (
            "examples/decl/stacks/decl_demo.df",
            crate::schema::fake(),
            &[],
        ),
        (
            "examples/adopt/stacks/adopt_demo.df",
            crate::schema::fake(),
            &prod,
        ),
    ];
    for (file, schema, extra) in cases {
        let (r, _, s) = run_file(file, &schema, extra);
        assert!(r.stuck.is_empty(), "{file}: {:?}", r.stuck);
        assert!(s.pending.is_empty(), "{file}: {:?}", s.pending);
    }
}

/// E §7.4 / F 4.4 per key: three definite, three pending on the
/// cluster's endpoint and ca (the kubernetes provider's configuration),
/// one pending group (a nodepool per zone), one undetermined policy. The
/// deletion_protection policy is decided (under coarse Rule 3 it was
/// spuriously undetermined). (Was sim.rs's
/// gke_two_phase_rule3_coarse_fires_spuriously.)
#[test]
fn gke_two_phase_sections_per_key() {
    // The k8s objects are the k8s mock's; the client's token the gke
    // mock's data source answers.
    let k8s = crate::schema::Schema::parse(crate::schema::builtin("k8s").unwrap(), "k8s").unwrap();
    let answers = crate::parser::parse_program(crate::schema::builtin_answers("gke").unwrap())
        .unwrap()
        .statements
        .into_iter()
        .filter_map(|s| match s {
            Stmt::Fact(a) => Some(a),
            _ => None,
        })
        .collect::<Vec<_>>();
    let (r, violations, s) = run_file(
        "examples/gke/stacks/gke_two_phase.df",
        &crate::schema::gke().merge(k8s).unwrap(),
        &answers,
    );
    assert!(violations.is_empty(), "{violations:?}");
    let pending: Vec<String> = s.pending.keys().map(|(t, a)| format!("{t} {a}")).collect();
    assert_eq!(
        pending,
        [
            "k8s.deployment api",
            "k8s.namespace pngu",
            "k8s.secret db_credentials"
        ]
    );
    for on in s.pending.values() {
        assert_eq!(
            on.iter().cloned().collect::<Vec<_>>(),
            [
                "google.container_cluster/pngu#ca_certificate",
                "google.container_cluster/pngu#endpoint"
            ]
        );
    }
    assert_eq!(facts_of(&r, "want").len(), 6);
    assert_eq!(s.pending_groups.len(), 1, "{:?}", s.pending_groups);
    assert!(
            s.pending_groups[0].starts_with(
                "want(\"google.container_node_pool\", _) x unknown, on ?google.container_cluster[\"pngu\"].zones"
            )
        );
    assert_eq!(s.undetermined.len(), 1, "{:?}", s.undetermined);
    assert!(s.undetermined[0].starts_with("deny \"cluster must be in at least two zones\""));
    assert!(
        !r.stuck.iter().any(|x| x.text.contains("deletion")),
        "{:?}",
        r.stuck
    );
}

/// F DR-2 revised, last clause, for a resource rule: a rule that
/// positively reads a helper with a stuck instance derives nothing yet
/// and may derive after the boundary; so may a reader of that reader.
/// Only the helper's instance the rule's key reads is named.
#[test]
fn a_resource_rule_reading_a_stuck_helper_may_derive() {
    let (r, violations) = run_with(
        r#"resource db.postgres a {}
               resource db.postgres b {}
               up(d) where attr(db.postgres, d, "endpoint", e), e != ""
               ready(v) where up(v)
               resource net.subnet s {
                 cidr = "10.0.1.0/24"
               } where up("a")
               resource net.subnet t {
                 cidr = "10.0.2.0/24"
               } where ready("b")"#,
        &crate::schema::fake().facts,
    )
    .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    assert!(
        facts_of(&r, "want")
            .iter()
            .all(|w| !w.contains("net.subnet"))
    );
    let may: Vec<String> = r
        .may_derive
        .iter()
        .map(|m| {
            format!(
                "{} {} ({})",
                spell::atom(&m.head),
                m.nulls_text(),
                m.reason()
            )
        })
        .collect();
    assert!(
            may.contains(&r#"want("net.subnet", "s") ?db.postgres["a"].endpoint (reads up("a"), which is stuck)"#.to_string()),
            "{may:#?}"
        );
    assert!(
            may.contains(&r#"want("net.subnet", "t") ?db.postgres["b"].endpoint (reads ready("b"), which may derive after a boundary)"#.to_string()),
            "{may:#?}"
        );
    assert!(
        !may.iter()
            .any(|m| m.contains(r#"db.postgres["b"]"#) && m.contains("\"s\"")),
        "{may:#?}"
    );
    let docs = crate::ir::compile_resources(r.facts.iter().cloned(), &crate::schema::fake())
        .unwrap()
        .into_iter()
        .map(|d| ((d.addr.typ, d.addr.name), d.attrs))
        .collect();
    let s = stuck::sections(
        &r.stuck,
        &r.may_derive,
        &r.facts,
        &docs,
        &crate::schema::fake(),
    );
    assert_eq!(s.pending_groups.len(), 2, "{:?}", s.pending_groups);
}

/// adv2: per-key Rule 3. An unrelated negation and an unrelated
/// aggregate over `want` are decided; a negation whose pattern unifies
/// with the stuck nodepool head is undetermined. (Was sim.rs's
/// adv2_rule3_coarse_vs_perkey.)
#[test]
fn adv2_rule3_per_key() {
    let (r, violations, s) = run_file(
        "tests/fixtures/adversarial/adv2_rule3_coarse.df",
        &crate::schema::gke(),
        &[],
    );
    // Decided, and it holds: there is no deployment.
    assert!(
        violations
            .iter()
            .any(|v| v.starts_with("namespace without deployment")),
        "{violations:?}"
    );
    // Decided, and it does not hold: ns_count is [pngu].
    assert_eq!(facts_of(&r, "ns_count"), ["ns_count([\"pngu\"])"]);
    assert!(
        !violations
            .iter()
            .any(|v| v.contains("need the pngu namespace"))
    );
    assert_eq!(s.undetermined.len(), 1, "{:?}", s.undetermined);
    assert!(
        s.undetermined[0].starts_with("deny \"no nodepool in zone b\""),
        "{:?}",
        s.undetermined
    );
}

fn why_leaves(r: &EvalResult, fact: &str) -> BTreeSet<Leaf> {
    let f = r
        .facts
        .iter()
        .find(|a| spell::atom(a) == fact)
        .unwrap_or_else(|| {
            let all: Vec<String> = r.facts.iter().map(spell::atom).collect();
            panic!("no fact {fact} in {all:?}")
        });
    r.circuit
        .why(&circuit_fact(f))
        .into_iter()
        .flatten()
        .collect()
}

/// DR-10: provenance is always on; every fact the evaluator returns has
/// a node in the circuit, and the circuit holds no other fact.
#[test]
fn every_fact_has_a_circuit_node() {
    let (r, _, _) = run_file(
        "examples/demo/stacks/dform.df",
        &crate::schema::fake(),
        &[input("env", Value::Str("prod".into()))],
    );
    for a in &r.facts {
        assert!(
            r.circuit.has(&circuit_fact(a)),
            "no node: {}",
            spell::atom(a)
        );
    }
    assert_eq!(r.circuit.facts().len(), r.facts.len());
}

#[test]
fn a_firing_records_its_rule_body_facts_and_negations() {
    let (r, _) = run("p(1)\np(2)\ns(2)\nq(x) where p(x), not s(x)").unwrap();
    let why = why_leaves(&r, "q(1)");
    assert!(why.contains(&Leaf::Base {
        span: "<input>:1:1 (p)".into()
    }));
    assert!(
        why.iter()
            .any(|l| matches!(l, Leaf::Absent { atom } if crate::spell::atom(atom) == "s(1)"))
    );
    let Some(Leaf::Rule { id }) = why.iter().find(|l| matches!(l, Leaf::Rule { .. })) else {
        panic!("no rule leaf: {why:?}");
    };
    assert_eq!(r.circuit.rule_text(id), Some("q(X) :- p(X), not s(X)"));
    assert_eq!(r.circuit.rule_at(id), Some("<input>:4:1"));
    let q = r
        .circuit
        .fact_id(&circuit_fact(
            &r.facts.iter().find(|a| a.pred == "q").unwrap().clone(),
        ))
        .unwrap();
    let crate::circuit::View::Fact { alts, .. } = r.circuit.view(q) else {
        panic!()
    };
    let crate::circuit::View::Times { bindings, .. } = r.circuit.view(alts[0]) else {
        panic!()
    };
    assert_eq!(bindings, &[("X".to_string(), Value::Int(1))]);
}

#[test]
fn an_attribute_carries_every_contribution() {
    let src = r#"
            want("t", "a")
            arg("t", "a", "tags", {x: 1}, "normal")
            arg("t", "a", "tags", {y: 2}, "normal") where want("t", "a")
        "#;
    let (r, _) = run(src).unwrap();
    let why = why_leaves(&r, r#"attr("t", "a", "tags", {x: 1, y: 2})"#);
    assert!(why.contains(&Leaf::Rule {
        id: ATTR_SIGMA.into()
    }));
    let bases = why
        .iter()
        .filter(|l| matches!(l, Leaf::Base { .. }))
        .count();
    assert_eq!(bases, 2, "the fact contribution and want: {why:?}");
}

#[test]
fn a_given_fact_is_an_input_leaf() {
    let (r, _) = run_with(
        "env(e) where input(\"env\", e)",
        &[input("env", Value::Str("prod".into()))],
    )
    .unwrap();
    assert!(why_leaves(&r, "env(\"prod\")").contains(&Leaf::Input {
        source: "--set env=prod".into()
    }));
}

/// gke_two_phase with a program appended, on the gke schema.
fn gke_with(extra: &str) -> Result<(EvalResult, Vec<String>)> {
    let mut program =
        crate::loader::load_program(&[repo_file("examples/gke/stacks/gke_two_phase.df")]).unwrap();
    program
        .statements
        .extend(crate::parser::parse_program(extra).unwrap().statements);
    eval(&program, &crate::schema::gke().facts)
}

/// stuck/4 is a relation a policy reads (to refuse a plan with any
/// stuck instance): it is derived above every rule that can stick, so the
/// reader sees every instance, those of the rules above it included
/// (the zone-count deny is stuck in the top stratum).
#[test]
fn a_policy_denies_on_any_stuck_instance() {
    let (r, violations) = gke_with(
        r#"deny "stuck" { rule: r, on: n } where stuck(r, _, _, n)
               seen(r, h) where stuck(r, h, _, _)"#,
    )
    .unwrap();
    assert!(!r.stuck.is_empty());
    assert!(
        violations.iter().any(|v| v.starts_with("stuck ctx=")),
        "{violations:?}"
    );
    let seen: BTreeSet<String> = facts_of(&r, "seen").into_iter().collect();
    let want: BTreeSet<String> = r
        .stuck
        .iter()
        .map(|s| {
            let f = s.fact();
            format!(
                "seen({}, {})",
                spell::term(&f.args[0]),
                spell::term(&f.args[1])
            )
        })
        .collect();
    assert_eq!(seen, want);
    assert!(
        r.stuck
            .iter()
            .any(|s| s.text.contains("at least two zones")),
        "{:?}",
        r.stuck
    );
}

/// A reader of stuck/4 whose head feeds a rule that can stick would
/// have to see its own consequences: a negative cycle, rejected.
#[test]
fn a_stuck_reader_on_a_cycle_is_an_error() {
    let err = gke_with(
        r#"flag(r) where stuck(r, _, _, _)
               deny "flagged" where flag(r), r > 3"#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("negative cycle through"), "{err}");
    assert!(err.contains("can stick (stuck/4)"), "{err}");
}

#[test]
fn a_rule_cannot_define_stuck() {
    let err = run("stuck(1, \"a\", \"b\", \"c\") where input(\"x\", \"y\")").unwrap_err();
    assert!(
        err.to_string()
            .contains("stuck/4 is derived by the evaluator")
    );
}

/// A resumed evaluation derives stuck/4 again when a constraint reads
/// its later facts: the constraint is checked after the strata, and its
/// instances are stuck/4's too.
#[test]
fn a_resumed_evaluation_counts_a_constraints_stuck_instance() {
    let program = crate::parser::parse_program(
        r#"decl later(a)
               strict(r) where stuck(r, _, _, _)
               deny "later is positive" where later(x), x > 0"#,
    )
    .unwrap();
    let (_, _, resumable) = eval_resumable(&program, &[], &["later"]).unwrap();
    let open = Value::Null {
        label: "t/a#n".into(),
        class: crate::value::NullClass::Open,
        ty: "int".into(),
    };
    let later = Atom {
        pred: "later".into(),
        args: vec![Term::Val(open)],
        record: None,
        span: Default::default(),
    };
    let (r, _) = resumable.with(&[later]).unwrap();
    assert_eq!(r.stuck.len(), 1, "{:?}", r.stuck);
    assert_eq!(facts_of(&r, "strict").len(), 1);
}

/// The policy pass evaluates again only the rules that read its facts
/// and those that read what they derive (After R-123): the others ran
/// once, and the result is one evaluation's with every fact given.
#[test]
fn a_resumed_evaluation_runs_only_the_readers_of_its_facts_again() {
    let program = crate::parser::parse_program(
        r#"decl later(a)
               decl base(a)
               decl quiet(a)
               decl loud(a)
               decl loudest(a)
               base(x) where x in [1, 2, 3]
               quiet(x) where base(x), not later(x)
               loud(x) where later(x), base(x)
               loudest(x) where loud(x), x > 1
               deny "quiet three" where quiet(3)"#,
    )
    .unwrap();
    let (first, violations, resumable) = eval_resumable(&program, &[], &["later"]).unwrap();
    assert_eq!(facts_of(&first, "quiet").len(), 3);
    assert_eq!(violations.len(), 1);
    let again: Vec<String> = resumable
        .again
        .as_ref()
        .expect("split")
        .iter()
        .zip(resumable.c.rules.iter())
        .filter(|(a, _)| **a)
        .map(|(_, r)| r.head.pred.clone())
        .collect();
    assert_eq!(again, ["quiet", "loud", "loudest", "deny"]);
    let later = |n: i64| Atom {
        pred: "later".into(),
        args: vec![Term::Val(Value::Int(n))],
        record: None,
        span: Default::default(),
    };
    let more = [later(2), later(3)];
    let (r, violations) = resumable.with(&more).unwrap();
    let (whole, whole_violations) = eval(&program, &more).unwrap();
    assert_eq!(r.facts, whole.facts);
    assert_eq!(violations, whole_violations);
    assert!(violations.is_empty());
    assert_eq!(facts_of(&r, "loudest").len(), 2);
}

/// The operator table (`functions::OPERATORS`, docs/grammar.md) is
/// what the engine implements (R-155): each operator is over the types
/// it lists and no other, a sample of each type tried.
#[test]
fn the_operators_are_over_the_types_the_table_lists() {
    use crate::functions::{OPERATORS, body};
    let read = |ty: &str, s: &str| crate::value::read_typed(ty, &Value::Str(s.into())).unwrap();
    let samples: Vec<(&str, Value, Value)> = vec![
        ("int", Value::Int(7), Value::Int(2)),
        (
            "float",
            Value::Float(crate::value::Float::new(2.5).unwrap()),
            Value::Float(crate::value::Float::new(0.5).unwrap()),
        ),
        ("bool", Value::Bool(true), Value::Bool(false)),
        ("string", Value::Str("abc".into()), Value::Str("b".into())),
        ("bytes", read("bytes", "1Gi"), read("bytes", "512Mi")),
        ("cpu", read("cpu", "500m"), read("cpu", "250m")),
        ("duration", read("duration", "1h"), read("duration", "30m")),
        (
            "time",
            read("time", "2026-10-02T09:00:00Z"),
            read("time", "2026-10-03T09:00:00Z"),
        ),
        ("semver", read("semver", "1.2.3"), read("semver", "2.0.0")),
        ("ip", read("ip", "10.0.0.1"), read("ip", "10.0.0.2")),
        (
            "inet",
            read("inet", "10.0.0.0/8"),
            read("inet", "10.1.0.0/16"),
        ),
        (
            "range",
            read("range(ip)", "10.0.0.1..=10.0.0.9"),
            read("range(ip)", "10.0.0.2..=10.0.0.3"),
        ),
        (
            "uri",
            read("uri", "https://example.com/a"),
            read("uri", "https://example.com/b"),
        ),
        (
            "oci",
            read("oci", "ghcr.io/o/app:1"),
            read("oci", "ghcr.io/o/app:2"),
        ),
        (
            "list",
            Value::List(vec![Value::Int(1)]),
            Value::List(vec![Value::Int(2)]),
        ),
        (
            "object",
            Value::Obj([("a".to_string(), Value::Int(1))].into()),
            Value::Obj(Default::default()),
        ),
    ];
    let call = |f: &str, a: &[Value]| body(f).and_then(|b| b(a));
    let int = Value::Int(2);
    let duration = read("duration", "1h");
    for (op, types) in OPERATORS {
        for (ty, a, b) in &samples {
            let has = match *op {
                "+ -" => {
                    let b = if *ty == "time" { &duration } else { b };
                    call("add", &[a.clone(), b.clone()]).is_some()
                        && call("sub", &[a.clone(), b.clone()]).is_some()
                }
                "* /" => {
                    let by = if matches!(a, Value::Quantity(_)) {
                        &int
                    } else {
                        b
                    };
                    call("mul", &[a.clone(), by.clone()]).is_some()
                        && call("div", &[a.clone(), by.clone()]).is_some()
                }
                "%" => call("mod", &[a.clone(), b.clone()]).is_some(),
                "< <= > >=" => order(a, b).is_ok(),
                "in" => match a {
                    // A list's `in` is the membership a body enumerates.
                    Value::List(_) => {
                        facts_of(&run("p() where 1 in [1]").unwrap().0, "p") == ["p()"]
                    }
                    Value::Str(_) => holds(a, b).is_ok(),
                    _ => holds(a, &read("ip", "10.0.0.2")).is_ok(),
                },
                "${..}" => call(
                    crate::address::FORMAT,
                    &[Value::Str("%s".into()), a.clone()],
                )
                .is_some(),
                _ => unreachable!("{op}"),
            };
            assert_eq!(has, types.contains(ty), "`{op}` over {ty}");
        }
    }
}

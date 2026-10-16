//! The resolver's unit tests: what a statement lowers to, and the
//! errors it says.

use crate::ast::{Lit, Stmt, Term};
use crate::spell;

#[test]
fn a_core_variable_reads_as_the_source_name() {
    use super::source_name;
    assert_eq!(source_name("AvailabilityZone"), "availability_zone");
    assert_eq!(source_name("Z"), "z");
    assert_eq!(source_name("__v1"), "__v1");
}

fn lits(ls: &[Lit]) -> String {
    ls.iter().map(spell::lit).collect::<Vec<_>>().join(", ")
}

/// Every statement as the core prints it, one per line.
fn show(stmts: &[Stmt]) -> Vec<String> {
    stmts
        .iter()
        .filter_map(|s| {
            Some(match s {
                Stmt::Rule(r) => spell::rule(r),
                Stmt::Fact(a) => spell::atom(a),
                Stmt::Resource(r) => {
                    let fields = r
                        .fields
                        .iter()
                        .map(|f| format!("{} = {}", f.key, spell::term(&f.value)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let body = r.body.as_deref().map(lits).unwrap_or_default();
                    format!(
                        "resource {} {} {{ {fields} }} :- {body}",
                        spell::term(&r.typ),
                        spell::term(&r.name)
                    )
                }
                Stmt::Instance(i) => {
                    let inputs = i
                        .inputs
                        .iter()
                        .map(|(k, v, _)| format!("{k} = {}", spell::term(v)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let body = i.body.as_deref().map(lits).unwrap_or_default();
                    format!("instance {} {} {{ {inputs} }} :- {body}", i.module, i.name)
                }
                Stmt::Output(o) => {
                    format!(
                        "output {} = {:?}",
                        o.name,
                        o.value.as_ref().map(spell::term)
                    )
                }
                Stmt::Module(m) => {
                    format!("module {} {{ {} }}", m.name, show(&m.body).join("; "))
                }
                _ => return None,
            })
        })
        .collect()
}

/// `src` lowered as text that is not a file (the core relations
/// writable), or, `file`, as a program file.
fn parse_as(src: &str, file: bool) -> anyhow::Result<crate::ast::Program> {
    let file_id = crate::diag::add_source("t.df", src);
    let parse = crate::syntax::parser::parse(src);
    assert!(parse.errors.is_empty(), "{:?}", parse.errors);
    let units = [super::Unit {
        file: file_id,
        root: parse.syntax(),
        path: None,
        project: false,
    }];
    super::lower(&units, &[0], file, super::Mode::Program)
        .map_err(|d| crate::diag::Diagnostics(d).into())
}

fn parse(src: &str) -> anyhow::Result<crate::ast::Program> {
    parse_as(src, false)
}

/// The scope tree built alone is the tree the lowering builds: a
/// stack's file, a module's file, a component in each, their names.
#[test]
fn the_scopes_alone_are_the_lowerings() {
    let unit = |name: &str, src: &str, path: Option<&str>| {
        let parse = crate::syntax::parser::parse(src);
        assert!(parse.errors.is_empty(), "{:?}", parse.errors);
        super::Unit {
            file: crate::diag::add_source(name, src),
            root: parse.syntax(),
            path: path.map(str::to_string),
            project: false,
        }
    };
    let units = [
        unit(
            "main.df",
            "\ninput n: int = 1\nlet k = n\nuse lan {}\n\
             component box { input size: int\nlet twice = size * 2 }\n\
             resource box b { size = k }\n",
            None,
        ),
        unit(
            "lan.df",
            "\ninput cidr: string = \"10.0.0.0/8\"\n\
             component sub { input c: string\nlet d = c }\n",
            Some("lan"),
        ),
    ];
    let lowered = super::lower_stack(&units, &[0], true, super::Mode::Program, None, &[])
        .unwrap_or_else(|d| panic!("{d:?}"));
    let nodes = lowered.nodes.expect("the program's nodes");
    let alone = super::scopes(&units, &[0], &[]);
    assert_eq!(format!("{alone:?}"), format!("{:?}", nodes.scopes));
    assert!(alone.ids().count() >= 4, "{alone:?}");
}

fn lower(src: &str) -> Vec<String> {
    match parse(src) {
        Ok(p) => show(&p.statements),
        Err(e) => panic!("{e:#}"),
    }
}

fn error(src: &str) -> String {
    match parse(src) {
        Ok(p) => panic!("lowered: {:?}", show(&p.statements)),
        Err(e) => format!("{e:#}"),
    }
}

/// The errors of `src` as a program file.
fn file_error(src: &str) -> String {
    match parse_as(src, true) {
        Ok(p) => panic!("lowered: {:?}", show(&p.statements)),
        Err(e) => format!("{e:#}"),
    }
}

/// `let NAME = LITERAL [@rank]` is built as a `let` item (R-211): it
/// lowers to its `let` fact, its errors the resolver's.
#[test]
fn a_literal_let_is_built_as_the_resolver_lowers_it() {
    let lowered =
        lower("let a = 1\nlet b = \"x\" @default\nlet c = 500m\nlet d = true\np(v) where v = a");
    assert!(
        lowered.contains(&"let(\"b\", \"x\", \"default\")".to_string()),
        "{lowered:?}"
    );
    for (src, msg) in [
        ("let a = 99999999999999999999", "integer out of range"),
        ("let a = 1 @later", "unknown rank `@later`"),
    ] {
        let e = error(src);
        assert!(e.contains(msg), "{src}: {e}");
    }
}

/// Every form of `let` is built as a `let` item (R-211 step 5) and
/// lowers to its rules: typed (its `decl`), secret (its
/// `__secret_let` paths), reading, with a clause, a `not { }` in it, a
/// comprehension's helper in its value, an aggregate folded by its
/// head or through a helper.
#[test]
fn every_let_is_built_as_a_let_item() {
    let src = "p(1)\nq(2)\np(2)\n\
         let a: int = 1\n\
         let b = x + 1 where p(x), x > 1\n\
         let c = a * 2\n\
         let s: secret(string) = \"pw\"\n\
         let o: { host: string, password: secret(string) } = { host = \"h\", password = \"p\" }\n\
         let r = x where p(x), not { q(x) }\n\
         let l = [y | p(y), not { q(y) }]\n\
         let n = count(x) where p(x)\n\
         let m = k where k = count(x), p(x), k > 1\n";
    let lowered = lower(src);
    for s in [
        "let(\"n\", count(X), \"normal\") :- p(X)",
        "__secret_let(\"o\", \"password\")",
        "__agg_0(count(X)) :- p(X)",
    ] {
        assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
    }
}

/// Every form of rule and fact is built as a `rule` item (R-211 step
/// 5) and lowers to its rules: a fact, a record fact, a
/// ranked write, a head that reads (its reads after the clause), a
/// helper its head's terms make, a constraint read as data, an
/// aggregate folded by the head or through helpers.
#[test]
fn every_rule_is_built_as_a_rule_item() {
    let src = "decl zone(name, index)\n\
         p(1)\n\
         p(2)\n\
         zone(name: \"a\", index: 1)\n\
         arg(\"net.vpc\", \"a\", \"cidr\", \"x\") @override\n\
         let o = { a = 1 }\n\
         r(o.a, x) where p(x)\n\
         l([y | p(y), not { q(y) }]) where p(1)\n\
         q(2)\n\
         type_refine(\"t\", regex(\"^a$\"))\n\
         c(n) where n = count(x), p(x)\n\
         g(n) where n = count(x), p(x), n > 1\n";
    let lowered = lower(src);
    for s in [
        "arg(\"net.vpc\", \"a\", \"cidr\", \"x\", \"override\")",
        "r(__path(O, \"a\"), X) :- p(X), o(O)",
        "__neg_0(Y) :- p(Y), q(Y)",
        "c(count(X)) :- p(X)",
        "g(N) :- __agg_0(N), N > 1",
    ] {
        assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
    }
}

/// Every form of check, output and input is built as an item (R-211
/// step 5) and lowers to its rules: a deny with a detail
/// and an aggregate, a warn, an output declared at its first row with a
/// value or a rule, an object output, an exported relation, an input
/// with a default, a refinement, a clause and object fields, one
/// declared under two clauses (R-104).
#[test]
fn every_check_output_and_input_is_an_item() {
    let src = "input size: int = 3 check size > 0\n\
         input region: string = \"eu\" where cloud(\"aws\")\n\
         input region: string = \"us\" where cloud(\"gcp\")\n\
         input db { host: string = \"h\", port: int = 5432 check port > 0 }\n\
         p(1)\np(2)\ncloud(\"aws\")\n\
         deny \"too many: ${n}\" { n } where n = count(x), p(x), n > 1\n\
         warn \"p ${x}\" where p(x), not { cloud(\"gcp\") }\n\
         output total = 3\n\
         output each = x where p(x)\n\
         output each = 9\n\
         output obj { a = 1, b: { c = \"d\" } }\n\
         output p\n";
    let lowered = parse_as(src, true);
    let lowered = lowered
        .map(|p| show(&p.statements))
        .unwrap_or_else(|e| panic!("{e:#}"));
    assert!(
        lowered.iter().any(|l| l.contains("__agg_0")),
        "{lowered:#?}"
    );
}

/// Every form of `set` is built as a `Set` item (R-211 step 5) and
/// lowers to its rules: an attribute, `+=`, ranked, a
/// `[_]` target (its reads before the value's), an element of a keyed
/// list by its variable (R-69) and by its key (R-35), an input, a
/// block whose lines share one clause (its aggregates folded per line,
/// each through helpers of its own), `set from` a document.
#[test]
fn every_set_is_built_as_a_set_item() {
    let src = "input size: int = 1\n\
         resource db.postgres pg { public = false }\n\
         let k = 1\n\
         p(1)\np(2)\n\
         set pg.tags = { a = 1 } where p(1)\n\
         set pg.tags += { b = 2 } where p(2)\n\
         set pg.public = true @override where p(2)\n\
         set db.postgres[_].public = true where p(1)\n\
         set db.postgres[_].tags = { a = k } where p(1)\n\
         set c.owner = \"o\" where c in pg.databases\n\
         set pg.databases[\"a\"].owner = \"o\" where p(1)\n\
         set size = 2 where p(2)\n\
         set { pg.public = false @default, pg.tags = { c = n } } where n = count(x), p(x), n > 1\n\
         set from yaml.decode(io.read(\"c.yaml\")) where p(1)\n";
    let lowered = parse_as(src, true);
    let lowered = lowered
        .map(|p| show(&p.statements))
        .unwrap_or_else(|e| panic!("{e:#}"));
    for s in [
        "arg_add(\"db.postgres\", \"pg\", \"tags\", {b = 2}) :- p(2)",
        "\"override\") :- p(2)",
        "__agg_0(count(X))",
        "__agg_1(count(X))",
        "arg(\"input\", \"\", Path, Value, \"normal\")",
    ] {
        assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
    }
}

/// Every form of resource block is built as a `Resource` item (R-211
/// step 5) and lowers to its rules: entries that read
/// (`+=`, ranked, an indexed path, a comprehension's helper), a
/// bare, a quoted and a dotted header, a name from the clause bound
/// after the entries' reads, a value body written out and one read.
#[test]
fn every_resource_is_built_as_a_resource_item() {
    let src = "let o = { public = true }\n\
         let k = 1\n\
         p(1)\np(2)\nq(2)\n\
         resource db.postgres a @default { public = false, tags += { a = k } @override }\n\
         resource db.postgres \"b.c\" { tags = { l = [x | p(x), not { q(x) }] } }\n\
         resource db.postgres \"d-${n}\" { tags = { n = k } } where p(n)\n\
         resource db.postgres e = { public = true }\n\
         resource db.postgres f = o\n\
         resource db.postgres g { databases[0].owner = \"o\" }\n";
    let lowered = lower(src);
    for s in [
        "resource \"db.postgres\" Addr { tags = {n = K} } :- p(N), k(K), Addr = __segment(str.format(\"d-%s\", N))",
        "resource \"db.postgres\" \"\\\"b.c\\\"\"",
        "__neg_0(X) :- p(X), q(X)",
        "resource \"db.postgres\" \"f\" {  = O } :- o(O)",
    ] {
        assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
    }
    // A block binds no aggregate: a `let` does.
    let e = error("p(1)\nresource db.postgres h { tags = { n = n } } where n = count(x), p(x)\n");
    assert!(
        e.contains("an aggregate is bound in the body of a rule"),
        "{e}"
    );
}

/// Every form of relation input is built as a `RelationInput` item
/// (R-211 step 5) and lowers to its rules: a stack's
/// rows from a document under a clause, a component's relation its
/// user gives.
#[test]
fn every_relation_input_is_an_item() {
    let src = "input p from csv.decode(io.read(\"p.csv\")) where z(1)\n\
         decl p(a: int, b: string)\n\
         z(1)\n\
         component c {\n  input q\n  decl q(a: int)\n}\n";
    let lowered = parse_as(src, true);
    let lowered = lowered
        .map(|p| show(&p.statements))
        .unwrap_or_else(|e| panic!("{e:#}"));
    let s = "p(A, B) :- z(1), Path = \"p.csv\", table.csv.p(Path, At, A, B)";
    assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
}

/// Every form of `let` with parameters is built as a `LetFn` item
/// (R-211 step 5) and lowers to its rules: no clause, a
/// clause whose `not { }` takes the demand among its positive
/// literals, a value that reads, an aggregate folded by the head.
#[test]
fn every_let_with_parameters_is_an_item() {
    let src = "p(1, 2)\nq(2)\nlet k = 3\n\
         let f(a) = a + k\n\
         let g(a, b) = x where p(a, x), not { q(b) }\n\
         let h(a) = n where n = count(x), p(x, a)\n\
         r(f(1), g(1, 2), h(2))\n";
    let lowered = lower(src);
    for s in [
        "f(A, add(A, K)) :- f?(A), k(K)",
        "__neg_0(B) :- g?(A, B), p(A, X), q(B)",
        "h(A, count(X)) :- h?(A), p(X, A)",
    ] {
        assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
    }
}

/// Every form of a provider's `use` is built as a `Provider` item
/// (R-211 step 5) and lowers to its rules: its source and
/// settings, renamed (R-115), an `expect_account` under the clause
/// again (its `not { }` a helper of its own), declared twice under
/// clauses (R-104).
#[test]
fn every_provider_use_is_an_item() {
    let src = "let k = \"eu\"\nz(1)\nq(2)\n\
         use p1 { source = \"./p1\", region = k, expect_account = \"a\" } where z(1), not { q(1) }\n\
         use p2 { source = \"./p2\", r = 1 } where z(1)\n\
         use p2 { r = 2 } where q(2)\n\
         use p3 as c { source = \"./p3\" }\n";
    let lowered = parse_as(src, true);
    let lowered = lowered
        .map(|p| show(&p.statements))
        .unwrap_or_else(|e| panic!("{e:#}"));
    for s in [
        "provider_config(\"p1\", {region = K}) :- z(1), not __neg_0(), k(K)",
        "provider_expect_account(\"p1\", \"a\") :- z(1), not __neg_1()",
        "__declared(\"use p2\", 1) :- q(2)",
    ] {
        assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
    }
}

/// A row a copy's block gives its component's relation input folds
/// its aggregate as a rule does: the head applies it, or a helper
/// does (it was left in the body as `N = count(X)`, a call of no
/// function), a table's rows too.
#[test]
fn a_row_of_a_copy_folds_its_aggregate() {
    let src = "z(1)\nz(2)\n\
         component c {\n  input p\n  input t\n  decl p(a: int)\n  decl t(a: int)\n  q(x) where p(x), t(x)\n}\n\
         resource c one {\n  p(n) where n = count(x), z(x)\n\
           t from csv.decode(io.read(\"t.csv\")) where n = count(x), z(x), n > 1\n}\n";
    let lowered = parse_as(src, true).unwrap_or_else(|e| panic!("{e:#}"));
    let rows: Vec<String> = lowered
        .statements
        .iter()
        .filter_map(|s| match s {
            Stmt::Instance(i) => Some(show(&i.rows)),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        rows,
        [
            "p(count(X)) :- z(X)",
            "t(A) :- __agg_0(A, N), N > 1",
            "__agg_0(A, count(X)) :- z(X), Path = \"t.csv\", table.csv.t(Path, At, A)",
        ]
    );
}

/// An aggregate is bound in the body of a rule, a check or a `let`
/// (R-59): an input's clause, which every read of the input repeats,
/// and a provider's `use` bind none, as a resource's and a copy's do
/// not; the help says where it is bound instead.
#[test]
fn an_inputs_and_a_providers_clause_bind_no_aggregate() {
    let want = "an aggregate is bound in the body of a rule, a check or a `let`; bind it in \
                a `let` and read that here";
    for src in [
        "input region: string = \"eu\" where n = count(x), z(x), n > 1\nz(1)\n",
        "input region: string = \"eu\" where n = count(x), z(x), n > 1\n\
         input region: string = \"us\" where n = count(x), z(x), n <= 1\nz(1)\n",
        "z(1)\nuse fake where n = count(x), z(x), n > 1\n",
    ] {
        let e = file_error(src);
        assert!(e.contains(want), "{src}: {e}");
    }
    // Bound in a `let` and read there, the clause holds where it does.
    let ok = "input region: string = \"eu\" where many\n\
              let many = true where n = count(x), z(x), n > 1\nz(1)\n";
    parse_as(ok, true).unwrap_or_else(|e| panic!("{e:#}"));
}

/// Every form of copy is built as a `Copy` item (R-211 step 5) and
/// lowers to its rules: inputs that read, a name from
/// the clause (R-191) bound before the inputs' reads, rows its block
/// gives (a rule whose `not { }` helper follows the copy, a table),
/// under a clause.
#[test]
fn every_copy_is_an_item() {
    let src = "let k = 1\nz(1)\nz(2)\nq(3)\n\
         component c {\n  input size: int\n  input p\n  input t\n  decl p(a: int)\n  decl t(a: int)\n  r(x) where p(x), t(x)\n}\n\
         resource c one {\n  size = k\n  p(x) where z(x), not { q(x) }\n\
           t from csv.decode(io.read(\"t.csv\"))\n}\n\
         resource c \"n-${i}\" { size = i } where z(i)\n";
    let lowered = parse_as(src, true);
    let lowered = lowered
        .map(|p| show(&p.statements))
        .unwrap_or_else(|e| panic!("{e:#}"));
    for s in [
        "instance c one { size = K } :- k(K)",
        "__neg_0(X) :- z(X), q(X)",
        "instance c n-${i} { size = I } :- z(I), Addr = __segment(str.format(\"n-%s\", I))",
    ] {
        assert!(lowered.iter().any(|l| l.contains(s)), "{s}: {lowered:#?}");
    }
}

#[test]
fn value_names_are_read_by_name() {
    let got = lower(
        "input env: enum(\"a\", \"b\") = \"a\"\n\
         p(x) where q(x), env == \"a\"\n\
         r(env) where q(_)\n\
         s(x) where q(x), not env, has env\n\
         let serving = \"blue\" where q(1)\n\
         t(serving)\n",
    );
    assert_eq!(
        &got[..],
        [
            "p(X) :- q(X), env(\"a\")",
            "r(Env) :- q(_), env(Env)",
            "s(X) :- q(X), not env(true), env(_)",
            "let(\"serving\", \"blue\", \"normal\") :- q(1)",
            "t(Serving) :- serving(Serving)",
        ]
    );
}

/// The worked example of the proposal (section 5.6): the block's reads
/// follow its clause, the interpolated name comes last.
#[test]
fn a_resource_block_lowers_to_its_shared_body() {
    let got = lower(
        "resource net.vpc vpc { cidr = \"10.0.0.0/16\" }\n\
         zone_index(\"a\", 0)\n\
         resource net.subnet \"private-${z}\" {\n\
           vpc        = vpc\n\
           cidr       = inet.subnet(vpc.cidr, 4, zone_index[z])\n\
           zone       = z\n\
           visibility = \"private\"\n\
         } where data(\"zone\", z)\n",
    );
    assert_eq!(
        got[2],
        "resource \"net.subnet\" Addr { vpc = __ref(\"net.vpc\", \"vpc\", \"\"), \
         cidr = inet.subnet(Cidr, 4, ZoneIndex), zone = Z, visibility = \"private\" } :- \
         data(\"zone\", Z), attr(\"net.vpc\", \"vpc\", \"cidr\", Cidr), \
         zone_index(Z, ZoneIndex), Addr = __segment(str.format(\"private-%s\", Z))"
    );
}

/// An entry that is only a name is its pun (R-33), resolved as the
/// value would be: a clause variable, a `let` (read through its cell),
/// with a rank.
#[test]
fn a_bare_entry_is_its_name() {
    let got = lower(
        "let tags = { team = \"x\" }\n\
         resource net.vpc vpc { cidr = \"10.0.0.0/16\" }\n\
         resource net.subnet \"s-${zone}\" {\n\
           zone\n\
           meta.zone = zone\n\
           tags @default\n\
         } where data(\"zone\", zone)\n",
    );
    assert_eq!(
        got[got.len() - 1],
        "resource \"net.subnet\" Addr { zone = Zone, meta.zone = Zone, tags = Tags } \
         :- data(\"zone\", Zone), tags(Tags), Addr = __segment(str.format(\"s-%s\", Zone))"
    );
    let ranked = parse("let tags = {}\nresource net.vpc v {\n  tags @default\n}\n")
        .unwrap()
        .statements
        .into_iter()
        .any(|s| matches!(s, Stmt::Resource(r) if r.fields.iter().any(|f| f.rank.is_some())));
    assert!(ranked);
    let e = error("resource net.vpc v {\n  a[0]\n}\n");
    assert!(e.contains("`a[0]` names no value"), "{e}");
}

#[test]
fn a_dot_is_a_reference_in_a_field_and_a_read_elsewhere() {
    let got = lower(
        "resource k8s.namespace web { name = \"web\" }\n\
         resource k8s.deployment a { namespace = web.name }\n\
         resource k8s.deployment b {\n  namespace = ns\n} where ns = web.name\n\
         p(web.name, x) where x = web.name\n",
    );
    assert_eq!(
        &got[1..],
        [
            "resource \"k8s.deployment\" \"a\" { namespace = __ref(\"k8s.namespace\", \"web\", \"name\") } :- ",
            "resource \"k8s.deployment\" \"b\" { namespace = Ns } :- attr(\"k8s.namespace\", \"web\", \"name\", Ns)",
            "p(__ref(\"k8s.namespace\", \"web\", \"name\"), X) :- attr(\"k8s.namespace\", \"web\", \"name\", X)",
        ]
    );
}

/// H-6: a `let` holding a reference is a value, its key, and a dot on
/// it reads through the reference.
#[test]
fn a_let_holding_a_reference_reads_through_it() {
    let got = lower(
        "input env: string = \"dev\"\n\
         resource db.pg other { size = 1 }\n\
         let pg = db.pg[\"main\"]\n\
         resource net.vpc v { cidr = pg.net.cidr, name = \"${pg.name}-vpc\" }\n\
         deny \"x\" where pg.size > 3\n",
    );
    assert_eq!(
        &got[1..],
        [
            "let(\"pg\", \"main\", \"normal\")",
            "resource \"net.vpc\" \"v\" { cidr = __ref(\"db.pg\", Pg, \"net.cidr\"), name = \
             str.format(\"%s-vpc\", Name) } :- pg(Pg), attr(\"db.pg\", Pg, \"name\", Name)",
            "deny(\"x\") :- pg(Pg), attr(\"db.pg\", Pg, \"size\", Size), Size > 3",
        ]
    );
    let e = error(
        "resource db.pg a {}\nlet x = db.pg[\"a\"]\nlet x = 1 where q(1)\np(x.y) where q(1)\n",
    );
    assert!(
        e.contains("`let x` is a db.pg reference in one row and a value in another"),
        "{e}"
    );
}

#[test]
fn a_namespace_no_builtin_schema_knows_is_a_providers() {
    let got = lower(
        "resource acme.gadget g {}\n\
         p(x) where x in acme.widget, acme.widget[x].size > 1\n",
    );
    assert_eq!(
        got.last().unwrap(),
        // `p`'s column holds the widget itself (R-185).
        "p(__ref(\"acme.widget\", X, \"\")) :- want(\"acme.widget\", X), attr(\"acme.widget\", \
         X, \"size\", Size), Size > 1"
    );
}

#[test]
fn a_resource_named_settings_is_read_by_its_name() {
    let got = lower(
        "resource net.vpc settings { name = \"s\" }\n\
         p(n) where n = settings.name\n",
    );
    assert_eq!(
        got.last().unwrap(),
        "p(N) :- attr(\"net.vpc\", \"settings\", \"name\", N)"
    );
}

#[test]
fn the_type_after_in_picks_among_resources_of_one_name() {
    let got = lower(
        "resource db.postgres main {}\n\
         resource net.vpc main {}\n\
         ok(1) where main in db.postgres\n",
    );
    assert_eq!(
        got.last().unwrap(),
        "ok(1) :- want(\"db.postgres\", \"main\")"
    );
}

#[test]
fn membership_indexing_and_negation() {
    let got = lower(
        "resource db.postgres pg { public = false }\n\
         deny \"public\" { resource = p } where p in db.postgres, not p.public == false\n\
         set r.tags = { team = \"x\" } where r in resource\n\
         let xs = [1, 2]\n\
         let ys = [{ name = \"a\", net = 1 }]\n\
         q(x) where x = xs[i], i >= 0, x not in [3]\n\
         ok(1) where pg in db.postgres, has pg.public, not \"other\" in db.postgres\n\
         big(n) where n in world.net.vpc, world.net.vpc[n].size > 3\n\
         pair(n, c) where ys[_] = { name = n, net = c }\n",
    );
    assert_eq!(
        &got[1..],
        [
            "deny(\"public\", {resource = P}) :- want(\"db.postgres\", P), not attr(\"db.postgres\", P, \"public\", false)",
            "arg(Type, R, \"tags\", {team = \"x\"}) :- want(Type, R)",
            "let(\"xs\", [1, 2], \"normal\")",
            "let(\"ys\", [{name = \"a\", net = 1}], \"normal\")",
            "q(X) :- xs(Xs), member(Xs, I, Item), X = Item, I >= 0, not member([3], X)",
            // `has` of a resource's attribute is marked for the
            // compiler, which answers it from the schema or the value.
            "ok(1) :- want(\"db.postgres\", \"pg\"), __has(\"db.postgres\", \"pg\", \"public\", 1), attr(\"db.postgres\", \"pg\", \"public\", _), not want(\"db.postgres\", \"other\")",
            "big(N) :- cloud_exists(\"net.vpc\", N), cloud_attr(\"net.vpc\", N, \"size\", Size), Size > 3",
            "pair(N, C) :- ys(Ys), member(Ys, _, Obj), N = __path(Obj, \"name\"), C = __path(Obj, \"net\")",
        ]
    );
}

#[test]
fn modules_instances_and_outputs() {
    let got = lower(
        "component m {\n  input n: int\n  resource net.vpc vpc { size = n }\n  \
         output vpc: net.vpc = vpc\n  output ids: list(ref(net.vpc)) = [vpc]\n}\n\
         resource m a { n = 1 }\n\
         inst(\"a\")\n\
         p(v, s) where inst(i), v = m[i].vpc, s = a.vpc.size\n\
         q(x) where x = a.ids, \"a.vpc\" in net.vpc\n",
    );
    assert_eq!(
        got[0],
        "module m { resource \"net.vpc\" \"vpc\" { size = N } :- n(N); output vpc = None; \
         output vpc = Some(\"\\\"vpc\\\"\"); output ids = None; \
         output ids = Some(\"[__ref(\\\"net.vpc\\\", \\\"vpc\\\", \\\"\\\")]\") }"
    );
    assert_eq!(
        &got[3..],
        [
            "p(V, S) :- inst(I), instance_of(\"m\", \"\", I), output(I, \"vpc\", V), output(\"a\", \"vpc\", Vpc), attr(\"net.vpc\", Vpc, \"size\", S)",
            "q(X) :- output(\"a\", \"ids\", X), want(\"net.vpc\", \"a.vpc\")",
        ]
    );
}

#[test]
fn interpolation_and_lookups() {
    let got = lower(
        "extern file.json(+path, -value)\n\
         p(\"{x} $${x} ${x}%\") where q(x)\n\
         r(v) where v = file.json[\"a.json\"]\n\
         s(y) where q(x), y = \"n-${x}\", \"n-${x}\" in net.route_table\n",
    );
    assert_eq!(
        &got[..],
        [
            "p(str.format(\"{x} $${x} %s%\", X)) :- q(X)",
            "r(V) :- file.json(\"a.json\", V)",
            "s(Y) :- q(X), Y = str.format(\"n-%s\", X), Name = str.format(\"n-%s\", X), want(\"net.route_table\", Name)",
        ]
    );
}

#[test]
fn a_negated_body_is_a_helper() {
    let got = lower("p(x) where q(x), not { r(x, y), s(y) }\n");
    assert_eq!(
        &got[..],
        [
            "p(X) :- q(X), not __neg_0(X)",
            "__neg_0(X) :- q(X), r(X, Y), s(Y)",
        ]
    );
}

/// `has` and `not ==` on a nested path or a value's field: the walk
/// to it, and a helper for the negation.
#[test]
fn a_nested_path_is_walked_and_its_negation_is_a_helper() {
    let got = lower(
        "resource db.pg d { s = { a = 1 } }\n\
         c(x) where q(x), has x.limits\n\
         n(x) where q(x), not has x.limits\n\
         r(1) where not d.s.a == 2\n",
    );
    assert_eq!(
        &got[1..],
        [
            "c(X) :- q(X), Has' = __path(X, \"limits\")",
            "n(X) :- q(X), not __neg_0(X)",
            "__neg_0(X) :- q(X), Has' = __path(X, \"limits\")",
            "r(1) :- not __neg_1()",
            "__neg_1() :- attr(\"db.pg\", \"d\", \"s\", S), __path(S, \"a\") = 2",
        ]
    );
}

/// H-4: a statement's own `where` is its condition.
#[test]
fn a_statement_takes_its_condition() {
    let got = lower(
        "input env: string = \"dev\"\na(1) where env == \"prod\"\n\
         b(x) where e(x), f(x)\n",
    );
    assert_eq!(&got[..], ["a(1) :- env(\"prod\")", "b(X) :- e(X), f(X)"]);
}

#[test]
fn a_variable_may_not_shadow_a_name() {
    let e = error("resource net.vpc main { cidr = \"x\" }\np(net) where q(net)\n");
    assert!(
        e.contains("variable `net` shadows the type namespace `net`"),
        "{e}"
    );
    let e = error("resource net.vpc main { cidr = \"x\" }\np(main) where q(main)\n");
    assert!(
        e.contains("t.df:2:17: variable `main` shadows the resource `main`"),
        "{e}"
    );
}

/// R-2: a variable written once joins nothing.
#[test]
fn a_variable_written_once_is_an_error() {
    let e = error("reaches(a, c) where reaches(a, b), link(bb, c)\n");
    assert!(
        e.contains("variable `b` is used once; a typo, or `_b`")
            && e.contains("variable `bb` is used once"),
        "{e}"
    );
    // The header, the clause, the entries and the holes count together;
    // a `not { }` body counts once; `_x` opts out.
    lower(
        "tenant(\"t\", 1)\n\
         resource net.vpc \"${t}\" { cidr = \"${n}\" } where tenant(t, n)\n\
         resource net.vpc \"v-${t}\" {} where tenant(t, _n)\n\
         lonely(a) where tenant(a, _), not { tenant(a, z), z > 1 }\n",
    );
    let e = error("tenant(\"t\", 1)\nresource net.vpc \"${t}\" {} where tenant(t, n)\n");
    assert!(e.contains("variable `n` is used once"), "{e}");
    let e = error("tenant(\"t\", 1)\nlonely(a) where tenant(a, _), not { tenant(a, z) }\n");
    assert!(e.contains("variable `z` is used once"), "{e}");
}

#[test]
fn an_unbound_name_is_meant_as_a_string() {
    let e = error("input env: string = \"dev\"\np(1) where env == prod\n");
    assert!(e.contains("t.df:2:19: unknown name `prod`"), "{e}");
    assert!(e.contains("\"prod\""), "{e}");
    let e = error("resource net.vpc main { cidr = dev }\n");
    assert!(e.contains("unknown name `dev`"), "{e}");
}

#[test]
fn a_name_used_twice_is_named_by_its_address() {
    let e = error(
        "resource k8s.namespace web { n = 1 }\nresource k8s.service web { n = 1 }\n\
         resource x.y z { a = web.n }\n",
    );
    assert!(
        e.contains(
            "`web` names 2 resources: write one of k8s.namespace[\"web\"], k8s.service[\"web\"]"
        ),
        "{e}"
    );
    // Named by its address, the second resource `web` is not an error.
    lower(
        "resource k8s.namespace web { n = 1 }\nresource k8s.service web { n = 1 }\n\
         resource x.y z { a = k8s.service[\"web\"].n }\n",
    );
}

/// A bare header name is the literal name, a value in scope or not; a
/// name from the clause is a string (R-76).
#[test]
fn a_bare_block_name_is_literal() {
    let got = lower(
        "let env = 1\nresource net.vpc env { size = 1 }\nresource net.vpc shared { size = 2 }\n",
    );
    assert_eq!(
        &got[1..],
        [
            "resource \"net.vpc\" \"env\" { size = 1 } :- ",
            "resource \"net.vpc\" \"shared\" { size = 2 } :- ",
        ]
    );
    let e = error("t(\"a\")\nresource net.vpc t {\n size = 1 } where t(t)\n");
    assert!(
        e.contains(
            "`t` is bound by the clause, but a bare header name is the resource's literal name"
        ) && e.contains("`\"${t}\"`"),
        "{e}"
    );
    assert!(matches!(
        parse("resource net.vpc n { size = 1 }\n")
            .unwrap()
            .statements[0],
        crate::ast::Stmt::Resource(crate::ast::Resource {
            name: Term::Val(_),
            body: None,
            ..
        })
    ));
}

/// The compiler errors that name the one spelling (H-5, H-7, H-8,
/// H-10, H-15).
#[test]
fn a_second_spelling_is_an_error_naming_the_first() {
    for (src, want) in [
        // H-5: an unconditional `set` on a block of the same scope.
        (
            "resource net.vpc main { cidr = \"x\" }\nset main.tags = {}\n",
            "an entry of `resource net.vpc main`",
        ),
        (
            "input env: string = \"dev\"\nset env = \"prod\"\n",
            "sets the program's own input",
        ),
        // H-7: an output has a value.
        ("output vpc: string\n", "output vpc has no value"),
        // H-8: no rule reads deny/2.
        (
            "deny \"m\" where q(1)\np(m) where deny(m)\n",
            "a rule reads `deny`",
        ),
        // H-10: a resource in scope by its key, or by a dot after its type.
        (
            "resource net.vpc main { cidr = \"x\" }\np(c) where c = net.vpc[\"main\"].cidr\n",
            "write `main` (H-10)",
        ),
        (
            "resource net.vpc main { cidr = \"x\" }\np(c) where c = net.vpc.main.cidr\n",
            "names a resource by a dot after its type",
        ),
        (
            "p(c) where c = net.vcp.main.cidr\n",
            "unknown type `net.vcp.main.cidr`",
        ),
        // H-15: the core where a surface form says it.
        (
            "p(x) where want(net.vpc, x)\n",
            "write `x in net.vpc` (H-15)",
        ),
        (
            "p(v) where attr(net.vpc, \"main\", \"cidr\", v)\n",
            "write `v = net.vpc[\"main\"].cidr` (H-15)",
        ),
        (
            "arg(net.vpc, \"main\", \"cidr\", \"x\")\n",
            "write `set net.vpc[\"main\"].cidr = \"x\"` (H-15)",
        ),
        ("output(\"k\", 1)\n", "write `output k = 1` (H-15)"),
        (
            "deny(\"m\") where q(1)\n",
            "write `deny \"m\" where ..` (H-15)",
        ),
        ("p(x) where member([1], x)\n", "write `x in [1]` (H-9)"),
    ] {
        let e = file_error(src);
        assert!(e.contains(want), "{src}: {e}");
    }
    // The core stays writable where nothing else reaches: a variable
    // type or path, a read of the contributions before they merge.
    parse_as(
        "resource net.vpc main { cidr = \"x\" }\n\
         arg(t, n, p, v) where override(t, n, p, v), want(t, n)\n\
         override(net.vpc, \"main\", \"tags\", {})\n\
         p(v) where arg(net.vpc, \"main\", \"cidr\", v)\n",
        true,
    )
    .unwrap();
}

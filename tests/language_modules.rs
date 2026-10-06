//! Modules, components and instances (E DR-3, DESIGN.org R-5, R-65): a
//! module is a file, imported by `use` once under its name and read as
//! `m.x`, its inputs bound by the `use`'s block; a component is an item of
//! one, copied by `instance`, its predicates private per copy, its inputs
//! and outputs the data boundary; a module writes without a grant.

mod common;
use common::Scratch;

fn plan(src: &str) -> common::Run {
    let s = Scratch::new("lang-modules");
    s.write("p.df", src);
    s.run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
}

/// Two copies of one component each define `size/1`; privately, so
/// neither sees the other's. Were the relation global, each vpc would get
/// both sizes and conflict.
#[test]
fn a_component_predicate_is_private_to_its_copy() {
    let r = plan(
        r#"
component m {
  input n: int
  size(n_) where n(n_)
  resource net.vpc vpc {
    size = s
  } where size(s)
}
resource m a { n = 1 }
resource m b { n = 2 }
use fake
"#,
    )
    .success();
    assert!(
        r.stdout.contains("  + net.vpc[\"a.vpc\"]\n    size = 1\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("  + net.vpc[\"b.vpc\"]\n    size = 2\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("conflicts:"), "{}", r.stdout);
}

#[test]
fn reading_a_private_predicate_is_an_error_naming_the_component() {
    let r = plan(
        r#"
component m {
  size(1)
}
resource m a {}
big(s) where size(s)
use fake
"#,
    )
    .failure();
    assert!(
        r.stderr
            .contains("p.df:6:14: size/1 is private to component m"),
        "{}",
        r.stderr
    );
}

/// An output is `n.k` outside, `n` the copy's name; an `addr` output is
/// the copy's resource address, read with a variable copy.
#[test]
fn outputs_are_the_interface() {
    let r = plan(
        r#"
component m {
  input n: int
  size(n_) where n(n_)
  resource net.vpc vpc {
    size = s_
  } where size(s_)
  output size: int = s_ where size(s_)
  output vpc: addr = vpc
}
resource m a { n = 3 }
inst("a")
resource net.subnet s {
  size = s_
  vpc = v
} where s_ = a.size, inst(i), output(m[i], "vpc", v)
use fake
"#,
    )
    .success();
    assert!(
        r.stdout
            .contains("+ net.subnet[\"s\"]\n  size = 3\n  vpc = \"a.vpc\"\n"),
        "{}",
        r.stdout
    );
}

/// `export` and `contributes` are gone (DESIGN.org R-5, R-65): each is an
/// error naming what to write instead.
#[test]
fn export_and_contributes_are_errors() {
    let r = plan(
        r#"
component m {
  export size
  contributes need
  size(1)
}
resource m a {}
use fake
"#,
    )
    .failure();
    assert!(
        r.stderr
            .contains("p.df:3:3: expected a statement, found `export`"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("`export` is gone (R-65): a module's items are public"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("p.df:4:3: expected a statement, found `contributes`"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("`contributes` is gone (R-5): a write needs no grant"),
        "{}",
        r.stderr
    );
}

/// An input default is a `@default` contribution: the instance's value wins
/// where it sets one; a required input it does not set is a compile error.
/// An instance that sets nothing is written without a block (R-26).
#[test]
fn an_input_default_yields_to_the_instance() {
    let src = r#"
component m {
  input n: int = 7
  resource net.vpc vpc {
    size = n_
  } where n(n_)
}
resource m a {}
resource m b { n = 1 }
use fake
"#;
    let r = plan(src).success();
    assert!(
        r.stdout.contains("  + net.vpc[\"a.vpc\"]\n    size = 7\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("  + net.vpc[\"b.vpc\"]\n    size = 1\n"),
        "{}",
        r.stdout
    );

    let r = plan(&src.replace("input n: int = 7", "input n: int")).failure();
    assert!(
        r.stderr
            .contains("p.df:8:1: input a.n is required and has no value"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_refinement_on_a_component_input_is_a_deny() {
    let r = plan(
        r#"
component m {
  input n: int check n <= 5
  resource net.vpc vpc {
    size = n_
  } where n(n_)
}
resource m a { n = 9 }
use fake
"#,
    )
    .failure();
    assert!(
        r.stderr
            .contains("input n of a fails its refinement: n <= 5 ctx={\"value\":9}"),
        "{}",
        r.stderr
    );
}

/// A module writes any attribute without a grant (DESIGN.org R-5): ranks
/// decide, and the stratifier partitions the write by its head. Its own
/// relations are its own: the user reads them as `tags.team`, never bare.
#[test]
fn a_module_writes_without_a_grant_and_its_relations_are_its_own() {
    let s = Scratch::new("lang-modules-pack");
    s.write(
        "tags.df",
        r#"

team("x")
arg(t, a, "tags", { team: v }) where want(t, a), team(v)
set a.cidr = "10.9.0.0/16" @override where a in net.vpc
"#,
    );
    let src = r#"
resource net.vpc main { cidr = "10.0.0.0/16" }
use tags
use fake
"#;
    s.write("p.df", src);
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
        .success();
    assert!(r.stdout.contains("cidr = \"10.9.0.0/16\""), "{}", r.stdout);
    assert!(r.stdout.contains("team = \"x\""), "{}", r.stdout);
    s.write("p.df", &format!("{src}seen(v) where team(v)\n"));
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
        .failure();
    assert!(
        r.stderr
            .contains("p.df:5:15: team/1 is private to module tags"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("read it as tags.team"), "{}", r.stderr);
    s.write("p.df", &format!("{src}seen(v) where tags.team(v)\n"));
    s.run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
        .success();
}

/// A stack input passed to a component input of the same name: the
/// copy's input cell is its own node (partitioned by the copy's scope),
/// not the stack's, so `replicas = replicas` is not a read of the
/// aggregate it feeds. It was a negative cycle.
#[test]
fn a_stack_input_passed_to_a_component_input_of_the_same_name_stratifies() {
    let s = Scratch::new("lang-modules");
    s.write(
        "p.df",
        r#"
input replicas: int = 2
component app {
  input replicas: int
  resource compute.vm vm {
    count = replicas
  }
}
resource app blue { replicas = replicas }
resource app green { replicas = 7 }
use fake
"#,
    );
    let r = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "plan",
            "--why=none",
            "--set",
            "replicas=3",
            "p.df",
        ])
        .success();
    assert!(
        r.stdout
            .contains("  + compute.vm[\"blue.vm\"]\n    count = 3\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("  + compute.vm[\"green.vm\"]\n    count = 7\n"),
        "{}",
        r.stdout
    );
}

/// dform.df's peering rule joins each `vpc_peer_inst` edge to its own
/// instances' vpcs: with a second edge (to a third network) there are two
/// peerings, each named for its edge and holding its own pair. Unjoined, each
/// peering got both accepters and conflicted.
#[test]
fn dform_df_peers_each_edge_with_its_own_pair() {
    let s = Scratch::new("lang-modules-peering");
    common::copy_dir(&common::repo().join("examples/demo"), &s.dir);
    let third = r#"
resource network.vpc third {
  vpc_net = inet("10.70.0.0/16")
}
vpc_peer_inst("main", "third")
"#;
    s.write(
        "stacks/dform.df",
        &format!("{}{third}", s.read("stacks/dform.df")),
    );
    let r = s.run(&["plan", "--why=none", "dform"]).success();
    assert_eq!(
        r.stdout.matches("\n+ net.vpc_peering[").count(),
        2,
        "{}",
        r.stdout
    );
    for (name, accepter) in [("peer-main-peer", "peer"), ("peer-main-third", "third")] {
        let want = format!(
            "+ net.vpc_peering[\"{name}\"]\n  accepter_vpc = ?net.vpc[\"{accepter}.vpc\"]\n  requester_vpc = ?net.vpc[\"main.vpc\"]\n"
        );
        assert!(r.stdout.contains(&want), "{want}\n---\n{}", r.stdout);
    }
    assert!(!r.stdout.contains("conflict"), "{}", r.stdout);
}

/// A project of modules and components by path (R-65): `use config` reads
/// its values and relations as `config.x`; `use modules.lan` brings its
/// component item `vpc` and its alias `cidr`, read as `lan.vpc` and
/// `lan.cidr`; a module with resources (`postgres.df`, its input has a
/// default) is stamped once by `use`, under its name.
fn modules_project() -> Scratch {
    let s = Scratch::project("lang-modules-paths");
    s.write("config.df", "\n\nlet region = \"us-1\"\ntier(\"gold\")\n");
    s.write(
        "modules/lan.df",
        r#"

type cidr = inet

component vpc {
  input range: cidr
  resource net.vpc vpc {
    cidr = range
    tags = { region: config.region }
  }
  output vpc: net.vpc = vpc
}
"#,
    );
    s.write(
        "postgres.df",
        r#"

input size: int = 1

resource db.postgres db {
  public = false
  backup_days = size
  multi_az = false
}
"#,
    );
    s.write(
        "stacks/app.df",
        r#"

use fake

use config
use modules.lan

resource lan.vpc main { range = inet("10.1.0.0/16") }
resource modules.lan.vpc spare { range = inet("10.2.0.0/16") }
use postgres

tiers(t) where config.tier(t)
vpcs(n, v) where n in ["main", "spare"], v = lan.vpc[n].vpc
resource compute.vm bastion {
  tags = { region: config.region, tier: t }
} where tiers(t)
"#,
    );
    s
}

#[test]
fn a_module_is_used_and_a_component_instanced_by_its_path() {
    let s = modules_project();
    let r = s.run(&["plan", "--why=none", "app"]).success();
    for want in [
        "  + net.vpc[\"main.vpc\"]\n    cidr = \"10.1.0.0/16\"\n    tags.region = \"us-1\"\n",
        "  + net.vpc[\"spare.vpc\"]\n    cidr = \"10.2.0.0/16\"\n    tags.region = \"us-1\"\n",
        "+ db.postgres[\"postgres.db\"]\n  backup_days = 1\n",
        "+ compute.vm[\"bastion\"]\n  tags.region = \"us-1\"\n  tags.tier = \"gold\"\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
    let r = s.run(&["query", "vpcs(n, v)", "app"]).success();
    assert!(r.stdout.contains("\"main\"   \"main.vpc\""), "{}", r.stdout);
    assert!(
        r.stdout.contains("\"spare\"  \"spare.vpc\""),
        "{}",
        r.stdout
    );
}

/// A module's value is read by the name its `use` binds, never bare; a
/// module reached by no `use` is no name at all.
#[test]
fn a_module_value_is_read_through_its_use() {
    let s = modules_project();
    s.write(
        "stacks/app.df",
        "\n\nuse fake\n\nuse config\n\nwhere_(r) where r = region\n",
    );
    let r = s.run(&["plan", "--why=none", "app"]).failure();
    assert!(
        r.stderr.contains("unknown name") || r.stderr.contains("region"),
        "{}",
        r.stderr
    );
    s.write(
        "stacks/app.df",
        "\n\nuse fake\n\nuse config as c\n\nr(x) where x = c.region\nq(x) where x = c.nothing\n",
    );
    let r = s.run(&["plan", "--why=none", "app"]).failure();
    assert!(
        r.stderr
            .contains("the module config has no value, output or resource `nothing`"),
        "{}",
        r.stderr
    );
}

/// A name a module does not define reads outward, its user's: the same
/// module used by the stack and by a copy is two activations, each reading
/// its own user's `name`.
#[test]
fn a_used_module_reads_its_users_names() {
    let s = Scratch::new("lang-modules-outward");
    s.write("naming.df", "\n\nlabel(x) where name(x)\n");
    s.write(
        "p.df",
        r#"
use naming
name("stack")
component c {
  name("inner")
  use naming
  output labels = [ x | naming.label(x) ]
}
resource c one {}
got(x) where naming.label(x)
inner(l) where l = one.labels
use fake
"#,
    );
    let q = |pattern: &str| {
        s.run(&["dev", "--world", "w.json", "query", pattern, "p.df"])
            .success()
            .stdout
    };
    let got = q("got(x)");
    assert!(
        got.contains("\"stack\"") && !got.contains("\"inner\""),
        "{got}"
    );
    let inner = q("inner(l)");
    assert!(inner.contains("[\"inner\"]"), "{inner}");
}

/// A clause on an `instance` gates the copy's existence, inputs or none;
/// on a `use`, the module's rules and resources.
#[test]
fn a_clause_gates_a_copy_and_an_activation() {
    let s = Scratch::new("lang-modules-gates");
    s.write(
        "tagged.df",
        "\n\nset r.tags = { audited: true } where r in resource\n",
    );
    s.write(
        "p.df",
        r#"
input env: enum("dev", "prod") = "dev"
component bastion {
  resource compute.vm vm {
    size = 1
  }
}
resource bastion jump {} where env == "prod"
use tagged where env == "prod"
resource net.vpc main { cidr = "10.0.0.0/16" }
use fake
"#,
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
        .success();
    assert!(!r.stdout.contains("jump/vm"), "{}", r.stdout);
    assert!(!r.stdout.contains("audited"), "{}", r.stdout);
    let r = s
        .run(&[
            "dev", "--world", "w.json", "plan", "--set", "env=prod", "p.df",
        ])
        .success();
    assert!(r.stdout.contains("+ compute.vm jump.vm"), "{}", r.stdout);
    assert!(r.stdout.contains("tags.audited = true"), "{}", r.stdout);
}

/// A component instances components: a copy's copies are scoped under it
/// (`edge.left::vpc`), its keyed read ranges over its own copies, and its
/// outputs read theirs.
#[test]
fn a_copy_holds_copies_of_its_own() {
    let r = plan(
        r#"
component spoke {
  input range: string
  resource net.vpc vpc {
    cidr = range
  }
  output vpc: net.vpc = vpc
}
component pair {
  input a: string
  input b: string
  resource spoke left { range = a }
  resource spoke right { range = b }
  side("left")
  side("right")
  resource net.vpc_peering p {
    requester_vpc = left.vpc
    accepter_vpc = right.vpc
  }
  output vpcs = [ v | side(s), v = spoke[s].vpc ]
}
resource pair edge { a = "10.1.0.0/16", b = "10.2.0.0/16" }
n(c) where c = edge.vpcs
use fake
"#,
    )
    .success();
    for want in [
        "    + net.vpc[\"edge.left.vpc\"]\n      cidr = \"10.1.0.0/16\"\n",
        "    + net.vpc[\"edge.right.vpc\"]\n      cidr = \"10.2.0.0/16\"\n",
        "  + net.vpc_peering[\"edge.p\"]\n    accepter_vpc = ?net.vpc[\"edge.right.vpc\"]\n    requester_vpc = ?net.vpc[\"edge.left.vpc\"]\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
}

/// `use` from two stacks fires in both, by design: the module's rules
/// apply to each stack's own resources, and a module's resources are
/// stamped in each stack that uses it, each in that stack's state.
#[test]
fn a_module_used_from_two_stacks_fires_in_both() {
    let s = Scratch::project("lang-modules-two-stacks");
    s.write(
        "baseline.df",
        "\n\nset r.tags = { team: \"platform\" } where r in resource\n\
         deny \"no public vm\" where v in compute.vm, v.public == true\n",
    );
    s.write(
        "stacks/web.df",
        "\n\nuse fake\n\nuse baseline\n\nresource compute.vm web {\n  size = 2\n}\n",
    );
    s.write(
        "stacks/db.df",
        "\n\nuse fake\n\nuse baseline\n\nresource db.postgres db {\n  public = false\n}\n",
    );
    let web = s.run(&["plan", "--why=none", "web"]).success();
    assert!(
        web.stdout.contains("plan: 1 change (1 create)"),
        "{}",
        web.stdout
    );
    assert!(
        web.stdout.contains("+ compute.vm[\"web\"]"),
        "{}",
        web.stdout
    );
    assert!(
        web.stdout.contains("tags.team = \"platform\""),
        "{}",
        web.stdout
    );
    let db = s.run(&["plan", "--why=none", "db"]).success();
    assert!(
        db.stdout.contains("plan: 1 change (1 create)"),
        "{}",
        db.stdout
    );
    assert!(db.stdout.contains("+ db.postgres[\"db\"]"), "{}", db.stdout);
    assert!(
        db.stdout.contains("tags.team = \"platform\""),
        "{}",
        db.stdout
    );
    // A module with a resource stamps it once in each stack that uses it.
    s.write(
        "shared.df",
        "\n\nresource net.vpc vpc {\n  cidr = \"10.0.0.0/16\"\n}\n",
    );
    s.write("stacks/web.df", "\n\nuse fake\n\nuse shared\n");
    let r = s.run(&["plan", "--why=none", "web"]).success();
    assert_eq!(
        r.stdout.matches("+ net.vpc[\"shared.vpc\"]").count(),
        1,
        "{}",
        r.stdout
    );
}

/// `use` stamps a module once, under the path's last segment or the
/// `as`; its inputs are the block's, else their defaults (R-65).
#[test]
fn use_stamps_a_module_once() {
    let s = Scratch::new("lang-modules-stamp");
    s.write(
        "synapse.df",
        "\n\nresource compute.vm homeserver {\n  size = 2\n}\n\
         output host = \"matrix\"\n",
    );
    s.write(
        "forgejo.df",
        "\n\ninput size: int = 3\n\nresource compute.vm forge {\n  size\n}\n",
    );
    s.write(
        "postgres.df",
        "\n\ninput database: string\n\nresource db.postgres db {\n  public = false\n  \
         name = database\n}\n",
    );
    s.write(
        "p.df",
        "\nuse synapse\nuse forgejo as git\nuse postgres { database = \"matrix\" }\n\
         h(x) where x = synapse.host\nuse fake\n",
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
        .success();
    assert!(
        r.stdout
            .contains("+ compute.vm[\"synapse.homeserver\"]\n  size = 2\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("+ compute.vm[\"git.forge\"]\n  size = 3\n"),
        "{}",
        r.stdout
    );
    assert_eq!(
        r.stdout.matches("\n+ compute.vm[").count(),
        2,
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("+ db.postgres[\"postgres.db\"]\n  name = \"matrix\"\n"),
        "{}",
        r.stdout
    );
    let h = s
        .run(&["dev", "--world", "w.json", "query", "h(x)", "p.df"])
        .success()
        .stdout;
    assert!(h.contains("\"matrix\""), "{h}");
}

/// A used module's input nothing gives a value is a stack input's error,
/// fixed the same ways: a default on its declaration, or a value in the
/// `use` block. A copy of a component is named.
#[test]
fn an_unbound_input_of_a_used_module_is_a_stack_inputs_error() {
    let s = Scratch::new("lang-modules-unbound");
    s.write(
        "postgres.df",
        "\n\ninput database: string\n\nresource db.postgres db {\n  public = false\n}\n",
    );
    s.write("p.df", "\nuse postgres\nuse fake\n");
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
        .failure();
    assert!(
        r.stderr
            .contains("p.df:2:1: input postgres.database is required and has no value"),
        "{}",
        r.stderr
    );
    s.write("p.df", "\nuse postgres { database = \"x\" }\nuse fake\n");
    s.run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
        .success();
    // The stack gives a used module's input by its name there (R-55).
    s.write("p.df", "\nuse postgres\nuse fake\n");
    s.run(&[
        "dev",
        "--world",
        "w.json",
        "plan",
        "--why=none",
        "p.df",
        "--set",
        "postgres.database=y",
    ])
    .success();
    let r = s
        .run(&[
            "dev",
            "--world",
            "w.json",
            "why",
            "postgres.database",
            "p.df",
            "--set",
            "postgres.database=y",
        ])
        .success();
    assert!(
        r.stdout.contains("input postgres.database = \"y\""),
        "{}",
        r.stdout
    );
    s.write(
        "p.df",
        "\ncomponent vm {\n  resource compute.vm vm {\n    size = 1\n  }\n}\n\
         resource vm {}\nuse fake\n",
    );
    let r = s
        .run(&["dev", "--world", "w.json", "plan", "--why=none", "p.df"])
        .failure();
    assert!(
        r.stderr.contains("expected a resource name"),
        "{}",
        r.stderr
    );
}

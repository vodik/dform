//! A provider under two names (R-115): `use aws as east { .. }` beside
//! `use aws as west { .. }` starts the provider twice, each configured by
//! its own block, its types written `east.vpc`; the provider never learns
//! the name (its world holds `aws.vpc`), state and the plan carry it, and
//! `x in aws.vpc` ranges over both.

mod common;
use common::Scratch;

/// A project whose dform.toml names the aws mock `aws`.
fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\naws = { source = \"aws-mock\" }\n",
    );
    s
}

const TWO: &str = r#"
use aws as east { region = "us-east-1" }
use aws as west { region = "us-west-2" }

resource east.vpc a {
  cidr_block = "10.0.0.0/16"
}

resource west.vpc b {
  cidr_block = "10.1.0.0/16"
}

resource west.subnet s {
  vpc_id = b
  cidr_block = "10.1.1.0/24"
  tags = { peer: ref(a), peer_cidr: a.cidr_block }
}

vpcs(n) where v in aws.vpc, n = v.cidr_block
eastern(n) where v in east.vpc, n = v.cidr_block
"#;

/// Each name a resource of its own, one reading the other's; applied,
/// state keeps each by the name the program wrote, and each name's world
/// is its own, in the provider's own type names.
#[test]
fn two_names_of_one_provider_apply_each_its_own() {
    let s = project("alias-apply");
    s.write("main.df", TWO);
    let plan = s.run(&["plan", "main.df"]).success().stdout;
    for want in [
        "+ east.vpc a ",
        "+ west.vpc b ",
        "+ west.subnet s ",
        "tags = { peer: a, peer_cidr: \"10.0.0.0/16\" }",
    ] {
        assert!(plan.contains(want), "{want}\n{plan}");
    }
    s.run(&["apply", "-y", "main.df"]).success();
    let again = s.run(&["plan", "main.df"]).success().stdout;
    assert!(again.contains("is up to date"), "{again}");
    let state = s.run(&["state", "show", "main.df"]).success().stdout;
    for addr in ["east.vpc[\"a\"]", "west.vpc[\"b\"]", "west.subnet[\"s\"]"] {
        assert!(state.contains(addr), "{addr}\n{state}");
    }
    let one = s
        .run(&["state", "show", "--address", "west.vpc[\"b\"]", "main.df"])
        .success()
        .stdout;
    assert!(
        one.contains("west.vpc[\"b\"]") && !one.contains("east"),
        "{one}"
    );
    // One process per name, each its own world, in the provider's names:
    // a resource of `east` never reaches `west`.
    let east = s.read("dform.state/main/remote.east.json");
    let west = s.read("dform.state/main/remote.west.json");
    assert!(east.contains("\"aws.vpc::a\""), "{east}");
    assert!(
        !east.contains("aws.vpc::b") && !east.contains("east."),
        "{east}"
    );
    assert!(
        west.contains("\"aws.vpc::b\"") && west.contains("\"aws.subnet::s\""),
        "{west}"
    );
    assert!(
        !west.contains("aws.vpc::a") && !west.contains("west."),
        "{west}"
    );
    let why = s.run(&["why", "west.subnet s", "main.df"]).success().stdout;
    assert!(why.starts_with("west.subnet s "), "{why}");
}

/// `x in aws.vpc` ranges over every name's, `x in east.vpc` over one's;
/// each name's block configures its own provider.
#[test]
fn membership_and_settings_follow_the_name() {
    let s = project("alias-in");
    s.write("main.df", TWO);
    let all = s.run(&["dev", "query", "vpcs", "main.df"]).success().stdout;
    assert!(
        all.contains("\"10.0.0.0/16\"") && all.contains("\"10.1.0.0/16\""),
        "{all}"
    );
    let east = s
        .run(&["dev", "query", "eastern", "main.df"])
        .success()
        .stdout;
    assert!(
        east.contains("\"10.0.0.0/16\"") && !east.contains("10.1.0.0"),
        "{east}"
    );
    let config = s
        .run(&["dev", "query", "provider_config", "main.df"])
        .success()
        .stdout;
    assert!(
        config.contains("\"east\"  {region: \"us-east-1\"}")
            && config.contains("\"west\"  {region: \"us-west-2\"}"),
        "{config}"
    );
}

/// A data source is read under the name too: `east.availability_zone`
/// is the provider's `aws.availability_zone`, asked of `east`.
#[test]
fn an_extern_is_read_under_the_name() {
    let s = project("alias-extern");
    s.write(
        "main.df",
        r#"
use aws as east
use aws as west

resource east.vpc a {
  cidr_block = "10.0.0.0/16"
}

resource east.subnet "z-${zone}" {
  vpc_id = a
  cidr_block = inet.subnet(inet(a.cidr_block), 8, n)
  availability_zone = zone
} where east.availability_zone("available", zone, n)
"#,
    );
    let r = s.run(&["plan", "main.df"]).success();
    assert_eq!(
        r.summary(),
        "plan: 4 changes (4 create) over 1 tick",
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("+ east.subnet z-us-east-1c"),
        "{}",
        r.stdout
    );
}

/// A name is declared once; `ref(T)` attributes take their own name's
/// resources; a provider whose types are not named under it, or one dform
/// answers itself, takes no other name.
#[test]
fn what_a_name_may_not_do() {
    let s = project("alias-errors");
    for (file, text, want) in [
        (
            "p_twice.df",
            "\nuse aws\nuse aws\n",
            "`aws` is declared twice; give each a `where`",
        ),
        (
            "p_twice_as.df",
            "\nuse aws as east\nuse aws as east\n",
            "`east` is declared twice; give each a `where`",
        ),
        (
            "p_env.df",
            "\nuse env as e\n",
            "`use env as e`: env is answered by dform itself",
        ),
        (
            "p_fake.df",
            "\nuse fake as a\nresource net.vpc v { cidr = \"10.0.0.0/16\" }\n",
            "`use fake as a`: provider fake serves",
        ),
    ] {
        s.write(file, text);
        let r = s.run(&["plan", file]).failure();
        assert!(r.stderr.contains(want), "{file}: {want}\n{}", r.stderr);
    }
    s.write("cross.df", &TWO.replace("vpc_id = b", "vpc_id = a"));
    let r = s.run(&["plan", "cross.df"]).failure();
    assert!(
        r.stderr
            .contains("west.subnet[\"s\"].vpc_id takes a ref(west.vpc), got east.vpc[\"a\"]"),
        "{}",
        r.stderr
    );
}

/// A name a module's `use` gives is the deployment's, as the stack's
/// own (R-129): two blocks of one name that differ are a conflict.
#[test]
fn a_name_is_one_configuration_across_modules() {
    let s = project("alias-module");
    s.write(
        "edge.df",
        "\nuse aws as east { region = \"us-east-1\" }\nresource east.vpc a { cidr_block = \"10.0.0.0/16\" }\n",
    );
    s.write(
        "main.df",
        "\nuse edge\nuse aws as east { region = \"eu-west-1\" }\n",
    );
    let r = s.run(&["plan", "main.df"]).failure();
    let all = format!("{}{}", r.stdout, r.stderr);
    assert!(
        all.contains("provider east: two configurations disagree at region"),
        "{all}"
    );
    s.write(
        "same.df",
        "\nuse edge\nuse aws as east { region = \"us-east-1\" }\nuse aws as west\n\
         resource west.vpc b { cidr_block = \"10.1.0.0/16\" }\n",
    );
    let plan = s.run(&["plan", "same.df"]).success().stdout;
    assert!(
        plan.contains("+ east.vpc edge.a ") && plan.contains("+ west.vpc b "),
        "{plan}"
    );
}

/// The shape of a project with one `use` of its provider is unchanged:
/// `use aws { .. }` is configured as `aws`, its types `aws.vpc`.
#[test]
fn one_use_of_a_provider_is_as_before() {
    let s = project("alias-none");
    s.write(
        "main.df",
        "\nuse aws { region = \"us-east-1\" }\nresource aws.vpc a { cidr_block = \"10.0.0.0/16\" }\n\
         vpcs(n) where v in aws.vpc, n = v.cidr_block\n",
    );
    s.run(&["apply", "-y", "main.df"]).success();
    let state = s.run(&["state", "show", "main.df"]).success().stdout;
    assert!(state.contains("aws.vpc[\"a\"]"), "{state}");
    let config = s
        .run(&["dev", "query", "provider_config", "main.df"])
        .success()
        .stdout;
    assert!(
        config.contains("\"aws\"  {region: \"us-east-1\"}"),
        "{config}"
    );
    let vpcs = s.run(&["dev", "query", "vpcs", "main.df"]).success().stdout;
    assert!(vpcs.contains("\"10.0.0.0/16\""), "{vpcs}");
    assert!(
        s.read("dform.state/main/remote.json")
            .contains("aws.vpc::a")
    );
}

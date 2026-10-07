//! A provider is imported with `use` and configured by its block (R-112
//! amendment 2): `use fake { region = .. }`, a guarded `use .. where`
//! R-104's conditional provider, and a provider's name is in the scope's
//! one namespace. `provider`, the earlier statement, is an error naming
//! `use`.

mod common;
use common::Scratch;

/// A `use` of a provider plans against it and configures it with its
/// block's `provider_config` row; the `provider` statement is gone.
#[test]
fn a_provider_is_used_and_configured_by_its_block() {
    let s = Scratch::new("providers-use");
    let body = "\nresource net.vpc v {\n  cidr = \"10.0.0.0/16\"\n}\n";
    s.write(
        "p.df",
        &format!("use fake {{ region = \"eu-west-1\" }}{body}"),
    );
    let plan = s.run(&["plan", "--why=none", "p.df"]).success().stdout;
    assert!(plan.contains("+ net.vpc[\"v\"]"), "{plan}");
    let config = s
        .run(&["dev", "query", "provider_config", "p.df"])
        .success()
        .stdout;
    assert!(
        config.contains("\"fake\"  {region: \"eu-west-1\"}"),
        "{config}"
    );
    s.write(
        "old.df",
        &format!("provider fake {{ region = \"eu-west-1\" }}{body}"),
    );
    let r = s.run(&["plan", "old.df"]).failure();
    assert!(
        r.stderr.contains("`provider` is gone (R-112)")
            && r.stderr.contains("`use NAME { k = v }`"),
        "{}",
        r.stderr
    );
}

/// A guarded `use` is the conditional provider (R-104): configured only
/// where its clause holds.
#[test]
fn a_guarded_use_configures_the_provider_where_it_holds() {
    let s = Scratch::new("providers-use-guarded");
    s.write(
        "p.df",
        "\ninput cloud: enum(\"aws\", \"gcp\") = \"aws\"\n\
         use fake { region = \"eu-west-1\" } where cloud == \"aws\"\n\
         use fake { region = \"us-east-1\" } where cloud == \"gcp\"\n\
         resource net.vpc v { size = 1 }\n",
    );
    let config = |set: &str| {
        s.run(&["dev", "--set", set, "query", "provider_config", "p.df"])
            .success()
            .stdout
    };
    let aws = config("cloud=aws");
    assert!(
        aws.contains("{region: \"eu-west-1\"}") && !aws.contains("us-east"),
        "{aws}"
    );
    let gcp = config("cloud=gcp");
    assert!(
        gcp.contains("{region: \"us-east-1\"}") && !gcp.contains("eu-west"),
        "{gcp}"
    );
}

/// A built-in fact provider's externs come with its `use`.
#[test]
fn a_used_builtin_provider_brings_its_externs() {
    let s = Scratch::new("providers-use-env");
    s.write(
        "p.df",
        "\nuse fake\nuse env\ntok(t) where t = env.var(\"PATH\")\n",
    );
    s.run(&["plan", "p.df"]).success();
    s.write("q.df", "\nuse fake\ntok(t) where t = env.var(\"PATH\")\n");
    let r = s.run(&["plan", "q.df"]).failure();
    assert!(
        r.stderr
            .contains("env.var is the env provider's: declare `use env`"),
        "{}",
        r.stderr
    );
}

/// One name per scope: a module, a copy and a provider share it, so
/// `use fake` beside `use fake` is the error two uses are; a name
/// that is neither a module nor a provider is the module error, saying
/// so; `use fake as cloud` is read (R-115), and refused because the
/// fake cloud's types are not named under `fake`.
#[test]
fn a_provider_shares_the_scopes_one_namespace() {
    let s = Scratch::new("providers-use-twice");
    s.write("p.df", "\nuse fake\nuse fake\n");
    let r = s.run(&["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("`fake` is declared twice; give each a `where`"),
        "{}",
        r.stderr
    );
    s.write("t.df", "\nuse fak\n");
    let r = s.run(&["plan", "t.df"]).failure();
    assert!(
        r.stderr.contains("no module `fak`") && r.stderr.contains("nor a provider"),
        "{}",
        r.stderr
    );
    s.write("q.df", "\nuse fake as cloud\n");
    let r = s.run(&["plan", "q.df"]).failure();
    assert!(
        r.stderr
            .contains("`use fake as cloud`: provider fake serves"),
        "{}",
        r.stderr
    );
}

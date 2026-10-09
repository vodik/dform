//! A held secret's holder replaced (After R-218): the vault's token
//! (`vault.token.value`, sensitive and computed) in a server's user data
//! (write-only, compared by the keyed digest state keeps of what was
//! sent). A new token under the same label is a new value: what is
//! digested carries the holder's remote id, so the server's user data
//! differs and is sent again with the new token, or under `bootstrap`
//! (R-198) is kept, and the plan says so. Nothing replaced, nothing
//! differs.

mod common;
use common::{Run, Scratch};

/// The vault's token, its `scope` given at creation only by the API: a
/// new scope is a new token, made before the old one goes under the next
/// generation of its name (R-189), so its remote id is new.
const VAULT: &str = r#"
type_provider("vault.token", "vault")
type_attr("vault.token", "name", "string", ["required", "id"])
type_attr("vault.token", "scope", "string", ["force_new"])
type_attr("vault.token", "value", "string", ["computed", "sensitive"])
type_remote_name("vault.token", "name")
type_replace("vault.token", "create_first")
"#;

fn program(scope: &str, facts: &str) -> String {
    format!(
        r##"use vault
use fake {{ source = "prov" }}
resource vault.token t {{
  name = "t"
  scope = "{scope}"
}}
resource compute.vm vm {{
  name = "vm"
  weight = 1
  user_data = "#cloud-config\ntoken: ${{t.value}}\n"
}}
{facts}"##
    )
}

fn project(name: &str, body: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nvault = \"./vault\"\n",
    );
    s.write("vault/schema.df", VAULT);
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(common::repo().join("crates/dform-mock/schemas/fake.df"))
            .unwrap()
            + "type_attr(compute.vm, \"user_data\", \"string\", [\"write_only\", \"force_new\", \
               \"sensitive\"])\n"),
    );
    s.write("stacks/p.df", body);
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s
}

fn dform(s: &Scratch, args: &[&str]) -> Run {
    let mut args = common::yes(args);
    if args.first().is_some_and(|a| a == "apply") {
        args.extend(["--wait-timeout".into(), "5s".into()]);
    }
    Run::from(
        common::dform()
            .args(args)
            .current_dir(&s.dir)
            .output()
            .unwrap(),
    )
}

/// The token's value, from the vault's world: its one token's.
fn token(s: &Scratch) -> String {
    let world = s.json("dform.state/stacks.p/remote.json");
    let tokens: Vec<&serde_json::Value> = world["resources"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(k, _)| k.starts_with("vault.token::"))
        .map(|(_, v)| v)
        .collect();
    assert_eq!(tokens.len(), 1, "{world}");
    tokens[0]["computed"]["value"]
        .as_str()
        .expect("the token's value in the vault's world")
        .to_string()
}

/// The server's user data, from the plugin's world.
fn user_data(s: &Scratch) -> String {
    s.json("dform.state/stacks.p/remote.fakecloud.json")["resources"]["compute.vm::vm"]["attrs"]
        ["user_data"]
        .as_str()
        .expect("the server's user data")
        .to_string()
}

/// The token replaced: the server's user data differs (it is `force_new`,
/// so the server is replaced in the same tick, after the token) and is
/// sent with the new token's bytes. Nothing replaced, the plan is clean.
#[test]
fn a_replaced_holder_replaces_its_writer() {
    let s = project("rotation-update", &program("a", ""));
    dform(&s, &["apply", "p"]).success();
    let first = token(&s);
    let again = dform(&s, &["plan", "p"]).success();
    assert_eq!(again.summary(), "stack p is up to date", "{}", again.stdout);

    s.write("stacks/p.df", &program("b", ""));
    let plan = dform(&s, &["plan", "p"]).success();
    assert!(
        plan.stdout.contains(
            "  ± compute.vm vm                                   stacks/p.df:7  user_data \
             forces replace\n      user_data = (sensitive) → (sensitive)\n"
        ),
        "{}",
        plan.stdout
    );
    dform(&s, &["apply", "p"]).success();
    let second = token(&s);
    assert_ne!(first, second);
    assert_eq!(user_data(&s), format!("#cloud-config\ntoken: {second}\n"));
    let after = dform(&s, &["plan", "p"]).success();
    assert_eq!(after.summary(), "stack p is up to date", "{}", after.stdout);
}

/// Under `bootstrap` the server keeps what it was made with: the plan
/// says the user data is kept, and the apply sends none. Before the token
/// is replaced nothing is kept.
#[test]
fn a_replaced_holder_under_bootstrap_is_kept() {
    let bootstrap = "lifecycle(vm, \"bootstrap\", \"user_data\")\n";
    let s = project("rotation-bootstrap", &program("a", bootstrap));
    dform(&s, &["apply", "p"]).success();
    let first = token(&s);
    let clean = dform(&s, &["plan", "p"]).success();
    assert!(!clean.stdout.contains("kept"), "{}", clean.stdout);

    s.write("stacks/p.df", &program("b", bootstrap));
    let plan = dform(&s, &["plan", "p"]).success();
    assert!(
        plan.stdout.contains(
            "= compute.vm vm                                     stacks/p.df:7\n    \
             user_data differs (bootstrap): kept\n"
        ),
        "{}",
        plan.stdout
    );
    dform(&s, &["apply", "p"]).success();
    assert_ne!(token(&s), first);
    let plan = dform(&s, &["plan", "p"]).success();
    assert!(
        plan.stdout
            .contains("    user_data differs (bootstrap): kept\n"),
        "{}",
        plan.stdout
    );
    assert_eq!(plan.summary(), "stack p is up to date", "{}", plan.stdout);
    assert_eq!(user_data(&s), format!("#cloud-config\ntoken: {first}\n"));
}

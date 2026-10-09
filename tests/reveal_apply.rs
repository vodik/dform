//! A secret one provider holds, written by another (R-218): the vault's
//! token (`vault.token.value`, sensitive and computed: dform has its label
//! only) in a server's attributes, served by the mock run as a plugin
//! (`use fake { source = "prov" }`, a second provider). Written whole
//! (`token = t.value`) or inside a string template (`user_data =
//! "..${t.value}.."`), it is revealed by the vault into the plugin's Apply
//! call, under the run's lease, as into a Configure (R-45): the bytes are
//! in the plugin's world and the vault's, and in no other file and no
//! output. A template waits for nothing (it once waited forever); a
//! function that would need the bytes is an error; a reveal that does not
//! happen is the call's error, naming the attribute; a `bootstrap`
//! attribute (R-198) is not revealed after the object is made.

mod common;
use common::{Run, Scratch};
use std::path::Path;

const VAULT: &str = r#"
type_provider("vault.token", "vault")
type_attr("vault.token", "id", "string", ["computed", "id"])
type_attr("vault.token", "name", "string", ["required"])
type_attr("vault.token", "value", "string", ["computed", "sensitive"])
"#;

/// The mock's own schema, its server given user data (write-only, as
/// OVH's) and a token the API answers back (sensitive, read).
fn fake_schema() -> String {
    std::fs::read_to_string(common::repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
        + "type_attr(compute.vm, \"user_data\", \"string\", [\"write_only\", \"force_new\", \
           \"sensitive\"])\n\
           type_attr(compute.vm, \"token\", \"string\", [\"sensitive\"])\n"
}

/// The server's user data, its template's text `boot`.
fn program(boot: &str, facts: &str) -> String {
    format!(
        r##"use vault
use fake {{ source = "prov" }}
resource vault.token t {{ name = "t" }}
resource compute.vm vm {{
  name = "vm"
  weight = 1
  token = t.value
  user_data = "#cloud-config\n{boot}: ${{t.value}}\n"
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
    s.write("providers/fake/schema.df", &fake_schema());
    s.write("stacks/p.df", body);
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s
}

/// `dform ARGS` in `s`; an apply waits 5s at most (a wait that never ends
/// fails, as it did).
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

/// The token's value, from the vault's world.
fn token(s: &Scratch) -> String {
    s.json("dform.state/stacks.p/remote.json")["resources"]["vault.token::t"]["computed"]["value"]
        .as_str()
        .expect("the token's value in the vault's world")
        .to_string()
}

/// The server as the plugin's world has it.
fn server(s: &Scratch) -> serde_json::Value {
    s.json("dform.state/stacks.p/remote.fakecloud.json")["resources"]["compute.vm::vm"]["attrs"]
        .clone()
}

/// Every file under `dir` whose bytes contain `needle`, relative to `top`.
fn holding(top: &Path, dir: &Path, needle: &[u8], out: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_symlink() {
            continue;
        }
        if p.is_dir() {
            holding(top, &p, needle, out);
        } else if std::fs::read(&p)
            .unwrap()
            .windows(needle.len())
            .any(|w| w == needle)
        {
            out.push(p.strip_prefix(top).unwrap().display().to_string());
        }
    }
}

/// The vault forgets the token's value: what it holds can no longer be
/// revealed (a key the API answers once, a generation rotated away).
fn forget(s: &Scratch) {
    let path = "dform.state/stacks.p/remote.json";
    let mut world = s.json(path);
    world["resources"]["vault.token::t"]["computed"]
        .as_object_mut()
        .unwrap()
        .remove("value");
    s.write(path, &serde_json::to_string_pretty(&world).unwrap());
}

/// The token, whole and inside the user data's template, reaches the
/// plugin in the Apply that makes the server, in the tick that makes the
/// token: the bytes are in the two worlds only. The next plan, which
/// reveals nothing, is clean.
#[test]
fn a_held_secret_is_revealed_into_the_apply_that_writes_it() {
    let s = project("reveal-apply", &program("token", ""));
    let plan = dform(&s, &["plan", "p"]).success();
    assert!(plan.stdout.contains("over 1 tick"), "{}", plan.stdout);
    let apply = dform(&s, &["apply", "p"]).success();
    let value = token(&s);
    let vm = server(&s);
    assert_eq!(vm["token"], value.as_str(), "{vm}");
    assert_eq!(
        vm["user_data"],
        format!("#cloud-config\ntoken: {value}\n"),
        "{vm}"
    );
    let again = dform(&s, &["plan", "p"]).success();
    assert_eq!(again.summary(), "stack p is up to date", "{}", again.stdout);
    let json = dform(&s, &["plan", "--json", "p"]).success();
    let shown = dform(&s, &["state", "show", "p"]).success();
    let mut found = Vec::new();
    holding(&s.dir, &s.dir, value.as_bytes(), &mut found);
    found.sort();
    assert_eq!(
        found,
        [
            "dform.state/stacks.p/remote.fakecloud.json",
            "dform.state/stacks.p/remote.json"
        ],
        "{value}"
    );
    for r in [&plan, &apply, &again, &json, &shown] {
        assert!(
            !r.stdout.contains(&value) && !r.stderr.contains(&value),
            "{}\n{}",
            r.stdout,
            r.stderr
        );
    }
}

/// A reveal that does not happen is the Apply's error at the attribute,
/// with the holder's reason; nothing is sent.
#[test]
fn a_reveal_that_cannot_happen_is_the_calls_error() {
    let s = project("reveal-gone", &program("token", ""));
    dform(&s, &["apply", "p"]).success();
    forget(&s);
    // The template changes: the server is made again with it.
    s.write("stacks/p.df", &program("key", ""));
    let r = dform(&s, &["apply", "p"]).failure();
    assert!(
        r.stderr.contains(
            "! apply compute.vm vm: not sent\n    compute.vm vm.token holds the secret \
             vault.token[\"t\"].value, which was not revealed: reveal vault.token t#value: \
             vault.token t of p does not set value\n"
        ),
        "{}",
        r.stderr
    );
    assert!(
        server(&s)["user_data"]
            .as_str()
            .is_some_and(|u| u.contains("token: ")),
        "the server was not touched"
    );
}

/// Given at creation only, the user data is not compared once the server
/// exists: a later plan and apply reveal nothing (the vault could not),
/// and say the differing template is kept.
#[test]
fn a_bootstrap_attribute_is_not_revealed_after_the_object_is_made() {
    let bootstrap = "lifecycle(vm, \"bootstrap\", \"user_data\")\n";
    let s = project("reveal-bootstrap", &program("token", bootstrap));
    dform(&s, &["apply", "p"]).success();
    forget(&s);
    s.write(
        "stacks/p.df",
        &program("key", bootstrap).replace("  token = t.value\n", ""),
    );
    let plan = dform(&s, &["plan", "p"]).success();
    assert!(
        plan.stdout
            .contains("    user_data differs (bootstrap): kept\n"),
        "{}",
        plan.stdout
    );
    // An update for another reason sends no user data.
    s.write(
        "stacks/p.df",
        &program("key", bootstrap)
            .replace("  token = t.value\n", "")
            .replace("weight = 1", "weight = 2"),
    );
    dform(&s, &["apply", "p"]).success();
    assert_eq!(server(&s)["weight"], 2);
    let again = dform(&s, &["plan", "p"]).success();
    assert!(
        again.stdout.contains("user_data differs (bootstrap): kept")
            && again.summary() == "stack p is up to date",
        "{}",
        again.stdout
    );
}

/// No function but a template composes a secret dform does not have: one
/// that would need its bytes is an error at the attribute, not a wait.
#[test]
fn a_function_over_a_held_secret_is_an_error_not_a_wait() {
    let s = project(
        "reveal-encode",
        "use vault\nuse fake { source = \"prov\" }\nresource vault.token t { name = \"t\" }\n\
         resource compute.vm vm {\n  name = \"vm\"\n  user_data = json.encode({ token: t.value \
         })\n}\n",
    );
    let r = dform(&s, &["plan", "p"]).failure();
    assert!(
        r.stderr.ends_with(
            "Error: stacks/p.df:4, compute.vm vm: user_data reads the secret \
         vault.token[\"t\"].value through json.encode(), which dform cannot compute: a provider \
         holds it, and its bytes exist for dform only inside the call that writes it\n  help: \
         write it whole, or inside a string template: \"..${vault.token[\"t\"].value}..\"\n"
        ),
        "{}",
        r.stderr
    );
}

/// A template is revealed into a Configure as a whole value is (R-45):
/// the plugin is configured from `"acct-${t.value}"` once the token
/// exists, its account those bytes.
#[test]
fn a_template_over_a_held_secret_configures_a_provider() {
    let stack = |expect: &str| {
        format!(
            "use vault\nuse fake {{ source = \"prov\", account = \"acct-${{t.value}}\"{expect} }}\n\
             resource vault.token t {{ name = \"t\" }}\n\
             resource db.postgres server {{ name = \"server\" }}\n"
        )
    };
    let s = project("reveal-configure", &stack(""));
    let plan = dform(&s, &["plan", "p"]).success();
    assert!(
        plan.stdout.contains("waits on  provider fake"),
        "{}",
        plan.stdout
    );
    let r = dform(&s, &["apply", "p"]).success();
    let value = token(&s);
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains(&value), "{out}");
    }
    s.write(
        "stacks/p.df",
        &stack(&format!(", expect_account = \"acct-{value}\"")),
    );
    let r = dform(&s, &["plan", "p"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
}

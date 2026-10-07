//! A provider configured from a secret another provider holds (R-45's
//! `Held` reveal, R-130's `Reveal` call): a resource's sensitive computed
//! attribute (a managed cluster's kubeconfig, here the mock's
//! `vault.token.value`) never leaves its provider, so the program has only
//! its label; at the tick boundary after the object is made, dform asks
//! the holding provider to reveal it into the other provider's Configure,
//! and nowhere else. The configured provider is the mock run as a plugin
//! (`use fake { source = "prov", .. }`), its `account` the secret, which
//! `expect_account` checks.

mod common;
use common::{Run, Scratch};
use std::path::Path;

const VAULT: &str = r#"
type_provider("vault.token", "vault")
type_attr("vault.token", "id", "string", ["computed", "id"])
type_attr("vault.token", "name", "string", ["required"])
type_attr("vault.token", "value", "string", ["computed", "sensitive"])
"#;

fn stack(expect: &str) -> String {
    format!(
        r#"
use vault
use fake {{ source = "prov", account = t.value{expect} }}
resource vault.token t {{ name = "t" }}
resource db.postgres server {{ name = "server" }}
"#
    )
}

fn project() -> Scratch {
    let s = Scratch::project("reveal");
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nvault = \"./vault\"\n",
    );
    s.write("vault/schema.df", VAULT);
    s.write("stacks/p.df", &stack(""));
    std::fs::create_dir_all(s.path("prov")).unwrap();
    std::os::unix::fs::symlink(
        common::exe("dform-provider-fake"),
        s.path("prov/dform-provider-fake"),
    )
    .unwrap();
    s
}

fn dform(s: &Scratch, args: &[&str]) -> Run {
    let out = common::dform()
        .args(common::yes(args))
        .current_dir(&s.dir)
        .output()
        .unwrap();
    Run::from(out)
}

/// Every file under `dir` whose bytes contain `needle`.
fn holding(dir: &Path, needle: &[u8], out: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_symlink() {
            continue;
        }
        if p.is_dir() {
            holding(&p, needle, out);
        } else if std::fs::read(&p)
            .unwrap()
            .windows(needle.len())
            .any(|w| w == needle)
        {
            out.push(p.display().to_string());
        }
    }
}

#[test]
fn a_secret_a_provider_holds_configures_another_and_stays_in_memory() {
    let s = project();
    // Tick 1 makes the token; the provider it configures waits for it.
    let plan = dform(&s, &["plan", "p"]).success();
    assert!(plan.stdout.contains("+ vault.token t"), "{}", plan.stdout);
    assert!(
        plan.stdout.contains("waits on  provider fake"),
        "{}",
        plan.stdout
    );
    // Apply: at the boundary the vault reveals the token into the plugin's
    // Configure, and tick 2 makes what it serves.
    let r = dform(&s, &["apply", "p"]).success();
    assert!(
        r.stdout.ends_with("apply: complete\n"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
    let state = s.read("dform.state/p/state.json");
    assert!(state.contains("db.postgres::server"), "{state}");

    // The value is where its provider keeps it, its world.
    let world: serde_json::Value =
        serde_json::from_str(&s.read("dform.state/p/remote.json")).unwrap();
    let token = world["resources"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(k, _)| k.starts_with("vault.token::"))
        .and_then(|(_, o)| o["computed"]["value"].as_str())
        .expect("the token's value in the vault's world")
        .to_string();
    // And nowhere else: not in state, the audit log, the plugin's world;
    // in no output.
    let mut found = Vec::new();
    holding(&s.dir, token.as_bytes(), &mut found);
    assert_eq!(
        found,
        [s.path("dform.state/p/remote.json").display().to_string()],
        "{token}"
    );
    for out in [&plan.stdout, &plan.stderr, &r.stdout, &r.stderr] {
        assert!(!out.contains(&token), "{out}");
    }

    // The plugin got those bytes: they are the account it reports, which
    // the program expects (a literal here, as a test may).
    s.write(
        "stacks/p.df",
        &stack(&format!(", expect_account = \"{token}\"")),
    );
    let r = dform(&s, &["plan", "p"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    s.write("stacks/p.df", &stack(", expect_account = \"someone-else\""));
    let r = dform(&s, &["plan", "p"]).failure();
    assert!(
        r.stderr.contains("expect_account") && !r.stderr.contains(&token),
        "{}",
        r.stderr
    );
}

/// A reveal is the engine's call: a provider refuses one with no lease,
/// and one for a secret it does not hold, naming the object and path.
#[test]
fn a_reveal_without_the_lease_or_of_another_s_secret_is_refused() {
    use dform::plugin::backend::{Call, Handler, silent};
    use dform::plugin::pb;
    let mock = dform_mock::Mock::process();
    let held = pb::Held {
        provider: "fakecloud".into(),
        deployment: "p".into(),
        r#type: "vault.token".into(),
        remote: "t-1".into(),
        path: "value".into(),
        digest: String::new(),
    };
    let ask = |held: pb::Held, lease: &str| {
        mock.handle(
            Call::Reveal(pb::RevealRequest {
                held: Some(held),
                lease: lease.into(),
            }),
            &silent,
        )
        .unwrap_err()
        .to_string()
    };
    let e = ask(held.clone(), "");
    assert_eq!(
        e,
        "reveal vault.token t-1#value: refused without the deployment's lease (a reveal is the \
         engine's call)"
    );
    let e = ask(
        pb::Held {
            provider: "ovh".into(),
            ..held
        },
        "someone pid 1",
    );
    assert_eq!(
        e,
        "reveal vault.token t-1#value: it is held by ovh, not fakecloud"
    );
}

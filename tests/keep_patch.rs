//! `keep` (R-164): an update whose write-only secret a run without the
//! deployment's master proved unchanged is made without it, the secret
//! left as the object has it, when the provider can (its `keep`
//! capability): the mock, and the OVH provider against its fake API
//! (an instance's `user_data` is never sent again). A provider that
//! cannot keep it stops the update as before (tests/nokey_apply.rs).

mod common;
use common::{Run, Scratch, dform, repo, yes};
use dform_provider_ovh::fake::{self, Server};

const PASS: (&str, &str) = ("DFORM_TEST_PASSPHRASE", "correct horse battery staple");

fn run(s: &Scratch, env: &[(&str, String)], args: &[&str]) -> Run {
    let mut c = dform();
    c.args(yes(args))
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"))
        .env_remove("RANDOM_MASTER")
        .env_remove("DFORM_TEST_PASSPHRASE")
        .env_remove("OVH_CLOUD_PROJECT_SERVICE");
    for (k, v) in env {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

fn pass() -> (&'static str, String) {
    (PASS.0, PASS.1.to_string())
}

const SECRETS: &str = "\n[secrets]\npassphrase = \"env:DFORM_TEST_PASSPHRASE\"\n";

/// A vm whose write-only `user_data` holds a token only the master
/// derives; its weight changes without the master.
#[test]
fn the_mock_keeps_a_write_only_secret_on_an_update_without_the_master() {
    let s = Scratch::project("keep-mock");
    s.write("dform.toml", &(s.read("dform.toml") + SECRETS));
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_attr(compute.vm, \"user_data\", \"string\", [\"write_only\", \"force_new\", \"sensitive\"])\n"),
    );
    let program = |weight: u32| {
        format!(
            r##"use fake
let token = random.password("k3s")
resource compute.vm a {{
  name = "a"
  weight = {weight}
  user_data = "#cloud-config token: ${{token}}"
}}
"##
        )
    };
    s.write("stacks/p.df", &program(1));
    run(&s, &[pass()], &["apply", "p"]).success();
    let world = || s.json("dform.state/p/remote.json")["resources"]["compute.vm::a"].clone();
    let user_data = world()["attrs"]["user_data"].clone();
    assert!(
        user_data
            .as_str()
            .is_some_and(|u| u.starts_with("#cloud-config token: ")),
        "{user_data}"
    );
    s.write("stacks/p.df", &program(2));
    let r = run(&s, &[], &["plan", "p"]).success();
    assert!(
        r.stdout
            .lines()
            .any(|l| l.contains("~ compute.vm a") && l.ends_with("secrets unchanged")),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("needs the key"), "{}", r.stdout);
    let r = run(&s, &[], &["apply", "p"]).success();
    assert!(!r.stderr.contains("stopped"), "{}", r.stderr);
    let w = world();
    assert_eq!(w["attrs"]["weight"], 2, "{w}");
    // The token the master derives, left as it was: not a stand-in.
    assert_eq!(w["attrs"]["user_data"], user_data, "{w}");
    for env in [vec![], vec![pass()]] {
        let r = run(&s, &env, &["plan", "p"]).success();
        assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    }
    // A provider without the capability: the update needs the master.
    s.write("stacks/p.df", &program(3));
    let no_keep = ("DFORM_TEST_FAKE_NO_KEEP", "1".to_string());
    let r = run(&s, std::slice::from_ref(&no_keep), &["plan", "p"]).success();
    assert!(
        r.stdout
            .contains("secrets unchanged, a write-only one needs the key"),
        "{}",
        r.stdout
    );
    run(&s, &[no_keep], &["apply", "p"]).stopped();
    assert_eq!(world()["attrs"]["weight"], 2);
}

/// The OVH provider: an instance renamed by an operator without the
/// passphrase; its user data (a token the master derives) is not sent
/// again, and no PUT carries it.
#[test]
fn ovh_renames_an_instance_without_resending_its_user_data() {
    let server = Server::start();
    server.build_polls(1);
    let s = Scratch::project("keep-ovh");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\novh = {{ path = \"{}\" }}\n{SECRETS}\n[stacks.main]\n",
            common::exe("dform-provider-ovh")
        ),
    );
    let program = |name: &str| {
        format!(
            r##"
use ovh {{ endpoint = "{}", project = "{}" }}

let token = random.password("k3s")

resource ovh.ssh_key admin {{ name = "lab-admin", public_key = "ssh-ed25519 AAAAC3Nz lab" }}

resource ovh.instance server {{
  name = "{name}"
  region = "ca-east-tor"
  flavor = "b2-7"
  image = "Ubuntu 24.04"
  ssh_key = admin
  user_data = "#cloud-config token: ${{token}}"
}}
"##,
            server.endpoint,
            fake::DESCRIPTION
        )
    };
    let env = |with: bool| {
        let mut e: Vec<(&str, String)> = server.env();
        if with {
            e.push(pass());
        }
        e
    };
    s.write("main.df", &program("lab-server"));
    run(&s, &env(true), &["apply"]).success();
    // The create sent it (the fake keeps no user data: it is in the call).
    let sent = |from: usize| -> Vec<String> {
        server.seen()[from..]
            .iter()
            .filter_map(|c| c.body["userData"].as_str().map(String::from))
            .collect()
    };
    let made = sent(0);
    assert!(
        made.len() == 1 && made[0].starts_with("#cloud-config token: "),
        "{made:?}"
    );
    s.write("main.df", &program("lab-server-2"));
    let r = run(&s, &env(false), &["plan"]).success();
    assert!(
        r.stdout
            .lines()
            .any(|l| l.contains("~ ovh.instance server") && l.ends_with("secrets unchanged")),
        "{}",
        r.stdout
    );
    let before = server.seen().len();
    run(&s, &env(false), &["apply"]).success();
    let now = server.instances();
    assert_eq!(now.len(), 1, "renamed in place: {now:?}");
    assert_eq!(now[0]["name"], "lab-server-2");
    assert_eq!(
        sent(before),
        Vec::<String>::new(),
        "the update sent no user data"
    );
    assert!(
        server.calls()[before..]
            .iter()
            .any(|c| c.starts_with("PUT ") && c.contains("/instance/")),
        "{:?}",
        server.calls()
    );
    let r = run(&s, &env(false), &["plan"]).success();
    assert_eq!(r.summary(), "stack main is up to date", "{}", r.stdout);
}

//! The Tailscale provider (`dform-provider-tailscale`, R-197) against a
//! fake Tailscale API (`dform_provider_tailscale::fake`): the policy file
//! round-trips through HuJSON and a console edit is drift; an auth key's
//! value is held by the provider and never reaches state, the plan file,
//! the audit log or a message; a device is adopted by its hostname and
//! tagged, a create of one is refused, a hostname two devices have is
//! refused naming both; DNS splits a domain; destroy writes the policy
//! file's and the DNS settings' defaults and says so; `dform status`
//! shows a device's health. The provider's credential is an OAuth client
//! by Tailscale's own names.

mod common;
use common::{Run, Scratch};
use dform_provider_tailscale::fake::{self, Server};
use serde_json::{Value as Json, json};

const TAILNET: &str = "vodik.github";

fn tailscale() -> String {
    common::exe("dform-provider-tailscale")
}

/// `dform ARGS` in `s`, the OAuth client the fake knows in the
/// environment and no other credential.
fn dform(s: &Scratch, args: &[&str]) -> Run {
    let mut c = common::dform();
    c.args(common::yes(args))
        .current_dir(&s.dir)
        .env("DFORM_CREDENTIALS", s.path("credentials"))
        .env("RANDOM_MASTER", "tailscale-master")
        .env("TAILSCALE_OAUTH_CLIENT_ID", fake::CLIENT_ID)
        .env("TAILSCALE_OAUTH_CLIENT_SECRET", fake::CLIENT_SECRET);
    for k in [
        "TAILSCALE_API_KEY",
        "TAILSCALE_TAILNET",
        "TAILSCALE_BASE_URL",
    ] {
        c.env_remove(k);
    }
    Run::from(c.output().unwrap())
}

/// A project naming the provider, its program `use tailscale` pointed at
/// `server`, then `body`.
fn project(name: &str, server: &Server, body: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\nfake = \"fake\"\n\
             tailscale = {{ path = \"{}\" }}\n",
            tailscale()
        ),
    );
    write(&s, server, body);
    s
}

fn write(s: &Scratch, server: &Server, body: &str) {
    s.write(
        "main.df",
        &format!(
            "use tailscale {{ tailnet = \"{TAILNET}\", base_url = \"{}\" }}\n{body}",
            server.address()
        ),
    );
}

/// The policy the user's platform stack writes: tag:admin reaches tag:k8s
/// on 22 and 6443 only.
const POLICY: &str = r#"
resource tailscale.acl policy {
  policy = json.encode({
    tagOwners: { "tag:k8s": ["autogroup:admin"], "tag:admin": ["autogroup:admin"] },
    acls: [{ action: "accept", src: ["tag:admin"], dst: ["tag:k8s:22,6443"] }],
  })
}
"#;

fn canonical(text: &str) -> String {
    dform_provider_tailscale::hujson::canonical(text).unwrap()
}

/// The policy file is written as the program renders it; Read answers it
/// in the same form, so a second plan is clean; a console edit that only
/// adds comments is no change, one that changes a rule is drift, and the
/// next apply writes the program's policy back over it with `If-Match`.
#[test]
fn the_policy_file_round_trips_and_a_console_edit_is_drift() {
    let server = Server::start(TAILNET);
    let s = project("ts-acl", &server, POLICY);
    let plan = dform(&s, &["plan", "main.df"]).success();
    assert!(
        plan.stdout.contains("+ tailscale.acl policy"),
        "{}",
        plan.stdout
    );
    dform(&s, &["apply", "main.df"]).success();
    let written = canonical(&server.policy());
    assert!(
        written.contains(r#""dst":["tag:k8s:22,6443"],"src":["tag:admin"]"#),
        "{written}"
    );
    let again = dform(&s, &["plan", "main.df"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);

    // Comments, another order, a trailing comma: the same policy.
    server.console_edit(
        r#"{
  // edited in the console
  "acls": [{"src": ["tag:admin"], "action": "accept", "dst": ["tag:k8s:22,6443"],},],
  "tagOwners": {"tag:admin": ["autogroup:admin"], "tag:k8s": ["autogroup:admin"]},
}"#,
    );
    let again = dform(&s, &["plan", "main.df"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);

    // A rule changed in the console is drift.
    server.console_edit(
        r#"{"acls": [{"action": "accept", "src": ["*"], "dst": ["*:*"]}], "tagOwners": {}}"#,
    );
    let drift = dform(&s, &["plan", "main.df"]).success();
    assert!(
        drift.stdout.contains("tailscale.acl policy"),
        "{}",
        drift.stdout
    );
    assert!(drift.stdout.contains("policy"), "{}", drift.stdout);
    assert!(!drift.stdout.contains("is up to date"), "{}", drift.stdout);
    dform(&s, &["apply", "main.df"]).success();
    assert_eq!(canonical(&server.policy()), written);
    let post = server
        .seen()
        .into_iter()
        .filter(|c| c.method == "POST" && c.path.ends_with("/acl"))
        .last()
        .unwrap();
    assert!(post.if_match.is_some(), "{post:?}");
    let again = dform(&s, &["plan", "main.df"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);
}

/// A tailnet whose policy file somebody wrote is not taken over by a
/// create: adopt it. A policy not in the form the provider compares is
/// refused at the plan, naming the fix.
#[test]
fn a_written_policy_file_is_adopted_not_overwritten() {
    let server = Server::start(TAILNET);
    server
        .console_edit(r#"{"acls": [{"action": "accept", "src": ["group:ops"], "dst": ["*:22"]}]}"#);
    let s = project("ts-acl-adopt", &server, POLICY);
    let r = dform(&s, &["apply", "main.df"]).failure();
    assert!(
        r.stderr
            .contains("the tailnet's policy file is not its default: someone wrote it")
            && r.stderr
                .contains(&format!("adopt(RESOURCE, \"{TAILNET}\")")),
        "{}",
        r.stderr
    );
    assert!(server.policy().contains("group:ops"));
    write(
        &s,
        &server,
        &format!("{POLICY}adopt(policy, \"{TAILNET}\")\n"),
    );
    let plan = dform(&s, &["plan", "main.df"]).success();
    assert!(
        plan.stdout.contains("> tailscale.acl policy"),
        "{}",
        plan.stdout
    );
    dform(&s, &["apply", "main.df"]).success();
    assert!(server.policy().contains("tag:k8s:22,6443"));

    write(
        &s,
        &server,
        &format!(
            "resource tailscale.acl policy {{ policy = \"{{ \\\"acls\\\": [] }}\" }}\nadopt(policy, \"{TAILNET}\")\n"
        ),
    );
    let r = dform(&s, &["plan", "main.df"]).failure();
    assert!(
        r.stderr
            .contains("policy is not in the form the provider compares")
            && r.stderr.contains("json.encode"),
        "{}",
        r.stderr
    );
}

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
use serde_json::json;

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

/// Every file under `s` and the runs' output: none holds `secret`.
#[track_caller]
fn nowhere(s: &Scratch, runs: &[&Run], secret: &str) {
    nowhere_but(s, runs, secret, "");
}

/// The same, but the file `but` (a world the key was written to).
#[track_caller]
fn nowhere_but(s: &Scratch, runs: &[&Run], secret: &str, but: &str) {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            match p.is_dir() {
                true => walk(&p, out),
                false => out.push(p),
            }
        }
    }
    let mut files = Vec::new();
    walk(&s.dir, &mut files);
    for f in files.into_iter().filter(|f| *f != s.path(but)) {
        let bytes = std::fs::read(&f).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(secret),
            "{} holds the key",
            f.display()
        );
    }
    for r in runs {
        assert!(
            !r.stdout.contains(secret) && !r.stderr.contains(secret),
            "{}{}",
            r.stdout,
            r.stderr
        );
    }
}

/// The key the user's platform stack hands its nodes: tagged,
/// preauthorized, ephemeral, reusable.
const KEY: &str = r#"
resource tailscale.auth_key nodes {
  description = "k3s nodes"
  reusable = true
  ephemeral = true
  preauthorized = true
  tags = ["tag:k8s"]
  expiry = 7d
}
"#;

/// The key is made by the plan file's apply, its value answered once to
/// the provider: it reaches no file of the project's (state, the plan
/// file, the audit log) and no output, which say its label. A second plan
/// is clean; a change of a tag makes a new key first and revokes the old.
#[test]
fn an_auth_key_is_held_and_never_written() {
    let server = Server::start(TAILNET);
    let s = project("ts-key", &server, KEY);
    let plan = dform(&s, &["plan", "--out", "plan.json", "main.df"]).success();
    assert!(
        plan.stdout.contains("+ tailscale.auth_key nodes"),
        "{}",
        plan.stdout
    );
    let apply = dform(&s, &["apply", "plan.json"]).success();
    let keys = server.keys();
    assert_eq!(keys.len(), 1, "{keys:?}");
    let create = &keys[0]["capabilities"]["devices"]["create"];
    assert_eq!(create["tags"], json!(["tag:k8s"]));
    assert_eq!(
        (
            &create["reusable"],
            &create["ephemeral"],
            &create["preauthorized"]
        ),
        (&json!(true), &json!(true), &json!(true))
    );
    assert_eq!(
        keys[0]["expires"],
        json!(fake::rfc3339(fake::NOW + 7 * 86400))
    );
    let state = s.json("dform.state/main/state.json");
    let entry = &state["resources"]["tailscale.auth_key::nodes"];
    assert_eq!(entry["remote"], keys[0]["id"], "{state}");
    let again = dform(&s, &["plan", "main.df"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);
    let shown = dform(&s, &["state", "show", "main.df"]).success();
    let json = dform(&s, &["plan", "--json", "main.df"]).success();
    let value = server.issued().pop().unwrap();
    nowhere(&s, &[&plan, &apply, &again, &shown, &json], &value);

    write(&s, &server, &KEY.replace("tag:k8s", "tag:node"));
    let plan = dform(&s, &["plan", "main.df"]).success();
    assert!(
        plan.stdout.contains("tailscale.auth_key nodes"),
        "{}",
        plan.stdout
    );
    assert!(plan.stdout.contains("replace"), "{}", plan.stdout);
    let apply = dform(&s, &["apply", "main.df"]).success();
    let keys = server.keys();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert_eq!(keys[0]["revoked"], json!(true), "{keys:?}");
    assert_eq!(keys[1]["revoked"], json!(false), "{keys:?}");
    for v in server.issued() {
        nowhere(&s, &[&plan, &apply], &v);
    }
}

/// The engine reveals the key to the provider that takes it, inside the
/// call (here the mock's Configure, its `account`): the run that made the
/// key has it, the value reaches no file and no output. A later run has
/// no copy (the API answers a key once): its reveal is refused saying so,
/// and how to make a new one.
#[test]
fn a_key_is_revealed_only_in_the_run_that_made_it() {
    let server = Server::start(TAILNET);
    let s = project(
        "ts-reveal",
        &server,
        &format!(
            "{KEY}use fake {{ account = nodes.key }}\n\
             resource net.vpc v {{ cidr_block = \"10.0.0.0/16\" }}\n"
        ),
    );
    let apply = dform(&s, &["apply", "main.df"]).success();
    assert!(
        apply
            .stdout
            .contains("provider fake: configured after tick 1: account = (sensitive)"),
        "{}",
        apply.stdout
    );
    assert!(
        apply.stderr.contains("+ net.vpc v  made"),
        "{}",
        apply.stderr
    );
    let value = server.issued().pop().unwrap();
    nowhere(&s, &[&apply], &value);
    let later = dform(&s, &["plan", "main.df"]).failure();
    assert!(
        later
            .stderr
            .contains("provider tailscale reveals the secret tailscale.auth_key[\"nodes\"].key")
            && later.stderr.contains(
                "the API answers a key once, when it is made, and this run did not make it"
            ),
        "{}",
        later.stderr
    );
}

/// The done-when's cloud-init: the key in a mock instance's `user_data`,
/// read by the mock inside its Apply, `bootstrap` so a new key does not
/// replace the instance.
#[test]
fn a_key_reaches_an_instances_user_data_inside_the_call() {
    let server = Server::start(TAILNET);
    let s = project(
        "ts-user-data",
        &server,
        &format!(
            "{KEY}use cloud\n\
             resource cloud.vm node {{\n  name = \"k3s-1\"\n  \
             user_data = \"#cloud-config\\nruncmd: [tailscale up --authkey ${{nodes.key}}]\\n\"\n}}\n\
             lifecycle(node, \"bootstrap\", \"user_data\")\n"
        ),
    );
    // The mock plays an instance whose user data is sensitive.
    s.write(
        "providers/cloud/schema.df",
        "type_provider(cloud.vm, \"fakecloud\")\n\
         type_attr(cloud.vm, \"id\", \"string\", [\"computed\", \"id\"])\n\
         type_attr(cloud.vm, \"name\", \"string\", [\"required\"])\n\
         type_attr(cloud.vm, \"user_data\", \"string\", [\"sensitive\", \"force_new\"])\n",
    );
    let apply = dform(&s, &["apply", "main.df"]).success();
    let value = server.issued().pop().unwrap();
    let world = s.read("dform.state/main/remote.json");
    assert!(world.contains(&value), "{world}");
    let state = s.read("dform.state/main/state.json");
    assert!(!state.contains(&value), "{state}");
    assert!(
        apply.stdout.contains("user_data = (sensitive)"),
        "{}",
        apply.stdout
    );
    // A later run needs no reveal: the key is given at creation only.
    let again = dform(&s, &["plan", "main.df"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);
    // Not so given, a new template makes the node again: the key exists
    // in the run that made it only, so its reveal is refused, an error
    // at the attribute, and nothing is sent.
    write(
        &s,
        &server,
        &format!(
            "{KEY}use cloud\n\
             resource cloud.vm node {{\n  name = \"k3s-1\"\n  \
             user_data = \"#cloud-config\\nruncmd: [tailscale up --ssh --authkey ${{nodes.key}}]\\n\"\n}}\n"
        ),
    );
    let r = dform(&s, &["apply", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "! apply cloud.vm node: not sent\n    cloud.vm node.user_data holds the secret \
             tailscale.auth_key[\"nodes\"].key, which was not revealed: "
        ) && r
            .stderr
            .contains("the API answers a key once, when it is made, and this run did not make it"),
        "{}",
        r.stderr
    );
    nowhere_but(
        &s,
        &[&apply, &again, &r],
        &value,
        "dform.state/main/remote.json",
    );
}

/// The node's key, made by a first apply; the node joins with it as
/// `hostname` (as `tailscale up --authkey` does): its device id.
fn joined(s: &Scratch, server: &Server, hostname: &str) -> String {
    dform(s, &["apply", "main.df"]).success();
    let value = server.issued().pop().unwrap();
    server.join(&value, hostname).unwrap()
}

const DEVICE: &str = r#"
resource tailscale.device server {
  hostname = "k3s-1"
  tags = ["tag:k8s", "tag:admin"]
  routes = ["10.0.0.0/24"]
}
adopt(server, "k3s-1")
"#;

/// A device joined with the key is adopted by its hostname and kept by its
/// id; its tags and approved routes are written; a second plan is clean;
/// `dform status` says it is connected, and degraded once it is not.
#[test]
fn a_device_is_adopted_by_its_hostname_and_tagged() {
    let server = Server::start(TAILNET);
    let s = project("ts-device", &server, KEY);
    let id = joined(&s, &server, "k3s-1");
    server.advertise(&id, &["10.0.0.0/24", "10.1.0.0/24"]);
    assert_eq!(server.device(&id).unwrap()["tags"], json!(["tag:k8s"]));
    write(&s, &server, &format!("{KEY}{DEVICE}"));
    let plan = dform(&s, &["plan", "main.df"]).success();
    assert!(
        plan.stdout.contains("> tailscale.device server"),
        "{}",
        plan.stdout
    );
    dform(&s, &["apply", "main.df"]).success();
    let d = server.device(&id).unwrap();
    assert_eq!(d["tags"], json!(["tag:k8s", "tag:admin"]));
    assert_eq!(d["enabledRoutes"], json!(["10.0.0.0/24"]));
    let state = s.json("dform.state/main/state.json");
    assert_eq!(
        state["resources"]["tailscale.device::server"]["remote"],
        json!(id),
        "{state}"
    );
    let again = dform(&s, &["plan", "main.df"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);

    let status = dform(&s, &["status", "main.df"]).success();
    assert!(
        status.stdout.contains("tailscale.device server")
            && status.stdout.contains("healthy")
            && status.stdout.contains("connected at 100.64.0.1"),
        "{}",
        status.stdout
    );
    server.disconnect(&id);
    let status = dform(&s, &["status", "main.df"]);
    assert_eq!(status.code, Some(1), "{}{}", status.stdout, status.stderr);
    assert!(
        status.stdout.contains("degraded") && status.stdout.contains("not connected, last seen"),
        "{}",
        status.stdout
    );
}

/// A device removed from the program is let go, not removed from the
/// tailnet: its type's lifecycle is `retain` (`type_lifecycle`), which
/// the plan shows as a program's `retain`; `why` says the provider's
/// schema answered it.
#[test]
fn a_device_removed_from_the_program_stays_on_the_tailnet() {
    let server = Server::start(TAILNET);
    let s = project("ts-device-retain", &server, KEY);
    let id = joined(&s, &server, "k3s-1");
    write(&s, &server, &format!("{KEY}{DEVICE}"));
    dform(&s, &["apply", "main.df"]).success();
    let why = dform(&s, &["why", "lifecycle(server, \"retain\")", "main.df"]).success();
    assert!(
        why.stdout.contains(
            "dform  the schema of provider tailscale: each tailscale.device is \"retain\""
        ),
        "{}",
        why.stdout
    );
    write(&s, &server, KEY);
    let plan = dform(&s, &["plan", "main.df"]).success();
    assert!(
        plan.stdout.contains(
            "~ tailscale.device server  forgotten, kept in the world  (lifecycle retain)"
        ),
        "{}",
        plan.stdout
    );
    dform(&s, &["apply", "main.df"]).success();
    assert!(server.device(&id).is_some());
    let state = s.json("dform.state/main/state.json");
    assert!(
        state["resources"].get("tailscale.device::server").is_none(),
        "{state}"
    );
}

/// A device a program would create is refused at the plan, naming the
/// adopt; a hostname two devices have is refused naming both ids.
#[test]
fn a_device_is_not_created_and_a_shared_hostname_is_refused() {
    let server = Server::start(TAILNET);
    let s = project("ts-device-refused", &server, KEY);
    let first = joined(&s, &server, "k3s-1");
    write(
        &s,
        &server,
        &format!("{KEY}{}", DEVICE.replace("adopt(server, \"k3s-1\")", "")),
    );
    let r = dform(&s, &["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains("tailscale.device server")
            && r.stderr
                .contains("a device joins the tailnet with an auth key; adopt it"),
        "{}",
        r.stderr
    );
    // The node replaced: a second device has its hostname while the first
    // is still listed.
    let second = server
        .join(&server.issued().pop().unwrap(), "k3s-1")
        .unwrap();
    write(&s, &server, &format!("{KEY}{DEVICE}"));
    let r = dform(&s, &["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains("2 devices have the hostname \"k3s-1\"")
            && r.stderr.contains(&first)
            && r.stderr.contains(&second),
        "{}",
        r.stderr
    );
}

const DNS: &str = r#"
resource tailscale.dns dns {
  nameservers = ["1.1.1.1"]
  search_paths = ["home.vodik.xyz"]
  split = { "home.vodik.xyz": ["192.168.1.1"] }
}
"#;

/// DNS splits a domain: its nameservers, search paths and split domain
/// written, MagicDNS left as the tailnet has it, a second plan clean; a
/// split domain changed in place.
#[test]
fn dns_splits_a_domain() {
    let server = Server::start(TAILNET);
    let s = project("ts-dns", &server, DNS);
    let plan = dform(&s, &["plan", "main.df"]).success();
    assert!(
        plan.stdout.contains("+ tailscale.dns dns"),
        "{}",
        plan.stdout
    );
    dform(&s, &["apply", "main.df"]).success();
    assert_eq!(
        server.dns(),
        json!({
            "nameservers": ["1.1.1.1"],
            "magicDNS": true,
            "searchPaths": ["home.vodik.xyz"],
            "split": {"home.vodik.xyz": ["192.168.1.1"]},
        })
    );
    let again = dform(&s, &["plan", "main.df"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);
    write(&s, &server, &DNS.replace("192.168.1.1", "192.168.1.2"));
    let plan = dform(&s, &["plan", "main.df"]).success();
    assert!(
        plan.stdout.contains("~ tailscale.dns dns"),
        "{}",
        plan.stdout
    );
    dform(&s, &["apply", "main.df"]).success();
    assert_eq!(
        server.dns()["split"],
        json!({"home.vodik.xyz": ["192.168.1.2"]})
    );
}

/// Destroy removes what it can and writes the defaults of what a tailnet
/// always has, saying so: the policy file and the DNS settings; the key
/// is revoked.
#[test]
fn destroy_writes_the_defaults_and_says_so() {
    let server = Server::start(TAILNET);
    let s = project("ts-destroy", &server, &format!("{POLICY}{DNS}{KEY}"));
    dform(&s, &["apply", "main.df"]).success();
    assert!(server.policy().contains("tag:k8s"));
    let r = dform(&s, &["destroy", "--yes", "main.df"]).success();
    let said = format!("{}{}", r.stdout, r.stderr);
    for line in [
        "tailscale.acl \"vodik.github\": the tailnet's policy file is its default again",
        "tailscale.dns \"vodik.github\": the tailnet's DNS settings are its defaults again",
    ] {
        assert!(said.contains(line), "{line}\n{said}");
    }
    assert_eq!(canonical(&server.policy()), canonical(fake::DEFAULT_POLICY));
    assert_eq!(
        server.dns(),
        json!({"nameservers": [], "magicDNS": true, "searchPaths": [], "split": {}})
    );
    assert_eq!(server.keys()[0]["revoked"], json!(true));
}

/// The tailnet's users, a data source.
#[test]
fn the_users_are_a_data_source() {
    let server = Server::start(TAILNET);
    let s = project(
        "ts-users",
        &server,
        &format!(
            "deny \"${{l}} owns the tailnet\" where tailscale.user(\"{TAILNET}\", l, \"owner\")\n"
        ),
    );
    let r = dform(&s, &["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains("simon@example.com owns the tailnet"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("alice@example.com"), "{}", r.stderr);
}

/// The devices the tailnet lists, adopted or not, as rows a policy reads
/// (R-196, the data source under the type's name): a node that did not
/// join is a deny. `d in tailscale.device` is still the program's own
/// devices: none here, though the tailnet lists one.
#[test]
fn a_node_not_on_the_tailnet_is_a_deny() {
    let server = Server::start(TAILNET);
    server.add_device("k3s-1");
    let s = project(
        "ts-listing",
        &server,
        &format!(
            "deny \"k3s-2 is not on the tailnet\" where not {{ tailscale.device(\"{TAILNET}\", \"k3s-2\", _, _, _, _, _, _) }}\n\
             deny \"${{d}} is adopted\" where d in tailscale.device\n"
        ),
    );
    let r = dform(&s, &["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains("- k3s-2 is not on the tailnet\n") && !r.stderr.contains("is adopted"),
        "{}",
        r.stderr
    );
    server.add_device("k3s-2");
    dform(&s, &["plan", "main.df"]).success();
}

/// The listing read with the rest of its columns left out, as a pattern's
/// rest elsewhere is written.
#[test]
#[ignore = "R-196: `..` in a relation's arguments is not a rest yet (\"`..` is a spread\"): \
            each column is written, `_` for the ones not read; the resolver's to lift"]
fn the_listing_reads_with_a_rest() {
    let server = Server::start(TAILNET);
    server.add_device("k3s-1");
    let s = project(
        "ts-listing-rest",
        &server,
        &format!(
            "deny \"k3s-2 is not on the tailnet\" where not {{ tailscale.device(\"{TAILNET}\", \"k3s-2\", ..) }}\n"
        ),
    );
    let r = dform(&s, &["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains("- k3s-2 is not on the tailnet\n"),
        "{}",
        r.stderr
    );
}

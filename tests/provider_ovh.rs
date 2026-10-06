//! The OVH provider (`dform-provider-ovh`, R-44) against a fake OVH API
//! (`dform_provider_ovh::fake`): the conformance suite, and a program
//! planned, applied, planned clean, changed and destroyed, named in
//! dform.toml by `path =`. The provider's credentials come from the
//! environment the fake gives; HOME and XDG_CONFIG_HOME point into the
//! scratch directory, so no real `ovh.conf` is read.
//!
//! `OVH_INTEGRATION=1` runs one more test against the real account the
//! machine's own configuration names: it lists the regions, and changes
//! nothing.

mod common;
use common::{Run, Scratch};
use dform_provider_ovh::fake::{self, Server};
use serde_json::Value as Json;

fn ovh() -> String {
    common::exe("dform-provider-ovh")
}

/// `dform ARGS` in the scratch project, the provider pointed at `server`.
fn dform(s: &Scratch, server: &Server, args: &[&str]) -> Run {
    let mut c = common::dform();
    c.args(common::yes(args))
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"))
        .env_remove("OVH_CLOUD_PROJECT_SERVICE");
    for (k, v) in server.env() {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

fn project(name: &str, providers: &str, program: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\novh = {{ path = \"{}\"{providers} }}\n",
            ovh()
        ),
    );
    s.write("main.df", program);
    s
}

/// The program: a key, an instance with it and its user data, a record
/// of its address.
fn program(server: &Server, user_data: &str, flavor: &str) -> String {
    format!(
        r#"
use ovh {{ endpoint = "{}", project = "{}" }}

resource ovh.ssh_key admin {{ name = "lab-admin", public_key = "ssh-ed25519 AAAAC3Nz lab" }}

resource ovh.instance server {{
  name = "lab-server"
  region = "ca-east-tor"
  flavor = "{flavor}"
  image = "Ubuntu 24.04"
  ssh_key = admin
  user_data = "{user_data}"
}}

resource ovh.domain_record www {{
  zone = "example.com"
  subdomain = "www"
  type = "A"
  target = server.public_ip
}}
"#,
        server.endpoint,
        fake::DESCRIPTION
    )
}

fn names(objects: &[Json]) -> Vec<String> {
    let mut n: Vec<String> = objects
        .iter()
        .map(|o| o["name"].as_str().unwrap_or_default().to_string())
        .collect();
    n.sort();
    n
}

fn posts(server: &Server, path: &str) -> usize {
    server
        .calls()
        .iter()
        .filter(|c| c.starts_with("POST ") && c.ends_with(path))
        .count()
}

#[test]
fn provider_check_conforms_against_the_fake_api() {
    let server = Server::start();
    let s = Scratch::new("ovh-check");
    let mut c = common::dform();
    c.args(["provider", "check", &ovh()])
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"))
        .env("OVH_CLOUD_PROJECT_SERVICE", fake::PROJECT)
        .env("OVH_CHECK_REGION", "BHS5");
    for (k, v) in server.env() {
        c.env(k, v);
    }
    let r = Run::from(c.output().unwrap()).success();
    assert!(!r.stdout.contains("FAIL"), "{}", r.stdout);
    for check in [
        "ok    Schema serves its own types, with examples",
        "ok    Schema's types are named under the provider's name, ovh",
        "ok    Plan refuses a document without a required attribute",
        "ok    Plan marks a sensitive attribute sensitive",
        "ok    Apply CREATE again with the same idempotency key answers the object it made",
        "ok    Query provider.created answers what an idempotency key made",
        "ok    Apply UPDATE changes the object in place",
        "ok    Apply DELETE removes the object",
    ] {
        assert!(r.stdout.contains(check), "{check}\n{}", r.stdout);
    }
    assert!(server.instances().is_empty(), "{:?}", server.instances());
}

/// Planning reads the account and changes nothing: what is new is a
/// create; an instance made elsewhere, adopted by its id, is read.
#[test]
fn a_program_plans_against_the_account() {
    let server = Server::start();
    let s = project(
        "ovh-plan",
        "",
        &program(&server, "#cloud-config\\n", "b2-7"),
    );
    let plan = dform(&s, &server, &["plan", "main.df"]).success();
    for line in [
        "+ ovh.ssh_key admin",
        "+ ovh.instance server",
        "+ ovh.domain_record www",
        "user_data = (sensitive)",
        // A reference is the address it names; a value the provider
        // computes is the expression that reads it (R-111).
        "ssh_key = admin",
        "target = server.public_ip",
    ] {
        assert!(plan.stdout.contains(line), "{line}\n{}", plan.stdout);
    }
    assert!(server.instances().is_empty() && server.keys().is_empty());
    assert!(
        server.calls().iter().all(|c| c.starts_with("GET ")),
        "{:?}",
        server.calls()
    );

    let id = server.add_instance("old", "BHS5");
    s.write(
        "main.df",
        &format!(
            "use ovh {{ endpoint = \"{}\", project = \"lab\" }}\n\
             resource ovh.instance old {{\n  name = \"old\"\n  region = \"BHS5\"\n  \
             flavor = \"b2-7\"\n  image = \"Ubuntu 24.04\"\n}}\n\
             adopt(old, \"{id}\")\n",
            server.endpoint
        ),
    );
    let plan = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(
        plan.stdout.contains("> ovh.instance old"),
        "{}",
        plan.stdout
    );
    // Adopting it changes nothing there.
    dform(&s, &server, &["apply", "main.df"]).success();
    assert_eq!(names(&server.instances()), ["old"]);
    assert!(
        server.calls().iter().all(|c| c.starts_with("GET ")),
        "{:?}",
        server.calls()
    );
}

#[test]
fn a_program_plans_applies_and_plans_clean() {
    let server = Server::start();
    server.build_polls(3);
    let s = project(
        "ovh-apply",
        "",
        &program(&server, "#cloud-config\\n", "b2-7"),
    );
    let plan = dform(&s, &server, &["plan", "main.df"]).success();
    for line in [
        "ovh.ssh_key admin",
        "ovh.instance server",
        "ovh.domain_record www",
    ] {
        assert!(plan.stdout.contains(line), "{line}\n{}", plan.stdout);
    }
    // Planning creates nothing.
    assert!(server.instances().is_empty() && server.keys().is_empty());

    dform(&s, &server, &["apply", "main.df"]).success();
    let keys = server.keys();
    let instances = server.instances();
    assert_eq!(names(&keys), ["lab-admin"]);
    assert_eq!(names(&instances), ["lab-server"]);
    let server_doc = &instances[0];
    assert_eq!(server_doc["status"], "ACTIVE");
    // The reference is the key's id; the user data was sent.
    assert_eq!(server_doc["sshKeyId"], keys[0]["id"]);
    let sent = server
        .seen()
        .into_iter()
        .find(|c| c.method == "POST" && c.path.ends_with("/instance"))
        .unwrap();
    assert_eq!(sent.body["userData"], "#cloud-config\n");
    assert_eq!(sent.body["flavorId"], "flavor-b2-7-ca-east-tor");
    let records = server.records();
    assert_eq!(records.len(), 1);
    let ip = server_doc["ipAddresses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["version"] == 4)
        .unwrap()["ip"]
        .clone();
    assert_eq!(records[0]["target"], ip);
    assert_eq!(records[0]["subDomain"], "www");
    // The digest of the user data is kept in state beside the instance,
    // never the text (R-106).
    let state = s.read("dform.state/main/state.json");
    let st: Json = serde_json::from_str(&state).unwrap();
    let written = &st["resources"]["ovh.instance::server"]["written"]["user_data"];
    assert!(
        written.as_str().is_some_and(|d| d.contains("sha256:")),
        "{state}"
    );
    assert!(!state.contains("cloud-config"), "{state}");
    assert!(!s.path("dform.state/cache/ovh-user-data.json").exists());

    // Nothing changed: nothing to do, the user data included.
    let again = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(
        !again.stdout.contains("ovh.instance[\"server\"]"),
        "{}",
        again.stdout
    );

    // New user data replaces the instance.
    s.write(
        "main.df",
        &program(&server, "#cloud-config\\nruncmd: []\\n", "b2-7"),
    );
    let changed = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(
        changed.stdout.contains("ovh.instance server"),
        "{}",
        changed.stdout
    );
    assert!(changed.stdout.contains("replace"), "{}", changed.stdout);

    // Delete them all.
    s.write(
        "main.df",
        &format!(
            "use ovh {{ endpoint = \"{}\", project = \"lab\" }}\n",
            server.endpoint
        ),
    );
    dform(&s, &server, &["apply", "main.df"]).success();
    assert!(server.instances().is_empty(), "{:?}", server.instances());
    assert!(server.keys().is_empty() && server.records().is_empty());
}

/// A rename and a record's ttl change in place: the same objects, a PUT
/// each.
#[test]
fn a_rename_and_a_ttl_change_in_place() {
    let server = Server::start();
    let program = |name: &str, ttl: &str| {
        format!(
            "use ovh {{ endpoint = \"{}\", project = \"lab\" }}\n\
             resource ovh.instance vm {{\n  name = \"{name}\"\n  region = \"BHS5\"\n  \
             flavor = \"d2-2\"\n  image = \"Debian 13\"\n}}\n\
             resource ovh.domain_record apex {{\n  zone = \"example.com\"\n  type = \"TXT\"\n  \
             target = \"hello\"\n{ttl}}}\n",
            server.endpoint
        )
    };
    let s = project("ovh-in-place", "", &program("vm-a", ""));
    dform(&s, &server, &["apply", "main.df"]).success();
    let (vm, rec) = (server.instances(), server.records());
    assert_eq!(rec[0]["ttl"], 0);
    assert_eq!(rec[0]["subDomain"], "");
    s.write("main.df", &program("vm-b", "  ttl = 300\n"));
    let plan = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(
        plan.stdout.contains("~ ovh.instance vm")
            && plan.stdout.contains("~ ovh.domain_record apex"),
        "{}",
        plan.stdout
    );
    dform(&s, &server, &["apply", "main.df"]).success();
    assert_eq!(server.instances()[0]["id"], vm[0]["id"]);
    assert_eq!(names(&server.instances()), ["vm-b"]);
    assert_eq!(server.records()[0]["id"], rec[0]["id"]);
    assert_eq!(server.records()[0]["ttl"], 300);
    let again = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(again.stdout.contains("is up to date"), "{}", again.stdout);
}

#[test]
fn a_flavor_the_region_does_not_offer_is_refused_at_plan() {
    let server = Server::start();
    let s = project("ovh-flavor", "", &program(&server, "x", "b9-999"));
    let r = dform(&s, &server, &["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains("ovh.instance[\"server\"]")
            && r.stderr
                .contains("flavor \"b9-999\" is not offered in region ca-east-tor")
            && r.stderr.contains("b2-7"),
        "{}",
        r.stderr
    );
}

#[test]
fn a_project_is_found_by_its_description_and_credentials_are_named() {
    let server = Server::start();
    let key = "resource ovh.ssh_key k { name = \"k\", public_key = \"ssh-ed25519 A\" }\n";
    let s = project(
        "ovh-project",
        "",
        &format!(
            "use ovh {{ endpoint = \"{}\", project = \"nope\" }}\n{key}",
            server.endpoint
        ),
    );
    let r = dform(&s, &server, &["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains("no Public Cloud project \"nope\"")
            && r.stderr.contains(&format!("{} (lab)", fake::PROJECT)),
        "{}",
        r.stderr
    );
    // No credentials anywhere: the error says where they were looked for
    // (and nothing is asked of the real endpoint).
    s.write(
        "main.df",
        &format!("use ovh {{ endpoint = \"ovh-ca\", project = \"lab\" }}\n{key}"),
    );
    let mut c = common::dform();
    c.args(["plan", "main.df"])
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"));
    for k in [
        "OVH_ENDPOINT",
        "OVH_APPLICATION_KEY",
        "OVH_APPLICATION_SECRET",
        "OVH_CONSUMER_KEY",
    ] {
        c.env_remove(k);
    }
    let r = Run::from(c.output().unwrap()).failure();
    assert!(
        r.stderr
            .contains("no OVH application_key for the endpoint ovh-ca")
            && r.stderr.contains("ovh/ovh.conf"),
        "{}",
        r.stderr
    );
}

/// A Create whose answer does not come within dform's timeout (R-81) is
/// found by its key and adopted, not made twice; the record that reads
/// the instance's address waits for it.
#[test]
fn a_create_that_times_out_is_adopted() {
    let server = Server::start();
    server.build_polls(4000);
    let s = project(
        "ovh-timeout",
        ", timeout = \"1s\", backoff = \"10ms\"",
        &program(&server, "x", "b2-7"),
    );
    let r = dform(&s, &server, &["apply", "main.df"]);
    assert!(r.ok, "{}\n{}", r.stdout, r.stderr);
    assert_eq!(posts(&server, "/instance"), 1, "{:?}", server.calls());
    assert_eq!(names(&server.instances()), ["lab-server"]);
    assert_eq!(server.records().len(), 1);
}

/// A 503 on a Create is transient: dform sends it again (R-81).
#[test]
fn a_transient_failure_is_sent_again() {
    let server = Server::start();
    server.fail(
        &format!("POST /cloud/project/{}/sshkey", fake::PROJECT),
        &[503],
    );
    let s = project(
        "ovh-503",
        ", backoff = \"10ms\"",
        &format!(
            "use ovh {{ endpoint = \"{}\", project = \"lab\" }}\n\
             resource ovh.ssh_key k {{ name = \"k\", public_key = \"ssh-ed25519 A\" }}\n",
            server.endpoint
        ),
    );
    let r = dform(&s, &server, &["apply", "main.df"]);
    assert!(r.ok, "{}\n{}", r.stdout, r.stderr);
    assert_eq!(names(&server.keys()), ["k"]);
}

/// A data source as a table: the program picks the region's Debian image.
#[test]
fn the_images_of_a_region_are_a_table() {
    let server = Server::start();
    let s = project(
        "ovh-images",
        "",
        &format!(
            "use ovh {{ endpoint = \"{}\", project = \"lab\" }}\n\
             extern ovh.image(+region, -name, -id, -distribution)\n\
             resource ovh.instance db {{\n  name = \"db\"\n  region = \"BHS5\"\n  \
             flavor = \"d2-2\"\n  image\n}} where ovh.image(\"BHS5\", image, _, \"Debian\")\n",
            server.endpoint
        ),
    );
    let r = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(r.stdout.contains("image = \"Debian 13\""), "{}", r.stdout);
}

/// Against the real account (`OVH_INTEGRATION=1`, the machine's own
/// ovh.conf or OVH_* variables, `OVH_CLOUD_PROJECT_SERVICE` naming the
/// project): its regions are listed. Nothing is created.
#[test]
fn the_real_account_lists_its_regions() {
    if std::env::var("OVH_INTEGRATION").as_deref() != Ok("1") {
        eprintln!("skipped: OVH_INTEGRATION=1 runs it against the real account");
        return;
    }
    use dform_core::value::Value;
    let ovh = dform_provider_ovh::ovh::Ovh::new();
    let project = std::env::var("OVH_CLOUD_PROJECT_SERVICE")
        .expect("OVH_CLOUD_PROJECT_SERVICE names the project (its id or description)");
    let account = ovh
        .configure(&serde_json::json!({"settings": {"project": project}}))
        .unwrap();
    eprintln!("project {account:?}");
    let rows = ovh
        .query(
            "ovh.region",
            &[true, false, false],
            &[Value::Str(project.clone())],
        )
        .unwrap();
    assert!(!rows.is_empty(), "no regions");
    for r in &rows {
        eprintln!("{r:?}");
    }
}

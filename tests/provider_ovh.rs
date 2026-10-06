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
provider ovh {{ endpoint = "{}", project = "{}" }}

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
        "+ ovh.ssh_key[\"admin\"]",
        "+ ovh.instance[\"server\"]",
        "+ ovh.domain_record[\"www\"]",
        "user_data = (sensitive)",
        "ssh_key = ?ovh.ssh_key[\"admin\"]",
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
            "provider ovh {{ endpoint = \"{}\", project = \"lab\" }}\n\
             resource ovh.instance old {{\n  name = \"old\"\n  region = \"BHS5\"\n  \
             flavor = \"b2-7\"\n  image = \"Ubuntu 24.04\"\n}}\n\
             adopt(old, \"{id}\")\n",
            server.endpoint
        ),
    );
    let plan = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(
        plan.stdout.contains("> ovh.instance[\"old\"]"),
        "{}",
        plan.stdout
    );
    // Applying is the second half.
    let r = dform(&s, &server, &["apply", "main.df"]).failure();
    assert!(
        r.stderr.contains("does not create, change or delete yet"),
        "{}",
        r.stderr
    );
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
            "provider ovh {{ endpoint = \"{}\", project = \"nope\" }}\n{key}",
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
        &format!("provider ovh {{ endpoint = \"ovh-ca\", project = \"lab\" }}\n{key}"),
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

/// A data source as a table: the program picks the region's Debian image.
#[test]
fn the_images_of_a_region_are_a_table() {
    let server = Server::start();
    let s = project(
        "ovh-images",
        "",
        &format!(
            "provider ovh {{ endpoint = \"{}\", project = \"lab\" }}\n\
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

//! A provider's schema declares its data sources (R-106): the extern's
//! name, its columns' binding modes, names and types arrive as schema
//! facts (`type_extern`), so a program reads `ovh.image(..)` with no
//! `extern` line. A program's own `extern` line still works and wins.

mod common;
use common::{Run, Scratch};
use dform_provider_ovh::fake::Server;

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

fn project(program: &str) -> Scratch {
    let s = Scratch::project("schema-externs");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\novh = {{ path = \"{}\" }}\n",
            common::exe("dform-provider-ovh")
        ),
    );
    s.write("main.df", program);
    s
}

/// The shape of ~/src/ovh-infra: the region's image by its name, the
/// flavor's size, with no `extern` line.
#[test]
fn a_providers_data_sources_need_no_extern_line() {
    let server = Server::start();
    let s = project(&format!(
        r#"
use ovh {{ endpoint = "{}", project = "lab" }}
let region = "BHS5"
resource ovh.instance db {{
  name = "db"
  region
  flavor = "d2-2"
  image
}} where ovh.image(region, image, _, "Debian"), ovh.flavor(region, "d2-2", cpus, _, _), cpus >= 1
"#,
        server.endpoint
    ));
    let r = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(r.stdout.contains("+ ovh.instance db"), "{}", r.stdout);
    assert!(r.stdout.contains("image = \"Debian 13\""), "{}", r.stdout);
}

/// The schema's declaration is the extern's: a call of another arity is
/// refused as one of a declared extern is.
#[test]
fn a_schema_extern_is_called_with_its_declared_arity() {
    let server = Server::start();
    let s = project(&format!(
        r#"
use ovh {{ endpoint = "{}", project = "lab" }}
resource ovh.instance db {{
  name = "db"
  region = "BHS5"
  flavor = "d2-2"
  image
}} where ovh.image("BHS5", image)
"#,
        server.endpoint
    ));
    let r = dform(&s, &server, &["plan", "main.df"]).failure();
    assert!(
        r.stderr
            .contains("extern ovh.image takes 4 arguments, not 2"),
        "{}",
        r.stderr
    );
}

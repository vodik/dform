//! `dform test` reads a provider's data sources as plan reads them (R-196,
//! R-106): with no `extern` line, a table read is a read, not a relation
//! nothing defines. A provider the test leaves unconfigured answers "not
//! yet", so a deny over its rows is undetermined, said once.

mod common;
use common::{Run, Scratch};
use dform_provider_ovh::fake::Server;

/// The aws mock answers its zones under test: a deny over them is decided
/// there, either way.
#[test]
fn a_table_read_is_a_read_under_test() {
    let s = Scratch::new("test-table-mock");
    s.write(
        "p.df",
        "\nuse aws { region = \"us-east-1\" }\n\n\
         deny \"no zone\" where not aws.availability_zone(\"available\", _, _)\n\
         deny \"zone ${z} first\" where aws.availability_zone(\"available\", z, 0), z != \"us-east-1b\"\n",
    );
    let r = s
        .run(&[
            "dev",
            "--provider",
            "aws-mock",
            "--world",
            "w.json",
            "test",
            "p.df",
        ])
        .failure();
    assert!(
        r.stdout
            .contains("denied  dform plan p.df\n  - zone us-east-1a first\n")
            && !r.stdout.contains("no zone")
            && !r.stdout.contains("note:"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}

/// The OVH provider, left unconfigured by the test, answers its tables
/// "not yet": a deny over a zone's absence or an image's name neither
/// holds nor fails, the API is never asked, and the note says which reads
/// it did not decide, once for every combination.
#[test]
fn an_unconfigured_providers_table_is_undetermined() {
    let server = Server::start();
    server.hosting(&["example.com"]);
    let s = Scratch::project("test-table-ovh");
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\novh = {{ path = \"{}\" }}\n",
            common::exe("dform-provider-ovh")
        ),
    );
    s.write(
        "main.df",
        &format!(
            "input public: bool = false\n\
             use ovh {{ endpoint = \"{}\", project = \"lab\" }}\n\
             let zone = \"vodik.xyz\"\n\
             deny \"the zone ${{zone}} is not hosted\" where not ovh.zone(zone, _, _)\n\
             deny \"image ${{i}}\" where ovh.image(\"BHS5\", i, _, \"Debian\"), i != \"x\"\n",
            server.endpoint
        ),
    );
    let mut c = common::dform();
    c.args(["test", "main.df"])
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"))
        .env_remove("OVH_CLOUD_PROJECT_SERVICE");
    for (k, v) in server.env() {
        c.env(k, v);
    }
    let r = Run::from(c.output().unwrap()).success();
    assert!(
        r.stdout.contains(
            "note: no provider is configured, so its data sources answer \"not yet\" and what \
             reads them is undetermined: ovh.image(\"BHS5\"), ovh.zone(\"vodik.xyz\")\n"
        ) && r.stdout.contains("false   ok\ntrue    ok\n"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
    assert_eq!(
        r.stdout.matches("note: no provider").count(),
        1,
        "{}",
        r.stdout
    );
    assert!(server.calls().is_empty(), "{:?}", server.calls());
}

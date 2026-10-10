//! `ovh.zone(+name, -id, -nameservers)` (R-196): the DNS zones the account
//! hosts, a data source of the OVH provider read like `ovh.image`. A deny
//! over its absence is what makes a zone the account does not host loud;
//! a record takes its zone from it; the plan file records the read, so
//! apply asks the API nothing plan asked.

mod common;
use common::{Run, Scratch};
use dform_provider_ovh::fake::{NAMESERVERS, Server};

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

/// A project on the OVH provider pointed at `server`: `zone` the zone
/// its program names, then `body`.
fn project(name: &str, server: &Server, zone: &str, body: &str) -> Scratch {
    let s = Scratch::project(name);
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
            "use ovh {{ endpoint = \"{}\", project = \"lab\" }}\nlet zone = \"{zone}\"\n{body}",
            server.endpoint
        ),
    );
    s
}

/// The deny the provider's docs give, as a program writes it.
const DENY: &str =
    "deny \"the zone ${zone} is not hosted on this account\" where not { ovh.zone(zone, _, _) }\n";

/// The record of the docs: its zone the table's id.
const RECORD: &str = "resource ovh.domain_record k8s {\n  zone = z\n  subdomain = \"k8s-lab\"\n  \
                      type = \"A\"\n  target = \"51.79.29.179\"\n} where ovh.zone(zone, z, _)\n";

/// The account does not host the zone: the deny fires, naming it, and
/// nothing is asked of the zone's records. It does: the deny holds.
#[test]
fn a_zone_not_on_the_account_is_a_deny() {
    let server = Server::start();
    server.hosting(&["example.com"]);
    let s = project(
        "ovh-zone-deny",
        &server,
        "vodik.xyz",
        &format!("{DENY}{RECORD}"),
    );
    let r = dform(&s, &server, &["plan", "main.df"]).failure();
    assert!(
        r.stderr
            .contains("refused  the zone vodik.xyz is not hosted on this account\n"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
    // Without the deny the record is simply not wanted: correct, and
    // silent, which is why the docs give the deny.
    assert!(!r.stdout.contains("ovh.domain_record"), "{}", r.stdout);

    let s = project(
        "ovh-zone-held",
        &server,
        "example.com",
        &format!("{DENY}{RECORD}"),
    );
    let r = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(!r.stderr.contains("not hosted"), "{}", r.stderr);
}

/// A record takes its zone from the table: the id is the name the API
/// keys the zone by, and the nameservers are the API's.
#[test]
fn a_record_takes_its_zone_from_the_table() {
    let server = Server::start();
    server.hosting(&["example.com"]);
    let s = project(
        "ovh-zone-record",
        &server,
        "example.com",
        &format!(
            "{RECORD}deny \"served by ${{ns}}\" where ovh.zone(zone, _, ns), ns != {:?}\n",
            NAMESERVERS
        ),
    );
    let r = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(r.stdout.contains("+ ovh.domain_record k8s"), "{}", r.stdout);
    assert!(r.stdout.contains("zone = \"example.com\""), "{}", r.stdout);
    dform(&s, &server, &["apply", "main.df"]).success();
    let records = server.records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["zone"], "example.com");
}

/// The plan file records the zone the plan read; apply reads that, not
/// the API again.
#[test]
fn the_plan_file_records_the_zone() {
    let server = Server::start();
    server.hosting(&["example.com"]);
    let s = project("ovh-zone-file", &server, "example.com", RECORD);
    dform(&s, &server, &["plan", "--out", "plan.json", "main.df"]).success();
    let file = s.read("plan.json");
    assert!(
        file.contains("ovh.zone") && file.contains(NAMESERVERS[0]),
        "{file}"
    );
    let asked = |server: &Server| {
        server
            .calls()
            .iter()
            .filter(|c| c.ends_with("/domain/zone/example.com"))
            .count()
    };
    let before = asked(&server);
    dform(&s, &server, &["apply", "plan.json"]).success();
    assert_eq!(asked(&server), before, "{:?}", server.calls());
    assert_eq!(server.records().len(), 1);
}

/// The deny as the ticket writes it, `not` before the table itself: the
/// absence `not { ovh.zone(..) }` reads, and the form `dform fmt` writes
/// the block in.
#[test]
fn a_bare_not_over_the_table_is_the_deny() {
    let server = Server::start();
    server.hosting(&["example.com"]);
    let s = project(
        "ovh-zone-bare",
        &server,
        "vodik.xyz",
        "deny \"the zone ${zone} is not hosted on this account\" where not ovh.zone(zone, _, _)\n",
    );
    let r = dform(&s, &server, &["plan", "main.df"]).failure();
    assert!(
        r.stderr
            .contains("the zone vodik.xyz is not hosted on this account"),
        "{}",
        r.stderr
    );

    let s = project(
        "ovh-zone-bare-held",
        &server,
        "example.com",
        "deny \"the zone ${zone} is not hosted on this account\" where not ovh.zone(zone, _, _)\n",
    );
    let r = dform(&s, &server, &["plan", "main.df"]).success();
    assert!(!r.stderr.contains("not hosted"), "{}", r.stderr);
}

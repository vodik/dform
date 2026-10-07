//! What a plan costs over the network (R-123): a project whose state is in
//! a bucket (a fake S3) and whose resources are the OVH provider's (a
//! fake OVH API that takes a while to answer each call, as the real one
//! does from far away), applied once and then planned. A plan reads the
//! deployment's objects over one connection and writes none; its
//! provider calls go out together, not one after another.

mod common;
use common::{Run, Scratch};
use dform_provider_ovh::fake::{self, Server};
use std::time::{Duration, Instant};

/// Each OVH call's answer takes this long.
const LATENCY: Duration = Duration::from_millis(300);

fn ovh() -> String {
    common::exe("dform-provider-ovh")
}

fn s3() -> &'static dform_s3::fake::Server {
    static SERVER: std::sync::OnceLock<dform_s3::fake::Server> = std::sync::OnceLock::new();
    SERVER.get_or_init(dform_s3::fake::Server::start)
}

/// Two instances and their key and records, under `prefix` in the bucket.
fn project(name: &str, server: &Server) -> Scratch {
    let s = Scratch::project(name);
    let prefix = format!("plan-cost-{}-{name}", std::process::id());
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\novh = {{ path = \"{}\" }}\n\n\
             [defaults]\nbackend = 's3(\"dform\", \"{prefix}/{{stack}}\", \
             {{endpoint: \"{}\", region: \"us-east-1\"}})'\n",
            ovh(),
            s3().endpoint
        ),
    );
    s.write(
        "main.df",
        &format!(
            r#"
use ovh {{ endpoint = "{}", project = "{}" }}

resource ovh.ssh_key admin {{ name = "lab-admin", public_key = "ssh-ed25519 AAAAC3Nz lab" }}

resource ovh.instance "lab-${{i}}" {{
  name = "lab-${{i}}"
  region = "ca-east-tor"
  flavor = "b2-7"
  image = "Ubuntu 24.04"
  ssh_key = admin
}} where i in 0..2

resource ovh.domain_record "lab-${{i}}" {{
  zone = "example.com"
  subdomain = "lab-${{i}}"
  type = "A"
  target = ovh.instance["lab-${{i}}"].public_ip
}} where i in 0..2
"#,
            server.endpoint,
            fake::DESCRIPTION
        ),
    );
    s
}

fn dform(s: &Scratch, server: &Server, args: &[&str]) -> Run {
    let mut c = common::dform();
    c.args(common::yes(args))
        .current_dir(&s.dir)
        .env("HOME", &s.dir)
        .env("XDG_CONFIG_HOME", s.path("config"))
        .env("DFORM_S3_ACCESS_KEY_ID", "fake")
        .env("DFORM_S3_SECRET_ACCESS_KEY", "fake")
        .env("DFORM_LOG", "debug")
        .env_remove("OVH_CLOUD_PROJECT_SERVICE");
    for (k, v) in server.env() {
        c.env(k, v);
    }
    Run::from(c.output().unwrap())
}

/// The S3 requests and connections, and the OVH calls, `f` made.
struct Cost {
    s3: Vec<String>,
    s3_connections: usize,
    ovh: Vec<String>,
    took: Duration,
}

fn cost(server: &Server, prefix: &str, f: impl FnOnce()) -> Cost {
    let (r0, c0, o0) = (
        s3().requests().len(),
        s3().connections(),
        server.calls().len(),
    );
    let t = Instant::now();
    f();
    let took = t.elapsed();
    Cost {
        s3: s3().requests()[r0..]
            .iter()
            .filter(|r| r.contains(prefix) || !r.contains("plan-cost-"))
            .cloned()
            .collect(),
        s3_connections: s3().connections() - c0,
        ovh: server.calls()[o0..].to_vec(),
        took,
    }
}

#[test]
fn a_plan_reads_over_one_connection_writes_nothing_and_calls_its_provider_at_once() {
    let server = Server::start();
    let s = project("plan-cost", &server);
    dform(&s, &server, &["apply"]).success();
    server.slow(LATENCY);
    let prefix = format!("plan-cost-{}-plan-cost", std::process::id());
    let mut out = None;
    let c = cost(&server, &prefix, || {
        out = Some(dform(&s, &server, &["plan"]).success());
    });
    let out = out.unwrap();
    eprintln!(
        "plan: {:?}, {} S3 requests over {} connections, {} OVH calls (at most {} at once)\n{}",
        c.took,
        c.s3.len(),
        c.s3_connections,
        c.ovh.len(),
        server.most_at_once(),
        out.stderr
    );
    assert!(out.stdout.contains("up to date"), "{}", out.stdout);
    // Nothing a plan did not change is written.
    let writes: Vec<&String> = c.s3.iter().filter(|r| !r.starts_with("GET ")).collect();
    assert!(writes.is_empty(), "a plan wrote {writes:?}");
    assert_eq!(c.s3_connections, 1, "{:?}", c.s3);
    // The five objects' reads go out together.
    assert!(server.most_at_once() >= 5, "{:?}", c.ovh);
    // Every line of the log is there.
    for phase in [
        "s3 get",
        "started (spawn and handshake)",
        "provider ovh: Configure",
        "provider ovh: Read ovh.instance lab-0",
        "evaluated",
        "planned",
        "finished",
    ] {
        assert!(
            out.stderr.contains(phase),
            "no {phase:?} in\n{}",
            out.stderr
        );
    }
}

/// A large document as a resource's body (After R-126): each of its
/// leaves' why is found once per statement, not once per leaf with the
/// whole document rendered again. Four config maps of 1500 entries plan
/// in about 2s on a debug build; finding each leaf's why afresh took 70s.
#[test]
fn a_large_documents_leaves_are_explained_in_linear_time() {
    let s = Scratch::project("plan-cost-manifest");
    s.write("dform.toml", "[project]\nedition = \"2026\"\n");
    let mut docs = String::new();
    for d in 0..4 {
        docs.push_str(&format!(
            "---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: cm{d}\ndata:\n"
        ));
        for i in 0..1500 {
            docs.push_str(&format!(
                "  key{i:04}: \"value number {i} of document {d}, long enough to print\"\n"
            ));
        }
    }
    s.write("cms.yml", &docs);
    s.write(
        "main.df",
        "use k8s\nresource k8s.config_map \"${d.metadata.name}\" = d where d in yaml(\"cms.yml\")\n",
    );
    let t = Instant::now();
    let r = s.run(&["plan", "main.df"]).success();
    let took = t.elapsed();
    assert!(
        r.stdout
            .contains("key1499: \"value number 1499 of document 3"),
        "{}",
        &r.stdout[..r.stdout.len().min(2000)]
    );
    assert!(
        took < Duration::from_secs(30),
        "the plan took {took:?} (loadavg {})",
        std::fs::read_to_string("/proc/loadavg").unwrap_or_default()
    );
}

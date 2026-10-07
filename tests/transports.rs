//! One model for reading a document (R-153): a loader (`text`, `yaml`,
//! `toml`, `json`, `csv`) takes a location, a path from the project root
//! or a uri whose scheme is a host transport. These are the schemes that
//! need no network: `data:`, `file:`, a repository's file through the
//! mirror (`git+https://..?ref=`, a held tag read with no fetch), and
//! `s3://` against the fake S3 server. `ssh://` and `git+ssh://` are
//! tests/transport_ssh.rs's, a provider's scheme and its grants
//! tests/host_grpc.rs's.

mod common;
use common::{Run, Scratch, dform, yes};
use dform_core::store::{Cond, S3Spec, Store};
use std::path::Path;
use std::process::Command;

/// `dform ARGS` in `s`, its cache (the git mirrors) its own, the fake S3
/// credentials set.
fn run(s: &Scratch, args: &[&str]) -> Run {
    let out = dform()
        .args(yes(args))
        .current_dir(&s.dir)
        .env("XDG_CACHE_HOME", s.path("cache"))
        .env("DFORM_S3_ACCESS_KEY_ID", "fake")
        .env("DFORM_S3_SECRET_ACCESS_KEY", "fake")
        .output()
        .unwrap();
    Run::from(out)
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// A resource per document of the stream `loader` reads.
fn vpcs(loader: &str) -> String {
    format!(
        "\nuse fake\nresource net.vpc \"${{d.name}}\" {{ cidr_block = \"10.0.0.0/16\" }} \
         where d in {loader}\n"
    )
}

/// `data:` (RFC 2397) and `file:` are locations like any: the bytes in
/// the location itself, a file of the project. `text` is the whole of one
/// as a string; a relation is not read from text.
#[test]
fn data_and_file_locations() {
    let s = Scratch::project("transport-data");
    s.write("vpcs.yml", "name: a\n---\nname: b\n");
    for loader in [
        "yaml(\"vpcs.yml\")",
        "yaml(\"file:vpcs.yml\")",
        "yaml(\"data:,name%3A%20a%0A---%0Aname%3A%20b%0A\")",
        "yaml(\"data:text/yaml;base64,bmFtZTogYQotLS0KbmFtZTogYgo=\")",
    ] {
        s.write("p.df", &vpcs(loader));
        let r = run(&s, &["plan", "p.df"]).success();
        assert!(
            r.stdout.contains("+ net.vpc a") && r.stdout.contains("+ net.vpc b"),
            "{loader}: {}",
            r.stdout
        );
    }
    s.write(
        "p.df",
        "\nuse fake\nlet name = text(\"data:,blue\")\n\
         resource net.vpc \"${name}\" { cidr_block = \"10.0.0.0/16\" }\n",
    );
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(r.stdout.contains("+ net.vpc blue"), "{}", r.stdout);
    s.write(
        "p.df",
        "\ninput vpc from text(\"data:,a\")\ndecl vpc(name: string)\nuse fake\n",
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains("a text document is one string"),
        "{}",
        r.stderr
    );
    // A scheme dform has no transport for, and plain http, are refused
    // naming what is read.
    for (loc, says) in [
        ("ftp://h/x.yml", "no transport reads `ftp:`"),
        ("http://h/x.yml", "write `https:`"),
    ] {
        s.write("p.df", &vpcs(&format!("yaml(\"{loc}\")")));
        let r = run(&s, &["plan", "p.df"]).failure();
        assert!(r.stderr.contains(says), "{loc}: {}", r.stderr);
    }
}

/// The Traefik CRDs from a tag of their repository
/// (`git+https://github.com/traefik/traefik/PATH?ref=v3.7.14`), read from
/// the mirror in the cache (`github.com-traefik/traefik.git`): a tag the
/// mirror holds is read with no fetch, so no network is touched. The
/// plan file records the commit read; `git(..)` is gone, naming the form.
#[test]
fn a_tag_is_read_from_the_mirror() {
    let s = Scratch::project("transport-git");
    let mirror = s.path("cache/dform/git/github.com-traefik");
    std::fs::create_dir_all(&mirror).unwrap();
    if git(&mirror, &["init", "-q", "--bare", "traefik.git"]).is_none() {
        eprintln!("skipped: no git to make the fixture with");
        return;
    }
    let w = s.path("work");
    git(
        &s.dir,
        &[
            "clone",
            "-q",
            &mirror.join("traefik.git").display().to_string(),
            "work",
        ],
    )
    .unwrap();
    let crds = "docs/content/reference/dynamic-configuration/kubernetes-crd-definition-v1.yml";
    std::fs::create_dir_all(w.join(Path::new(crds).parent().unwrap())).unwrap();
    std::fs::write(
        w.join(crds),
        "---\nname: ingressroutes\n---\nname: middlewares\n",
    )
    .unwrap();
    git(&w, &["add", "."]).unwrap();
    git(&w, &["commit", "-q", "-m", "crds"]).unwrap();
    git(&w, &["tag", "v3.7.14"]).unwrap();
    git(&w, &["push", "-q", "origin", "v3.7.14"]).unwrap();
    let commit = git(&w, &["rev-parse", "HEAD"]).unwrap();
    s.write(
        "p.df",
        &vpcs(&format!(
            "yaml(\"git+https://github.com/traefik/traefik/{crds}?ref=v3.7.14\")"
        )),
    );
    let r = run(&s, &["plan", "--out", "plan.json", "p.df"]).success();
    assert!(
        r.stdout.contains("+ net.vpc ingressroutes") && r.stdout.contains("+ net.vpc middlewares"),
        "{}",
        r.stdout
    );
    let plan = s.read("plan.json");
    assert!(
        plan.contains(&format!("github.com/traefik/traefik@{commit}:{crds}")),
        "{plan}"
    );

    // A ref the mirror does not hold is fetched; github.com is not asked
    // here: a host that is not there fails the fetch, naming it.
    s.write(
        "p.df",
        &vpcs(&format!(
            "yaml(\"git+https://git.invalid/traefik/traefik/{crds}?ref=main\")"
        )),
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(r.stderr.contains("fetch"), "{}", r.stderr);
    // No ref: said, with the form.
    s.write(
        "p.df",
        &vpcs("yaml(\"git+https://github.com/traefik/traefik/x.yml\")"),
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(r.stderr.contains("add `?ref=TAG`"), "{}", r.stderr);
    s.write("p.df", &vpcs("yaml(git(\"ops.git\", \"main\", \"x.yml\"))"));
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains("`git(..)` is gone (R-153)")
            && r.stderr
                .contains("git+https://HOST/OWNER/REPO/PATH?ref=TAG"),
        "{}",
        r.stderr
    );
}

fn fake_s3() -> &'static dform_s3::fake::Server {
    static SERVER: std::sync::OnceLock<dform_s3::fake::Server> = std::sync::OnceLock::new();
    SERVER.get_or_init(dform_s3::fake::Server::start)
}

/// `s3://BUCKET/KEY` through the S3 client, the bucket's endpoint and
/// region as the backend that names it in dform.toml says; an object not
/// there yet is waited on, as a host's file is.
#[test]
fn an_s3_object_is_read_with_the_backends_endpoint() {
    let s = Scratch::project("transport-s3");
    let spec = S3Spec {
        bucket: "docs".into(),
        prefix: String::new(),
        endpoint: Some(fake_s3().endpoint.clone()),
        region: Some("us-east-1".into()),
    };
    let store =
        dform_s3::S3Store::with_credentials(&spec, "", rusty_s3::Credentials::new("fake", "fake"))
            .unwrap();
    store
        .put("vpcs/prod.yml", b"name: a\n---\nname: b\n", &Cond::Any)
        .unwrap();
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[stacks.p]\nbackend = 's3(\"docs\", \
             \"state\", {{endpoint: \"{}\", region: \"us-east-1\"}})'\n",
            fake_s3().endpoint
        ),
    );
    s.write("p.df", &vpcs("yaml(\"s3://docs/vpcs/prod.yml\")"));
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("+ net.vpc a") && r.stdout.contains("+ net.vpc b"),
        "{}",
        r.stdout
    );
    s.write(
        "p.df",
        "\nuse fake\nlet name = text(\"s3://docs/vpcs/later.txt\")\n\
         resource net.vpc \"x\" { cidr_block = name }\n",
    );
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("waits on  s3://docs/vpcs/later.txt"),
        "{}",
        r.stdout
    );
}

/// What a read into a secret `let` holds never reaches the plan file,
/// only its digest; the same read into a value that is not one is
/// recorded. Declaring the secret is what keeps it (R-153): a public
/// place it reaches is the error any secret's is.
#[test]
fn a_secret_read_is_recorded_by_its_digest() {
    let s = Scratch::project("transport-secret");
    s.write("token.txt", "TOKEN-VALUE");
    s.write(
        "p.df",
        "\nuse fake\nlet raw: secret(string) = text(\"token.txt\")\n\
         resource net.vpc v { cidr_block = \"10.0.0.0/16\" }\noutput t: secret(string) = raw\n",
    );
    run(&s, &["plan", "--out", "plan.json", "p.df"]).success();
    let plan = s.read("plan.json");
    assert!(!plan.contains("TOKEN-VALUE"), "{plan}");
    assert!(
        plan.contains("\"sensitive\": \"table.text.document/token.txt#3\""),
        "{plan}"
    );
    s.write(
        "p.df",
        &s.read("p.df")
            .replace("output t: secret(string)", "output t: string"),
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains("a secret reaches output t"),
        "{}",
        r.stderr
    );
}

/// A scheme a provider's manifest declares is read by that provider: the
/// mock declares `mock` (as a google provider would `gs`), and a
/// program's `yaml("mock://..")` reaches it through the host; its "not
/// yet" is waited on as any read's.
#[test]
fn a_scheme_a_provider_declares_is_read_by_it() {
    let s = Scratch::project("transport-provider");
    s.write(
        "p.df",
        "\nuse fake\nlet d = yaml(\"mock://alpha/x.yml\")\n\
         resource net.vpc \"${d.host}\" { cidr_block = \"10.0.0.0/16\" }\n",
    );
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(r.stdout.contains("+ net.vpc alpha"), "{}", r.stdout);
    s.write(
        "p.df",
        "\nuse fake\nlet d = yaml(\"mock://alpha/later/x.yml\")\n\
         resource net.vpc \"x\" { cidr_block = d.host }\n",
    );
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("waits on  mock://alpha/later/x.yml"),
        "{}",
        r.stdout
    );
}

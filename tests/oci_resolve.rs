//! `oci.resolve(ref)` (R-132): a tag pinned to the digest its registry
//! names, at plan, through dform's HTTP client, against a fake registry
//! on the loopback (spoken to in the clear, as Docker speaks to
//! `localhost:5000`). The registry's token flow (`WWW-Authenticate:
//! Bearer realm=..`) anonymously for a public image and with the
//! credential `[io] credentials` names for a private one; the pinned
//! reference in the plan and the plan file, so `apply PLAN` applies what
//! plan resolved; a moved tag an update; a registry that cannot be
//! reached "not yet", the digest last resolved standing in; a policy that
//! asks for a digest holds on the resolved value.

mod common;
use common::{Run, Scratch};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

#[derive(Default)]
struct World {
    /// Each repository's tags and their digests.
    tags: BTreeMap<(String, String), String>,
    /// Repositories pulled only with the account's credential.
    private: BTreeSet<String>,
    /// The tokens handed out, by the repository they pull.
    tokens: BTreeMap<String, String>,
    /// Each request: method and path.
    seen: Vec<String>,
    /// Every connection is closed unanswered: the registry is gone.
    down: bool,
    /// Manifests by their digest.
    manifests: BTreeMap<String, String>,
}

/// A registry v2 API: `HEAD /v2/REPO/manifests/TAG` answers the tag's
/// digest with a token the realm `/token` hands out (anonymously, or for
/// a private repository with `Basic` of `USER:PASSWORD`).
struct Registry {
    port: u16,
    world: Arc<Mutex<World>>,
}

const USER: &str = "robot";
const PASSWORD: &str = "robot-password";

impl Registry {
    fn start() -> Registry {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let world = Arc::new(Mutex::new(World::default()));
        let w = world.clone();
        std::thread::spawn(move || {
            for conn in l.incoming().flatten() {
                let w = w.clone();
                std::thread::spawn(move || serve(conn, port, &w));
            }
        });
        Registry { port, world }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn tag(&self, repo: &str, tag: &str, d: &str) {
        let mut w = self.world.lock().unwrap();
        w.tags
            .insert((repo.to_string(), tag.to_string()), d.to_string());
    }

    fn private(&self, repo: &str) {
        self.world.lock().unwrap().private.insert(repo.to_string());
    }

    fn seen(&self) -> Vec<String> {
        self.world.lock().unwrap().seen.clone()
    }

    /// A manifest pushed: its digest.
    fn push(&self, body: &str) -> String {
        use sha2::Digest;
        let sum = sha2::Sha256::digest(body.as_bytes());
        let d = format!(
            "sha256:{}",
            sum.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let mut w = self.world.lock().unwrap();
        w.manifests.insert(d.clone(), body.to_string());
        d
    }

    fn down(&self) {
        self.world.lock().unwrap().down = true;
    }
}

fn serve(conn: std::net::TcpStream, port: u16, world: &Mutex<World>) {
    let mut r = BufReader::new(conn.try_clone().unwrap());
    let mut conn = conn;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (method, target) = (
            parts.next().unwrap_or_default().to_string(),
            parts.next().unwrap_or_default().to_string(),
        );
        let mut headers = BTreeMap::new();
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            if h == "\r\n" {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let len: usize = headers
            .get("content-length")
            .and_then(|l| l.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; len];
        let _ = r.read_exact(&mut body);
        let mut w = world.lock().unwrap();
        if w.down {
            return;
        }
        w.seen.push(format!("{method} {target}"));
        let (status, extra, text) = answer(&mut w, port, &method, &target, &headers);
        drop(w);
        let body = if method == "HEAD" { "" } else { text.as_str() };
        let reply = format!(
            "HTTP/1.1 {status} X\r\ncontent-length: {}\r\n{extra}\r\n{body}",
            body.len()
        );
        if conn.write_all(reply.as_bytes()).is_err() {
            return;
        }
    }
}

fn answer(
    w: &mut World,
    port: u16,
    method: &str,
    target: &str,
    headers: &BTreeMap<String, String>,
) -> (u16, String, String) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path == "/token" {
        let q: BTreeMap<&str, String> = query
            .split('&')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k, v.replace("%3A", ":").replace("%2F", "/")))
            .collect();
        let Some(repo) = q
            .get("scope")
            .and_then(|s| s.strip_prefix("repository:"))
            .and_then(|s| s.strip_suffix(":pull"))
        else {
            return (400, String::new(), "{}".into());
        };
        let basic = format!("Basic {}", base64(&format!("{USER}:{PASSWORD}")));
        if w.private.contains(repo) && headers.get("authorization") != Some(&basic) {
            return (
                401,
                String::new(),
                r#"{"errors":[{"code":"UNAUTHORIZED"}]}"#.into(),
            );
        }
        let t = format!("token-{}", w.tokens.len() + 1);
        w.tokens.insert(t.clone(), repo.to_string());
        return (200, String::new(), format!(r#"{{"token":"{t}"}}"#));
    }
    let Some((repo, tag)) = path
        .strip_prefix("/v2/")
        .and_then(|p| p.split_once("/manifests/"))
    else {
        return (404, String::new(), String::new());
    };
    let token = headers
        .get("authorization")
        .and_then(|a| a.strip_prefix("Bearer "));
    if token.and_then(|t| w.tokens.get(t)).map(String::as_str) != Some(repo) {
        return (
            401,
            format!(
                "www-authenticate: Bearer realm=\"http://127.0.0.1:{port}/token\",service=\"fake\",scope=\"repository:{repo}:pull\"\r\n"
            ),
            String::new(),
        );
    }
    let _ = method;
    if let Some(m) = w.manifests.get(tag) {
        return (200, format!("docker-content-digest: {tag}\r\n"), m.clone());
    }
    match w.tags.get(&(repo.to_string(), tag.to_string())) {
        Some(d) => (200, format!("docker-content-digest: {d}\r\n"), "{}".into()),
        None => (404, String::new(), String::new()),
    }
}

fn base64(s: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `dform ARGS` in `s`, its cache and credentials its own.
fn run(s: &Scratch, args: &[&str]) -> Run {
    let out = common::dform()
        .args(common::yes(args))
        .current_dir(&s.dir)
        .env("XDG_CACHE_HOME", s.path("cache"))
        .env("DFORM_CREDENTIALS", s.path("credentials"))
        .output()
        .unwrap();
    Run::from(out)
}

/// A project with a Deployment whose image is `image`, and the policy
/// crud-api has: an image not pinned by digest is denied.
fn project(name: &str, image: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n",
    );
    s.write("p.df", &program(image));
    s
}

fn program(image: &str) -> String {
    format!(
        r#"use k8s
let release = "v1.2"
resource k8s.deployment app {{
  metadata.name = "app"
  metadata.namespace = "default"
  spec.selector.matchLabels = {{ app: "app" }}
  spec.template.metadata.labels = {{ app: "app" }}
  spec.template.spec.containers = [{{ name: "app", image: {image} }}]
}}
deny "image not pinned by digest" {{ image: c.image }} where {{
  c in app.spec.template.spec.containers
  not oci.pinned(c.image)
}}
"#
    )
}

/// The tag pinned at plan: the plan prints the reference with its
/// digest, the deny that wants a digest holds, the plan file records the
/// answer so `apply PLAN` applies it without asking the registry; a tag
/// that moved since is an update at the next plan.
#[test]
fn a_tag_is_pinned_at_plan_and_a_moved_tag_is_an_update() {
    let reg = Registry::start();
    reg.tag("acme/app", "v1.2", &digest('a'));
    let image = format!(
        "oci.resolve(oci.with_tag(\"{}/acme/app\", release))",
        reg.host()
    );
    let s = project("oci-resolve", &image);
    let r = run(&s, &["plan", "--out", "plan.json", "p.df"]).success();
    let pinned = format!("{}/acme/app:v1.2@{}", reg.host(), digest('a'));
    assert!(r.stdout.contains(&pinned), "{}", r.stdout);
    assert!(!r.stdout.contains("not pinned"), "{}", r.stdout);
    let plan = s.read("plan.json");
    assert!(plan.contains("oci.resolve"), "{plan}");
    assert!(plan.contains(&digest('a')), "{plan}");
    // The anonymous token flow: a 401 naming the realm, a token, the HEAD.
    let seen = reg.seen();
    assert_eq!(
        seen,
        [
            "HEAD /v2/acme/app/manifests/v1.2".to_string(),
            "GET /token?service=fake&scope=repository%3Aacme%2Fapp%3Apull".to_string(),
            "HEAD /v2/acme/app/manifests/v1.2".to_string(),
        ],
    );

    // The tag moves after the plan: the apply applies what plan resolved.
    reg.tag("acme/app", "v1.2", &digest('b'));
    run(&s, &["apply", "plan.json"]).success();
    assert_eq!(reg.seen().len(), 3, "{:?}", reg.seen());

    // What was applied is the first digest: the next plan updates it.
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(r.stdout.contains("~ k8s.deployment app"), "{}", r.stdout);
    let (a, b) = ("a".repeat(24), "b".repeat(24));
    assert!(r.stdout.contains(&format!("{a}\" → ")), "{}", r.stdout);
    assert!(r.stdout.contains(&format!("{b}\"")), "{}", r.stdout);
}

/// Unresolved, the deny names the image: the policy is what holds the
/// program to a digest, and `oci.resolve` is what satisfies it.
#[test]
fn a_tag_left_unresolved_is_denied() {
    let s = project(
        "oci-unresolved",
        "oci.with_tag(\"ghcr.io/acme/app\", release)",
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stdout.contains("image not pinned by digest")
            || r.stderr.contains("image not pinned by digest"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
}

/// A private repository's token is asked for with the credential dform.toml
/// names for `oci://REGISTRY/REPO`; without it the refusal says what to
/// name.
#[test]
fn a_private_image_is_pulled_with_the_credential_named() {
    let reg = Registry::start();
    reg.tag("acme/private", "1", &digest('c'));
    reg.private("acme/private");
    let s = project(
        "oci-private",
        &format!("oci.resolve(\"{}/acme/private:1\")", reg.host()),
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(r.stderr.contains("[io] credentials"), "{}", r.stderr);
    s.write(
        "dform.toml",
        &format!(
            "[project]\nedition = \"2026\"\n\n[providers]\nk8s = \"k8s\"\n\n[io]\n\
             credentials = {{ \"oci://{}/acme/*\" = \"basic:registry\" }}\n",
            reg.host()
        ),
    );
    s.write("credentials/basic/registry", &format!("{USER}:{PASSWORD}"));
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(r.stdout.contains(&digest('c')), "{}", r.stdout);
}

/// A tag the registry does not have yet is "not yet": the deployment
/// waits on it. A registry that cannot be reached is too, unless this
/// machine resolved the tag before: then that digest stands in.
#[test]
fn offline_the_last_digest_stands_in() {
    let reg = Registry::start();
    reg.tag("acme/app", "v1.2", &digest('d'));
    let image = format!("oci.resolve(\"{}/acme/app:v1.2\")", reg.host());
    let s = project("oci-offline", &image);
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(r.stdout.contains(&digest('d')), "{}", r.stdout);

    s.write("p.df", &program(&image.replace("v1.2", "v1.3")));
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains(&format!(
            "waits on  oci.resolve(\"{}/acme/app:v1.3\")",
            reg.host()
        )),
        "{}",
        r.stdout
    );

    // The registry is gone: the digest resolved before stands in; a tag
    // never resolved here waits.
    reg.down();
    s.write("p.df", &program(&image));
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(r.stdout.contains(&digest('d')), "{}", r.stdout);
    s.write("p.df", &program(&image.replace("v1.2", "v1.4")));
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(r.stdout.contains("waits on  oci.resolve("), "{}", r.stdout);
}

/// `dform test` answers `oci.resolve` as plan does: the Deployment is
/// there, its image pinned, so the policy holds on the resolved value and
/// not because nothing was planned.
#[test]
fn dform_test_resolves_a_tag() {
    let reg = Registry::start();
    reg.tag("acme/app", "v1.2", &digest('a'));
    let image = format!(
        "oci.resolve(oci.with_tag(\"{}/acme/app\", release))",
        reg.host()
    );
    let s = project("oci-test", &image);
    s.write(
        "p.df",
        &(program(&image) + "deny \"the app is planned\" where not app in k8s.deployment\n"),
    );
    let r = run(&s, &["test", "p.df"]);
    assert!(r.ok, "{}\n{}", r.stdout, r.stderr);
    assert!(r.stdout.contains("1 combination, 0 failed"), "{}", r.stdout);
    assert_eq!(reg.seen().len(), 3, "{:?}", reg.seen());
}

/// `io.read("oci://REGISTRY/REPO@sha256:..")` reads the manifest the
/// digest names, through the same token flow, checked against the digest
/// and kept, so a run that cannot reach the registry reads it too; a tag
/// is not a location.
#[test]
fn a_manifest_is_read_by_its_digest() {
    let reg = Registry::start();
    let d = reg.push(
        r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"digest":"sha256:cfg"}}"#,
    );
    let s = Scratch::project("oci-read");
    s.write(
        "p.df",
        &format!(
            "use fake\nlet m = json.decode(io.read(\"oci://{}/acme/app@{d}\"))\n\
             config(c) where c = m.config.digest\n",
            reg.host()
        ),
    );
    let r = run(&s, &["query", "config(C)", "p.df"]).success();
    assert!(r.stdout.ends_with("\n\"sha256:cfg\"\n"), "{}", r.stdout);
    assert_eq!(
        reg.seen()[2],
        format!("GET /v2/acme/app/manifests/{d}"),
        "{:?}",
        reg.seen()
    );
    // Offline: what was read by its digest is read again.
    reg.down();
    let r = run(&s, &["query", "config(C)", "p.df"]).success();
    assert!(r.stdout.ends_with("\n\"sha256:cfg\"\n"), "{}", r.stdout);
    s.write(
        "p.df",
        &format!(
            "let m = io.read(\"oci://{}/acme/app:v1\")\nuse fake\n",
            reg.host()
        ),
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("an `oci://` location names a manifest by its digest"),
        "{}",
        r.stderr
    );
}

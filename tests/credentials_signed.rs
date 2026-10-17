//! A `signed` credential (docs/providers.md, "Grants and credentials"):
//! the provider's signer makes its material of what the host performs
//! for it, and signs each request the host sends with it, apart from the
//! provider; the OVH provider's signer signs as OVH's own clients do, at
//! the API's time.

use dform::plugin::credentials::{self, Kind, Secret};
use dform::plugin::host::{Calls, Grants, HttpRequest};
use dform_host::services::Services;
use dform_provider_ovh::fake::{self, Server};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::sync::Arc;

/// `$1$` and the hex SHA-1 of what OVH signs, apart from the signer's code.
fn ovh_signature(method: &str, url: &str, body: &str, time: &str) -> String {
    let signed = format!(
        "{}+{}+{method}+{url}+{body}+{time}",
        fake::APPLICATION_SECRET,
        fake::CONSUMER_KEY
    );
    let hex: String = Sha1::digest(signed.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("$1${hex}")
}

/// A host of the provider `ovh` with its signer, and the fake's keys held
/// for it, opened: the handle.
fn signing_host(server: &Server) -> (Services, credentials::Holding, u64) {
    let held = server.signature();
    let host =
        Services::new(Grants::none("ovh")).with_signer(Arc::new(dform_provider_ovh_signer::Ovh));
    let h = host.open(held.name()).unwrap().handle;
    (host, held, h)
}

/// A POST with a body, sent twice with the credential: the signer made
/// its material once, asking the API's clock once (the host performing
/// the fetch), and each request carries exactly the application key, the
/// consumer key, the API's time and `$1$` and the SHA-1 of the secret, the
/// consumer key, the method, the URL, the body and that time, joined by
/// `+`; the request's own headers kept.
#[test]
fn the_host_signs_each_request_with_the_providers_signer() {
    let server = Server::start();
    server.clock_at(1457018875);
    let (host, _held, h) = signing_host(&server);
    let url = format!("{}/cloud/project/{}/sshkey", server.endpoint, fake::PROJECT);
    let body = r#"{"name":"k","publicKey":"ssh-ed25519 A"}"#;
    for _ in 0..2 {
        let req = HttpRequest {
            method: "POST".into(),
            url: url.clone(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.into(),
            ..HttpRequest::default()
        };
        let r = host.send(req, Some(h), None).unwrap();
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    }
    assert_eq!(server.times_asked(), 1);
    let seen = server.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    for s in seen {
        let sent: BTreeMap<String, String> = s
            .headers
            .into_iter()
            .filter(|(k, _)| k.starts_with("x-ovh-") || k == "content-type")
            .collect();
        // The API's time is its clock's answer and the seconds since,
        // on the host's clock: the same second, or the next.
        let time = sent["x-ovh-timestamp"].clone();
        assert!(
            ["1457018875", "1457018876"].contains(&time.as_str()),
            "{time}"
        );
        let want: BTreeMap<String, String> = [
            ("content-type", "application/json".to_string()),
            ("x-ovh-application", fake::APPLICATION_KEY.into()),
            ("x-ovh-consumer", fake::CONSUMER_KEY.into()),
            ("x-ovh-signature", ovh_signature("POST", &url, body, &time)),
            ("x-ovh-timestamp", time),
        ]
        .map(|(k, v)| (k.to_string(), v))
        .into();
        assert_eq!(sent, want);
    }
}

/// A refusal of the API's clock is the credential's error, naming it;
/// nothing is sent unsigned.
#[test]
fn no_clock_is_no_credential() {
    let server = Server::start();
    server.fail("GET /auth/time", &[403]);
    let held = server.signature();
    let host =
        Services::new(Grants::none("ovh")).with_signer(Arc::new(dform_provider_ovh_signer::Ovh));
    let e = host.open(held.name()).unwrap_err();
    assert!(
        e.message.starts_with(
            "provider ovh: the credential the program gave it: the API's time: HTTP 403"
        ),
        "{e:?}"
    );
    assert!(server.calls().is_empty(), "{:?}", server.calls());
}

/// A provider with no signer cannot open a `signed` credential: the host
/// does not know how.
#[test]
fn a_signed_credential_needs_its_providers_signer() {
    let parts = [("keys".to_string(), Secret::new(b"x".to_vec()))];
    let held = credentials::hold(Kind::Signed, "plain", parts.into());
    let e = Services::new(Grants::none("plain"))
        .open(held.name())
        .unwrap_err();
    assert_eq!(
        e.message,
        format!(
            "provider plain: the credential {} is its signer's to make, and the provider has no \
             signer",
            held.name()
        )
    );
}

/// A kubeconfig held for the provider `p` whose exec plugin answers
/// `t1`, `t2`, .. (one more each run, counted in `dir`), expiring at
/// `expires`, for the server `url`.
fn exec_kubeconfig(dir: &std::path::Path, url: &str, expires: &str) -> credentials::Holding {
    let script = "n=$(( $(cat \"$DIR/runs\" 2>/dev/null || echo 0) + 1 )); echo $n > \"$DIR/runs\"; \
                  printf '{\"status\": {\"token\": \"t%s\", \"expirationTimestamp\": \"%s\"}}' $n \"$EXP\"";
    let kc = serde_json::json!({
        "current-context": "c",
        "contexts": [{"name": "c", "context": {"cluster": "k", "user": "u"}}],
        "clusters": [{"name": "k", "cluster": {"server": url}}],
        "users": [{"name": "u", "user": {"exec": {
            "apiVersion": "client.authentication.k8s.io/v1",
            "command": "sh", "args": ["-c", script],
            "env": [{"name": "DIR", "value": dir.display().to_string()},
                    {"name": "EXP", "value": expires}]}}}]
    });
    let parts = [(
        "kubeconfig".to_string(),
        Secret::new(kc.to_string().into_bytes()),
    )];
    credentials::hold(Kind::Kubeconfig, "p", parts.into())
}

/// A kubeconfig is the k8s provider's signer's to make, its exec plugin
/// run by the host: its token is used until it expires, then the signer
/// makes it again for the next request, the plugin run again; one that
/// does not expire runs once. Its server is what the provider reads off
/// the credential.
#[test]
fn a_kubeconfigs_exec_plugin_runs_again_when_its_token_expires() {
    use dform::http::fake::Server;
    let server = Server::http1();
    server.answer(|s| {
        (
            200,
            Vec::new(),
            s.header("authorization").unwrap_or("").into(),
        )
    });
    let scratch = crate::common::Scratch::new("kubeconfig-exec");
    let dir = scratch.path("runs");
    std::fs::create_dir_all(&dir).unwrap();
    let two_requests = |held: &credentials::Holding| {
        let host =
            Services::new(Grants::none("p")).with_signer(Arc::new(dform_provider_k8s_signer::Kube));
        let opened = host.open(held.name()).unwrap();
        assert_eq!(opened.endpoint, Some(server.url("")));
        (0..2)
            .map(|_| {
                let req = HttpRequest {
                    method: "GET".into(),
                    url: server.url("/"),
                    ..HttpRequest::default()
                };
                let r = host.send(req, Some(opened.handle), None).unwrap();
                String::from_utf8(r.body).unwrap()
            })
            .collect::<Vec<_>>()
    };
    let held = exec_kubeconfig(&dir, &server.url(""), "2000-01-01T00:00:00Z");
    assert_eq!(two_requests(&held), ["Bearer t2", "Bearer t3"]);
    std::fs::remove_file(dir.join("runs")).unwrap();
    let held = exec_kubeconfig(&dir, &server.url(""), "2999-01-01T00:00:00Z");
    assert_eq!(two_requests(&held), ["Bearer t1", "Bearer t1"]);
}

/// A program's kubeconfig that does not parse is said without the name
/// dform made for its credential, and never quoted.
#[test]
fn a_programs_kubeconfig_that_does_not_parse_says_so() {
    let host =
        Services::new(Grants::none("k8s")).with_signer(Arc::new(dform_provider_k8s_signer::Kube));
    let bad = [("kubeconfig".to_string(), Secret::new(b"s3cret: [".to_vec()))];
    let held = credentials::hold(Kind::Kubeconfig, "k8s", bad.into());
    let e = host.open(held.name()).unwrap_err();
    assert_eq!(
        e.message,
        "provider k8s: the credential the program gave it: it is not a kubeconfig (YAML)"
    );
}

#[cfg(feature = "wasm")]
mod wasm {
    use super::*;
    use crate::common;
    use dform::plugin::bundle;
    use dform::plugin::signer::{Got, Outcome, Request, Response, Signer};
    use dform_host::wasm::Read;
    use dform_host::wasm::signer::{Component, of_provider};

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// The OVH signer as a component, built beside the provider's.
    /// The OVH wasm provider, as `cargo xtask components` builds it,
    /// carrying its signer.
    fn ovh_wasm() -> String {
        dform_host::wasm::compile_cache(crate::host_trace::wasm_cache());
        crate::host_trace::built("dform_provider_ovh.wasm", "cargo xtask components")
    }

    /// The signer the OVH wasm provider carries.
    fn signer() -> Component {
        let path = std::path::PathBuf::from(ovh_wasm());
        let read = Read::file(&path).unwrap();
        let bytes = bundle::section(&read.bytes, bundle::SIGNER).unwrap();
        Component::new(bytes.expect("the provider carries its signer"), &path).unwrap()
    }

    /// The signer component, given the environment's keys and endpoint,
    /// asks the API's clock; given its answer (75 s ahead of the host's),
    /// makes the material; and signs a request with it at a known `now`
    /// exactly as go-ovh's vectors have it.
    #[test]
    fn the_signer_component_signs_as_ovhs_clients_do() {
        let signer = signer();
        let env = Got::Env(vec![
            (s("OVH_APPLICATION_KEY"), s("ak")),
            (
                s("OVH_APPLICATION_SECRET"),
                s("9ufkBmLaTQ9nz5yMUlg79taH0GNnzDjk"),
            ),
            (s("OVH_CONSUMER_KEY"), s("5mBuy6SUQcRw2ZUxg0cG68BoDKpED4KY")),
            (s("OVH_ENDPOINT"), s("ovh-eu")),
        ]);
        let mut got = vec![Got::Given(Vec::new()), env];
        let asked = signer.step(&got);
        assert!(
            matches!(asked, Ok(Outcome::More(ref n)) if n.len() == 1),
            "the clock is asked: {asked:?}"
        );
        got.push(Got::Fetch(Response {
            status: 200,
            body: b"1457018875".to_vec(),
            at: 1457018800,
            ..Response::default()
        }));
        let Ok(Outcome::Material(m)) = signer.step(&got) else {
            panic!("the material")
        };
        let req = Request {
            method: s("GET"),
            url: s("https://eu.api.ovh.com/1.0/auth"),
            ..Request::default()
        };
        let signed = signer.sign(req, &m, 1457018800).unwrap();
        assert_eq!(
            signed.headers,
            [
                ("X-Ovh-Application", "ak"),
                ("X-Ovh-Consumer", "5mBuy6SUQcRw2ZUxg0cG68BoDKpED4KY"),
                ("X-Ovh-Timestamp", "1457018875"),
                (
                    "X-Ovh-Signature",
                    "$1$e9556054b6309771395efa467c22e627407461ad"
                ),
            ]
            .map(|(k, v)| (s(k), s(v)))
        );
    }

    /// A provider whose signer imports anything is refused as it is
    /// linked, naming what its signer imports.
    #[test]
    fn a_signer_that_imports_anything_is_refused() {
        let scratch = common::Scratch::new("signer-imports");
        let signer = wat::parse_str(
            r#"(component
                 (import "wasi:sockets/types@0.3.0" (instance
                   (export "tcp-socket" (type (sub resource))))))"#,
        )
        .unwrap();
        let provider = wat::parse_str("(component)").unwrap();
        let provider = bundle::with_sections(&provider, &[(bundle::SIGNER, &signer)]).unwrap();
        let path = scratch.path("dform-provider-net.wasm");
        let e = format!(
            "{:#}",
            of_provider(&Read::of(provider), &path).err().unwrap()
        );
        assert!(
            e.contains(&format!(
                "the signer of {} imports wasi:sockets/types@0.3.0: a signer imports nothing",
                path.display()
            )),
            "{e}"
        );
    }

    /// A wasm provider is one file: its signer is the one it carries,
    /// whatever the file's name (cargo's hyphenated link to it too); one
    /// that carries none refuses a credential its signer would make,
    /// naming the file and the section.
    #[test]
    fn the_signer_is_the_one_the_provider_carries() {
        use dform::plugin::signer::Need;
        let scratch = common::Scratch::new("signer-carried");
        let hyphenated = scratch.path("dform-provider-ovh.wasm");
        std::fs::copy(ovh_wasm(), &hyphenated).unwrap();
        let read = Read::file(&hyphenated).unwrap();
        assert_eq!(
            of_provider(&read, &hyphenated).unwrap().needs().unwrap(),
            [Need::Given]
        );
        let bare = Read::of(wat::parse_str("(component)").unwrap());
        let e = of_provider(&bare, &hyphenated)
            .unwrap()
            .needs()
            .unwrap_err();
        assert_eq!(
            e,
            format!(
                "the provider has no signer: its component {} carries no `dform.signer` section",
                hyphenated.display()
            )
        );
    }

    /// The `.df` sources a wasm provider carries are written out under the
    /// host's cache when it is linked, where its manifest says.
    #[test]
    fn a_providers_sources_are_where_its_manifest_says() {
        use dform::plugin::host::Grants;
        let mut grants = Grants::none("ovh");
        grants.allow.insert("wasi:filesystem".into());
        let link = dform_host::wasm::link(std::path::Path::new(&ovh_wasm()), grants).unwrap();
        let manifest = link.hosting.as_ref().unwrap().manifest.as_ref().unwrap();
        let src = manifest.sources.as_ref().expect("its sources");
        let schema = crate::common::repo().join("crates/dform-provider-ovh/src/schema.df");
        assert_eq!(
            std::fs::read(src.join("src/schema.df")).unwrap(),
            std::fs::read(schema).unwrap()
        );
    }

    /// `dform plan` with the OVH provider as a wasm provider, as dform.toml
    /// names it: `env` the environment's (`OVH_*`), `grant` its
    /// credentials. The run, and the server.
    fn plan(name: &str, env: &[(&str, String)], grant: &str) -> (common::Run, Server) {
        let wasm = crate::host_trace::built(
            "dform_provider_ovh.wasm",
            "cargo build -p dform-provider-ovh --release --target wasm32-wasip3 --features \
             component",
        );
        let server = Server::start();
        let s = common::Scratch::project(name);
        s.write(
            "dform.toml",
            &format!(
                "[project]\nedition = \"2026\"\n\n[providers]\novh = {{ path = \"{wasm}\", \
                 allow = [\"wasi:filesystem\"]{grant} }}\n"
            ),
        );
        s.write(
            "main.df",
            &format!(
                "use ovh {{ endpoint = \"{}\", project = \"{}\" }}\n\n\
                 resource ovh.ssh_key admin {{ name = \"lab-admin\", public_key = \"ssh-ed25519 \
                 AAAAC3Nz lab\" }}\n",
                server.endpoint,
                fake::DESCRIPTION
            ),
        );
        let mut c = common::dform();
        c.args(["plan", "main.df"])
            .current_dir(&s.dir)
            .env("HOME", &s.dir)
            .env("XDG_CONFIG_HOME", s.path("config"))
            .env("XDG_CACHE_HOME", crate::host_trace::wasm_cache())
            .env("DFORM_OVH_RESOLVER", "127.0.0.1:9");
        for k in ["OVH_CLIENT_ID", "OVH_CLIENT_SECRET", "OVH_ACCESS_TOKEN"] {
            c.env_remove(k);
        }
        for (k, v) in server.env().into_iter().chain(env.iter().cloned()) {
            c.env(k, v);
        }
        (common::Run::from(c.output().unwrap()), server)
    }

    /// The OVH provider as a wasm provider, which holds no secret, plans
    /// with the keys dform.toml grants (`signed:environment`): its signer,
    /// a component of its own, signs every call the host sends.
    #[test]
    fn the_ovh_wasm_provider_plans_with_the_keys_its_signer_signs_with() {
        let (r, server) = plan(
            "ovh-signed-wasm",
            &[],
            ", credentials = [\"signed:environment\"]",
        );
        let r = r.success();
        assert!(r.stdout.contains("+ ovh.ssh_key admin"), "{}", r.stdout);
        let seen = server.seen();
        let signed = seen.iter().all(|s| {
            s.headers.get("x-ovh-consumer").map(String::as_str) == Some(fake::CONSUMER_KEY)
        });
        assert!(signed && !seen.is_empty(), "{seen:?}");
        assert_eq!(server.times_asked(), 1);
    }

    /// The wasm provider has no environment: given a service account's
    /// `OVH_CLIENT_ID` and `OVH_CLIENT_SECRET` in dform's, and no grant,
    /// it sees neither (no token is minted) and says it has no
    /// credentials.
    #[test]
    fn a_wasm_provider_sees_no_environment() {
        let env = [
            ("OVH_CLIENT_ID", fake::CLIENT_ID.to_string()),
            ("OVH_CLIENT_SECRET", fake::CLIENT_SECRET.to_string()),
        ];
        let (r, server) = plan("ovh-no-env", &env, "");
        let r = r.failure();
        assert!(r.stderr.contains("no OVH credentials"), "{}", r.stderr);
        assert_eq!(server.tokens_minted(), 0);
        assert!(server.calls().is_empty(), "{:?}", server.calls());
    }
}

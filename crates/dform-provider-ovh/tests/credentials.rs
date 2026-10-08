//! The provider's two forms of credentials against the fake API (R-179):
//! a service account's client id and secret, a bearer token minted from
//! them, kept and minted afresh; the three keys, a refused consumer key
//! said with its fix, and its validity and rights as Configure reads
//! them.

use dform_core::approval::parse_rfc3339;
use dform_provider_ovh::api::Client;
use dform_provider_ovh::config;
use dform_provider_ovh::credential::notes;
use dform_provider_ovh::fake::{self, Server};
use dform_provider_ovh::ovh::Ovh;
use serde_json::json;
use std::path::PathBuf;

fn lookup(env: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
    move |k| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
}

/// A client of `server` with the credentials `env` gives.
fn client(server: &Server, env: Vec<(&'static str, String)>) -> Client {
    Client::new(config::resolve(Some(&server.endpoint), &lookup(env), &[]).unwrap())
}

fn scratch(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("ovh-credentials-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A program's provider configured with a service account finds its
/// project with bearer calls, one token minted for all of them.
#[test]
fn a_service_account_configures_the_provider() {
    let server = Server::start();
    let ovh = Ovh::new();
    let account = ovh
        .configure_with(
            &json!({"settings": {"endpoint": server.endpoint, "project": fake::DESCRIPTION}}),
            &lookup(server.oauth_env()),
            &[],
        )
        .unwrap();
    assert_eq!(account.as_deref(), Some(fake::PROJECT));
    assert_eq!(server.tokens_minted(), 1);
    assert!(server.calls().contains(&"GET /cloud/project".to_string()));
    // No consumer key to describe, and nothing signed.
    assert_eq!((server.key_asked(), server.times_asked()), (0, 0));
}

/// A token is kept until it is about to expire, and a refused one (a
/// 401) is minted afresh, once.
#[test]
fn a_token_is_kept_and_minted_afresh_when_refused() {
    let server = Server::start();
    let c = client(&server, server.oauth_env());
    c.get("/cloud/project").unwrap();
    c.get("/cloud/project").unwrap();
    assert_eq!(server.tokens_minted(), 1);
    server.revoke_tokens();
    c.get("/cloud/project").unwrap();
    assert_eq!(server.tokens_minted(), 2);
    // Refused again after a fresh token: the refusal is the answer.
    server.fail("GET /cloud/project", &[401, 401]);
    let e = c.get("/cloud/project").unwrap_err();
    assert_eq!(e.status(), Some(401), "{e}");
    assert_eq!(server.tokens_minted(), 3);
    // A token good for less than the margin is minted for each call.
    let server = Server::start();
    server.token_life(30);
    let c = client(&server, server.oauth_env());
    c.get("/cloud/project").unwrap();
    c.get("/cloud/project").unwrap();
    assert_eq!(server.tokens_minted(), 2);
}

/// A wrong client secret is refused a token, naming the service account
/// and the endpoint.
#[test]
fn a_wrong_client_secret_is_named() {
    let server = Server::start();
    let mut env = server.oauth_env();
    env.retain(|(k, _)| *k != "OVH_CLIENT_SECRET");
    env.push(("OVH_CLIENT_SECRET", "wrong".into()));
    let e = client(&server, env)
        .get("/cloud/project")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(&format!(
            "HTTP 401: the service account {} was refused a token for {}: client \
             authentication failed",
            fake::CLIENT_ID,
            server.endpoint
        )),
        "{e}"
    );
}

/// A consumer key the API no longer takes ("This credential is not
/// valid") is said as expired or revoked, with where to make one that
/// does not expire.
#[test]
fn an_expired_consumer_key_says_so_and_the_fix() {
    let server = Server::start();
    server.revoke_key();
    let e = Ovh::new()
        .configure_with(
            &json!({"settings": {"endpoint": server.endpoint, "project": fake::PROJECT}}),
            &lookup(server.env()),
            &[],
        )
        .unwrap_err();
    let base = server.endpoint.strip_suffix("/1.0").unwrap();
    assert!(
        format!("{e:#}").ends_with(&format!(
            "HTTP 403: the consumer key for {} expired or was revoked; make one with unlimited \
             validity at {base}/createToken/ or use a service account (docs/providers/ovh.md)",
            server.endpoint
        )),
        "{e:#}"
    );
}

/// A consumer key that expires within a week is warned of, from the
/// key's description, which a program's run keeps in the cache; one that
/// never expires is not. Outside a program the form, the validity and
/// each missing right are said.
#[test]
fn a_short_validity_is_warned_of_and_the_rights_named() {
    let server = Server::start();
    let c = client(&server, server.env());
    let now = parse_rfc3339("2026-10-08T10:00:00Z").unwrap();
    let cache = scratch("validity");
    server.key_expires(Some("2026-10-09T08:00:00+02:00"));
    let warning = format!(
        "warning: provider ovh: the consumer key for {} expires 2026-10-09T06:00:00Z, in 20 \
         hours; make one with unlimited validity at {}/createToken/ or use a service account \
         (docs/providers/ovh.md)",
        server.endpoint,
        server.endpoint.strip_suffix("/1.0").unwrap()
    );
    assert_eq!(
        notes(&c, false, Some(&cache), now),
        std::slice::from_ref(&warning)
    );
    assert_eq!(
        notes(&c, false, Some(&cache), now),
        std::slice::from_ref(&warning)
    );
    assert_eq!(server.key_asked(), 1, "the second run reads the cache");
    // A week and more ahead: nothing to say.
    let later = parse_rfc3339("2026-10-01T00:00:00Z").unwrap();
    assert!(notes(&c, false, Some(&cache), later).is_empty());

    // Outside a program: the form, the validity, what it lacks.
    server.key_rules(&[
        ("GET", "/*"),
        ("POST", "/cloud/project/*"),
        ("PUT", "/cloud/project/*"),
        ("DELETE", "/cloud/project/*"),
    ]);
    let e = &server.endpoint;
    assert_eq!(
        notes(&c, true, None, now),
        [
            format!(
                "provider ovh: credentials for {e}: an application key and a consumer key; the \
                 consumer key expires 2026-10-09T06:00:00Z"
            ),
            format!("provider ovh: the consumer key for {e} lacks POST /domain/zone/*"),
            format!("provider ovh: the consumer key for {e} lacks PUT /domain/zone/*"),
            format!("provider ovh: the consumer key for {e} lacks DELETE /domain/zone/*"),
            warning,
        ]
    );
    server.key_expires(None);
    assert_eq!(
        notes(&c, true, None, now),
        [
            format!(
                "provider ovh: credentials for {e}: an application key and a consumer key; the \
                 consumer key does not expire"
            ),
            format!("provider ovh: the consumer key for {e} lacks POST /domain/zone/*"),
            format!("provider ovh: the consumer key for {e} lacks PUT /domain/zone/*"),
            format!("provider ovh: the consumer key for {e} lacks DELETE /domain/zone/*"),
        ]
    );
    // A service account's rights are not the API's to answer.
    let c = client(&server, server.oauth_env());
    assert_eq!(
        notes(&c, true, None, now),
        [format!(
            "provider ovh: credentials for {e}: the service account {}; its rights are its IAM \
             policies",
            fake::CLIENT_ID
        )]
    );
}

/// An access token minted elsewhere is carried as it is: the provider
/// configures with it, mints nothing and signs nothing; refused, it is
/// said as expired, with the fix.
#[test]
fn an_access_token_is_used_as_it_is() {
    let server = Server::start();
    let ovh = Ovh::new();
    let account = ovh
        .configure_with(
            &json!({"settings": {"endpoint": server.endpoint, "project": fake::DESCRIPTION}}),
            &lookup(server.token_env()),
            &[],
        )
        .unwrap();
    assert_eq!(account.as_deref(), Some(fake::PROJECT));
    assert_eq!(
        (
            server.tokens_minted(),
            server.times_asked(),
            server.key_asked()
        ),
        (0, 0, 0)
    );
    let c = client(&server, server.token_env());
    let e = &server.endpoint;
    assert_eq!(
        notes(&c, true, None, 0),
        [format!(
            "provider ovh: credentials for {e}: an access token, used as it is; its rights are \
             its own"
        )]
    );
    server.revoke_tokens();
    let err = c.get("/cloud/project").unwrap_err().to_string();
    assert!(
        err.ends_with(&format!(
            "HTTP 401: the access token for {e} expired or was revoked: one given as \
             access_token (OVH_ACCESS_TOKEN) is used as it is and never minted again; give a new \
             one, or a service account's client_id and client_secret (docs/providers/ovh.md)"
        )),
        "{err}"
    );
    assert_eq!(server.tokens_minted(), 0);
}

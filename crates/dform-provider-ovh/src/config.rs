//! The provider's credentials: its own configuration, never the program's
//! (R-44 amendment, R-40). As OVH's SDKs read them: `OVH_ENDPOINT`,
//! `OVH_APPLICATION_KEY`, `OVH_APPLICATION_SECRET` and `OVH_CONSUMER_KEY`
//! in the environment, else `ovh.conf`:
//!
//! ```text
//! [default]
//! endpoint=ovh-ca
//!
//! [ovh-ca]
//! application_key=...
//! application_secret=...
//! consumer_key=...
//! ```
//!
//! or, for a service account (R-179), `client_id` and `client_secret`
//! (`OVH_CLIENT_ID`, `OVH_CLIENT_SECRET`) in place of the three keys: the
//! client mints a bearer token from them (`api`); or `access_token`
//! (`OVH_ACCESS_TOKEN`), a bearer token minted elsewhere, used as it is.
//! A section holds one form of the three.
//!
//! read, as OVH's SDKs read them (python-ovh's `config.py`, go-ovh's
//! `configuration.go`), from `/etc/ovh.conf`, `~/.ovh.conf` and
//! `./ovh.conf` (the working directory: dform's `-C` directory), a later
//! file overriding an earlier one key by key. The program's `use ovh {
//! endpoint }` names the account's endpoint and wins over both; its keys
//! come from the section of that name.

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The API endpoints OVH's SDKs know, by name.
pub const ENDPOINTS: [(&str, &str); 7] = [
    ("ovh-eu", "https://eu.api.ovh.com/1.0"),
    ("ovh-ca", "https://ca.api.ovh.com/1.0"),
    ("ovh-us", "https://api.us.ovhcloud.com/1.0"),
    ("kimsufi-eu", "https://eu.api.kimsufi.com/1.0"),
    ("kimsufi-ca", "https://ca.api.kimsufi.com/1.0"),
    ("soyoustart-eu", "https://eu.api.soyoustart.com/1.0"),
    ("soyoustart-ca", "https://ca.api.soyoustart.com/1.0"),
];

/// OAuth2's token endpoint of each API endpoint that offers it, as OVH's
/// own SDKs know them.
pub const TOKEN_URLS: [(&str, &str); 3] = [
    ("ovh-eu", "https://www.ovh.com/auth/oauth2/token"),
    ("ovh-ca", "https://ca.ovh.com/auth/oauth2/token"),
    ("ovh-us", "https://us.ovhcloud.com/auth/oauth2/token"),
];

/// What an authenticated call needs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Credentials {
    /// The endpoint as named (`ovh-ca`, or a URL).
    pub endpoint: String,
    /// Its base URL, no trailing slash: `https://ca.api.ovh.com/1.0`.
    pub url: String,
    pub auth: Auth,
}

/// How calls are authenticated.
#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    /// An application's key and secret and a consumer key: each call is
    /// signed (`sign`).
    Keys {
        application_key: String,
        application_secret: String,
        consumer_key: String,
    },
    /// A service account's OAuth2 client credentials: each call carries
    /// a bearer token minted from them at `token_url`.
    OAuth2 {
        client_id: String,
        client_secret: String,
        token_url: String,
    },
    /// A bearer token minted elsewhere, which each call carries as it
    /// is: nothing mints another when it expires.
    Token { access_token: String },
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Keys {
                application_key, ..
            } => f
                .debug_struct("Keys")
                .field("application_key", application_key)
                .finish_non_exhaustive(),
            Auth::OAuth2 {
                client_id,
                token_url,
                ..
            } => f
                .debug_struct("OAuth2")
                .field("client_id", client_id)
                .field("token_url", token_url)
                .finish_non_exhaustive(),
            Auth::Token { .. } => f.debug_struct("Token").finish_non_exhaustive(),
        }
    }
}

impl Credentials {
    /// Where a consumer key is made for this endpoint:
    /// `https://ca.api.ovh.com/createToken/`.
    pub fn create_token_url(&self) -> String {
        let base = self.url.strip_suffix("/1.0").unwrap_or(&self.url);
        format!("{base}/createToken/")
    }

    /// What a refused access token means, and the fix.
    pub fn expired_token(&self) -> String {
        format!(
            "the access token for {} expired or was revoked: one given as access_token \
             (OVH_ACCESS_TOKEN) is used as it is and never minted again; give a new one, or a \
             service account's client_id and client_secret (docs/providers/ovh.md)",
            self.endpoint
        )
    }

    /// What a refused consumer key means, and the fix.
    pub fn expired_key(&self) -> String {
        format!(
            "the consumer key for {} expired or was revoked; make one with unlimited validity at \
             {} or use a service account (docs/providers/ovh.md)",
            self.endpoint,
            self.create_token_url()
        )
    }
}

/// The OAuth2 token endpoint of `endpoint` (its base URL `url`): a known
/// name's, or, for a URL, `/auth/oauth2/token` at its origin.
fn token_url(endpoint: &str, url: &str) -> Option<String> {
    if let Some((_, t)) = TOKEN_URLS.iter().find(|(n, _)| *n == endpoint) {
        return Some(t.to_string());
    }
    if !endpoint.contains("://") {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split('/').next()?;
    Some(format!("{scheme}://{host}/auth/oauth2/token"))
}

/// An endpoint's base URL: a known name's, or the URL itself.
pub fn endpoint_url(endpoint: &str) -> Option<String> {
    if endpoint.starts_with("https://") || endpoint.starts_with("http://") {
        return Some(endpoint.trim_end_matches('/').to_string());
    }
    ENDPOINTS
        .iter()
        .find(|(n, _)| *n == endpoint)
        .map(|(_, u)| u.to_string())
}

/// An INI file's sections, each key to its value. `;` and `#` start a
/// comment line; keys before any section are in the section `""`.
pub fn parse_ini(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_string();
            out.entry(section.clone()).or_default();
            continue;
        }
        if let Some((k, v)) = line.split_once('=').or_else(|| line.split_once(':')) {
            out.entry(section.clone())
                .or_default()
                .insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

/// The configuration files, lowest precedence first: the machine's,
/// the user's, the working directory's.
pub fn default_files() -> Vec<PathBuf> {
    files_in(
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::current_dir().ok(),
    )
}

/// The configuration files for the home directory `home` and the working
/// directory `cwd`, as OVH's SDKs list them.
pub fn files_in(home: Option<PathBuf>, cwd: Option<PathBuf>) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("/etc/ovh.conf")];
    out.extend(home.map(|h| h.join(".ovh.conf")));
    out.extend(cwd.map(|d| d.join("ovh.conf")));
    out
}

/// The credentials for `endpoint` (the program's setting, if it gives one)
/// from `env` (a variable's value) and the files `files`, lowest
/// precedence first. Missing a key is an error naming where it was looked
/// for.
pub fn resolve(
    endpoint: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
    files: &[PathBuf],
) -> Result<Credentials> {
    let sources = Sources::read(env, files);
    let endpoint = sources.endpoint(endpoint)?;
    let Some(url) = endpoint_url(&endpoint) else {
        bail!(
            "the OVH endpoint {endpoint:?} is not one of {}, nor a URL",
            ENDPOINTS.map(|(n, _)| n).join(", ")
        );
    };
    let auth = sources.auth(&endpoint, &url)?;
    Ok(Credentials {
        endpoint,
        url,
        auth,
    })
}

/// Where credentials are read: the environment, over the files' sections
/// merged (a later file's key over an earlier one's).
struct Sources<'a> {
    env: &'a dyn Fn(&str) -> Option<String>,
    files: &'a [PathBuf],
    conf: BTreeMap<String, BTreeMap<String, String>>,
}

impl<'a> Sources<'a> {
    fn read(env: &'a dyn Fn(&str) -> Option<String>, files: &'a [PathBuf]) -> Self {
        let mut conf: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for f in files {
            if let Ok(text) = std::fs::read_to_string(f) {
                for (s, kv) in parse_ini(&text) {
                    conf.entry(s).or_default().extend(kv);
                }
            }
        }
        Sources { env, files, conf }
    }

    /// A variable's value, unless empty.
    fn env(&self, k: &str) -> Option<String> {
        (self.env)(k).filter(|v| !v.is_empty())
    }

    /// Where `var` was looked for, for a message.
    fn looked(&self, var: &str) -> String {
        let files = self
            .files
            .iter()
            .map(|f| f.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("the environment ({var}) and {files}")
    }

    /// The endpoint `given`, else the environment's, else the files'
    /// default.
    fn endpoint(&self, given: Option<&str>) -> Result<String> {
        if let Some(e) = given.filter(|e| !e.is_empty()) {
            return Ok(e.to_string());
        }
        match self.env("OVH_ENDPOINT").or_else(|| {
            self.conf
                .get("default")
                .and_then(|d| d.get("endpoint"))
                .cloned()
        }) {
            Some(e) => Ok(e),
            None => bail!(
                "no OVH endpoint: the provider block names none (`use ovh {{ endpoint = \
                 \"ovh-ca\" }}`), and neither does {}",
                self.looked("OVH_ENDPOINT")
            ),
        }
    }

    /// `endpoint`'s key `name`, or the variable `var`.
    fn key(&self, endpoint: &str, var: &str, name: &str) -> Result<String> {
        let section = self.conf.get(endpoint);
        match self
            .env(var)
            .or_else(|| section.and_then(|s| s.get(name)).cloned())
        {
            Some(v) => Ok(v),
            None => bail!(
                "no OVH {name} for the endpoint {endpoint}: set it under [{endpoint}] in \
                 ovh.conf, or {var}; looked in {}",
                self.looked(var)
            ),
        }
    }

    /// Each key of `keys` that is set for `endpoint`, by where it was set.
    fn set(&self, endpoint: &str, keys: &[(&str, &str)]) -> Vec<String> {
        let section = self.conf.get(endpoint);
        keys.iter()
            .filter_map(|(var, name)| match self.env(var) {
                Some(_) => Some(var.to_string()),
                None => section.and_then(|s| s.get(*name)).map(|_| name.to_string()),
            })
            .collect()
    }

    /// The one form of credentials set for `endpoint` (at `url`): an
    /// access token, a service account, or a consumer key's three keys
    /// when neither is; more than one form is an error naming them.
    fn auth(&self, endpoint: &str, url: &str) -> Result<Auth> {
        let three = self.set(endpoint, &KEYS);
        let client = self.set(endpoint, &CLIENT);
        let token = self.set(endpoint, &TOKEN);
        let given: Vec<String> = [
            ("a consumer key's", &three),
            ("a service account's", &client),
            ("an access token's", &token),
        ]
        .into_iter()
        .filter(|(_, keys)| !keys.is_empty())
        .map(|(form, keys)| format!("{form} ({})", keys.join(", ")))
        .collect();
        if let [first @ .., last] = given.as_slice()
            && !first.is_empty()
        {
            let (both, which) = match first.len() {
                1 => ("both ", "one or the other"),
                _ => ("", "one of them"),
            };
            bail!(
                "the credentials for the endpoint {endpoint} give {both}{} and {last}: they are \
                 {which}; remove the ones you do not use from [{endpoint}] in ovh.conf or the \
                 environment",
                first.join(", ")
            );
        }
        let key = |var: &str, name: &str| self.key(endpoint, var, name);
        Ok(if !token.is_empty() {
            Auth::Token {
                access_token: key("OVH_ACCESS_TOKEN", "access_token")?,
            }
        } else if client.is_empty() {
            Auth::Keys {
                application_key: key("OVH_APPLICATION_KEY", "application_key")?,
                application_secret: key("OVH_APPLICATION_SECRET", "application_secret")?,
                consumer_key: key("OVH_CONSUMER_KEY", "consumer_key")?,
            }
        } else {
            let Some(token_url) = token_url(endpoint, url) else {
                bail!(
                    "the OVH endpoint {endpoint} takes no service account: give it \
                     application_key, application_secret and consumer_key under [{endpoint}] \
                     in ovh.conf"
                );
            };
            Auth::OAuth2 {
                client_id: key("OVH_CLIENT_ID", "client_id")?,
                client_secret: key("OVH_CLIENT_SECRET", "client_secret")?,
                token_url,
            }
        })
    }
}

/// The three keys' variables and names in ovh.conf.
const KEYS: [(&str, &str); 3] = [
    ("OVH_APPLICATION_KEY", "application_key"),
    ("OVH_APPLICATION_SECRET", "application_secret"),
    ("OVH_CONSUMER_KEY", "consumer_key"),
];

/// A service account's.
const CLIENT: [(&str, &str); 2] = [
    ("OVH_CLIENT_ID", "client_id"),
    ("OVH_CLIENT_SECRET", "client_secret"),
];

/// A bearer token minted elsewhere.
const TOKEN: [(&str, &str); 1] = [("OVH_ACCESS_TOKEN", "access_token")];

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("dform-ovh-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The three keys of the keys form.
    fn keys(c: &Credentials) -> (&str, &str, &str) {
        match &c.auth {
            Auth::Keys {
                application_key,
                application_secret,
                consumer_key,
            } => (application_key, application_secret, consumer_key),
            a => panic!("not the keys form: {a:?}"),
        }
    }

    const CONF: &str = "\
; written by ovh's own tool
[default]
endpoint=ovh-ca

[ovh-ca]
application_key=ak-ca
application_secret = as-ca
consumer_key=ck-ca

[ovh-eu]
application_key=ak-eu
application_secret=as-eu
consumer_key=ck-eu
";

    #[test]
    fn reads_the_ini_sections() {
        let ini = parse_ini(CONF);
        assert_eq!(ini["default"]["endpoint"], "ovh-ca");
        assert_eq!(ini["ovh-ca"]["application_secret"], "as-ca");
        assert_eq!(ini["ovh-eu"].len(), 3);
    }

    #[test]
    fn the_default_endpoint_and_its_section() {
        let d = scratch("default");
        std::fs::write(d.join("ovh.conf"), CONF).unwrap();
        let none = |_: &str| None;
        let c = resolve(None, &none, &[d.join("ovh.conf")]).unwrap();
        assert_eq!(c.endpoint, "ovh-ca");
        assert_eq!(c.url, "https://ca.api.ovh.com/1.0");
        assert_eq!(keys(&c), ("ak-ca", "as-ca", "ck-ca"));
        // The program's endpoint picks the section.
        let c = resolve(Some("ovh-eu"), &none, &[d.join("ovh.conf")]).unwrap();
        assert_eq!(keys(&c).0, "ak-eu");
        assert_eq!(c.url, "https://eu.api.ovh.com/1.0");
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let d = scratch("env");
        std::fs::write(d.join("ovh.conf"), CONF).unwrap();
        let env = |k: &str| match k {
            "OVH_ENDPOINT" => Some("ovh-eu".to_string()),
            "OVH_CONSUMER_KEY" => Some("ck-env".to_string()),
            _ => None,
        };
        let c = resolve(None, &env, &[d.join("ovh.conf")]).unwrap();
        assert_eq!(c.endpoint, "ovh-eu");
        assert_eq!((keys(&c).0, keys(&c).2), ("ak-eu", "ck-env"));
    }

    #[test]
    fn a_later_file_overrides_an_earlier_one_key_by_key() {
        let d = scratch("layers");
        std::fs::write(d.join("etc.conf"), CONF).unwrap();
        std::fs::write(d.join("home.conf"), "[ovh-ca]\nconsumer_key=ck-home\n").unwrap();
        let none = |_: &str| None;
        let c = resolve(None, &none, &[d.join("etc.conf"), d.join("home.conf")]).unwrap();
        assert_eq!((keys(&c).0, keys(&c).2), ("ak-ca", "ck-home"));
    }

    #[test]
    fn what_is_missing_is_named() {
        let d = scratch("missing");
        let none = |_: &str| None;
        let e = resolve(None, &none, &[d.join("ovh.conf")])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no OVH endpoint") && e.contains("OVH_ENDPOINT"),
            "{e}"
        );
        std::fs::write(d.join("ovh.conf"), "[ovh-ca]\napplication_key=a\n").unwrap();
        let e = resolve(Some("ovh-ca"), &none, &[d.join("ovh.conf")])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("application_secret") && e.contains("[ovh-ca]"),
            "{e}"
        );
        let e = resolve(Some("ovh-mars"), &none, &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("ovh-mars") && e.contains("ovh-ca"), "{e}");
    }

    #[test]
    fn a_url_is_an_endpoint() {
        assert_eq!(
            endpoint_url("http://127.0.0.1:9/1.0/").as_deref(),
            Some("http://127.0.0.1:9/1.0")
        );
        assert_eq!(endpoint_url("nope"), None);
    }

    #[test]
    fn the_secrets_are_not_debug_printed() {
        let c = Credentials {
            endpoint: "ovh-ca".into(),
            url: "u".into(),
            auth: Auth::Keys {
                application_key: "ak".into(),
                application_secret: "hunter2".into(),
                consumer_key: "ck".into(),
            },
        };
        assert!(!format!("{c:?}").contains("hunter2"));
        let c = Credentials {
            auth: Auth::OAuth2 {
                client_id: "id".into(),
                client_secret: "hunter3".into(),
                token_url: "t".into(),
            },
            ..c
        };
        assert!(!format!("{c:?}").contains("hunter3"));
    }

    /// A section with a service account's client id and secret is the
    /// OAuth2 form, its token endpoint the endpoint's own.
    #[test]
    fn a_service_account_in_place_of_the_keys() {
        let d = scratch("oauth");
        std::fs::write(
            d.join("ovh.conf"),
            "[ovh-ca]\nclient_id=sa-id\nclient_secret=sa-secret\n",
        )
        .unwrap();
        let none = |_: &str| None;
        let c = resolve(Some("ovh-ca"), &none, &[d.join("ovh.conf")]).unwrap();
        assert_eq!(
            c.auth,
            Auth::OAuth2 {
                client_id: "sa-id".into(),
                client_secret: "sa-secret".into(),
                token_url: "https://ca.ovh.com/auth/oauth2/token".into(),
            }
        );
        assert_eq!(c.create_token_url(), "https://ca.api.ovh.com/createToken/");
        // A URL's token endpoint is at its origin; the environment's
        // client id serves as the file's does.
        let env = |k: &str| match k {
            "OVH_CLIENT_ID" => Some("env-id".to_string()),
            "OVH_CLIENT_SECRET" => Some("env-secret".to_string()),
            _ => None,
        };
        let c = resolve(Some("http://127.0.0.1:9/1.0"), &env, &[]).unwrap();
        assert!(
            matches!(&c.auth, Auth::OAuth2 { client_id, token_url, .. }
                if client_id == "env-id" && token_url == "http://127.0.0.1:9/auth/oauth2/token"),
            "{c:?}"
        );
        // Half a service account names the other half.
        std::fs::write(d.join("ovh.conf"), "[ovh-ca]\nclient_id=sa-id\n").unwrap();
        let e = resolve(Some("ovh-ca"), &none, &[d.join("ovh.conf")])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no OVH client_secret for the endpoint ovh-ca"),
            "{e}"
        );
        // An endpoint without OAuth2 says so.
        let e = resolve(Some("kimsufi-eu"), &env, &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("kimsufi-eu takes no service account"), "{e}");
    }

    /// The files OVH's SDKs read, lowest precedence first: the machine's,
    /// the user's, the working directory's; no other.
    #[test]
    fn the_files_are_the_vendors() {
        assert_eq!(
            files_in(Some("/home/u".into()), Some("/work/infra".into())),
            [
                PathBuf::from("/etc/ovh.conf"),
                PathBuf::from("/home/u/.ovh.conf"),
                PathBuf::from("/work/infra/ovh.conf"),
            ]
        );
    }

    /// An access token minted elsewhere is a form of its own, from the
    /// file or `OVH_ACCESS_TOKEN`, for any endpoint; given with another
    /// form it is the error, each form named.
    #[test]
    fn an_access_token_in_place_of_the_keys() {
        let d = scratch("token");
        std::fs::write(d.join("ovh.conf"), "[kimsufi-eu]\naccess_token=tok\n").unwrap();
        let none = |_: &str| None;
        let c = resolve(Some("kimsufi-eu"), &none, &[d.join("ovh.conf")]).unwrap();
        assert_eq!(
            c.auth,
            Auth::Token {
                access_token: "tok".into()
            }
        );
        assert!(!format!("{c:?}").contains("tok\""), "{c:?}");
        let env = |k: &str| (k == "OVH_ACCESS_TOKEN").then(|| "env-tok".to_string());
        let c = resolve(Some("ovh-ca"), &env, &[]).unwrap();
        assert_eq!(
            c.auth,
            Auth::Token {
                access_token: "env-tok".into()
            }
        );
        std::fs::write(
            d.join("ovh.conf"),
            "[ovh-ca]\nconsumer_key=c\nclient_id=i\nclient_secret=s\n",
        )
        .unwrap();
        let e = resolve(Some("ovh-ca"), &env, &[d.join("ovh.conf")])
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "the credentials for the endpoint ovh-ca give a consumer key's (consumer_key), a \
             service account's (client_id, client_secret) and an access token's \
             (OVH_ACCESS_TOKEN): they are one of them; remove the ones you do not use from \
             [ovh-ca] in ovh.conf or the environment"
        );
    }

    /// Both forms in one section is an error naming each key and where
    /// it was given.
    #[test]
    fn both_forms_is_an_error_naming_the_two() {
        let d = scratch("both");
        std::fs::write(
            d.join("ovh.conf"),
            "[ovh-ca]\napplication_key=a\napplication_secret=s\nconsumer_key=c\nclient_id=i\n",
        )
        .unwrap();
        let env = |k: &str| (k == "OVH_CLIENT_SECRET").then(|| "x".to_string());
        let e = resolve(Some("ovh-ca"), &env, &[d.join("ovh.conf")])
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "the credentials for the endpoint ovh-ca give both a consumer key's \
             (application_key, application_secret, consumer_key) and a service account's \
             (client_id, OVH_CLIENT_SECRET): they are one or the other; remove the ones you do \
             not use from [ovh-ca] in ovh.conf or the environment"
        );
    }
}

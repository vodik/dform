//! The provider's credentials: the operator's, never the program's
//! (R-44 amendment, R-40). An application key and secret and a consumer
//! key sign each call, and the host holds them and signs: dform.toml
//! grants the provider `signed:environment` (`OVH_APPLICATION_KEY`,
//! ..) or `signed:NAME` (a file), the Configure names it, and the
//! provider has only that name, natively and as a component alike. Else,
//! as OVH's SDKs read them, from `OVH_*` in the environment or
//! `ovh.conf`:
//!
//! ```text
//! [default]
//! endpoint=ovh-ca
//!
//! [ovh-ca]
//! client_id=...
//! client_secret=...
//! ```
//!
//! a service account's (R-179) `client_id` and `client_secret`
//! (`OVH_CLIENT_ID`, `OVH_CLIENT_SECRET`): the client mints a bearer token
//! from them (`api`); or `access_token` (`OVH_ACCESS_TOKEN`), a bearer
//! token minted elsewhere, used as it is. One form of the three is given.
//! The program may give a token instead (`use ovh { token = .. }`): dform
//! holds it in the host, which applies it to each call, and the provider
//! has only its name.
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

/// The API endpoints OVH's SDKs know, and `ovh.conf` as they read it:
/// the signer's, one reading for both.
pub use dform_provider_ovh_signer::{ENDPOINTS, endpoint_url, parse_ini};

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
    /// An application's key and secret and a consumer key the host holds
    /// (`signed:NAME`, dform.toml's grant): the host signs each
    /// call, and the keys never reach the provider.
    Signed { name: String },
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
    /// A bearer token the host holds, the program's `token` setting
    /// (`bearer:NAME`): the host applies it to each call, and its value
    /// never reaches the provider.
    Held { name: String },
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Signed { name } => f.debug_struct("Signed").field("name", name).finish(),
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
            Auth::Held { name } => f.debug_struct("Held").field("name", name).finish(),
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

    /// What a refused token the host holds means, and the fix.
    pub fn refused_held(&self, name: &str) -> String {
        format!(
            "the token {name} for {} expired or was revoked: the program's `token` is used as it \
             is; give a new one (docs/providers/ovh.md)",
            self.endpoint
        )
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

/// The configuration files, lowest precedence first: the machine's,
/// the user's, the working directory's. A component reads none
/// (`machine`).
#[cfg(target_family = "wasm")]
pub fn default_files() -> Vec<PathBuf> {
    Vec::new()
}

/// The configuration files, lowest precedence first: the machine's,
/// the user's, the working directory's.
#[cfg(not(target_family = "wasm"))]
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

/// What the host holds for the provider, each by the name it opens it
/// by: the program's token, the signature dform.toml grants it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Held<'a> {
    /// The program's `token` (`bearer:ovh.N`): in place of any other form.
    pub token: Option<&'a str>,
    /// The operator's keys the host signs with (`signed:NAME`), as
    /// the Configure names them.
    pub signature: Option<&'a str>,
}

/// The credentials for `endpoint` (the program's setting, if it gives one)
/// from what the host holds (`held`), `env` (a variable's value) and the
/// files `files`, lowest precedence first. None of them, or a missing
/// key, is an error naming where they were looked for.
pub fn resolve(
    endpoint: Option<&str>,
    held: Held,
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
    let auth = match held.token {
        Some(name) => Auth::Held {
            name: name.to_string(),
        },
        None => sources.auth(&endpoint, &url, held.signature)?,
    };
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
            if let Some(text) = crate::machine::read(f).and_then(|b| String::from_utf8(b).ok()) {
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
        format!("the environment ({var}) and {}", self.files_list())
    }

    /// The configuration files, for a message.
    fn files_list(&self) -> String {
        self.files
            .iter()
            .map(|f| f.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
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

    /// No form of credentials for `endpoint`: how to give one, naming the
    /// signing keys where they were found (the host's now, not read here).
    fn none_given(&self, endpoint: &str) -> String {
        let found = self.set(endpoint, &KEYS);
        let found = match found.as_slice() {
            [] => String::new(),
            keys => format!(
                " ({} found: the host holds the keys and signs, not the provider)",
                keys.join(", ")
            ),
        };
        format!(
            "no OVH credentials for the endpoint {endpoint}{found}: grant the provider the \
             host's signature in dform.toml, [providers.ovh] credentials = \
             [\"signed:environment\"] (OVH_APPLICATION_KEY, OVH_APPLICATION_SECRET and \
             OVH_CONSUMER_KEY, else ~/.ovh.conf) or [\"signed:NAME\"] (the file \
             credentials/signed/NAME, an ovh.conf); or give a service account's client_id and \
             client_secret, or an access_token, under [{endpoint}] in ovh.conf or the \
             environment ({})",
            self.files_list()
        )
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
    /// access token, a service account, or the keys the host signs with,
    /// `signature` (the name dform.toml's grant gives them); more than one
    /// form is an error naming them, none an error saying how to give one.
    fn auth(&self, endpoint: &str, url: &str, signature: Option<&str>) -> Result<Auth> {
        let signed: Vec<String> = signature.map(String::from).into_iter().collect();
        let client = self.set(endpoint, &CLIENT);
        let token = self.set(endpoint, &TOKEN);
        let given: Vec<String> = [
            ("the host's signature", &signed),
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
            let grant = match signature {
                Some(_) => ", or the grant from [providers.ovh] credentials in dform.toml",
                None => "",
            };
            bail!(
                "the credentials for the endpoint {endpoint} give {both}{} and {last}: they are \
                 {which}; remove the ones you do not use from [{endpoint}] in ovh.conf or the \
                 environment{grant}",
                first.join(", ")
            );
        }
        let key = |var: &str, name: &str| self.key(endpoint, var, name);
        Ok(if !token.is_empty() {
            Auth::Token {
                access_token: key("OVH_ACCESS_TOKEN", "access_token")?,
            }
        } else if let Some(name) = signature {
            Auth::Signed {
                name: name.to_string(),
            }
        } else if client.is_empty() {
            bail!("{}", self.none_given(endpoint))
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

/// The three keys' variables and names in ovh.conf: the host's to sign
/// with, never read here (a credential `signed:..`).
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

    /// Nothing held for the provider in the host.
    const NONE: Held = Held {
        token: None,
        signature: None,
    };

    /// The keys the host signs with, as dform.toml grants them.
    const SIGNED: Held = Held {
        token: None,
        signature: Some("signed:lab"),
    };

    /// The service account's id and secret.
    fn account(c: &Credentials) -> (&str, &str) {
        match &c.auth {
            Auth::OAuth2 {
                client_id,
                client_secret,
                ..
            } => (client_id, client_secret),
            a => panic!("not a service account: {a:?}"),
        }
    }

    const CONF: &str = "\
; written by ovh's own tool
[default]
endpoint=ovh-ca

[ovh-ca]
client_id=id-ca
client_secret = cs-ca

[ovh-eu]
client_id=id-eu
client_secret=cs-eu
";

    #[test]
    fn reads_the_ini_sections() {
        let ini = parse_ini(CONF);
        assert_eq!(ini["default"]["endpoint"], "ovh-ca");
        assert_eq!(ini["ovh-ca"]["client_secret"], "cs-ca");
        assert_eq!(ini["ovh-eu"].len(), 2);
    }

    #[test]
    fn the_default_endpoint_and_its_section() {
        let d = scratch("default");
        std::fs::write(d.join("ovh.conf"), CONF).unwrap();
        let none = |_: &str| None;
        let c = resolve(None, NONE, &none, &[d.join("ovh.conf")]).unwrap();
        assert_eq!(c.endpoint, "ovh-ca");
        assert_eq!(c.url, "https://ca.api.ovh.com/1.0");
        assert_eq!(account(&c), ("id-ca", "cs-ca"));
        // The program's endpoint picks the section.
        let c = resolve(Some("ovh-eu"), NONE, &none, &[d.join("ovh.conf")]).unwrap();
        assert_eq!(account(&c).0, "id-eu");
        assert_eq!(c.url, "https://eu.api.ovh.com/1.0");
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let d = scratch("env");
        std::fs::write(d.join("ovh.conf"), CONF).unwrap();
        let env = |k: &str| match k {
            "OVH_ENDPOINT" => Some("ovh-eu".to_string()),
            "OVH_CLIENT_SECRET" => Some("cs-env".to_string()),
            _ => None,
        };
        let c = resolve(None, NONE, &env, &[d.join("ovh.conf")]).unwrap();
        assert_eq!(c.endpoint, "ovh-eu");
        assert_eq!(account(&c), ("id-eu", "cs-env"));
    }

    #[test]
    fn a_later_file_overrides_an_earlier_one_key_by_key() {
        let d = scratch("layers");
        std::fs::write(d.join("etc.conf"), CONF).unwrap();
        std::fs::write(d.join("home.conf"), "[ovh-ca]\nclient_secret=cs-home\n").unwrap();
        let none = |_: &str| None;
        let files = [d.join("etc.conf"), d.join("home.conf")];
        let c = resolve(None, NONE, &none, &files).unwrap();
        assert_eq!(account(&c), ("id-ca", "cs-home"));
    }

    /// The keys are the host's: granted, the provider signs with them by
    /// their name, and never reads the three keys of a file or the
    /// environment.
    #[test]
    fn the_host_signs_with_the_granted_keys() {
        let d = scratch("signed");
        std::fs::write(
            d.join("ovh.conf"),
            "[ovh-ca]\napplication_key=a\napplication_secret=s\nconsumer_key=c\n",
        )
        .unwrap();
        let none = |_: &str| None;
        let c = resolve(Some("ovh-ca"), SIGNED, &none, &[d.join("ovh.conf")]).unwrap();
        assert_eq!(
            c.auth,
            Auth::Signed {
                name: "signed:lab".into()
            }
        );
        // The program's token is used in place of any other form.
        let held = Held {
            token: Some("bearer:ovh.1"),
            ..SIGNED
        };
        let c = resolve(Some("ovh-ca"), held, &none, &[]).unwrap();
        assert_eq!(
            c.auth,
            Auth::Held {
                name: "bearer:ovh.1".into()
            }
        );
    }

    #[test]
    fn what_is_missing_is_named() {
        let d = scratch("missing");
        let none = |_: &str| None;
        let e = resolve(None, NONE, &none, &[d.join("ovh.conf")])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no OVH endpoint") && e.contains("OVH_ENDPOINT"),
            "{e}"
        );
        let e = resolve(Some("ovh-ca"), NONE, &none, &[d.join("ovh.conf")])
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with(
                "no OVH credentials for the endpoint ovh-ca: grant the provider the host's \
                 signature in dform.toml, [providers.ovh] credentials = \
                 [\"signed:environment\"]"
            ) && e.ends_with(&format!("({})", d.join("ovh.conf").display())),
            "{e}"
        );
        // The three keys where the provider used to read them are named:
        // the host's now.
        std::fs::write(d.join("ovh.conf"), "[ovh-ca]\napplication_key=a\n").unwrap();
        let env = |k: &str| (k == "OVH_CONSUMER_KEY").then(|| "c".to_string());
        let e = resolve(Some("ovh-ca"), NONE, &env, &[d.join("ovh.conf")])
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with(
                "no OVH credentials for the endpoint ovh-ca (application_key, OVH_CONSUMER_KEY \
                 found: the host holds the keys and signs, not the provider): grant"
            ),
            "{e}"
        );
        let e = resolve(Some("ovh-mars"), NONE, &none, &[])
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
            auth: Auth::OAuth2 {
                client_id: "id".into(),
                client_secret: "hunter3".into(),
                token_url: "t".into(),
            },
        };
        assert!(!format!("{c:?}").contains("hunter3"));
        let c = Credentials {
            auth: Auth::Token {
                access_token: "hunter4".into(),
            },
            ..c
        };
        assert!(!format!("{c:?}").contains("hunter4"));
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
        let c = resolve(Some("ovh-ca"), NONE, &none, &[d.join("ovh.conf")]).unwrap();
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
        let c = resolve(Some("http://127.0.0.1:9/1.0"), NONE, &env, &[]).unwrap();
        assert!(
            matches!(&c.auth, Auth::OAuth2 { client_id, token_url, .. }
                if client_id == "env-id" && token_url == "http://127.0.0.1:9/auth/oauth2/token"),
            "{c:?}"
        );
        // Half a service account names the other half.
        std::fs::write(d.join("ovh.conf"), "[ovh-ca]\nclient_id=sa-id\n").unwrap();
        let e = resolve(Some("ovh-ca"), NONE, &none, &[d.join("ovh.conf")])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no OVH client_secret for the endpoint ovh-ca"),
            "{e}"
        );
        // An endpoint without OAuth2 says so.
        let e = resolve(Some("kimsufi-eu"), NONE, &env, &[])
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
        let c = resolve(Some("kimsufi-eu"), NONE, &none, &[d.join("ovh.conf")]).unwrap();
        assert_eq!(
            c.auth,
            Auth::Token {
                access_token: "tok".into()
            }
        );
        assert!(!format!("{c:?}").contains("tok\""), "{c:?}");
        let env = |k: &str| (k == "OVH_ACCESS_TOKEN").then(|| "env-tok".to_string());
        let c = resolve(Some("ovh-ca"), NONE, &env, &[]).unwrap();
        assert_eq!(
            c.auth,
            Auth::Token {
                access_token: "env-tok".into()
            }
        );
        std::fs::write(
            d.join("ovh.conf"),
            "[ovh-ca]\nclient_id=i\nclient_secret=s\n",
        )
        .unwrap();
        let e = resolve(Some("ovh-ca"), SIGNED, &env, &[d.join("ovh.conf")])
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "the credentials for the endpoint ovh-ca give the host's signature \
             (signed:lab), a service account's (client_id, client_secret) and an access \
             token's (OVH_ACCESS_TOKEN): they are one of them; remove the ones you do not use \
             from [ovh-ca] in ovh.conf or the environment, or the grant from [providers.ovh] \
             credentials in dform.toml"
        );
    }

    /// Two forms is an error naming each key and where it was given.
    #[test]
    fn both_forms_is_an_error_naming_the_two() {
        let d = scratch("both");
        std::fs::write(d.join("ovh.conf"), "[ovh-ca]\nclient_id=i\n").unwrap();
        let env = |k: &str| (k == "OVH_CLIENT_SECRET").then(|| "x".to_string());
        let e = resolve(Some("ovh-ca"), SIGNED, &env, &[d.join("ovh.conf")])
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "the credentials for the endpoint ovh-ca give both the host's signature \
             (signed:lab) and a service account's (client_id, OVH_CLIENT_SECRET): they \
             are one or the other; remove the ones you do not use from [ovh-ca] in ovh.conf or \
             the environment, or the grant from [providers.ovh] credentials in dform.toml"
        );
    }
}

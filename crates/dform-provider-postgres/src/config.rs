//! The provider's settings: where the server is, who to connect as, and
//! how the connection is protected. From the program's `use postgres {
//! .. }`, the password a secret (revealed into Configure, in memory only):
//!
//! ```text
//! use postgres { url = "postgres://dform_admin@db.example:5432/postgres", password = admin_pw }
//! use postgres { host = "db.example", user = "dform_admin", password = admin_pw }
//! ```
//!
//! A program that names no server (no `url`, no `host`) is configured
//! from the environment as libpq reads it: `PGHOST`, `PGPORT`,
//! `PGDATABASE`, `PGUSER`, `PGPASSWORD`, `PGSSLMODE` (`dform provider
//! check` configures a provider so).
//!
//! `sslmode` is `require` unless written: `disable` only when the program
//! (or `PGSSLMODE`) says so, and Configure then prints a warning;
//! `prefer` and `allow` are refused, because they fall back to the clear
//! without saying.
//!
//! `kubeconfig` reaches a ClusterIP service through the Kubernetes API
//! (`forward`): the host is the service's DNS name,
//! `SERVICE.NAMESPACE.svc[.cluster.local]`, or `service` and `namespace`
//! say it.

use anyhow::{Result, anyhow, bail};
use serde_json::Value as Json;

/// How the connection is protected, as libpq's `sslmode` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    /// In the clear.
    Disable,
    /// TLS, the server's certificate not checked (libpq's `require`).
    Require,
    /// TLS, the certificate checked against the roots, not its name.
    VerifyCa,
    /// TLS, the certificate checked against the roots and the host's name.
    VerifyFull,
}

impl SslMode {
    pub fn parse(s: &str) -> Result<SslMode> {
        Ok(match s {
            "disable" => SslMode::Disable,
            "require" => SslMode::Require,
            "verify-ca" => SslMode::VerifyCa,
            "verify-full" => SslMode::VerifyFull,
            "prefer" | "allow" => bail!(
                "sslmode {s:?} falls back to an unencrypted connection without saying: \
                 write \"require\" (the default), \"verify-full\", or \"disable\" to choose the clear"
            ),
            other => {
                bail!("sslmode {other:?} is not one of disable, require, verify-ca, verify-full")
            }
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        }
    }
}

/// A ClusterIP service reached through the Kubernetes API's port-forward.
#[derive(Clone, PartialEq, Eq)]
pub struct Forward {
    /// The kubeconfig, YAML (a secret: never printed).
    pub kubeconfig: String,
    pub namespace: String,
    pub service: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Settings {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub sslmode: SslMode,
    /// PEM certificates the server's chain is checked against (verify-*),
    /// instead of the machine's roots.
    pub root_cert: Option<String>,
    pub forward: Option<Forward>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("account", &self.account())
            .field("sslmode", &self.sslmode)
            .field(
                "forward",
                &self.forward.as_ref().map(|f| (&f.namespace, &f.service)),
            )
            .finish_non_exhaustive()
    }
}

/// The keys `use postgres { .. }` takes.
const KEYS: [&str; 11] = [
    "url",
    "host",
    "port",
    "database",
    "user",
    "password",
    "sslmode",
    "root_cert",
    "kubeconfig",
    "namespace",
    "service",
];

fn string(settings: &Json, key: &str) -> Result<Option<String>> {
    match settings.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s.clone())),
        Some(_) => bail!("provider_config postgres: {key} is a string"),
    }
}

/// `%XX` decoded.
fn decode(s: &str) -> Result<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| anyhow!("a bad %-escape in {s:?}"))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| anyhow!("{s:?} is not UTF-8 once decoded"))
}

/// What a `postgres://` URL says: host, port, database, user, sslmode.
#[derive(Debug, Default, PartialEq, Eq)]
struct Url {
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    user: Option<String>,
    sslmode: Option<String>,
}

fn parse_url(url: &str) -> Result<Url> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| {
            anyhow!("provider_config postgres: url is postgres://USER@HOST:PORT/DATABASE")
        })?;
    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (rest, None),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let mut u = Url::default();
    let hostport = match authority.rsplit_once('@') {
        Some((userinfo, hp)) => {
            if userinfo.contains(':') {
                bail!(
                    "provider_config postgres: url holds a password: write it as \
                     `password = ..`, a secret, so it is never printed"
                );
            }
            u.user = Some(decode(userinfo)?).filter(|s| !s.is_empty());
            hp
        }
        None => authority,
    };
    let (host, port) = match hostport.strip_prefix('[') {
        // An IPv6 address: `[::1]:5432`.
        Some(v6) => {
            let (h, p) = v6
                .split_once(']')
                .ok_or_else(|| anyhow!("provider_config postgres: url: an unclosed `[`"))?;
            (h.to_string(), p.strip_prefix(':'))
        }
        None => match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (hostport.to_string(), None),
        },
    };
    u.host = Some(decode(&host)?).filter(|s| !s.is_empty());
    if let Some(p) = port.filter(|p| !p.is_empty()) {
        u.port = Some(
            p.parse()
                .map_err(|_| anyhow!("provider_config postgres: url: port {p:?} is not a port"))?,
        );
    }
    u.database = path.map(decode).transpose()?.filter(|s| !s.is_empty());
    for pair in query
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "sslmode" => u.sslmode = Some(decode(v)?),
            "password" => bail!(
                "provider_config postgres: url holds a password: write it as \
                 `password = ..`, a secret, so it is never printed"
            ),
            other => bail!(
                "provider_config postgres: url: the parameter {other:?} is not taken \
                 (sslmode is)"
            ),
        }
    }
    Ok(u)
}

/// A service's DNS name, `SERVICE.NAMESPACE.svc[.cluster.local]`.
fn service_of(host: &str) -> Option<(String, String)> {
    let h = host
        .strip_suffix(".cluster.local")
        .unwrap_or(host)
        .strip_suffix(".svc")?;
    let (service, namespace) = h.split_once('.')?;
    (!service.is_empty() && !namespace.is_empty() && !namespace.contains('.'))
        .then(|| (service.to_string(), namespace.to_string()))
}

impl Settings {
    /// From the provider block's settings, else (none naming a server)
    /// `env`, libpq's variables.
    pub fn parse(settings: &Json, env: &dyn Fn(&str) -> Option<String>) -> Result<Settings> {
        if let Json::Object(m) = settings {
            let unknown: Vec<&str> = m
                .keys()
                .map(String::as_str)
                .filter(|k| !KEYS.contains(k))
                .collect();
            if !unknown.is_empty() {
                bail!(
                    "provider_config postgres: {} not taken (it takes {})",
                    unknown.join(", "),
                    KEYS.join(", ")
                );
            }
        }
        let url = string(settings, "url")?
            .map(|u| parse_url(&u))
            .transpose()?;
        let url = url.unwrap_or_default();
        let written = |key: &str| string(settings, key);
        let from_env = url.host.is_none() && written("host")?.is_none();
        let pick = |key: &str, from_url: Option<String>, var: &str| -> Result<Option<String>> {
            if let Some(v) = written(key)? {
                return Ok(Some(v));
            }
            if from_url.is_some() {
                return Ok(from_url);
            }
            Ok(if from_env { env(var) } else { None })
        };
        let host = pick("host", url.host.clone(), "PGHOST")?.ok_or_else(|| {
            anyhow!(
                "provider_config postgres: no server: write `url` or `host` \
                 (or set PGHOST in the environment)"
            )
        })?;
        let port = match settings.get("port") {
            Some(Json::Number(n)) => Some(
                n.as_u64()
                    .and_then(|p| u16::try_from(p).ok())
                    .ok_or_else(|| anyhow!("provider_config postgres: port {n} is not a port"))?,
            ),
            Some(Json::Null) | None => match url.port {
                Some(p) => Some(p),
                None if from_env => env("PGPORT")
                    .map(|p| p.parse().map_err(|_| anyhow!("PGPORT {p:?} is not a port")))
                    .transpose()?,
                None => None,
            },
            Some(_) => bail!("provider_config postgres: port is an int"),
        };
        let database = pick("database", url.database.clone(), "PGDATABASE")?
            .unwrap_or_else(|| "postgres".to_string());
        let user = pick("user", url.user.clone(), "PGUSER")?.ok_or_else(|| {
            anyhow!(
                "provider_config postgres: no user: write `user` (an admin role the program \
                 does not manage), or the url's USER@"
            )
        })?;
        let password = match written("password")? {
            Some(p) => Some(p),
            None if from_env => env("PGPASSWORD"),
            None => None,
        };
        let sslmode = match pick("sslmode", url.sslmode.clone(), "PGSSLMODE")? {
            Some(s) => SslMode::parse(&s).map_err(|e| anyhow!("provider_config postgres: {e}"))?,
            None => SslMode::Require,
        };
        let root_cert = written("root_cert")?;
        if root_cert.is_some() && !matches!(sslmode, SslMode::VerifyCa | SslMode::VerifyFull) {
            bail!(
                "provider_config postgres: root_cert is only read with sslmode \
                 \"verify-ca\" or \"verify-full\" (sslmode is {:?})",
                sslmode.name()
            );
        }
        let forward = match written("kubeconfig")? {
            None => {
                for k in ["namespace", "service"] {
                    if settings.get(k).is_some() {
                        bail!(
                            "provider_config postgres: {k} names a Kubernetes service, \
                             reached with `kubeconfig`, which is not written"
                        );
                    }
                }
                None
            }
            Some(kubeconfig) => {
                let named = service_of(&host);
                let service = written("service")?.or_else(|| named.as_ref().map(|n| n.0.clone()));
                let namespace =
                    written("namespace")?.or_else(|| named.as_ref().map(|n| n.1.clone()));
                match (service, namespace) {
                    (Some(service), Some(namespace)) => Some(Forward {
                        kubeconfig,
                        namespace,
                        service,
                    }),
                    _ => bail!(
                        "provider_config postgres: with kubeconfig the host is a service's \
                         name, SERVICE.NAMESPACE.svc, or `service` and `namespace` say it \
                         (host is {host:?})"
                    ),
                }
            }
        };
        Ok(Settings {
            host,
            port: port.unwrap_or(5432),
            database,
            user,
            password,
            sslmode,
            root_cert,
            forward,
        })
    }

    /// Who it connects as, where: `dform_admin@db.example:5432/postgres`.
    pub fn account(&self) -> String {
        format!(
            "{}@{}:{}/{}",
            self.user, self.host, self.port, self.database
        )
    }

    /// The line Configure prints for a connection in the clear; none for
    /// one with TLS.
    pub fn warning(&self) -> Option<String> {
        (self.sslmode == SslMode::Disable).then(|| match &self.forward {
            Some(f) => format!(
                "sslmode=disable: the connection to {} is not encrypted by Postgres; the \
                 Kubernetes API's TLS covers it to the node, the kubelet carries it from there \
                 to the pod of service {}/{}",
                self.account(),
                f.namespace,
                f.service
            ),
            None => format!(
                "sslmode=disable: the connection to {} is not encrypted: what the provider \
                 sends (each statement, the password verifiers) crosses the network in the clear",
                self.account()
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn a_url_and_its_settings() {
        let s = Settings::parse(
            &json!({"url": "postgres://dform_admin@db.example:6432/app", "password": "pw"}),
            &none,
        )
        .unwrap();
        assert_eq!(s.account(), "dform_admin@db.example:6432/app");
        assert_eq!(s.password.as_deref(), Some("pw"));
        assert_eq!(s.sslmode, SslMode::Require, "require unless written");
        assert!(s.warning().is_none());
        assert!(!format!("{s:?}").contains("pw"));

        let s = Settings::parse(&json!({"host": "db", "user": "a", "port": 5433}), &none).unwrap();
        assert_eq!(s.account(), "a@db:5433/postgres");

        let s = Settings::parse(
            &json!({"url": "postgresql://a@[::1]/x?sslmode=disable"}),
            &none,
        )
        .unwrap();
        assert_eq!((s.host.as_str(), s.port), ("::1", 5432));
        assert_eq!(s.sslmode, SslMode::Disable);
        assert!(s.warning().unwrap().contains("not encrypted"));
    }

    #[test]
    fn what_is_refused() {
        let e = |v: Json| format!("{:#}", Settings::parse(&v, &none).unwrap_err());
        assert!(e(json!({"url": "postgres://a:pw@db/x"})).contains("holds a password"));
        assert!(e(json!({"host": "db", "user": "a", "sslmode": "prefer"})).contains("falls back"));
        assert!(e(json!({"host": "db"})).contains("no user"));
        assert!(e(json!({"user": "a"})).contains("no server"));
        assert!(e(json!({"host": "db", "user": "a", "colour": 1})).contains("colour not taken"));
        assert!(
            e(json!({"host": "db.example", "user": "a", "kubeconfig": "k"}))
                .contains("SERVICE.NAMESPACE.svc")
        );
    }

    #[test]
    fn libpq_variables_when_the_program_names_no_server() {
        let env = |k: &str| match k {
            "PGHOST" => Some("127.0.0.1".to_string()),
            "PGPORT" => Some("15432".to_string()),
            "PGUSER" => Some("admin".to_string()),
            "PGPASSWORD" => Some("secret".to_string()),
            "PGSSLMODE" => Some("disable".to_string()),
            _ => None,
        };
        let s = Settings::parse(&json!({}), &env).unwrap();
        assert_eq!(s.account(), "admin@127.0.0.1:15432/postgres");
        assert_eq!(s.password.as_deref(), Some("secret"));
        assert_eq!(s.sslmode, SslMode::Disable);
        // A program that names its server takes nothing from them.
        let s = Settings::parse(&json!({"host": "db", "user": "u"}), &env).unwrap();
        assert_eq!((s.password, s.sslmode), (None, SslMode::Require));
    }

    #[test]
    fn a_service_reached_through_the_kubernetes_api() {
        let s = Settings::parse(
            &json!({"host": "synapse-db.apps.svc", "user": "a", "kubeconfig": "yaml"}),
            &none,
        )
        .unwrap();
        let f = s.forward.unwrap();
        assert_eq!(
            (f.service.as_str(), f.namespace.as_str()),
            ("synapse-db", "apps")
        );
        assert_eq!(
            service_of("db.ns.svc.cluster.local"),
            Some(("db".into(), "ns".into()))
        );
        assert_eq!(service_of("db.example.com"), None);
    }
}

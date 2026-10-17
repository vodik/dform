//! A kubeconfig's current context, as kubectl reads it: its cluster's
//! server, CA (`certificate-authority-data`, or the file
//! `certificate-authority` names), `insecure-skip-tls-verify`,
//! `tls-server-name` and `proxy-url`; its user's token (`token`,
//! `tokenFile`), client certificate and key (`-data`, or the files),
//! `username` and `password`, or exec plugin; its namespace. A user with
//! none of these is anonymous; one with an `auth-provider` is refused.
//!
//! Several files (`KUBECONFIG`'s) merge as kubectl merges them: the first
//! to name a cluster, a user or a context wins, and the first current
//! context; each file's relative paths are its own directory's. A
//! program's kubeconfig may be its parts instead ([`Config::of_parts`]),
//! and in a pod it is the service account's ([`Config::in_cluster`]).
//!
//! No error quotes a value: a kubeconfig that does not parse says so and
//! no more.

use crate::yaml::Yaml;
use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use base64::Engine;

/// Bytes a kubeconfig has inline, or the file it names, whose bytes the
/// host reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Data {
    Inline(Vec<u8>),
    File(String),
}

/// A user's exec plugin (`client.authentication.k8s.io`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub api_version: String,
}

/// A kubeconfig's current context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub server: Option<String>,
    pub namespace: Option<String>,
    pub ca: Option<Data>,
    pub insecure: bool,
    pub server_name: Option<String>,
    pub proxy: Option<String>,
    pub token: Option<Data>,
    /// `username` and `password`.
    pub basic: Option<(String, String)>,
    /// The client certificate and its key.
    pub client_cert: Option<(Data, Data)>,
    pub exec: Option<Exec>,
}

/// `data`, base64, decoded; `k` names it in the error.
pub fn base64(k: &str, data: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|_| format!("{k} is not base64"))
}

/// `v`'s `KEY-data` (base64), else the file `KEY` names.
fn data(v: &Yaml, key: &str) -> Result<Option<Data>, String> {
    let inline = format!("{key}-data");
    if let Some(d) = v.str(&inline) {
        return base64(&inline, d).map(|b| Some(Data::Inline(b)));
    }
    Ok(v.str(key).map(|path| Data::File(path.to_string())))
}

/// The directory of the file `path`, as written: `~/.kube` of
/// `~/.kube/config`; empty for a bare name.
pub fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// `path` under `dir` when it is relative and `dir` is named.
fn under(dir: &str, path: &str) -> String {
    match dir.is_empty() || path.starts_with('/') || path.starts_with("~/") {
        true => path.to_string(),
        false => format!("{dir}/{path}"),
    }
}

/// Each path a kubeconfig's clusters and users name (a CA, a client
/// certificate and key, a token file, an exec plugin's command that is a
/// path) made relative to `dir`, the kubeconfig's directory.
pub fn absolute(doc: &mut Yaml, dir: &str) {
    if dir.is_empty() {
        return;
    }
    let fix = |v: &mut Yaml| {
        if let Yaml::Scalar(p, _) = v {
            *p = under(dir, p);
        }
    };
    for (list, entry, keys) in [
        ("clusters", "cluster", &["certificate-authority"][..]),
        (
            "users",
            "user",
            &["client-certificate", "client-key", "tokenFile"],
        ),
    ] {
        let Some(Yaml::Seq(es)) = doc.get_mut(list) else {
            continue;
        };
        for e in es.iter_mut().filter_map(|e| e.get_mut(entry)) {
            for k in keys {
                if let Some(v) = e.get_mut(k) {
                    fix(v);
                }
            }
            // A command naming a path is the kubeconfig's; a bare name is
            // looked up on PATH.
            if let Some(cmd) = e.get_mut("exec").and_then(|x| x.get_mut("command"))
                && cmd.text().is_some_and(|c| c.contains('/'))
            {
                fix(cmd);
            }
        }
    }
}

/// `docs` merged as kubectl merges them: the first to name a cluster, a
/// user or a context wins, and the first current context.
pub fn merged(docs: Vec<Yaml>) -> Result<Yaml, String> {
    let mut out: Vec<(String, Yaml)> = Vec::new();
    for doc in docs {
        let Yaml::Map(pairs) = doc else {
            return Err("it is not a kubeconfig".into());
        };
        for (k, v) in pairs {
            match (out.iter_mut().find(|(have, _)| *have == k), v) {
                (Some((_, Yaml::Seq(have))), Yaml::Seq(more)) => {
                    for e in more {
                        let name = e.str("name").map(String::from);
                        if !have.iter().any(|h| h.str("name") == name.as_deref()) {
                            have.push(e);
                        }
                    }
                }
                (Some((_, have @ Yaml::Scalar(..))), v) if have.text() == Some("") => *have = v,
                (Some(_), _) => {}
                (None, v) => out.push((k, v)),
            }
        }
    }
    Ok(Yaml::Map(out))
}

impl Config {
    /// The current context of the kubeconfig `doc`.
    pub fn of(doc: &Yaml) -> Result<Config, String> {
        let current = doc
            .str("current-context")
            .filter(|c| !c.is_empty())
            .ok_or("the kubeconfig has no current-context")?;
        let ctx = doc
            .named("contexts", current)
            .and_then(|c| c.get("context"))
            .ok_or_else(|| format!("no context {current:?}"))?;
        let cluster = ctx
            .str("cluster")
            .and_then(|n| doc.named("clusters", n))
            .and_then(|c| c.get("cluster"))
            .ok_or("the context names no cluster")?;
        let anonymous = Yaml::Map(Vec::new());
        let user = ctx
            .str("user")
            .and_then(|n| doc.named("users", n))
            .and_then(|u| u.get("user"))
            .unwrap_or(&anonymous);
        if let Some(p) = user.get("auth-provider") {
            return Err(format!(
                "its user's auth-provider ({}) is not supported: use an exec plugin",
                p.str("name").unwrap_or("unnamed")
            ));
        }
        let token = match (user.str("token"), user.str("tokenFile")) {
            (Some(t), _) => Some(Data::Inline(t.as_bytes().to_vec())),
            (None, Some(f)) => Some(Data::File(f.to_string())),
            (None, None) => None,
        };
        let client_cert = data(user, "client-certificate")?.zip(data(user, "client-key")?);
        Ok(Config {
            server: cluster.str("server").map(String::from),
            namespace: ctx.str("namespace").map(String::from),
            ca: data(cluster, "certificate-authority")?,
            insecure: cluster
                .get("insecure-skip-tls-verify")
                .is_some_and(Yaml::is_true),
            server_name: cluster.str("tls-server-name").map(String::from),
            proxy: cluster.str("proxy-url").map(String::from),
            token,
            basic: user
                .str("username")
                .zip(user.str("password"))
                .map(|(u, p)| (u.to_string(), p.to_string())),
            client_cert,
            exec: user.get("exec").map(Exec::of).transpose()?,
        })
    }

    /// One of a program's settings, by name: `host` with `ca`, and a
    /// `token` or a `client_certificate` and `client_key` (PEM, or base64
    /// of it).
    pub fn of_parts(parts: &[(String, Vec<u8>)]) -> Result<Config, String> {
        let part = |k: &str| -> Result<Option<&str>, String> {
            parts
                .iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| core::str::from_utf8(v).map_err(|_| format!("{k} is not text")))
                .transpose()
        };
        // PEM as it is, or base64 of it (as a kubeconfig carries it).
        let pem = |k: &str| -> Result<Option<Data>, String> {
            match part(k)? {
                None => Ok(None),
                Some(v) if v.trim_start().starts_with("-----BEGIN") => {
                    Ok(Some(Data::Inline(v.as_bytes().to_vec())))
                }
                Some(v) => base64(k, v).map(|b| Some(Data::Inline(b))),
            }
        };
        let host = part("host")?.ok_or("its parts name no host")?.trim();
        let client_cert = pem("client_certificate")?.zip(pem("client_key")?);
        let token = part("token")?.map(|t| Data::Inline(t.as_bytes().to_vec()));
        if token.is_none() && client_cert.is_none() {
            return Err("host needs a token, or a client_certificate and a client_key".into());
        }
        Ok(Config {
            server: Some(match host.contains("://") {
                true => host.to_string(),
                false => format!("https://{host}"),
            }),
            ca: pem("ca")?,
            token,
            client_cert,
            ..Config::default()
        })
    }

    /// A pod's service account: the API server its environment names
    /// (`host`, `port`), the account's token, CA and namespace, by file.
    pub fn in_cluster(host: &str, port: &str, account: &str) -> Config {
        let server = match host.contains(':') {
            true => format!("https://[{host}]:{port}"),
            false => format!("https://{host}:{port}"),
        };
        Config {
            server: Some(server),
            ca: Some(Data::File(format!("{account}/ca.crt"))),
            token: Some(Data::File(format!("{account}/token"))),
            ..Config::default()
        }
    }

    /// The files it names, in order: its CA, its token, its client
    /// certificate and key.
    pub fn files(&self) -> Vec<String> {
        let (cert, key) = match &self.client_cert {
            Some((cert, key)) => (Some(cert), Some(key)),
            None => (None, None),
        };
        [self.ca.as_ref(), self.token.as_ref(), cert, key]
            .into_iter()
            .flatten()
            .filter_map(|d| match d {
                Data::File(path) => Some(path.to_owned()),
                Data::Inline(_) => None,
            })
            .collect()
    }

    /// Each file it names ([`Config::files`]) as `got` has it, in order;
    /// a file that is not there is named.
    pub fn read(&mut self, mut got: impl Iterator<Item = Option<Vec<u8>>>) -> Result<(), String> {
        let (cert, key) = match &mut self.client_cert {
            Some((cert, key)) => (Some(cert), Some(key)),
            None => (None, None),
        };
        let named = [
            ("certificate-authority", self.ca.as_mut()),
            ("tokenFile", self.token.as_mut()),
            ("client-certificate", cert),
            ("client-key", key),
        ];
        for (what, d) in named {
            if let Some(d) = d
                && let Data::File(path) = d
            {
                let bytes = got
                    .next()
                    .flatten()
                    .ok_or_else(|| format!("{what} {path} cannot be read"))?;
                *d = Data::Inline(bytes);
            }
        }
        Ok(())
    }
}

impl Exec {
    fn of(e: &Yaml) -> Result<Exec, String> {
        let strings = |k: &str| -> Vec<String> {
            e.get(k)
                .map(Yaml::items)
                .unwrap_or_default()
                .iter()
                .filter_map(|a| a.text().map(String::from))
                .collect()
        };
        Ok(Exec {
            command: e
                .str("command")
                .ok_or("its exec plugin names no command")?
                .to_string(),
            args: strings("args"),
            env: e
                .get("env")
                .map(Yaml::items)
                .unwrap_or_default()
                .iter()
                .filter_map(|v| Some((v.str("name")?.into(), v.str("value")?.into())))
                .collect(),
            api_version: e
                .str("apiVersion")
                .unwrap_or("client.authentication.k8s.io/v1")
                .to_string(),
        })
    }
}

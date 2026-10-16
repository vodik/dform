//! The `kubeconfig` kind: a kubeconfig's current context as the host
//! applies it. Its cluster's server is the credential's endpoint, its
//! CA (`certificate-authority-data`, or the file
//! `certificate-authority` names) is trusted beside the machine's, and
//! `insecure-skip-tls-verify` skips verifying the server,
//! `tls-server-name` is the name TLS verifies in place of the server's
//! host, and `proxy-url` the proxy its requests go through; its user's
//! token (`token`, `tokenFile`), client certificate (`-data`, or the
//! files), `username` and `password`, or what its exec plugin answers
//! (run by the host, as kubectl runs it, again once what it answered
//! expires), authenticate; its namespace is the credential's. A user with
//! none of these is anonymous. A file a kubeconfig names is read relative
//! to the kubeconfig's directory.
//!
//! What a kubeconfig is, given:
//!
//! - its text (the operator's file; a program's `kubeconfig` setting);
//! - its parts, as a provider's settings name them: `host` (the server,
//!   `https://` when it names no scheme), `ca`, and a `token` or a
//!   `client_certificate` and `client_key`, each PEM or base64 of it
//!   ([`Kubeconfig::of_parts`]);
//! - the environment's ([`Kubeconfig::environment`]): `KUBECONFIG`'s files
//!   merged as kubectl merges them (the first to name a cluster, user or
//!   context, or the current context, wins), else `~/.kube/config`, else
//!   in a pod its service account.
//!
//! No error quotes a value: a kubeconfig that does not parse says so and
//! no more.

use super::{Credential, Secret};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A pod's service account (`KUBERNETES_SERVICE_HOST` set).
const SERVICE_ACCOUNT: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// A kubeconfig's current context, its files read.
pub struct Kubeconfig {
    /// What it is, for messages: `the credential kubeconfig:prod`.
    what: String,
    server: Option<String>,
    namespace: Option<String>,
    ca: Option<Vec<u8>>,
    insecure: bool,
    server_name: Option<String>,
    proxy: Option<String>,
    token: Option<Secret>,
    /// `USER:PASSWORD`.
    basic: Option<Secret>,
    client_cert: Option<(Secret, Secret)>,
    exec: Option<Exec>,
}

/// A user's exec plugin (`client.authentication.k8s.io`): run when the
/// credential is opened; it answers an `ExecCredential`.
struct Exec {
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    api_version: String,
}

/// `v`'s field `k` as text.
fn text<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// The entry named `want` of `doc`'s list `list` (`clusters`, `users`,
/// `contexts`).
fn named<'a>(doc: &'a Value, list: &str, want: &str) -> Option<&'a Value> {
    doc.get(list)?
        .as_sequence()?
        .iter()
        .find(|e| text(e, "name") == Some(want))
}

fn base64(what: &str, k: &str, data: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|_| anyhow!("{what}: {k} is not base64"))
}

/// The bytes of `v`'s `KEY-data` (base64), else of the file `KEY` names.
fn data(what: &str, v: &Value, key: &str) -> Result<Option<Vec<u8>>> {
    if let Some(d) = text(v, &format!("{key}-data")) {
        return base64(what, &format!("{key}-data"), d).map(Some);
    }
    match text(v, key) {
        None => Ok(None),
        Some(path) => std::fs::read(path)
            .with_context(|| format!("{what}: {key} {path}"))
            .map(Some),
    }
}

impl Kubeconfig {
    /// The current context of the kubeconfig `text`, its files relative
    /// to `dir`.
    pub fn parse(what: &str, text: &str, dir: Option<&Path>) -> Result<Kubeconfig> {
        let mut doc: Value =
            serde_yaml::from_str(text).map_err(|_| anyhow!("{what} is not a kubeconfig (YAML)"))?;
        if let Some(dir) = dir {
            absolute(&mut doc, dir);
        }
        Kubeconfig::of(what, &doc)
    }

    /// The current context of `doc`.
    fn of(what: &str, doc: &Value) -> Result<Kubeconfig> {
        let current = text(doc, "current-context")
            .filter(|c| !c.is_empty())
            .ok_or_else(|| anyhow!("{what}: the kubeconfig has no current-context"))?;
        let ctx = named(doc, "contexts", current)
            .and_then(|c| c.get("context"))
            .ok_or_else(|| anyhow!("{what}: no context {current:?}"))?;
        let cluster = text(ctx, "cluster")
            .and_then(|n| named(doc, "clusters", n))
            .and_then(|c| c.get("cluster"))
            .ok_or_else(|| anyhow!("{what}: the context names no cluster"))?;
        let none = Value::Null;
        let user = text(ctx, "user")
            .and_then(|n| named(doc, "users", n))
            .and_then(|u| u.get("user"))
            .unwrap_or(&none);
        if let Some(p) = user.get("auth-provider") {
            bail!(
                "{what}: its user's auth-provider ({}) is not supported: use an exec plugin",
                text(p, "name").unwrap_or("unnamed")
            );
        }
        let token = match (text(user, "token"), text(user, "tokenFile")) {
            (Some(t), _) => Some(t.as_bytes().to_vec()),
            (None, Some(f)) => {
                Some(std::fs::read(f).with_context(|| format!("{what}: tokenFile {f}"))?)
            }
            (None, None) => None,
        };
        let basic = text(user, "username")
            .zip(text(user, "password"))
            .map(|(u, p)| Secret::new(format!("{u}:{p}").into_bytes()));
        let client_cert = match (
            data(what, user, "client-certificate")?,
            data(what, user, "client-key")?,
        ) {
            (Some(cert), Some(key)) => Some((Secret::new(cert), Secret::new(key))),
            _ => None,
        };
        Ok(Kubeconfig {
            what: what.to_string(),
            server: text(cluster, "server").map(String::from),
            namespace: text(ctx, "namespace").map(String::from),
            ca: data(what, cluster, "certificate-authority")?,
            insecure: cluster
                .get("insecure-skip-tls-verify")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            server_name: text(cluster, "tls-server-name").map(String::from),
            proxy: text(cluster, "proxy-url").map(String::from),
            token: token.map(Secret::new),
            basic,
            client_cert,
            exec: user.get("exec").map(|e| Exec::of(what, e)).transpose()?,
        })
    }

    /// One from a provider's settings, by name: `kubeconfig` (its text),
    /// else `host` with `ca`, and a `token` or a `client_certificate` and
    /// `client_key` (PEM, or base64 of it).
    pub fn of_parts(name: &str, parts: &BTreeMap<String, Secret>) -> Result<Kubeconfig> {
        let what = "the program's kubeconfig";
        if let Some(text) = parts.get("kubeconfig") {
            return Kubeconfig::parse(what, text.text(name)?, None);
        }
        let part = |k: &str| parts.get(k).map(|s| s.text(name)).transpose();
        // PEM as it is, or base64 of it (as a kubeconfig carries it).
        let pem = |k: &str| -> Result<Option<Vec<u8>>> {
            match part(k)? {
                None => Ok(None),
                Some(v) if v.trim_start().starts_with("-----BEGIN") => {
                    Ok(Some(v.as_bytes().to_vec()))
                }
                Some(v) => base64(what, k, v).map(Some),
            }
        };
        let host = part("host")?
            .ok_or_else(|| anyhow!("{what}: its parts name no host"))?
            .trim();
        let client_cert = match (pem("client_certificate")?, pem("client_key")?) {
            (Some(cert), Some(key)) => Some((Secret::new(cert), Secret::new(key))),
            _ => None,
        };
        let token = part("token")?.map(|t| Secret::new(t.as_bytes().to_vec()));
        if token.is_none() && client_cert.is_none() {
            bail!("host needs a token, or a client_certificate and a client_key");
        }
        Ok(Kubeconfig {
            what: what.to_string(),
            server: Some(match host.contains("://") {
                true => host.to_string(),
                false => format!("https://{host}"),
            }),
            namespace: None,
            ca: pem("ca")?,
            insecure: false,
            server_name: None,
            proxy: None,
            token,
            basic: None,
            client_cert,
            exec: None,
        })
    }

    /// The environment's: `KUBECONFIG`'s files (else `~/.kube/config`)
    /// merged, else in a pod its service account. Its error is one
    /// sentence, as the user would look: `no kubeconfig at
    /// ~/.kube/config and not in a cluster`, `the kubeconfig at P does
    /// not load` (why, under `DFORM_LOG=debug`).
    pub fn environment() -> Result<Kubeconfig> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let paths: Vec<PathBuf> = match std::env::var_os("KUBECONFIG") {
            Some(v) if !v.is_empty() => std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect(),
            _ => home.iter().map(|h| h.join(".kube/config")).collect(),
        };
        let shown = |p: &Path| match home.as_deref().and_then(|h| p.strip_prefix(h).ok()) {
            Some(rest) => format!("~/{}", rest.display()),
            None => p.display().to_string(),
        };
        let found: Vec<&PathBuf> = paths.iter().filter(|p| p.exists()).collect();
        if let Some(first) = found.first() {
            let what = format!("the kubeconfig at {}", shown(first));
            return merged(&found, &what).map_err(|e| {
                tracing::debug!(target: crate::timing::TARGET, "{e:#}");
                anyhow!("{what} does not load")
            });
        }
        if std::env::var_os("KUBERNETES_SERVICE_HOST").is_some() {
            return in_cluster().map_err(|e| {
                tracing::debug!(target: crate::timing::TARGET, "{e:#}");
                anyhow!("the pod's service account does not load")
            });
        }
        match paths.is_empty() {
            true => bail!("no kubeconfig and not in a cluster"),
            false => bail!(
                "no kubeconfig at {} and not in a cluster",
                paths
                    .iter()
                    .map(|p| shown(p))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Its parts on `c`: what its exec plugin answers, run now, until it
    /// expires.
    pub fn apply(self, c: &mut Credential) -> Result<()> {
        c.endpoint = self.server;
        c.namespace = self.namespace;
        c.ca = self.ca;
        c.insecure = self.insecure;
        c.server_name = self.server_name;
        c.proxy = self.proxy;
        let (mut token, mut client_cert) = (self.token, self.client_cert);
        if let Some(exec) = &self.exec {
            let answer = exec.run(&self.what)?;
            token = token.or(answer.token);
            client_cert = client_cert.or(answer.client_cert);
            c.expires = answer.expires;
        }
        if let Some(t) = token {
            c.bearer(t.text(&self.what)?);
        }
        if let Some(pair) = self.basic {
            c.basic(pair.text(&self.what)?);
        }
        c.client_cert = client_cert;
        Ok(())
    }
}

/// `files` merged as kubectl merges them: the first to name a cluster, a
/// user or a context wins, and the first current context; each file's
/// relative paths are its own directory's.
fn merged(files: &[&PathBuf], what: &str) -> Result<Kubeconfig> {
    let mut doc = serde_yaml::Mapping::new();
    for path in files {
        let read = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
        let mut one: Value = serde_yaml::from_str(&read)
            .map_err(|_| anyhow!("{} is not a kubeconfig (YAML)", path.display()))?;
        if let Some(dir) = path.parent() {
            absolute(&mut one, dir);
        }
        let Value::Mapping(one) = one else {
            bail!("{} is not a kubeconfig", path.display());
        };
        for (k, v) in one {
            match (doc.get_mut(&k), v) {
                (Some(Value::Sequence(have)), Value::Sequence(more)) => {
                    for e in more {
                        let name = text(&e, "name").map(String::from);
                        if !have.iter().any(|h| text(h, "name") == name.as_deref()) {
                            have.push(e);
                        }
                    }
                }
                (Some(Value::String(s)), v) if s.is_empty() => {
                    doc.insert(k, v);
                }
                (Some(_), _) => {}
                (None, v) => {
                    doc.insert(k, v);
                }
            }
        }
    }
    Kubeconfig::of(what, &Value::Mapping(doc))
}

/// A pod's service account: the API server the pod's environment names,
/// the account's token, CA and namespace.
fn in_cluster() -> Result<Kubeconfig> {
    let var = |k: &str| std::env::var(k).with_context(|| format!("{k} is not set"));
    let host = var("KUBERNETES_SERVICE_HOST")?;
    let port = var("KUBERNETES_SERVICE_PORT")?;
    let read = |f: &str| {
        let path = Path::new(SERVICE_ACCOUNT).join(f);
        std::fs::read(&path).with_context(|| path.display().to_string())
    };
    let server = match host.contains(':') {
        true => format!("https://[{host}]:{port}"),
        false => format!("https://{host}:{port}"),
    };
    Ok(Kubeconfig {
        what: "the pod's service account".into(),
        server: Some(server),
        namespace: read("namespace")
            .ok()
            .and_then(|n| String::from_utf8(n).ok()),
        ca: Some(read("ca.crt")?),
        insecure: false,
        server_name: None,
        proxy: None,
        token: Some(Secret::new(read("token")?)),
        basic: None,
        client_cert: None,
        exec: None,
    })
}

/// Each path a kubeconfig's clusters and users name (a CA, a client
/// certificate and key, a token file, an exec plugin's command that is a
/// relative path) made relative to `dir`.
fn absolute(doc: &mut Value, dir: &Path) {
    let fix = |v: &mut Value| {
        if let Some(p) = v.as_str()
            && Path::new(p).is_relative()
        {
            *v = Value::String(dir.join(p).display().to_string());
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
        let Some(Value::Sequence(es)) = doc.get_mut(list) else {
            continue;
        };
        for e in es.iter_mut().filter_map(|e| e.get_mut(entry)) {
            for k in keys {
                if let Some(v) = e.get_mut(*k) {
                    fix(v);
                }
            }
            // A command naming a path: the kubeconfig's; a bare name is
            // looked up on PATH.
            if let Some(cmd) = e.get_mut("exec").and_then(|x| x.get_mut("command"))
                && cmd.as_str().is_some_and(|c| c.contains('/'))
            {
                fix(cmd);
            }
        }
    }
}

/// What an exec plugin answers (its `ExecCredential`'s status): a token,
/// or a client certificate and its key, and when they expire.
struct Answer {
    token: Option<Secret>,
    client_cert: Option<(Secret, Secret)>,
    expires: Option<std::time::SystemTime>,
}

impl Exec {
    fn of(what: &str, e: &Value) -> Result<Exec> {
        let strings = |k: &str| -> Vec<String> {
            e.get(k)
                .and_then(Value::as_sequence)
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str().map(String::from))
                .collect()
        };
        Ok(Exec {
            command: text(e, "command")
                .ok_or_else(|| anyhow!("{what}: its exec plugin names no command"))?
                .to_string(),
            args: strings("args"),
            env: e
                .get("env")
                .and_then(Value::as_sequence)
                .into_iter()
                .flatten()
                .filter_map(|v| Some((text(v, "name")?.into(), text(v, "value")?.into())))
                .collect(),
            api_version: text(e, "apiVersion")
                .unwrap_or("client.authentication.k8s.io/v1")
                .to_string(),
        })
    }

    /// Run the plugin, not interactively: what its `ExecCredential`
    /// answers.
    fn run(&self, what: &str) -> Result<Answer> {
        let info = serde_json::json!({
            "apiVersion": self.api_version,
            "kind": "ExecCredential",
            "spec": {"interactive": false},
        });
        let out = std::process::Command::new(&self.command)
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .env("KUBERNETES_EXEC_INFO", info.to_string())
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .output()
            .with_context(|| format!("{what}: run its exec plugin {}", self.command))?;
        if !out.status.success() {
            bail!(
                "{what}: its exec plugin {} failed ({})",
                self.command,
                out.status
            );
        }
        let answer: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| {
            anyhow!(
                "{what}: its exec plugin {} answered no ExecCredential",
                self.command
            )
        })?;
        let status = &answer["status"];
        let s = |k: &str| status.get(k).and_then(serde_json::Value::as_str);
        let token = s("token").map(|t| Secret::new(t.as_bytes().to_vec()));
        let cert = s("clientCertificateData")
            .zip(s("clientKeyData"))
            .map(|(c, k)| {
                (
                    Secret::new(c.as_bytes().to_vec()),
                    Secret::new(k.as_bytes().to_vec()),
                )
            });
        if token.is_none() && cert.is_none() {
            bail!(
                "{what}: its exec plugin {} answered neither a token nor a client certificate",
                self.command
            );
        }
        let expires = match s("expirationTimestamp") {
            None => None,
            Some(t) => {
                let secs = crate::approval::parse_rfc3339(t).ok_or_else(|| {
                    anyhow!(
                        "{what}: its exec plugin {} answered an expirationTimestamp that is not \
                         an RFC 3339 time in UTC",
                        self.command
                    )
                })?;
                Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            }
        };
        Ok(Answer {
            token,
            client_cert: cert,
            expires,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(k: Kubeconfig) -> Credential {
        let mut c = Credential::named("kubeconfig:t");
        k.apply(&mut c).unwrap();
        c
    }

    /// Files a kubeconfig names are its directory's; a user with nothing
    /// is anonymous.
    #[test]
    fn files_are_read_beside_it() {
        let dir = std::env::temp_dir().join(format!("dform-kc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ca.pem"), "CA").unwrap();
        std::fs::write(dir.join("tok"), "t0k\n").unwrap();
        let kc = "current-context: c\ncontexts: [{name: c, context: {cluster: k, user: u}}]\n\
                  clusters: [{name: k, cluster: {server: 'https://k:6443', certificate-authority: ca.pem}}]\n\
                  users: [{name: u, user: {tokenFile: tok}}, {name: v, user: {}}]\n";
        let c = applied(Kubeconfig::parse("t", kc, Some(&dir)).unwrap());
        assert_eq!(c.ca.as_deref(), Some(&b"CA"[..]));
        assert_eq!(c.headers[0].1.expose(), b"Bearer t0k");
        let anon = kc.replace("user: u}", "user: v}");
        let c = applied(Kubeconfig::parse("t", &anon, Some(&dir)).unwrap());
        assert!(c.headers.is_empty() && c.client_cert.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A kubeconfig that is not YAML says so, and never quotes itself.
    #[test]
    fn a_bad_kubeconfig_quotes_nothing() {
        let e = Kubeconfig::parse("the program's kubeconfig", "s3cret: [", None)
            .err()
            .unwrap();
        assert_eq!(
            format!("{e:#}"),
            "the program's kubeconfig is not a kubeconfig (YAML)"
        );
    }

    /// An exec plugin is run by the host and its token applied.
    #[test]
    fn an_exec_plugins_token_is_applied() {
        let kc = r#"current-context: c
contexts: [{name: c, context: {cluster: k, user: u}}]
clusters: [{name: k, cluster: {server: 'https://k'}}]
users: [{name: u, user: {exec: {apiVersion: client.authentication.k8s.io/v1, command: sh,
  args: [-c, 'echo "{\"status\": {\"token\": \"$T\"}}"'], env: [{name: T, value: from-exec}]}}}]
"#;
        let c = applied(Kubeconfig::parse("t", kc, None).unwrap());
        assert_eq!(c.headers[0].1.expose(), b"Bearer from-exec");
    }

    /// A cluster's `tls-server-name` and `proxy-url` are the credential's;
    /// an exec plugin's `expirationTimestamp` is when it expires.
    #[test]
    fn the_server_name_the_proxy_and_an_expiry_are_read() {
        let kc = r#"current-context: c
contexts: [{name: c, context: {cluster: k, user: u}}]
clusters: [{name: k, cluster: {server: 'https://10.0.0.1:6443', tls-server-name: k.example,
  proxy-url: 'http://proxy:3128'}}]
users: [{name: u, user: {exec: {command: sh, args: [-c,
  'echo ''{"status": {"token": "t", "expirationTimestamp": "2030-01-02T03:04:05Z"}}''']}}}]
"#;
        let c = applied(Kubeconfig::parse("t", kc, None).unwrap());
        assert_eq!(c.server_name.as_deref(), Some("k.example"));
        assert_eq!(c.proxy.as_deref(), Some("http://proxy:3128"));
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_893_553_445);
        assert_eq!(c.expires, Some(at));
        assert!(!c.expired_by(at - std::time::Duration::from_secs(1)) && c.expired_by(at));
    }

    /// Parts as a provider's settings name them: a host, a token.
    #[test]
    fn parts_make_one() {
        let parts = BTreeMap::from([
            ("host".to_string(), Secret::new(b"k.example:6443".to_vec())),
            ("token".to_string(), Secret::new(b"t".to_vec())),
        ]);
        let c = applied(Kubeconfig::of_parts("kubeconfig:t", &parts).unwrap());
        assert_eq!(c.endpoint.as_deref(), Some("https://k.example:6443"));
        assert_eq!(c.headers[0].1.expose(), b"Bearer t");
        let host = BTreeMap::from([("host".to_string(), Secret::new(b"k".to_vec()))]);
        assert!(Kubeconfig::of_parts("kubeconfig:t", &host).is_err());
    }
}

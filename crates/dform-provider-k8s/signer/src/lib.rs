//! The Kubernetes provider's signer ([`Kube`], a `dform_signer::Signer`):
//! the operator's kubeconfig made into a `kubeconfig` credential's
//! material, which dform's host applies; it signs no request. The host
//! performs what it needs and holds what it makes, so neither the
//! provider nor this code reaches the world.
//!
//! The kubeconfig is the grant's ([`kubeconfig`]): the program's (`use
//! k8s { kubeconfig }`, or its parts, `host`, `ca`, `token`, ..), the
//! operator's file (`kubeconfig:NAME`), or the environment's
//! (`kubeconfig:environment`: `KUBECONFIG`'s files merged, else
//! `~/.kube/config`, else in a pod its service account). Its current
//! context's files are read (a CA, a token file, a client certificate
//! and key), and its user's exec plugin, if it has one, is run by the
//! host as kubectl runs it; what it answers expires when it says, and
//! the material is made again then. The material is the cluster's server
//! (the credential's endpoint) and the context's namespace, and how the
//! host applies it: the CA, `insecure-skip-tls-verify`,
//! `tls-server-name`, `proxy-url`, and the user's token, Basic pair or
//! client certificate.

// A component imports nothing, not even std's WASI: `no_std` there, its
// runtime dform-signer's; std natively and as the provider's dependency.
#![cfg_attr(feature = "component", no_std)]

extern crate alloc;

mod exec;
pub mod kubeconfig;
mod yaml;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use base64::Engine;
use dform_signer::{Got, Material, Need, Outcome, Recipe, Request, Signer, TlsClient};
use exec::Answer;
use kubeconfig::{Config, Data};
use yaml::Yaml;

#[cfg(feature = "component")]
dform_signer::signer!(crate::Kube);

/// Where kubectl looks with no `KUBECONFIG`.
const DEFAULT: &str = "~/.kube/config";

/// A pod's service account's directory.
const SERVICE_ACCOUNT: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// The variables it asks, under `kubeconfig:environment`: the
/// kubeconfig's files, the pod's API server, and the home a path is
/// shown under.
const VARS: [&str; 4] = [
    "KUBECONFIG",
    "KUBERNETES_SERVICE_HOST",
    "KUBERNETES_SERVICE_PORT",
    "HOME",
];

/// The Kubernetes provider's signer.
#[derive(Debug, Clone, Copy, Default)]
pub struct Kube;

/// What it asks first: the grant's value, and the environment's.
fn first() -> Vec<Need> {
    vec![Need::Given, Need::Env(VARS.map(String::from).to_vec())]
}

impl Signer for Kube {
    fn needs(&self) -> Result<Vec<Need>, String> {
        Ok(first())
    }

    /// The kubeconfig the grant gives, else the environment's; the files
    /// its current context names; its exec plugin's answer; then the
    /// material.
    fn step(&self, got: &[Got]) -> Result<Outcome, String> {
        match material(got) {
            Ok(m) => Ok(Outcome::Material(m)),
            Err(Stop::More(needs)) => Ok(Outcome::More(needs)),
            Err(Stop::Refused(e)) => Err(e),
        }
    }

    fn sign(&self, _: Request, _: &Material, _: u64) -> Result<Request, String> {
        Err("a kubeconfig signs no request: the host applies it".into())
    }

    /// Made again from the start: the kubeconfig read again, its exec
    /// plugin run again.
    fn refresh(&self, _: &Material, _: u64) -> Result<Outcome, String> {
        self.needs().map(Outcome::More)
    }
}

/// Why a step stops short of the material: more to ask, or a refusal.
enum Stop {
    More(Vec<Need>),
    Refused(String),
}

impl From<String> for Stop {
    fn from(e: String) -> Stop {
        Stop::Refused(e)
    }
}

impl From<&str> for Stop {
    fn from(e: &str) -> Stop {
        Stop::Refused(e.to_string())
    }
}

/// What the host got, taken in the order the signer asked.
struct Taken<'a>(&'a [Got]);

impl<'a> Taken<'a> {
    /// The next `n` the host got, once it got them; else what to ask.
    fn take(&mut self, n: usize, ask: impl FnOnce() -> Vec<Need>) -> Result<&'a [Got], Stop> {
        let (now, rest) = self
            .0
            .split_at_checked(n)
            .ok_or_else(|| Stop::More(ask()))?;
        self.0 = rest;
        Ok(now)
    }
}

/// The variables the host gave, by name, the empty as unset.
struct Env<'a>(&'a [(String, String)]);

impl Env<'_> {
    fn get(&self, k: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, v)| n == k && !v.is_empty())
            .map(|(_, v)| v.as_str())
    }

    /// `path` as the operator knows it: under their home, `~/`.
    fn shown(&self, path: &str) -> String {
        match self
            .get("HOME")
            .and_then(|h| path.strip_prefix(h)?.strip_prefix('/'))
        {
            Some(rest) => format!("~/{rest}"),
            None => path.to_string(),
        }
    }
}

/// A file's bytes as the host got them; none if it is not there.
fn file(got: &Got) -> Option<Vec<u8>> {
    match got {
        Got::File(bytes) => bytes.clone(),
        _ => None,
    }
}

/// The material `got` makes, once it is all there.
fn material(got: &[Got]) -> Result<Material, Stop> {
    let mut got = Taken(got);
    let [Got::Given(given), Got::Env(env)] = got.take(2, first)? else {
        return Err("the k8s signer is given the grant's value and the environment first".into());
    };
    let (what, mut config) = match given.is_empty() {
        false => (None, given_config(given)?),
        true => environment_config(&mut got, &Env(env))?,
    };
    let at = |e: String| match &what {
        Some(what) => Stop::Refused(format!("{what}: {e}")),
        None => Stop::Refused(e),
    };
    let files = config.files();
    let read = got.take(files.len(), || files.into_iter().map(Need::File).collect())?;
    config.read(read.iter().map(file)).map_err(at)?;
    let answer = match &config.exec {
        None => Answer::default(),
        Some(exec) => match got.take(1, || vec![Need::Exec(exec.command())])? {
            [Got::Exec(out)] => exec.answer(out).map_err(at)?,
            _ => return Err("the k8s signer is given its exec plugin's output".into()),
        },
    };
    config.material(answer).map_err(at)
}

/// The kubeconfig the grant gives: a program's, or the operator's file
/// (`kubeconfig`), else a program's parts.
fn given_config(given: &[(String, Vec<u8>)]) -> Result<Config, Stop> {
    match given.iter().find(|(name, _)| name == "kubeconfig") {
        Some((_, text)) => {
            let doc = Yaml::parse(text).ok_or("it is not a kubeconfig (YAML)")?;
            Ok(Config::of(&doc)?)
        }
        None => Ok(Config::of_parts(given)?),
    }
}

/// The environment's kubeconfig: `KUBECONFIG`'s files (else
/// `~/.kube/config`) merged, else in a pod its service account; and what
/// it is, for messages, `the kubeconfig at ~/.kube/config`.
fn environment_config(got: &mut Taken, env: &Env) -> Result<(Option<String>, Config), Stop> {
    let paths: Vec<String> = match env.get("KUBECONFIG") {
        Some(v) => v
            .split(':')
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect(),
        None => vec![DEFAULT.to_string()],
    };
    let files = got.take(paths.len(), || {
        paths.iter().cloned().map(Need::File).collect()
    })?;
    let found: Vec<(&String, Vec<u8>)> = paths
        .iter()
        .zip(files)
        .filter_map(|(path, got)| Some((path, file(got)?)))
        .collect();
    let Some((first, _)) = found.first() else {
        return in_cluster(got, env, &paths);
    };
    let what = format!("the kubeconfig at {}", env.shown(first));
    let mut docs = Vec::new();
    for (path, text) in &found {
        let mut doc = Yaml::parse(text)
            .ok_or_else(|| format!("{what}: {} is not a kubeconfig (YAML)", env.shown(path)))?;
        kubeconfig::absolute(&mut doc, kubeconfig::dir_of(path));
        docs.push(doc);
    }
    let config = kubeconfig::merged(docs)
        .and_then(|doc| Config::of(&doc))
        .map_err(|e| format!("{what}: {e}"))?;
    Ok((Some(what), config))
}

/// In a pod (`KUBERNETES_SERVICE_HOST` set), its service account, with
/// its namespace; else no kubeconfig at `paths`.
fn in_cluster(
    got: &mut Taken,
    env: &Env,
    paths: &[String],
) -> Result<(Option<String>, Config), Stop> {
    let (Some(host), Some(port)) = (
        env.get("KUBERNETES_SERVICE_HOST"),
        env.get("KUBERNETES_SERVICE_PORT"),
    ) else {
        let shown: Vec<String> = paths.iter().map(|p| env.shown(p)).collect();
        return Err(match shown.is_empty() {
            true => "no kubeconfig and not in a cluster".into(),
            false => format!("no kubeconfig at {} and not in a cluster", shown.join(", ")),
        }
        .into());
    };
    let namespace = format!("{SERVICE_ACCOUNT}/namespace");
    let read = got.take(1, || vec![Need::File(namespace)])?;
    let mut config = Config::in_cluster(host, port, SERVICE_ACCOUNT);
    config.namespace = read
        .first()
        .and_then(file)
        .and_then(|n| String::from_utf8(n).ok())
        .map(|n| n.trim().to_string());
    Ok((Some("the pod's service account".into()), config))
}

impl Config {
    /// The material: what it is for, and how the host applies it, its
    /// files read and its exec plugin's `answer` beside it (its own token
    /// and certificate first).
    fn material(self, answer: Answer) -> Result<Material, String> {
        let mut recipe = Vec::new();
        if let Some(ca) = self.ca {
            recipe.push(Recipe::Ca(inline(ca)?));
        }
        if self.insecure {
            recipe.push(Recipe::Insecure);
        }
        recipe.extend(self.server_name.map(Recipe::ServerName));
        recipe.extend(self.proxy.map(Recipe::Proxy));
        let token = match self.token {
            Some(t) => Some(String::from_utf8(inline(t)?).map_err(|_| "its token is not text")?),
            None => answer.token,
        };
        recipe.extend(token.map(|t| Recipe::Bearer(t.trim().to_string())));
        if let Some((user, password)) = self.basic {
            let pair =
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
            recipe.push(Recipe::Headers(vec![(
                "Authorization".into(),
                format!("Basic {pair}"),
            )]));
        }
        let client_cert = match self.client_cert {
            Some((cert, key)) => Some((inline(cert)?, inline(key)?)),
            None => answer.client_cert,
        };
        recipe.extend(client_cert.map(|(cert, key)| Recipe::TlsClient(TlsClient { cert, key })));
        Ok(Material {
            recipe,
            expires: answer.expires,
            endpoint: self.server,
            namespace: self.namespace,
            ..Material::default()
        })
    }
}

/// `d`'s bytes, once its file is read.
fn inline(d: Data) -> Result<Vec<u8>, String> {
    match d {
        Data::Inline(bytes) => Ok(bytes),
        Data::File(path) => Err(format!("{path} was not read")),
    }
}

#[cfg(test)]
mod tests;

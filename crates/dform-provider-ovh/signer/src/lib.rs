//! The OVH API's signature (https://help.ovhcloud.com/csm/en-gb-api-getting-started-ovhcloud-api),
//! as the OVH provider's signer ([`Ovh`], a `dform_signer::Signer`): the
//! host performs what it needs and holds what it makes, so neither the
//! provider nor this code reaches the world.
//!
//! Its material is an application's key and secret and a consumer key,
//! and the API's clock against the host's: from the environment
//! (`OVH_APPLICATION_KEY`, `OVH_APPLICATION_SECRET`, `OVH_CONSUMER_KEY`,
//! and `OVH_ENDPOINT`), else from `~/.ovh.conf` as OVH's own clients read
//! it (python-ovh, the ovhcloud CLI, the Terraform provider: `[default]
//! endpoint=ovh-ca`, then a section per endpoint), or the operator's file
//! (`signed:NAME`: an `ovh.conf`, or a section's three lines alone). The
//! endpoint named, its clock is fetched once (`GET ENDPOINT/auth/time`).
//! A request signed at the API's time `T` carries `X-Ovh-Application`,
//! `X-Ovh-Consumer`, `X-Ovh-Timestamp: T` and `X-Ovh-Signature`: `$1$`
//! and the hex SHA-1 of the secret, the consumer key, the method, the
//! whole URL, the body and `T`, joined by `+` ([`signature`]).

// A component imports nothing, not even std's WASI: `no_std` there, its
// runtime dform-signer's; std natively and as the provider's dependency.
#![cfg_attr(feature = "component", no_std)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use dform_signer::{Got, Material, Need, Outcome, Recipe, Request, Signer};
use sha1::{Digest, Sha1};

#[cfg(feature = "component")]
dform_signer::signer!(crate::Ovh);

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

/// An INI file's sections, each key to its value, as OVH's clients read
/// `ovh.conf`. `;` and `#` start a comment line; keys before any section
/// are in the section `""`.
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

/// The keys, by their name in `ovh.conf` and in the environment.
const KEYS: [(&str, &str); 3] = [
    ("application_key", "OVH_APPLICATION_KEY"),
    ("application_secret", "OVH_APPLICATION_SECRET"),
    ("consumer_key", "OVH_CONSUMER_KEY"),
];

/// Where the endpoint is named in the environment.
const ENDPOINT: &str = "OVH_ENDPOINT";

/// Where OVH's clients keep the keys.
const CONF: &str = "~/.ovh.conf";

/// The OVH provider's signer.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ovh;

/// The keys, and the endpoint they are for when one is named.
#[derive(Debug, PartialEq, Eq)]
struct Keys {
    /// By their name in `ovh.conf`.
    values: BTreeMap<String, String>,
    endpoint: Option<String>,
}

impl Keys {
    /// The environment's, when it sets any; a missing one is named.
    fn of_env(vars: &[(String, String)]) -> Result<Option<Keys>, String> {
        let get = |k: &str| {
            vars.iter()
                .find(|(n, v)| n == k && !v.is_empty())
                .map(|(_, v)| v.clone())
        };
        if KEYS.iter().all(|(_, var)| get(var).is_none()) {
            return Ok(None);
        }
        let mut values = BTreeMap::new();
        for (name, var) in KEYS {
            let v = get(var).ok_or_else(|| format!("{var} is not set in the environment"))?;
            values.insert(name.to_string(), v);
        }
        Ok(Some(Keys {
            values,
            endpoint: get(ENDPOINT),
        }))
    }

    /// A file's, as OVH's clients read `ovh.conf`: the section the
    /// environment's endpoint names, else the one `[default] endpoint`
    /// names, else the keys before any section (with an `endpoint` of
    /// their own, when they name one).
    fn of_file(text: &[u8], endpoint: Option<String>) -> Result<Keys, String> {
        let text = core::str::from_utf8(text).map_err(|_| "the file is not text".to_string())?;
        let ini = parse_ini(text);
        let named = |section: &str| ini.get(section).and_then(|s| s.get("endpoint")).cloned();
        let endpoint = endpoint.or_else(|| named("default")).or_else(|| named(""));
        let (section, at) = match &endpoint {
            Some(e) if ini.contains_key(e.as_str()) => (&ini[e.as_str()], format!("[{e}]")),
            _ => match ini.get("") {
                Some(top) => (top, "it".to_string()),
                None => {
                    return Err(match &endpoint {
                        Some(e) => format!("it has no section [{e}]"),
                        None => "it names no endpoint ([default] endpoint=..)".to_string(),
                    });
                }
            },
        };
        let mut values = BTreeMap::new();
        for (name, _) in KEYS {
            let v = section
                .get(name)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("no {name} in {at}"))?;
            values.insert(name.to_string(), v.clone());
        }
        Ok(Keys { values, endpoint })
    }

    fn get(&self, name: &str) -> &str {
        self.values.get(name).map_or("", String::as_str)
    }

    /// The material: the keys and `drift`, the API's clock less the
    /// host's, in seconds, `NAME=VALUE` lines.
    fn material(&self, drift: i64) -> Material {
        let mut text = String::new();
        for (name, value) in &self.values {
            text.push_str(&format!("{name}={value}\n"));
        }
        text.push_str(&format!("drift={drift}\n"));
        Material {
            bytes: text.into_bytes(),
            recipe: vec![Recipe::Signed],
            ..Material::default()
        }
    }

    /// The keys and the drift the material holds.
    fn of_material(m: &Material) -> Result<(Keys, i64), String> {
        let text = core::str::from_utf8(&m.bytes).map_err(|_| "not OVH material".to_string())?;
        let mut values = parse_ini(text).remove("").unwrap_or_default();
        let drift = values
            .remove("drift")
            .and_then(|d| d.parse().ok())
            .ok_or_else(|| "not OVH material".to_string())?;
        let keys = Keys {
            values,
            endpoint: None,
        };
        Ok((keys, drift))
    }
}

impl Keys {
    /// The keys the grant gives (the operator's file, or a program's
    /// setting held for the provider): one `ovh.conf`.
    fn of_given(given: &[(String, Vec<u8>)]) -> Result<Keys, String> {
        match given {
            [(_, text)] => Keys::of_file(text, None),
            more => Err(format!(
                "the grant gives {} values, and the OVH keys are one ovh.conf",
                more.len()
            )),
        }
    }

    /// The environment's keys, else the file's (`~/.ovh.conf`), the
    /// environment's endpoint before the file's.
    fn of_environment(env: &[(String, String)], file: &Option<Vec<u8>>) -> Result<Keys, String> {
        match (Keys::of_env(env)?, file) {
            (Some(keys), _) => Ok(keys),
            (None, Some(text)) => {
                let endpoint = env
                    .iter()
                    .find(|(n, _)| n == ENDPOINT)
                    .map(|(_, v)| v.clone());
                Keys::of_file(text, endpoint).map_err(|e| format!("{CONF}: {e}"))
            }
            (None, None) => Err(format!(
                "no OVH keys: neither {}, {} and {} in the environment nor {CONF}",
                KEYS[0].1, KEYS[1].1, KEYS[2].1
            )),
        }
    }
}

impl Signer for Ovh {
    fn needs(&self) -> Result<Vec<Need>, String> {
        Ok(vec![Need::Given])
    }

    /// The keys the grant gives, else the environment's, else the file's;
    /// then, the endpoint named, its clock; then the material.
    fn step(&self, got: &[Got]) -> Result<Outcome, String> {
        let (keys, rest) = match got {
            [Got::Given(given), rest @ ..] if !given.is_empty() => (Keys::of_given(given)?, rest),
            [Got::Given(_)] => {
                let mut vars: Vec<String> = KEYS.iter().map(|(_, var)| var.to_string()).collect();
                vars.push(ENDPOINT.to_string());
                return Ok(Outcome::More(vec![
                    Need::Env(vars),
                    Need::File(CONF.to_string()),
                ]));
            }
            [Got::Given(_), Got::Env(env), Got::File(file), rest @ ..] => {
                (Keys::of_environment(env, file)?, rest)
            }
            _ => {
                return Err(
                    "the OVH signer is given the grant's value, else the environment and a file"
                        .into(),
                );
            }
        };
        let url = keys.endpoint.as_deref().and_then(endpoint_url);
        match (rest.first(), url) {
            (Some(Got::Fetch(r)), _) => {
                let text = core::str::from_utf8(&r.body).unwrap_or_default().trim();
                match (r.status, text.parse::<i64>()) {
                    (200..=299, Ok(t)) => Ok(Outcome::Material(keys.material(t - r.at as i64))),
                    (status, _) => Err(format!("the API's time: HTTP {status}: {text}")),
                }
            }
            (_, Some(url)) => Ok(Outcome::More(vec![Need::Fetch(Request {
                method: "GET".into(),
                url: format!("{url}/auth/time"),
                ..Request::default()
            })])),
            // No endpoint to ask: the host's clock is the API's.
            (_, None) => Ok(Outcome::Material(keys.material(0))),
        }
    }

    fn sign(&self, mut req: Request, material: &Material, now: u64) -> Result<Request, String> {
        let (keys, drift) = Keys::of_material(material)?;
        let time = now as i64 + drift;
        let method = req.method.to_ascii_uppercase();
        let sig = signature(
            keys.get("application_secret").as_bytes(),
            keys.get("consumer_key").as_bytes(),
            &method,
            &req.url,
            &req.body,
            time,
        );
        req.headers.extend([
            (
                "X-Ovh-Application".into(),
                keys.get("application_key").into(),
            ),
            ("X-Ovh-Consumer".into(), keys.get("consumer_key").into()),
            ("X-Ovh-Timestamp".into(), time.to_string()),
            ("X-Ovh-Signature".into(), sig),
        ]);
        Ok(req)
    }

    /// An OVH consumer key's validity is the API's to say: the material
    /// does not expire.
    fn refresh(&self, material: &Material, _: u64) -> Result<Outcome, String> {
        Ok(Outcome::Material(material.clone()))
    }
}

/// The `X-Ovh-Signature` of one request: `$1$` and the hex SHA-1 of
/// `secret+consumer+METHOD+url+body+time`. `url` is the whole URL, query
/// included; `body` empty for a GET or a DELETE.
pub fn signature(
    secret: &[u8],
    consumer: &[u8],
    method: &str,
    url: &str,
    body: &[u8],
    time: i64,
) -> String {
    let time = time.to_string();
    let signed = [
        secret,
        consumer,
        method.as_bytes(),
        url.as_bytes(),
        body,
        time.as_bytes(),
    ]
    .join(&b'+');
    let hex: String = Sha1::digest(&signed)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("$1${hex}")
}

#[cfg(test)]
mod tests;

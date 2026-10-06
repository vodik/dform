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
//! read from `/etc/ovh.conf`, `~/.ovh.conf` and
//! `$XDG_CONFIG_HOME/ovh/ovh.conf` (`~/.config/ovh/ovh.conf`), a later file
//! overriding an earlier one key by key. The program's `provider ovh {
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

/// What a signed call needs.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// The endpoint as named (`ovh-ca`, or a URL).
    pub endpoint: String,
    /// Its base URL, no trailing slash: `https://ca.api.ovh.com/1.0`.
    pub url: String,
    pub application_key: String,
    pub application_secret: String,
    pub consumer_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("endpoint", &self.endpoint)
            .field("url", &self.url)
            .field("application_key", &self.application_key)
            .finish_non_exhaustive()
    }
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

/// The configuration files, lowest precedence first.
pub fn default_files() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    let mut out = vec![PathBuf::from("/etc/ovh.conf")];
    out.extend(home.map(|h| h.join(".ovh.conf")));
    out.extend(xdg.map(|x| x.join("ovh").join("ovh.conf")));
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
    let env = |k: &str| env(k).filter(|v| !v.is_empty());
    let mut conf: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for f in files {
        if let Ok(text) = std::fs::read_to_string(f) {
            for (s, kv) in parse_ini(&text) {
                conf.entry(s).or_default().extend(kv);
            }
        }
    }
    let looked = |var: &str| {
        let files = files
            .iter()
            .map(|f| f.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("the environment ({var}) and {files}")
    };
    let endpoint = match endpoint.filter(|e| !e.is_empty()) {
        Some(e) => e.to_string(),
        None => match env("OVH_ENDPOINT")
            .or_else(|| conf.get("default").and_then(|d| d.get("endpoint")).cloned())
        {
            Some(e) => e,
            None => bail!(
                "no OVH endpoint: the provider block names none (`provider ovh {{ endpoint = \
                 \"ovh-ca\" }}`), and neither does {}",
                looked("OVH_ENDPOINT")
            ),
        },
    };
    let Some(url) = endpoint_url(&endpoint) else {
        bail!(
            "the OVH endpoint {endpoint:?} is not one of {}, nor a URL",
            ENDPOINTS.map(|(n, _)| n).join(", ")
        );
    };
    let section = conf.get(&endpoint);
    let key = |var: &str, name: &str| -> Result<String> {
        match env(var).or_else(|| section.and_then(|s| s.get(name)).cloned()) {
            Some(v) => Ok(v),
            None => bail!(
                "no OVH {name} for the endpoint {endpoint}: set it under [{endpoint}] in \
                 ovh.conf, or {var}; looked in {}",
                looked(var)
            ),
        }
    };
    Ok(Credentials {
        application_key: key("OVH_APPLICATION_KEY", "application_key")?,
        application_secret: key("OVH_APPLICATION_SECRET", "application_secret")?,
        consumer_key: key("OVH_CONSUMER_KEY", "consumer_key")?,
        endpoint,
        url,
    })
}

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
        assert_eq!(
            (
                c.application_key.as_str(),
                c.application_secret.as_str(),
                c.consumer_key.as_str()
            ),
            ("ak-ca", "as-ca", "ck-ca")
        );
        // The program's endpoint picks the section.
        let c = resolve(Some("ovh-eu"), &none, &[d.join("ovh.conf")]).unwrap();
        assert_eq!(c.application_key, "ak-eu");
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
        assert_eq!(c.application_key, "ak-eu");
        assert_eq!(c.consumer_key, "ck-env");
    }

    #[test]
    fn a_later_file_overrides_an_earlier_one_key_by_key() {
        let d = scratch("layers");
        std::fs::write(d.join("etc.conf"), CONF).unwrap();
        std::fs::write(d.join("home.conf"), "[ovh-ca]\nconsumer_key=ck-home\n").unwrap();
        let none = |_: &str| None;
        let c = resolve(None, &none, &[d.join("etc.conf"), d.join("home.conf")]).unwrap();
        assert_eq!(c.consumer_key, "ck-home");
        assert_eq!(c.application_key, "ak-ca");
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
    fn the_secret_is_not_debug_printed() {
        let c = Credentials {
            endpoint: "ovh-ca".into(),
            url: "u".into(),
            application_key: "ak".into(),
            application_secret: "hunter2".into(),
            consumer_key: "ck".into(),
        };
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}

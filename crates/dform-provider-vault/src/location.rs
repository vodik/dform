//! A `vault://` location: `vault://MOUNT/PATH#KEY`, a KV version 2
//! secret's key; `?version=N` reads that version, which is how dform pins
//! a read to the version the plan saw (`vault://kv/app?version=3#key`).
//! Without `#KEY` the read is the secret's whole data, as JSON. The
//! version may also follow the key, as Vault's own CLI writes it
//! (`#key?version=3`).

/// A location, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub mount: String,
    /// The secret's path under the mount, no leading or trailing slash.
    pub path: String,
    pub key: Option<String>,
    pub version: Option<u64>,
}

fn decoded(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// `?version=3`'s version.
fn version_in(query: &str) -> Result<Option<u64>, String> {
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("version", v)) => {
                return v
                    .parse::<u64>()
                    .map(Some)
                    .map_err(|_| format!("version {v:?} is no number: write `?version=3`"));
            }
            _ => {
                return Err(format!(
                    "Vault reads no query {pair:?}: `?version=N` is the one it takes"
                ));
            }
        }
    }
    Ok(None)
}

impl Location {
    pub fn parse(text: &str) -> Result<Location, String> {
        let rest = text.strip_prefix("vault://").ok_or_else(|| {
            format!("{text} is no Vault location: a Vault location is `vault://MOUNT/PATH#KEY`")
        })?;
        let (rest, fragment) = match rest.split_once('#') {
            Some((r, f)) => (r, Some(f)),
            None => (rest, None),
        };
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let mut version = version_in(query)?;
        let key = match fragment {
            None => None,
            Some(f) => {
                let (k, q) = f.split_once('?').unwrap_or((f, ""));
                if let Some(v) = version_in(q)? {
                    version = Some(v);
                }
                Some(decoded(k)).filter(|k| !k.is_empty())
            }
        };
        let (mount, path) = rest.split_once('/').unwrap_or((rest, ""));
        let path = path.trim_matches('/');
        if mount.is_empty() || path.is_empty() {
            return Err(format!(
                "no secret named: write `vault://MOUNT/PATH#KEY` \
                 (`vault://kv/synapse/signing#key`)"
            ));
        }
        Ok(Location {
            mount: decoded(mount),
            path: path.to_string(),
            key,
            version,
        })
    }

    /// The KV v2 read's API path: `/v1/MOUNT/data/PATH[?version=N]`.
    pub fn api_path(&self) -> String {
        let v = self
            .version
            .map(|v| format!("?version={v}"))
            .unwrap_or_default();
        format!("/v1/{}/data/{}{v}", self.mount, self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_location_names_mount_path_key_and_version() {
        let l = Location::parse("vault://kv/synapse/signing#key").unwrap();
        assert_eq!(
            l,
            Location {
                mount: "kv".into(),
                path: "synapse/signing".into(),
                key: Some("key".into()),
                version: None
            }
        );
        assert_eq!(l.api_path(), "/v1/kv/data/synapse/signing");
        let pinned = Location::parse("vault://kv/synapse/signing?version=3#key").unwrap();
        assert_eq!(pinned.version, Some(3));
        assert_eq!(pinned.api_path(), "/v1/kv/data/synapse/signing?version=3");
        let cli = Location::parse("vault://kv/synapse/signing#key?version=4").unwrap();
        assert_eq!((cli.key.as_deref(), cli.version), (Some("key"), Some(4)));
        assert_eq!(Location::parse("vault://kv/app").unwrap().key, None);
        assert!(Location::parse("vault://kv").is_err());
        assert!(Location::parse("vault://kv/a?ver=1").is_err());
        assert!(Location::parse("vault://kv/a?version=x").is_err());
    }
}

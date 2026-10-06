//! What the API does not answer: an instance's user data is write-only, so
//! the provider keeps the SHA-256 of what it sent, by instance id, in
//! `ovh-user-data.json` in the run's cache directory (Configure's `cache`,
//! `dform.state/cache/`). Never the value. Plan compares the program's
//! user data with it: a different digest is a change, which replaces the
//! instance; no digest (an instance made elsewhere, or a cache that was
//! cleared) is no change.

use serde_json::{Map, Value as Json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const FILE: &str = "ovh-user-data.json";

/// The digest of a user data value as the program gives it (its text, or
/// a secret's marker), hex.
pub fn digest(v: &Json) -> String {
    let text = match v {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    };
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub struct Record {
    path: Option<PathBuf>,
}

impl Record {
    pub fn new(dir: Option<&Path>) -> Record {
        Record {
            path: dir.map(|d| d.join(FILE)),
        }
    }

    fn load(&self) -> Map<String, Json> {
        self.path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Json>(&t).ok())
            .and_then(|j| j.as_object().cloned())
            .unwrap_or_default()
    }

    /// The digest of what the instance `id` was made with, if kept.
    pub fn get(&self, id: &str) -> Option<String> {
        self.load()
            .get(id)
            .and_then(Json::as_str)
            .map(str::to_string)
    }

    /// Keep `digest` for `id` (`None`: forget it). A cache that cannot be
    /// written is not an error: the next plan sees no digest.
    pub fn set(&self, id: &str, digest: Option<String>) {
        let Some(path) = &self.path else {
            return;
        };
        let mut m = self.load();
        match digest {
            Some(d) => m.insert(id.to_string(), Json::String(d)),
            None => m.remove(id),
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, Json::Object(m).to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_a_digest_never_the_value() {
        let d = std::env::temp_dir().join(format!("dform-ovh-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let r = Record::new(Some(&d));
        assert_eq!(r.get("i-1"), None);
        r.set(
            "i-1",
            Some(digest(&Json::String("#cloud-config\nsecret".into()))),
        );
        let text = std::fs::read_to_string(d.join(FILE)).unwrap();
        assert!(!text.contains("secret"), "{text}");
        assert_eq!(
            r.get("i-1"),
            Some(digest(&Json::String("#cloud-config\nsecret".into())))
        );
        r.set("i-1", None);
        assert_eq!(r.get("i-1"), None);
        let _ = std::fs::remove_dir_all(&d);
        // No cache directory: nothing kept, nothing found.
        let none = Record::new(None);
        none.set("i-1", Some("x".into()));
        assert_eq!(none.get("i-1"), None);
    }
}

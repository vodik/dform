//! Who holds a deployment's master (R-163, R-164).
//!
//! A deployment's master is 32 random bytes, [`Key`]: the root of every
//! `random.*` value (HKDF of what it derives for "random master"), the
//! seal of a secret memo, and the key of every digest dform keeps of a
//! secret (a write-only attribute's, a plan file's, the audit log's). It
//! is made once, by the first run that needs one of a deployment that has
//! no state, and never again unless the operator says so: a run whose
//! master is not the one state was applied with refuses before it plans
//! ([`Master::check`]), naming both ids and where this run's came from.
//! `--new-master` accepts the change; then every derived secret changes,
//! and the plan says why.
//!
//! The master id ([`id`]) is public: an HMAC of the `random.*` input key
//! material, so it says nothing of the master, and state records it
//! (`State::master`) with each apply.
//!
//! `RANDOM_MASTER` in the environment is that input key material itself,
//! for tests and the editor. It is taken only where it is what state was
//! applied with (or there is no state): beside a deployment of its own
//! master it is said once, and refused when the ids differ.

use crate::zset::file::Key;
use anyhow::{Context, Result, bail};

/// The master id of the `random.*` input key material `ikm`: an HMAC of
/// it, hex. Public.
pub fn id(ikm: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let mut mac =
        <Hmac<sha2::Sha256>>::new_from_slice(ikm).expect("HMAC takes a key of any length");
    mac.update(b"dform master id");
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A master id as messages print it: its first 12 hex digits.
pub fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// `n` bytes from the system's random source.
pub fn random_bytes<const N: usize>(what: &str) -> Result<[u8; N]> {
    use std::io::Read;
    let mut b = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .with_context(|| format!("read /dev/urandom for {what}"))?;
    Ok(b)
}

/// What a run asks of the master.
#[derive(Debug, Clone, Copy, Default)]
pub struct Want {
    /// Make one when the deployment has none and no state: a run that
    /// writes (an apply, a plan file), or derives (`random.*`).
    pub make: bool,
    /// `--new-master`: accept a master other than the one state was
    /// applied with, made now when there is none.
    pub new_master: bool,
}

/// The master a run has.
#[derive(Clone, Default)]
pub struct Master {
    /// The key, when the run holds it.
    pub key: Option<Key>,
    /// The `random.*` input key material: `RANDOM_MASTER`, else what the
    /// key derives for it.
    pub random: Option<Vec<u8>>,
    /// The master id of `random`.
    pub id: Option<String>,
    /// Where it came from, as messages say it: `RANDOM_MASTER`, `the key
    /// file s3://../state.key`.
    pub source: String,
    /// Made by this run.
    pub made: bool,
    /// `--new-master` was given.
    pub accept: bool,
}

impl std::fmt::Debug for Master {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Master")
            .field("id", &self.id)
            .field("source", &self.source)
            .field("made", &self.made)
            .finish_non_exhaustive()
    }
}

impl Master {
    /// The master of a run that has none (an editor, a test of a
    /// program): `random.*` have no value.
    pub fn none() -> Master {
        Master::default()
    }

    /// The master a run whose key is `key` (from `source`) derives with:
    /// `RANDOM_MASTER` when it is set, else the key's.
    fn of(key: Option<Key>, source: String, env: Option<Vec<u8>>) -> Master {
        let (random, source) = match (env, &key) {
            (Some(e), _) => (Some(e), "RANDOM_MASTER".to_string()),
            (None, Some(k)) => (
                Some(crate::secrets::derived(k, "random master").to_vec()),
                source,
            ),
            (None, None) => (None, source),
        };
        Master {
            id: random.as_deref().map(id),
            key,
            random,
            source,
            ..Master::default()
        }
    }

    /// Refuse a master other than the one state was applied with
    /// (`applied`, its id), unless `--new-master` accepts it.
    pub fn check(&self, deployment: &str, applied: Option<&str>) -> Result<()> {
        let (Some(was), Some(now)) = (applied, self.id.as_deref()) else {
            return Ok(());
        };
        if was == now || self.accept {
            return Ok(());
        }
        let fix = match self.source.as_str() {
            "RANDOM_MASTER" => "unset RANDOM_MASTER",
            _ => "restore the key the deployment was applied with",
        };
        bail!(
            "{deployment}: the master this run derives from ({}, id {}) is not the one its state \
             was applied with (id {}): every random.* value and every secret digest would \
             change. {fix}, or run with --new-master to take this master and change them all",
            self.source,
            short(now),
            short(was),
        )
    }
}

/// The master of the deployment whose objects are `store`'s: `applied`
/// says whether it was ever applied with one. The key file is read; one is
/// made only for a deployment never applied (`want.make`), or when
/// `--new-master` asks for it.
pub fn resolve(
    store: &dyn crate::store::Store,
    deployment: &str,
    applied: &dyn Fn() -> Result<bool>,
    want: Want,
) -> Result<Master> {
    use crate::store::{Cond, KEY};
    let env = std::env::var("RANDOM_MASTER")
        .ok()
        .filter(|m| !m.is_empty())
        .map(String::into_bytes);
    let source = format!("the key file {}", store.locate(KEY));
    let mut made = false;
    let key = match Key::load(store)? {
        Some(k) => Some(k),
        None if applied()? && !want.new_master => bail!(
            "{deployment}: {} is missing, and the deployment was applied with it: every random.* \
             value and every secret digest derives from it. Restore it from a backup of the state \
             (state and key go together), or run with --new-master to make a new one and change \
             every derived secret on purpose",
            store.locate(KEY)
        ),
        None if want.make || want.new_master => {
            let key = Key::from_bytes(random_bytes::<32>("the master")?);
            match store
                .put(KEY, &key.bytes(), &Cond::IfAbsent)
                .with_context(|| format!("write the key {}", store.locate(KEY)))?
            {
                Some(_) => {
                    made = true;
                    Some(key)
                }
                // Made by another run meanwhile: that one is the key.
                None => Some(
                    Key::load(store)?
                        .ok_or_else(|| anyhow::anyhow!("the key {}: gone", store.locate(KEY)))?,
                ),
            }
        }
        None => None,
    };
    if env.is_some() && key.is_some() {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            eprintln!(
                "warning: RANDOM_MASTER is set: random.* derive from it, not from {deployment}'s \
                 own key"
            )
        });
    }
    Ok(Master {
        made,
        accept: want.new_master,
        ..Master::of(key, source, env)
    })
}

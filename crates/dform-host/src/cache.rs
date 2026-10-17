//! The cache the host keeps for a provider (`dform:host/cache`): the
//! provider names what a thing is (its key) and how widely it holds (its
//! [`Scope`]); where it is kept, how much and for how long are this
//! module's, one policy for every provider:
//!
//! - `session` and `run` are memory, the run's: a session's entries are
//!   its own, the run's every session's. At most [`MEMORY_MOST`] bytes a
//!   provider, the oldest put let go first.
//! - `account` and `provider` are files under
//!   `$XDG_CACHE_HOME/dform/providers/<provider>/`: an account's in a
//!   directory named by a digest of the provider and the account
//!   ([`account_of`]), so one account's entries are never read for
//!   another's; the provider's in one named by a digest of its build,
//!   which another build never reads. At most [`DISK_MOST`] bytes a
//!   provider, the least recently used let go first; an entry older than
//!   [`AGE`] is gone.
//! - An entry is at most [`ENTRY_MOST`] bytes: a bigger one is not kept.
//!
//! A get may miss whatever was put (evicted, aged, another build): the
//! provider answers the same either way, only slower. Each get and put is
//! a `cache` span under the call it is made in, its scope and whether it
//! hit its fields; never its key, which may name an account.
//!
//! [`account_of`]: dform_core::plugin::host::account_of

use dform_core::plugin::backend::Session;
use dform_core::plugin::host::{CacheAt, Scope};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// The most bytes a provider keeps in memory, `session` and `run`.
pub const MEMORY_MOST: usize = 64 << 20;
/// The most bytes a provider keeps on disk, `account` and `provider`.
pub const DISK_MOST: u64 = 256 << 20;
/// The most bytes one entry is.
pub const ENTRY_MOST: usize = 32 << 20;
/// How long an entry on disk is kept.
pub const AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// Where a provider's entries on disk are: `$XDG_CACHE_HOME/dform/providers`,
/// else `~/.cache/dform/providers`.
pub fn root() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|c| c.join("dform").join("providers"))
}

/// A provider's cache: its memory and its directory.
pub struct Cache {
    /// The provider's directory, by its name in dform.toml; `None`: no
    /// home to keep files under, memory only.
    dir: Option<PathBuf>,
    /// Its build, as its handshake said it (`provider`'s directory).
    build: Mutex<Option<String>>,
    memory: Mutex<Memory>,
}

/// What is kept in memory, by where and key, with the order it was put.
#[derive(Default)]
struct Memory {
    entries: BTreeMap<(Held, String), (u64, Vec<u8>)>,
    bytes: usize,
    next: u64,
}

/// Whose an entry in memory is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Held {
    Session(Session),
    Run,
}

impl Cache {
    /// The cache of the provider `provider` (its name in dform.toml),
    /// its files under `root`'s directory of that name.
    pub fn new(provider: &str, root: Option<PathBuf>) -> Cache {
        let name: String = provider
            .chars()
            .map(|c| match c.is_ascii_alphanumeric() || "-_.".contains(c) {
                true => c,
                false => '_',
            })
            .collect();
        Cache {
            dir: root.map(|r| r.join(name)),
            build: Mutex::new(None),
            memory: Mutex::new(Memory::default()),
        }
    }

    /// The provider's build is `version` (its handshake's): its
    /// `provider` entries are that build's.
    pub fn built(&self, version: &str) {
        *self.build.lock().unwrap_or_else(|e| e.into_inner()) = Some(version.to_string());
    }

    /// What is kept at `key` where `at` says.
    pub fn get(&self, at: &CacheAt, key: &str) -> Option<Vec<u8>> {
        match held(at) {
            Some(h) => self
                .memory()
                .entries
                .get(&(h, key.to_string()))
                .map(|e| e.1.clone()),
            None => read(&self.file(at, key)?, key),
        }
    }

    /// Keep `value` at `key` where `at` says, in place of what was.
    pub fn put(&self, at: &CacheAt, key: &str, value: Vec<u8>) {
        if value.len() > ENTRY_MOST {
            return;
        }
        match held(at) {
            Some(h) => self.memory().put((h, key.to_string()), value),
            None => {
                if let Some(file) = self.file(at, key) {
                    write(&file, key, &value);
                    if let Some(dir) = &self.dir {
                        evict(dir, DISK_MOST);
                    }
                }
            }
        }
    }

    /// The entries `session` kept while its number was not known yet
    /// (`provisional`, a Configure that opened it), now its own.
    pub fn opened(&self, provisional: Session, session: Session) {
        let mut m = self.memory();
        let moved: Vec<_> = m
            .entries
            .keys()
            .filter(|(h, _)| *h == Held::Session(provisional))
            .cloned()
            .collect();
        for k in moved {
            if let Some(e) = m.entries.remove(&k) {
                m.entries.insert((Held::Session(session), k.1), e);
            }
        }
    }

    fn memory(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The file of `key` where `at` says, on disk.
    fn file(&self, at: &CacheAt, key: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        let under = match at.scope {
            Scope::Account => format!("account-{}", digest(&at.account)),
            Scope::Provider => {
                let build = self
                    .build
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()?;
                format!("build-{}", digest(&build))
            }
            Scope::Session | Scope::Run => return None,
        };
        Some(dir.join(under).join(digest(key)))
    }
}

impl Memory {
    fn put(&mut self, k: (Held, String), value: Vec<u8>) {
        let n = self.next;
        self.next += 1;
        self.bytes += value.len();
        if let Some((_, old)) = self.entries.insert(k, (n, value)) {
            self.bytes -= old.len();
        }
        while self.bytes > MEMORY_MOST {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (n, _))| *n)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((_, v)) = self.entries.remove(&oldest) {
                self.bytes -= v.len();
            }
        }
    }
}

/// Whose `at`'s entry is in memory; `None`: on disk.
fn held(at: &CacheAt) -> Option<Held> {
    match at.scope {
        Scope::Session => Some(Held::Session(at.session)),
        Scope::Run => Some(Held::Run),
        Scope::Account | Scope::Provider => None,
    }
}

/// `s`'s sha256, the first 16 bytes in hex: a name on disk that names
/// nothing of what it is a digest of.
fn digest(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The value in `file`, when it is `key`'s and younger than [`AGE`]; it
/// is used now, for [`evict`]. An aged one is removed.
fn read(file: &Path, key: &str) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(file).ok()?;
    let age = meta
        .modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok());
    if age.is_some_and(|a| a > AGE) {
        let _ = std::fs::remove_file(file);
        return None;
    }
    let bytes = std::fs::read(file).ok()?;
    let (len, rest) = bytes.split_first_chunk::<8>()?;
    let len = usize::try_from(u64::from_be_bytes(*len)).ok()?;
    if rest.get(..len)? != key.as_bytes() {
        return None;
    }
    if let Ok(f) = std::fs::File::options().append(true).open(file) {
        let _ = f.set_modified(SystemTime::now());
    }
    Some(rest[len..].to_vec())
}

/// `value` written as `key`'s to `file`: the key's length, the key, the
/// value; aside and renamed, so a concurrent run never reads half.
fn write(file: &Path, key: &str, value: &[u8]) {
    let Some(dir) = file.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut bytes = Vec::with_capacity(8 + key.len() + value.len());
    bytes.extend_from_slice(&(key.len() as u64).to_be_bytes());
    bytes.extend_from_slice(key.as_bytes());
    bytes.extend_from_slice(value);
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, file).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The provider's files under `dir` kept to `most` bytes: the least
/// recently used removed first.
fn evict(dir: &Path, most: u64) {
    let mut files: Vec<(SystemTime, u64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten().flatten())
        .filter_map(|f| {
            let m = f.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), f.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, len, path) in files {
        if total <= most {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

/// Drop every account's entries the provider `provider` (its name in
/// dform.toml) keeps on disk: `dform update PROVIDER`'s. How many
/// accounts' were dropped.
pub fn forget(provider: &str) -> std::io::Result<usize> {
    let Some(root) = root() else { return Ok(0) };
    forget_in(&Cache::new(provider, Some(root)))
}

fn forget_in(cache: &Cache) -> std::io::Result<usize> {
    let Some(dir) = &cache.dir else { return Ok(0) };
    let mut n = 0;
    for d in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if d.file_name().to_string_lossy().starts_with("account-") {
            std::fs::remove_dir_all(d.path())?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(scope: Scope, session: u64, account: &str) -> CacheAt {
        CacheAt {
            scope,
            session: Session(session),
            account: account.into(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("dform-host-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A session's entries are its own, the run's every session's.
    #[test]
    fn a_sessions_entries_are_its_own_and_the_runs_every_sessions() {
        let c = Cache::new("k8s", None);
        c.put(&at(Scope::Session, 1, ""), "k", b"one".to_vec());
        c.put(&at(Scope::Run, 1, ""), "k", b"run".to_vec());
        assert_eq!(
            c.get(&at(Scope::Session, 1, ""), "k").as_deref(),
            Some(&b"one"[..])
        );
        assert_eq!(c.get(&at(Scope::Session, 2, ""), "k"), None);
        assert_eq!(
            c.get(&at(Scope::Run, 2, "other"), "k").as_deref(),
            Some(&b"run"[..])
        );
        // A Configure that opened session 3 kept its entries as 0's.
        c.put(&at(Scope::Session, 0, ""), "j", b"new".to_vec());
        c.opened(Session(0), Session(3));
        assert_eq!(
            c.get(&at(Scope::Session, 3, ""), "j").as_deref(),
            Some(&b"new"[..])
        );
        assert_eq!(c.get(&at(Scope::Session, 0, ""), "j"), None);
    }

    /// An account's entries are kept from one cache to the next (a run to
    /// the next) and never read for another account; the provider's are
    /// its build's; `forget` drops the accounts' alone.
    #[test]
    fn an_accounts_entries_outlive_the_run_and_no_other_account_reads_them() {
        let root = scratch("accounts");
        let one = Cache::new("k8s", Some(root.clone()));
        one.built("0.1.0+a");
        one.put(
            &at(Scope::Account, 1, "kubeconfig:prod"),
            "doc",
            b"prod".to_vec(),
        );
        one.put(
            &at(Scope::Provider, 1, "kubeconfig:prod"),
            "table",
            b"t".to_vec(),
        );
        let next = Cache::new("k8s", Some(root.clone()));
        next.built("0.1.0+a");
        let get = |c: &Cache, s, a, k| c.get(&at(s, 7, a), k);
        assert_eq!(
            get(&next, Scope::Account, "kubeconfig:prod", "doc").as_deref(),
            Some(&b"prod"[..])
        );
        assert_eq!(get(&next, Scope::Account, "kubeconfig:lab", "doc"), None);
        assert_eq!(
            get(&next, Scope::Provider, "kubeconfig:lab", "table").as_deref(),
            Some(&b"t"[..])
        );
        let other = Cache::new("k8s", Some(root.clone()));
        other.built("0.2.0+b");
        assert_eq!(
            get(&other, Scope::Provider, "", "table"),
            None,
            "another build's"
        );
        assert_eq!(
            get(
                &Cache::new("ovh", Some(root.clone())),
                Scope::Account,
                "kubeconfig:prod",
                "doc"
            ),
            None
        );
        // No file is named by what it holds.
        let names: Vec<String> = walk(&root);
        assert!(
            names
                .iter()
                .all(|n| !n.contains("prod") && !n.contains("doc")),
            "{names:?}"
        );
        assert_eq!(forget_in(&next).unwrap(), 1);
        assert_eq!(get(&next, Scope::Account, "kubeconfig:prod", "doc"), None);
        assert_eq!(
            get(&next, Scope::Provider, "", "table").as_deref(),
            Some(&b"t"[..])
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn walk(d: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            out.push(e.file_name().to_string_lossy().into_owned());
            out.extend(walk(&e.path()));
        }
        out
    }

    /// One policy: memory past its most lets the oldest go, disk past its
    /// most the least recently used, an entry past its most is not kept,
    /// and one past its age is gone.
    #[test]
    fn size_and_age_are_the_hosts() {
        let mut m = Memory::default();
        let big = vec![0u8; MEMORY_MOST / 2];
        m.put((Held::Run, "a".into()), big.clone());
        m.put((Held::Run, "b".into()), big.clone());
        m.put((Held::Run, "c".into()), vec![1]);
        assert!(
            !m.entries.contains_key(&(Held::Run, "a".into())),
            "the oldest let go"
        );
        assert!(m.entries.contains_key(&(Held::Run, "c".into())));

        let root = scratch("policy");
        let c = Cache::new("p", Some(root.clone()));
        let acct = at(Scope::Account, 1, "a");
        c.put(&acct, "huge", vec![0; ENTRY_MOST + 1]);
        assert_eq!(c.get(&acct, "huge"), None, "too big to keep");
        c.put(&acct, "old", vec![1; 10]);
        c.put(&acct, "new", vec![2; 10]);
        let old = c.file(&acct, "old").unwrap();
        let aged = SystemTime::now() - AGE - Duration::from_secs(60);
        std::fs::File::options()
            .append(true)
            .open(&old)
            .unwrap()
            .set_modified(aged)
            .unwrap();
        evict(root.join("p").as_path(), 30);
        assert!(!old.exists(), "the least recently used goes first");
        assert_eq!(c.get(&acct, "new").as_deref(), Some(&[2u8; 10][..]));
        c.put(&acct, "old", vec![1; 10]);
        std::fs::File::options()
            .append(true)
            .open(&old)
            .unwrap()
            .set_modified(aged)
            .unwrap();
        assert_eq!(c.get(&acct, "old"), None, "past its age");
        let _ = std::fs::remove_dir_all(&root);
    }
}

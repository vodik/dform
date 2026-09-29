//! Where a deployment's state lives: its stack's backend as a [`Store`],
//! objects by key with ETags, conditional writes and leases (README
//! "State backends"). `local("DIR")` is [`LocalStore`], the directory it
//! always was: the same files, its lock the holder's pid. `s3(...)` is
//! `dform-s3`'s, the objects of a bucket under a prefix; dform-core has no
//! network stack, and the command line wires the S3 store in.
//!
//! A deployment's objects: [`STATE`], [`KEY`] (the plan key), [`AUDIT`]
//! (and, where a store cannot append in place, its segments under
//! [`AUDIT_SEGMENTS`]), [`LOCK`], [`OUTPUTS`] (what other stacks read),
//! and the controller's [`MEMO`], [`PENDING`] and drop directory
//! [`DROPS`]. [`Location`] says where they are: a directory or a bucket
//! prefix.
//!
//! Leases, in a store that fences ([`Store::fenced`]): the lock is an
//! object holding the holder, an expiry and a fencing counter
//! ([`LeaseRecord`]). Taking it is a write conditional on the version
//! read, so two takers never both win; an expired lease is taken over with
//! the counter one higher. The holder renews it from a thread every
//! `lease_renewal` ([`Guard`]): the apply's completion loop waits on a
//! provider's answer, and one Apply call (a cluster's create) can take
//! longer than a lease. Having taken the lease, the holder writes the
//! state once with its counter in it (`fence`), which moves the state's
//! ETag; every later state write first checks that the lease is still its
//! own and is conditional on the ETag it last wrote. A holder that stalled
//! past its lease and wakes after a takeover is refused either way: by the
//! check, or by the ETag the new holder's first write moved. Expiry is
//! wall-clock time: the clocks of the machines sharing a backend must
//! agree to well within a lease.
//!
//! Provider calls are fenced too, as far as they can be: the executor asks
//! [`Deployment::check_fence`] before it submits each Apply call, so a
//! stale holder makes no call once it can see its lease is gone. What it
//! can still do: the calls it submitted before the lease was lost carry
//! on at the provider (at most `--parallel` of them, none recalled), and
//! a call whose check passed is sent however long the holder stalls
//! between the check and the send. Neither answer can be written down
//! (the state write is fenced); the new holder finds them as uncertain
//! calls and resolves them by their idempotency keys.

use crate::state::State;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// The deployment's state.
pub const STATE: &str = "state.json";
/// The deployment's plan key (`zset::file::Key`).
pub const KEY: &str = "state.key";
/// The deployment's audit log (`audit`).
pub const AUDIT: &str = "state.audit.jsonl";
/// The deployment's lock: the local backend's pid file, else the lease.
pub const LOCK: &str = "state.lock";
/// The audit log's segments, in a store that does not append in place
/// (`audit`): `state.audit/000001.jsonl`, ...
pub const AUDIT_SEGMENTS: &str = "state.audit/";
/// The deployment's published outputs: what other stacks read as
/// `stack_output/3`, apart from the state (`stack::Published`).
pub const OUTPUTS: &str = "outputs.json";
/// The controller's memo (`controller`).
pub const MEMO: &str = "controller.json";
/// The controller's approvals drop directory: one token per object under
/// `approvals/`.
pub const DROPS: &str = "approvals";
/// Where the controller publishes the digest a held approval waits for.
pub const PENDING: &str = "approval-pending.json";

/// Is `key` one of a deployment's own objects (not a keyed deployment's
/// under it, nor anything else a directory holds)? Its own are the
/// top-level objects but the local world file, and those under its drop
/// directory and its audit segments.
pub fn own_key(key: &str) -> bool {
    match key.split_once('/') {
        None => key != crate::state::WORLD,
        Some((first, _)) => first == DROPS || format!("{first}/") == AUDIT_SEGMENTS,
    }
}

/// An object and its version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub bytes: Vec<u8>,
    pub etag: String,
}

/// When a write takes effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cond {
    /// Always.
    Any,
    /// Only over the version with this ETag (`If-Match`).
    IfMatch(String),
    /// Only when there is no object (`If-None-Match: *`).
    IfAbsent,
}

/// A backend's objects, keyed relative to the deployment.
pub trait Store: Send + Sync {
    /// How messages name `key`: a path, or `s3://bucket/prefix/key`.
    fn locate(&self, key: &str) -> String;

    /// The object at `key`, if there is one.
    fn get(&self, key: &str) -> Result<Option<Object>>;

    /// Write `bytes` at `key` when `cond` holds: the new ETag, or `None`
    /// when it did not hold (the object changed, or exists).
    fn put(&self, key: &str, bytes: &[u8], cond: &Cond) -> Result<Option<String>>;

    /// The keys that start with `prefix`, in order.
    fn list(&self, prefix: &str) -> Result<Vec<String>>;

    /// Remove the object at `key` (none is fine).
    fn delete(&self, key: &str) -> Result<()>;

    /// Are state writes fenced by a lease? Not the local backend's: its
    /// lock file is held by a live process, which never goes stale.
    fn fenced(&self) -> bool {
        true
    }

    /// Take the lease at `key` for `holder` for `ttl`; `stack` names the
    /// deployment in messages. Refused ([`Held`]) while another holder's
    /// lease is live.
    fn acquire(&self, key: &str, stack: &str, holder: &str, ttl: Duration) -> Result<Lease> {
        acquire(self, key, stack, holder, ttl)
    }

    /// Extend `lease` to `ttl` from now; an error ([`Lost`]) when it is no
    /// longer this holder's.
    fn renew(&self, lease: &mut Lease, ttl: Duration) -> Result<()> {
        renew(self, lease, ttl)
    }

    /// Give `lease` up (a lease already lost is left alone).
    fn release(&self, lease: &Lease) -> Result<()> {
        release(self, lease)
    }

    /// `dform stack unlock`: break the lease at `key`, whoever holds it.
    /// Returns what was done, for the user.
    fn break_lease(&self, key: &str, stack: &str) -> Result<String> {
        break_lease(self, key, stack)
    }

    /// Append to the object at `key` what `line` makes of its current
    /// content, without losing a concurrent append.
    fn append(&self, key: &str, line: &mut dyn FnMut(&[u8]) -> Vec<u8>) -> Result<()> {
        append(self, key, line)
    }

    /// Does [`Store::append`] add to the object in place? Else it rewrites
    /// it whole, and a log that grows is kept in segments (`audit`).
    fn appends_in_place(&self) -> bool {
        false
    }
}

/// Opens the store of an s3 location: the command line's (dform-core has
/// no network stack).
pub type OpenS3<'a> = &'a dyn Fn(&S3Spec) -> Result<Arc<dyn Store>>;

/// Where one deployment's objects are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A directory: the state is `DIR/state.json`.
    Local(PathBuf),
    /// A bucket prefix, the deployment's own (a keyed deployment's
    /// segment included).
    S3(S3Spec),
}

impl Location {
    /// The location of the keyed deployment `seg` under this one (the
    /// stack's), or this one itself.
    pub fn child(&self, seg: Option<&str>) -> Location {
        let Some(seg) = seg else {
            return self.clone();
        };
        match self {
            Location::Local(d) => Location::Local(d.join(seg)),
            Location::S3(spec) => Location::S3(S3Spec {
                prefix: match spec.prefix.as_str() {
                    "" => seg.to_string(),
                    p => format!("{p}/{seg}"),
                },
                ..spec.clone()
            }),
        }
    }

    /// The store of its objects.
    pub fn open(&self, s3: OpenS3) -> Result<Arc<dyn Store>> {
        match self {
            Location::Local(d) => Ok(Arc::new(LocalStore::beside(&d.join(STATE)))),
            Location::S3(spec) => s3(spec),
        }
    }
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Location::Local(d) => write!(f, "{}", d.display()),
            Location::S3(spec) => write!(f, "{spec}"),
        }
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A lease object's content. Released (and broken) leases keep their
/// counter, so it only ever grows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    /// Who holds it; empty once released.
    #[serde(default)]
    pub holder: String,
    /// When it expires, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub expires_ms: u64,
    /// The fencing counter: one more at every taking.
    #[serde(default)]
    pub fence: u64,
}

impl LeaseRecord {
    fn live(&self, now: u64) -> bool {
        !self.holder.is_empty() && self.expires_ms > now
    }

    fn bytes(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(self).expect("a lease record serializes")
    }

    fn parse(bytes: &[u8], at: &str) -> Result<LeaseRecord> {
        serde_json::from_slice(bytes).with_context(|| format!("parse the lease {at}"))
    }
}

/// A lease this process holds.
#[derive(Debug, Clone)]
pub struct Lease {
    pub key: String,
    pub holder: String,
    pub fence: u64,
    pub expires_ms: u64,
    /// The lease object's ETag as this holder last wrote it.
    pub etag: String,
    /// The expired lease this one took over.
    pub took_over: Option<LeaseRecord>,
}

/// The lease is another holder's, and live.
#[derive(Debug)]
pub struct Held {
    pub stack: String,
    pub at: String,
    pub record: LeaseRecord,
}

impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stack {} is locked by another apply ({}, fence {}, its lease expires {}): {}; \
             wait for it, or `dform stack unlock {}` if no apply is running",
            self.stack,
            self.record.holder,
            self.record.fence,
            time(self.record.expires_ms),
            self.at,
            self.stack
        )
    }
}

impl std::error::Error for Held {}

/// The lease is no longer this holder's: it expired and was taken over, or
/// was broken.
#[derive(Debug)]
pub struct Lost {
    pub at: String,
    pub fence: u64,
    pub now: Option<LeaseRecord>,
}

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the lease {} (fence {}) was lost: ", self.at, self.fence)?;
        match &self.now {
            Some(r) if !r.holder.is_empty() => write!(
                f,
                "{} holds it now (fence {}); this run's writes are fenced off",
                r.holder, r.fence
            ),
            _ => write!(f, "it was broken (`dform stack unlock`)"),
        }
    }
}

impl std::error::Error for Lost {}

/// Milliseconds since the epoch as RFC 3339.
fn time(ms: u64) -> String {
    crate::approval::rfc3339(ms / 1000)
}

fn acquire<S: Store + ?Sized>(
    s: &S,
    key: &str,
    stack: &str,
    holder: &str,
    ttl: Duration,
) -> Result<Lease> {
    for _ in 0..5 {
        let now = now_ms();
        let (prev, cond) = match s.get(key)? {
            None => (None, Cond::IfAbsent),
            Some(o) => {
                let r = LeaseRecord::parse(&o.bytes, &s.locate(key))?;
                if r.live(now) {
                    return Err(Held {
                        stack: stack.to_string(),
                        at: s.locate(key),
                        record: r,
                    }
                    .into());
                }
                (Some(r), Cond::IfMatch(o.etag))
            }
        };
        let rec = LeaseRecord {
            holder: holder.to_string(),
            expires_ms: now + ttl.as_millis() as u64,
            fence: prev.as_ref().map_or(0, |r| r.fence) + 1,
        };
        // Lost the race to another taker: the next look says who.
        if let Some(etag) = s.put(key, &rec.bytes(), &cond)? {
            return Ok(Lease {
                key: key.to_string(),
                holder: rec.holder,
                fence: rec.fence,
                expires_ms: rec.expires_ms,
                etag,
                took_over: prev.filter(|r| !r.holder.is_empty()),
            });
        }
    }
    bail!(
        "stack {stack}: could not take the lease {}: it changed under every attempt",
        s.locate(key)
    )
}

fn renew<S: Store + ?Sized>(s: &S, lease: &mut Lease, ttl: Duration) -> Result<()> {
    let rec = LeaseRecord {
        holder: lease.holder.clone(),
        expires_ms: now_ms() + ttl.as_millis() as u64,
        fence: lease.fence,
    };
    match s.put(&lease.key, &rec.bytes(), &Cond::IfMatch(lease.etag.clone()))? {
        Some(etag) => {
            lease.etag = etag;
            lease.expires_ms = rec.expires_ms;
            Ok(())
        }
        None => {
            let now = s
                .get(&lease.key)?
                .and_then(|o| LeaseRecord::parse(&o.bytes, &s.locate(&lease.key)).ok());
            Err(Lost {
                at: s.locate(&lease.key),
                fence: lease.fence,
                now,
            }
            .into())
        }
    }
}

fn release<S: Store + ?Sized>(s: &S, lease: &Lease) -> Result<()> {
    let rec = LeaseRecord {
        holder: String::new(),
        expires_ms: 0,
        fence: lease.fence,
    };
    s.put(&lease.key, &rec.bytes(), &Cond::IfMatch(lease.etag.clone()))?;
    Ok(())
}

fn break_lease<S: Store + ?Sized>(s: &S, key: &str, stack: &str) -> Result<String> {
    let at = s.locate(key);
    let Some(o) = s.get(key)? else {
        return Ok(format!("stack {stack} is not locked"));
    };
    let r = LeaseRecord::parse(&o.bytes, &at)?;
    if r.holder.is_empty() {
        return Ok(format!("stack {stack} is not locked"));
    }
    let broken = LeaseRecord {
        holder: String::new(),
        expires_ms: 0,
        fence: r.fence,
    };
    if s.put(key, &broken.bytes(), &Cond::IfMatch(o.etag))?
        .is_none()
    {
        bail!("stack {stack}: the lease {at} changed while it was being broken; run unlock again");
    }
    let state = if r.live(now_ms()) {
        format!(
            "it was live until {}; if that apply still runs, its next state write is refused",
            time(r.expires_ms)
        )
    } else {
        format!("it expired {}", time(r.expires_ms))
    };
    Ok(format!(
        "stack {stack} unlocked: the lease of {} (fence {}) is broken ({state}): {at}",
        r.holder, r.fence
    ))
}

fn append<S: Store + ?Sized>(
    s: &S,
    key: &str,
    line: &mut dyn FnMut(&[u8]) -> Vec<u8>,
) -> Result<()> {
    for _ in 0..20 {
        let (mut bytes, cond) = match s.get(key)? {
            None => (Vec::new(), Cond::IfAbsent),
            Some(o) => (o.bytes, Cond::IfMatch(o.etag)),
        };
        let add = line(&bytes);
        bytes.extend(add);
        if s.put(key, &bytes, &cond)?.is_some() {
            return Ok(());
        }
    }
    bail!(
        "append to {}: it changed under every attempt",
        s.locate(key)
    )
}

/// The ETag of `bytes` in the stores that make their own: a digest of the
/// content, so the same bytes are the same version, as S3's MD5 ETag is.
fn content_etag(bytes: &[u8]) -> String {
    format!("\"{}\"", &crate::approval::sha256_hex(bytes)[..32])
}

/// The local backend: a deployment's directory. A key's file is the key
/// under it, prefixed as the state file is: `state.json`, or beside a
/// `--world` file `<stem>.state.json` (and `<stem>.state.lock`, ...).
pub struct LocalStore {
    dir: PathBuf,
    prefix: String,
}

impl LocalStore {
    /// The store whose state file is `state`.
    pub fn beside(state: &Path) -> LocalStore {
        let dir = state.parent().unwrap_or(Path::new("")).to_path_buf();
        let name = state
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let prefix = name.strip_suffix(STATE).unwrap_or_default().to_string();
        LocalStore { dir, prefix }
    }

    /// The file of `key`.
    pub fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}{key}", self.prefix))
    }

    fn mkdir(path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        }
        Ok(())
    }
}

impl Store for LocalStore {
    fn locate(&self, key: &str) -> String {
        self.path(key).display().to_string()
    }

    fn get(&self, key: &str) -> Result<Option<Object>> {
        let path = self.path(key);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(Object {
                etag: content_etag(&bytes),
                bytes,
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// `IfMatch` is checked, then written: not atomic, but the local
    /// backend's writers of one object are one at a time (its lock file,
    /// or the audit log's `flock`). `IfAbsent` makes the file readable by
    /// its owner only: it is the plan key.
    fn put(&self, key: &str, bytes: &[u8], cond: &Cond) -> Result<Option<String>> {
        let path = self.path(key);
        LocalStore::mkdir(&path)?;
        match cond {
            Cond::Any => {}
            Cond::IfMatch(etag) => {
                if self.get(key)?.map(|o| o.etag).as_ref() != Some(etag) {
                    return Ok(None);
                }
            }
            Cond::IfAbsent => {
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
                let mut f = match opts.open(&path) {
                    Ok(f) => f,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
                    Err(e) => return Err(e).with_context(|| format!("write {}", path.display())),
                };
                use std::io::Write;
                f.write_all(bytes)
                    .with_context(|| format!("write {}", path.display()))?;
                return Ok(Some(content_etag(bytes)));
            }
        }
        std::fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))?;
        Ok(Some(content_etag(bytes)))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) -> Result<()> {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
            };
            for e in entries {
                let e = e?;
                let name = format!("{rel}{}", e.file_name().to_string_lossy());
                if e.file_type()?.is_dir() {
                    walk(&e.path(), &format!("{name}/"), out)?;
                } else {
                    out.push(name);
                }
            }
            Ok(())
        }
        let mut all = Vec::new();
        walk(&self.dir, "", &mut all)?;
        let mut out: Vec<String> = all
            .into_iter()
            .filter_map(|k| k.strip_prefix(&self.prefix).map(String::from))
            .filter(|k| k.starts_with(prefix))
            .collect();
        out.sort();
        Ok(out)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let path = self.path(key);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("remove {}", path.display()))
            }
            _ => Ok(()),
        }
    }

    fn fenced(&self) -> bool {
        false
    }

    /// The file `key`, created exclusively, holding the holder's pid. A
    /// lock whose holder is gone (a killed apply) is taken over, with a
    /// note.
    fn acquire(&self, key: &str, stack: &str, _holder: &str, _ttl: Duration) -> Result<Lease> {
        let path = self.path(key);
        LocalStore::mkdir(&path)?;
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    use std::io::Write;
                    writeln!(f, "{}", std::process::id())
                        .with_context(|| format!("write {}", path.display()))?;
                    return Ok(Lease {
                        key: key.to_string(),
                        holder: std::process::id().to_string(),
                        fence: 0,
                        expires_ms: u64::MAX,
                        etag: String::new(),
                        took_over: None,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = std::fs::read_to_string(&path).unwrap_or_default();
                    let pid: Option<u32> = holder.trim().parse().ok();
                    if let Some(pid) = pid
                        && !alive(pid)
                    {
                        eprintln!(
                            "note: stack {stack}: taking over the lock of pid {pid}, which is gone ({})",
                            path.display()
                        );
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    bail!(
                        "stack {stack} is locked by another apply (pid {}): {}; \
                         wait for it, or `dform stack unlock {stack}` if no apply is running",
                        pid.map(|p| p.to_string())
                            .unwrap_or_else(|| "unknown".into()),
                        path.display()
                    );
                }
                Err(e) => return Err(e).with_context(|| format!("lock {}", path.display())),
            }
        }
        bail!("stack {stack}: could not take the lock {}", path.display())
    }

    fn renew(&self, _lease: &mut Lease, _ttl: Duration) -> Result<()> {
        Ok(())
    }

    fn release(&self, lease: &Lease) -> Result<()> {
        let _ = std::fs::remove_file(self.path(&lease.key));
        Ok(())
    }

    /// Remove the lock file, unless its holder is running.
    fn break_lease(&self, key: &str, stack: &str) -> Result<String> {
        let path = self.path(key);
        if !path.exists() {
            return Ok(format!("stack {stack} is not locked"));
        }
        let holder = std::fs::read_to_string(&path).unwrap_or_default();
        let pid: Option<u32> = holder.trim().parse().ok();
        if let Some(pid) = pid
            && alive(pid)
        {
            bail!(
                "stack {stack} is locked by a running apply (pid {pid}): {}; stop it first",
                path.display()
            );
        }
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        Ok(format!(
            "stack {stack} unlocked (the lock of pid {} is gone): {}",
            pid.map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".into()),
            path.display()
        ))
    }

    fn appends_in_place(&self) -> bool {
        true
    }

    /// The file is locked (`flock`) while it is read and written, so two
    /// processes appending (a `plan --out` beside an apply) do not fork an
    /// audit log's chain.
    fn append(&self, key: &str, line: &mut dyn FnMut(&[u8]) -> Vec<u8>) -> Result<()> {
        use std::io::{Read, Seek, Write};
        let path = self.path(key);
        LocalStore::mkdir(&path)?;
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        f.lock()
            .with_context(|| format!("lock {}", path.display()))?;
        let mut text = Vec::new();
        f.seek(std::io::SeekFrom::Start(0))?;
        f.read_to_end(&mut text)
            .with_context(|| format!("read {}", path.display()))?;
        f.write_all(&line(&text))
            .with_context(|| format!("write {}", path.display()))?;
        f.unlock()?;
        Ok(())
    }
}

/// Is process `pid` running? (Linux: `/proc/<pid>` exists.) Elsewhere
/// every holder is taken as alive.
fn alive(pid: u32) -> bool {
    if !Path::new("/proc").exists() {
        return true;
    }
    Path::new(&format!("/proc/{pid}")).exists()
}

/// A store in memory that keeps S3's rules: content ETags, conditional
/// writes. For tests (and `dform-s3`'s fake server).
#[derive(Default)]
pub struct MemoryStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryStore {
    pub fn new() -> MemoryStore {
        MemoryStore::default()
    }
}

impl Store for MemoryStore {
    fn locate(&self, key: &str) -> String {
        format!("memory:{key}")
    }

    fn get(&self, key: &str) -> Result<Option<Object>> {
        let objects = self.objects.lock().expect("memory store");
        Ok(objects.get(key).map(|b| Object {
            bytes: b.clone(),
            etag: content_etag(b),
        }))
    }

    fn put(&self, key: &str, bytes: &[u8], cond: &Cond) -> Result<Option<String>> {
        let mut objects = self.objects.lock().expect("memory store");
        let now = objects.get(key).map(|b| content_etag(b));
        let holds = match cond {
            Cond::Any => true,
            Cond::IfMatch(e) => now.as_ref() == Some(e),
            Cond::IfAbsent => now.is_none(),
        };
        if !holds {
            return Ok(None);
        }
        objects.insert(key.to_string(), bytes.to_vec());
        Ok(Some(content_etag(bytes)))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let objects = self.objects.lock().expect("memory store");
        Ok(objects
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.objects.lock().expect("memory store").remove(key);
        Ok(())
    }
}

/// `s3("BUCKET", "PREFIX", {endpoint: URL, region: R})`: a stack's objects
/// in BUCKET under PREFIX (a deployment of a keyed stack's under
/// `PREFIX/<k>=<v>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Spec {
    pub bucket: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
}

impl std::fmt::Display for S3Spec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "s3://{}/{}", self.bucket, self.prefix)
    }
}

/// How long a lease lasts, and how often its holder renews it (the
/// manifest's `[defaults] lease_duration`, `lease_renewal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseTimes {
    pub duration: Duration,
    pub renewal: Duration,
}

impl Default for LeaseTimes {
    fn default() -> LeaseTimes {
        LeaseTimes {
            duration: Duration::from_secs(60),
            renewal: Duration::from_secs(20),
        }
    }
}

/// `500ms`, `30s`, `2m`, or a count of seconds.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (n, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = n.parse().ok()?;
    match unit.trim() {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n * 60)),
        _ => None,
    }
}

/// One deployment's state in its store: what reads and writes it, and its
/// lock.
#[derive(Clone)]
pub struct Deployment {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn Store>,
    name: String,
    times: LeaseTimes,
    /// The state's ETag as this run last read or wrote it: `None` before
    /// it is read, `Some(None)` when there was none.
    etag: Mutex<Option<Option<String>>>,
    lease: Mutex<Option<Lease>>,
    /// Why the lease was lost, once the renewer found out.
    lost: Mutex<Option<String>>,
    writes: AtomicUsize,
    submits: AtomicUsize,
}

impl Deployment {
    /// The deployment `name` (`app[env=prod]`) in `store`.
    pub fn new(store: Arc<dyn Store>, name: &str, times: LeaseTimes) -> Deployment {
        Deployment {
            inner: Arc::new(Inner {
                store,
                name: name.to_string(),
                times,
                etag: Mutex::new(None),
                lease: Mutex::new(None),
                lost: Mutex::new(None),
                writes: AtomicUsize::new(0),
                submits: AtomicUsize::new(0),
            }),
        }
    }

    /// The local deployment whose state file is `state`.
    pub fn local(state: &Path, name: &str) -> Deployment {
        Deployment::new(
            Arc::new(LocalStore::beside(state)),
            name,
            LeaseTimes::default(),
        )
    }

    pub fn store(&self) -> &Arc<dyn Store> {
        &self.inner.store
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// How messages name the deployment's object `key`.
    pub fn locate(&self, key: &str) -> String {
        self.inner.store.locate(key)
    }

    /// Is there state?
    pub fn has_state(&self) -> Result<bool> {
        Ok(self.inner.store.get(STATE)?.is_some())
    }

    /// The deployment's state; empty when there is none.
    pub fn load_state(&self) -> Result<State> {
        let (st, etag) = match self.inner.store.get(STATE)? {
            None => (
                State {
                    version: 1,
                    ..State::default()
                },
                None,
            ),
            Some(o) => (
                serde_json::from_slice(&o.bytes).context("parse state")?,
                Some(o.etag),
            ),
        };
        *self.inner.etag.lock().expect("etag") = Some(etag);
        Ok(st)
    }

    /// Write the deployment's state. In a store that fences, only under
    /// its lease ([`Deployment::lock`]), with the lease's fencing counter
    /// in it, and only over the version this run last read or wrote.
    pub fn save_state(&self, st: &State) -> Result<()> {
        let inner = &self.inner;
        stall_at("DFORM_TEST_STALL_AT_WRITE", &inner.writes);
        if inner.store.fenced() {
            self.save_fenced(st)?;
        } else {
            inner
                .store
                .put(STATE, &serde_json::to_vec_pretty(st)?, &Cond::Any)?;
        }
        Ok(())
    }

    fn save_fenced(&self, st: &State) -> Result<()> {
        let inner = &self.inner;
        let fence = self.check_lease()?;
        let mut v = serde_json::to_value(st)?;
        if let Some(m) = v.as_object_mut() {
            m.insert("fence".into(), fence.into());
        }
        let bytes = serde_json::to_vec_pretty(&v)?;
        let mut etag = inner.etag.lock().expect("etag");
        let cond = match &*etag {
            Some(Some(e)) => Cond::IfMatch(e.clone()),
            Some(None) => Cond::IfAbsent,
            None => bail!(
                "internal: stack {}: state written before it was read",
                inner.name
            ),
        };
        match inner.store.put(STATE, &bytes, &cond)? {
            Some(e) => {
                *etag = Some(Some(e));
                Ok(())
            }
            None => bail!(
                "stack {}: state write refused by fencing: {} changed since this run (fence \
                 {fence}) last read or wrote it; another apply holds the stack now, and \
                 nothing was written",
                inner.name,
                inner.store.locate(STATE)
            ),
        }
    }

    /// Before an Apply call: in a store that fences, is the lease still
    /// this run's? A stale holder makes no call once it can see it is not
    /// (the module doc says what it can still do).
    pub fn check_fence(&self) -> Result<()> {
        stall_at("DFORM_TEST_STALL_AT_SUBMIT", &self.inner.submits);
        if !self.inner.store.fenced() {
            return Ok(());
        }
        self.check_lease()
            .map(|_| ())
            .context("no provider call was made")
    }

    /// Publish the deployment's outputs (`stack::Published`) beside its
    /// state, under its lease in a store that fences. Unchanged, nothing
    /// is written.
    pub fn publish(&self, bytes: &[u8]) -> Result<()> {
        let inner = &self.inner;
        if inner.store.get(OUTPUTS)?.is_some_and(|o| o.bytes == bytes) {
            return Ok(());
        }
        if inner.store.fenced() {
            self.check_lease()
                .context("the outputs were not published")?;
        }
        inner.store.put(OUTPUTS, bytes, &Cond::Any)?;
        Ok(())
    }

    /// Write the controller's memo ([`MEMO`]) beside the state. In a store
    /// that fences, only under this run's lease, checked first, so a stale
    /// controller cannot overwrite a newer one's: a run that holds no lease
    /// (its apply ended, or never took it) writes nothing (`Ok(false)`),
    /// and one whose lease was taken over is refused.
    pub fn put_memo(&self, bytes: &[u8]) -> Result<bool> {
        let inner = &self.inner;
        if inner.store.fenced() {
            if inner.lease.lock().expect("lease").is_none() {
                return Ok(false);
            }
            self.check_lease()
                .context("the controller's memo was not written")?;
        }
        inner.store.put(MEMO, bytes, &Cond::Any)?;
        Ok(true)
    }

    /// The fencing counter of this run's lease, once the lease object says
    /// it is still this run's (renewed first when it lapsed without being
    /// taken over).
    fn check_lease(&self) -> Result<u64> {
        let inner = &self.inner;
        if let Some(why) = inner.lost.lock().expect("lost").clone() {
            bail!(
                "stack {}: state write refused by fencing: {why}",
                inner.name
            );
        }
        let mut guard = inner.lease.lock().expect("lease");
        let Some(lease) = guard.as_mut() else {
            bail!(
                "internal: stack {}: state written without its lease",
                inner.name
            );
        };
        let at = inner.store.locate(LOCK);
        let now = match inner.store.get(LOCK)? {
            Some(o) => Some(LeaseRecord::parse(&o.bytes, &at)?),
            None => None,
        };
        match now {
            Some(r) if r.holder == lease.holder && r.fence == lease.fence => {
                if r.expires_ms <= now_ms() {
                    inner
                        .store
                        .renew(lease, inner.times.duration)
                        .with_context(|| {
                            format!("stack {}: state write refused by fencing", inner.name)
                        })?;
                }
                Ok(lease.fence)
            }
            now => Err(anyhow!(Lost {
                at,
                fence: lease.fence,
                now,
            }))
            .with_context(|| format!("stack {}: state write refused by fencing", inner.name)),
        }
    }

    /// Take the deployment's lock: one apply at a time. Held until the
    /// guard drops; in a store that fences, renewed until then.
    pub fn lock(&self) -> Result<Guard> {
        let inner = &self.inner;
        let holder = format!("{} pid {}", crate::audit::who(), std::process::id());
        let lease = inner
            .store
            .acquire(LOCK, &inner.name, &holder, inner.times.duration)?;
        if let Some(prev) = &lease.took_over {
            eprintln!(
                "note: stack {}: taking over the lease of {} (fence {}), which expired {}: {}",
                inner.name,
                prev.holder,
                prev.fence,
                time(prev.expires_ms),
                inner.store.locate(LOCK)
            );
        }
        *inner.lease.lock().expect("lease") = Some(lease);
        *inner.lost.lock().expect("lost") = None;
        let mut guard = Guard {
            inner: self.inner.clone(),
            stop: None,
            renewer: None,
        };
        if inner.store.fenced() {
            guard.renew_every(inner.times);
            self.fence_state()?;
        }
        // Tests hold the lock until a file appears, to run a second apply
        // against a held stack.
        if let Some(release) = std::env::var_os("DFORM_TEST_HOLD_LOCK") {
            while !Path::new(&release).exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(guard)
    }

    /// Write the state once with the new lease's fencing counter in it, so
    /// its ETag moves and a write of an earlier holder's is refused. The
    /// state must be the version this run read, if it read one.
    fn fence_state(&self) -> Result<()> {
        let inner = &self.inner;
        let fence = inner
            .lease
            .lock()
            .expect("lease")
            .as_ref()
            .map_or(0, |l| l.fence);
        let mut etag = inner.etag.lock().expect("etag");
        let changed = || {
            anyhow!(
                "stack {}: its state {} changed since this run read it (another apply wrote \
                 it); run again",
                inner.name,
                inner.store.locate(STATE)
            )
        };
        let Some(o) = inner.store.get(STATE)? else {
            if matches!(&*etag, Some(Some(_))) {
                return Err(changed());
            }
            return Ok(());
        };
        if let Some(read) = &*etag
            && read.as_deref() != Some(o.etag.as_str())
        {
            return Err(changed());
        }
        let mut v: serde_json::Value = serde_json::from_slice(&o.bytes).context("parse state")?;
        if let Some(m) = v.as_object_mut() {
            m.insert("fence".into(), fence.into());
        }
        match inner.store.put(
            STATE,
            &serde_json::to_vec_pretty(&v)?,
            &Cond::IfMatch(o.etag),
        )? {
            Some(e) => {
                if etag.is_some() {
                    *etag = Some(Some(e));
                }
                Ok(())
            }
            None => Err(changed()),
        }
    }

    /// `dform stack unlock`: break the deployment's lock.
    pub fn unlock(&self) -> Result<String> {
        self.inner.store.break_lease(LOCK, &self.inner.name)
    }

    /// The deployment's audit log; each entry also to `sink`.
    pub fn audit(&self, sink: Option<String>) -> crate::audit::Log {
        crate::audit::Log::new(self.inner.store.clone(), sink)
    }

    /// The deployment's plan key, made on first use.
    pub fn plan_key(&self) -> Result<crate::zset::file::Key> {
        crate::zset::file::Key::load_or_create(self.inner.store.as_ref())
    }

    /// The plan key, when the deployment has one; a read makes none.
    pub fn existing_plan_key(&self) -> Result<Option<crate::zset::file::Key>> {
        crate::zset::file::Key::load(self.inner.store.as_ref())
    }
}

/// Tests stop an apply as it is about to make its Nth state write
/// (`DFORM_TEST_STALL_AT_WRITE`), or its Nth Apply call
/// (`DFORM_TEST_STALL_AT_SUBMIT`, before the lease check), to kill or
/// pause it there: `VAR=N:DIR` makes `DIR/stalled` (holding the pid) and
/// waits for `DIR/resume`. `count` counts them.
fn stall_at(var: &str, count: &AtomicUsize) {
    let n = count.fetch_add(1, Ordering::SeqCst) + 1;
    let Some(spec) = std::env::var_os(var) else {
        return;
    };
    let spec = spec.to_string_lossy();
    let Some((at, dir)) = spec.split_once(':') else {
        return;
    };
    if at.parse() != Ok(n) {
        return;
    }
    let dir = Path::new(dir);
    let _ = std::fs::write(dir.join("stalled"), std::process::id().to_string());
    while !dir.join("resume").exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A deployment's lock, held until dropped. In a store that fences, a
/// thread renews the lease every `lease_renewal` until then; dropping the
/// guard stops the thread (at once: it waits on a channel the guard
/// closes), joins it and releases the lease.
pub struct Guard {
    inner: Arc<Inner>,
    stop: Option<mpsc::Sender<()>>,
    renewer: Option<std::thread::JoinHandle<()>>,
}

impl Guard {
    fn renew_every(&mut self, times: LeaseTimes) {
        let (tx, rx) = mpsc::channel::<()>();
        let inner = self.inner.clone();
        let renewer = std::thread::Builder::new()
            .name("dform-lease".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(times.renewal) {
                    let mut lease = inner.lease.lock().expect("lease");
                    let Some(l) = lease.as_mut() else {
                        return;
                    };
                    if let Err(e) = inner.store.renew(l, times.duration) {
                        eprintln!("warning: stack {}: {e:#}", inner.name);
                        *inner.lost.lock().expect("lost") = Some(format!("{e:#}"));
                        return;
                    }
                }
            })
            .expect("spawn the lease renewer");
        self.stop = Some(tx);
        self.renewer = Some(renewer);
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(r) = self.renewer.take() {
            let _ = r.join();
        }
        let lease = self.inner.lease.lock().expect("lease").take();
        let lost = self.inner.lost.lock().expect("lost").is_some();
        if let Some(l) = lease
            && !lost
        {
            let _ = self.inner.store.release(&l);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(ms: u64) -> LeaseTimes {
        LeaseTimes {
            duration: Duration::from_millis(ms),
            renewal: Duration::from_millis(ms / 4),
        }
    }

    fn deployment(store: &Arc<MemoryStore>, ms: u64) -> Deployment {
        Deployment::new(store.clone(), "app", times(ms))
    }

    #[test]
    fn a_conditional_write_is_refused_over_another_version() {
        let s = MemoryStore::new();
        let e1 = s.put("k", b"one", &Cond::IfAbsent).unwrap().unwrap();
        assert_eq!(s.put("k", b"two", &Cond::IfAbsent).unwrap(), None);
        let e2 = s
            .put("k", b"two", &Cond::IfMatch(e1.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(s.put("k", b"three", &Cond::IfMatch(e1)).unwrap(), None);
        assert_eq!(s.get("k").unwrap().unwrap().etag, e2);
        assert_eq!(s.get("k").unwrap().unwrap().bytes, b"two");
    }

    #[test]
    fn a_live_lease_is_refused_and_an_expired_one_taken_over() {
        let s = MemoryStore::new();
        let a = s
            .acquire(LOCK, "app", "a", Duration::from_secs(60))
            .unwrap();
        assert_eq!(a.fence, 1);
        let e = s
            .acquire(LOCK, "app", "b", Duration::from_secs(60))
            .unwrap_err();
        let held = e.downcast_ref::<Held>().expect("held");
        assert_eq!((held.record.holder.as_str(), held.record.fence), ("a", 1));
        assert!(
            e.to_string()
                .contains("stack app is locked by another apply (a, fence 1")
        );
        let short = s.acquire("short", "app", "a", Duration::ZERO).unwrap();
        let b = s
            .acquire("short", "app", "b", Duration::from_secs(60))
            .unwrap();
        assert_eq!(b.fence, short.fence + 1);
        assert_eq!(b.took_over.as_ref().map(|r| r.holder.as_str()), Some("a"));
        // The old holder's renewal and release find it gone.
        let mut short = short;
        let lost = s.renew(&mut short, Duration::from_secs(60)).unwrap_err();
        assert!(
            lost.to_string().contains("b holds it now (fence 2)"),
            "{lost}"
        );
        s.release(&short).unwrap();
        assert!(
            s.acquire("short", "app", "c", Duration::from_secs(60))
                .is_err()
        );
        // Released, it is free, and its counter carries on.
        s.release(&b).unwrap();
        assert_eq!(
            s.acquire("short", "app", "c", Duration::ZERO)
                .unwrap()
                .fence,
            3
        );
    }

    #[test]
    fn a_stale_holders_state_write_is_refused_by_fencing() {
        let store = Arc::new(MemoryStore::new());
        let a = deployment(&store, 200);
        let mut st = a.load_state().unwrap();
        let ga = a.lock().unwrap();
        a.save_state(&st).unwrap();
        // A stalls: its renewer stops, its lease runs out.
        let mut ga = ga;
        drop(ga.stop.take());
        ga.renewer.take().unwrap().join().unwrap();
        std::thread::sleep(Duration::from_millis(250));
        // B takes the lease over; A wakes before B has written anything.
        let b = deployment(&store, 60_000);
        b.load_state().unwrap();
        let gb = b.lock().unwrap();
        st.keys = 7;
        let e = a.save_state(&st).unwrap_err();
        assert!(format!("{e:#}").contains("refused by fencing"), "{e:#}");
        let now: State = serde_json::from_slice(&store.get(STATE).unwrap().unwrap().bytes).unwrap();
        assert_eq!(now.keys, 0);
        drop(gb);
        drop(ga);
    }

    #[test]
    fn a_stale_holders_first_write_is_refused_when_there_was_no_state_to_fence() {
        // No state yet: the takeover has nothing to write its counter into,
        // so the lease check alone refuses the stale holder.
        let store = Arc::new(MemoryStore::new());
        let a = deployment(&store, 60_000);
        let st = a.load_state().unwrap();
        let ga = a.lock().unwrap();
        store.break_lease(LOCK, "app").unwrap();
        let b = deployment(&store, 60_000);
        b.load_state().unwrap();
        let gb = b.lock().unwrap();
        let e = a.save_state(&st).unwrap_err();
        assert!(format!("{e:#}").contains("refused by fencing"), "{e:#}");
        assert!(store.get(STATE).unwrap().is_none());
        b.save_state(&st).unwrap();
        drop(gb);
        drop(ga);
    }

    #[test]
    fn the_new_holders_first_write_moves_the_etag_under_a_stale_writer() {
        // Past the lease check: a stale holder whose write lands after the
        // takeover is refused by the ETag the takeover's fence write moved,
        // though the state's content is otherwise the same.
        let store = Arc::new(MemoryStore::new());
        let a = deployment(&store, 60_000);
        let st = a.load_state().unwrap();
        let ga = a.lock().unwrap();
        a.save_state(&st).unwrap();
        let before = store.get(STATE).unwrap().unwrap().etag;
        // Break A's lease and let B take it: B's fence write.
        store.break_lease(LOCK, "app").unwrap();
        let b = deployment(&store, 60_000);
        b.load_state().unwrap();
        let _gb = b.lock().unwrap();
        let after = store.get(STATE).unwrap().unwrap().etag;
        assert_ne!(before, after);
        let v = serde_json::to_vec_pretty(
            &serde_json::json!({"version": 1, "resources": {}, "fence": 1}),
        )
        .unwrap();
        assert_eq!(store.put(STATE, &v, &Cond::IfMatch(before)).unwrap(), None);
        drop(ga);
    }

    #[test]
    fn a_lease_is_renewed_while_held_and_released_on_drop() {
        let store = Arc::new(MemoryStore::new());
        let a = deployment(&store, 200);
        a.load_state().unwrap();
        let g = a.lock().unwrap();
        std::thread::sleep(Duration::from_millis(600));
        // Renewed: still live, so B is refused.
        let b = deployment(&store, 200);
        assert!(b.lock().is_err());
        a.save_state(&State::default()).unwrap();
        drop(g);
        let r: LeaseRecord =
            serde_json::from_slice(&store.get(LOCK).unwrap().unwrap().bytes).unwrap();
        assert_eq!((r.holder.as_str(), r.fence), ("", 1));
        b.load_state().unwrap();
        assert_eq!(
            b.lock()
                .unwrap()
                .inner
                .lease
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .fence,
            2
        );
    }

    #[test]
    fn a_local_store_keeps_the_files_beside_its_state() {
        let s = LocalStore::beside(Path::new("/x/w.state.json"));
        assert_eq!(s.path(STATE), Path::new("/x/w.state.json"));
        assert_eq!(s.path(LOCK), Path::new("/x/w.state.lock"));
        assert_eq!(s.path(KEY), Path::new("/x/w.state.key"));
        assert_eq!(s.path(AUDIT), Path::new("/x/w.state.audit.jsonl"));
        let s = LocalStore::beside(Path::new("/x/app/state.json"));
        assert_eq!(s.path(AUDIT), Path::new("/x/app/state.audit.jsonl"));
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("1h"), None);
    }
}

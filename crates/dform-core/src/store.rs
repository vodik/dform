//! Where a deployment's state lives: its stack's backend as a [`Store`],
//! objects by key with ETags, conditional writes and leases (README
//! "State backends"). `local("DIR")` is [`LocalStore`], the directory it
//! always was: the same files, its lock the holder's pid. `s3(...)` is
//! `dform-s3`'s, the objects of a bucket under a prefix; dform-core has no
//! network stack, and the command line wires the S3 store in.
//!
//! A deployment's objects: [`STATE`], [`KEY`] (its master in the clear,
//! a local backend's) or [`MASTER`] (its master sealed), [`AUDIT`]
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
/// The deployment's key file: its master (`custody`, `zset::file::Key`).
pub const KEY: &str = "state.key";
/// The deployment's master as the backend keeps it under a passphrase
/// (`custody::Record`): its id, and the master sealed, never in the clear.
pub const MASTER: &str = "state.master";
/// The deployment's audit log (`audit`).
pub const AUDIT: &str = "state.audit.jsonl";
/// The deployment's lock: the local backend's locked file, else the lease.
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
    /// lock is the kernel's, held by a live process and gone with it.
    fn fenced(&self) -> bool {
        true
    }

    /// Is it the machine's own disk (the local backend), where a key file
    /// beside the state is the operator's like any file of theirs; a
    /// bucket is shared with whoever may read the state (`custody`).
    fn local(&self) -> bool {
        false
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

    /// Give `lease` up (one another holder took is left alone).
    fn release(&self, lease: &Lease) -> Result<()> {
        release(self, lease)
    }

    /// `dform stack unlock`: break the lease at `key`, whoever holds it.
    /// Returns what was done, for the user.
    fn break_lease(&self, key: &str, stack: &str) -> Result<String> {
        break_lease(self, key, stack)
    }

    /// Append to the object at `key` what `line` makes of its current
    /// content, without losing a concurrent append. `line` is given the
    /// content, or (a store that appends in place) at least its last
    /// [`TAIL_LINES`] lines.
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

/// The local backend's lock is held by another run (the message names
/// it): as [`Held`] is a lease's.
#[derive(Debug)]
pub struct Locked(pub String);

impl std::fmt::Display for Locked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Locked {}

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
    match put_own(s, lease, &rec)? {
        Ok(etag) => {
            lease.etag = etag;
            lease.expires_ms = rec.expires_ms;
            Ok(())
        }
        Err(now) => Err(Lost {
            at: s.locate(&lease.key),
            fence: lease.fence,
            now,
        }
        .into()),
    }
}

fn release<S: Store + ?Sized>(s: &S, lease: &Lease) -> Result<()> {
    let rec = LeaseRecord {
        holder: String::new(),
        expires_ms: 0,
        fence: lease.fence,
    };
    // A lease no longer this holder's is left alone.
    let _ = put_own(s, lease, &rec)?;
    Ok(())
}

/// Write `rec` over `lease`'s object while it is this holder's: the new
/// ETag, or what holds it instead (`None`: the object is gone). A write
/// refused over the ETag this holder last wrote, though the object still
/// names this holder and counter, is one whose answer was lost after it
/// landed (a timeout): it is written again over what is there.
fn put_own<S: Store + ?Sized>(
    s: &S,
    lease: &Lease,
    rec: &LeaseRecord,
) -> Result<std::result::Result<String, Option<LeaseRecord>>> {
    let mut etag = lease.etag.clone();
    for _ in 0..3 {
        if let Some(etag) = s.put(&lease.key, &rec.bytes(), &Cond::IfMatch(etag))? {
            return Ok(Ok(etag));
        }
        let Some(o) = s.get(&lease.key)? else {
            return Ok(Err(None));
        };
        let Ok(now) = LeaseRecord::parse(&o.bytes, &s.locate(&lease.key)) else {
            return Ok(Err(None));
        };
        if now.holder != lease.holder || now.fence != lease.fence {
            return Ok(Err(Some(now)));
        }
        etag = o.etag;
    }
    bail!(
        "the lease {} changed under every write, though it still names this holder",
        s.locate(&lease.key)
    )
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
    /// The lock files this store holds locked, by key: dropping one
    /// unlocks it.
    held: Mutex<BTreeMap<String, std::fs::File>>,
    /// Per key appended to, its length and its last lines as this store
    /// last wrote or read them: the next append that finds the file that
    /// long reads nothing.
    tails: Mutex<BTreeMap<String, (u64, Vec<u8>)>>,
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
        LocalStore {
            dir,
            prefix,
            held: Mutex::new(BTreeMap::new()),
            tails: Mutex::new(BTreeMap::new()),
        }
    }

    /// The file of `key`.
    pub fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}{key}", self.prefix))
    }

    /// Remove the temporaries a write of `key` left when it was killed
    /// before its rename ([`write_atomic`]).
    fn sweep(&self, key: &str) {
        let start = format!(".{}{key}.", self.prefix);
        let dir = match self.dir.as_os_str().is_empty() {
            true => Path::new("."),
            false => self.dir.as_path(),
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(&start) && is_temporary(&name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
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

    /// Every write is whole: a temporary file beside the target, fsynced,
    /// renamed over it ([`write_atomic`]), so a crash or a full disk
    /// leaves the old content or the new, never a cut one. `IfMatch` is
    /// checked, then written: the local backend's writers of one object
    /// are one at a time (its lock, or the audit log's `flock`).
    /// `IfAbsent` links the temporary in place, which fails when there is
    /// a file, and makes it readable by its owner only: it is the plan key.
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
                return Ok(create_atomic(&path, bytes, 0o600)?.then(|| content_etag(bytes)));
            }
        }
        let before = (key == STATE).then_some("renamed");
        write_atomic_at(&path, bytes, None, before)?;
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
                // A write's temporary is no object (`write_atomic`).
                if is_temporary(&e.file_name().to_string_lossy()) {
                    continue;
                }
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

    fn local(&self) -> bool {
        true
    }

    /// The file `key`, locked (`flock`, the kernel's lock, which dies
    /// with its holder): the open file is kept until [`Store::release`].
    /// The pid written inside is for messages only. A file a killed apply
    /// left is taken, since no one holds its lock, with a note.
    fn acquire(&self, key: &str, stack: &str, _holder: &str, _ttl: Duration) -> Result<Lease> {
        use std::io::{Seek, Write};
        let path = self.path(key);
        LocalStore::mkdir(&path)?;
        // A file removed (released, or broken) between its open and its
        // lock is not the lock any more: look again.
        for _ in 0..20 {
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .with_context(|| format!("lock {}", path.display()))?;
            match f.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(Locked(format!(
                        "stack {stack} is locked by another apply (pid {}): {}; \
                         wait for it, or `dform stack unlock {stack}` if no apply is running",
                        lock_holder(&path).unwrap_or_else(|| "unknown".into()),
                        path.display()
                    ))
                    .into());
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("lock {}", path.display()));
                }
            }
            if !same_file(&f, &path) {
                continue;
            }
            // What a write killed between its temporary and its rename
            // left: no one else writes the state while the lock is held.
            self.sweep(STATE);
            if let Some(pid) = lock_holder(&path) {
                eprintln!(
                    "note: stack {stack}: taking over the lock of pid {pid}, which no longer \
                     holds it ({})",
                    path.display()
                );
            }
            f.set_len(0)
                .and_then(|()| f.rewind())
                .and_then(|()| writeln!(f, "{}", std::process::id()))
                .with_context(|| format!("write {}", path.display()))?;
            self.held
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.to_string(), f);
            return Ok(Lease {
                key: key.to_string(),
                holder: std::process::id().to_string(),
                fence: 0,
                expires_ms: u64::MAX,
                etag: String::new(),
                took_over: None,
            });
        }
        bail!(
            "stack {stack}: could not take the lock {}: it was removed under every attempt",
            path.display()
        )
    }

    fn renew(&self, _lease: &mut Lease, _ttl: Duration) -> Result<()> {
        Ok(())
    }

    /// The file is removed while it is still locked, then unlocked: a
    /// taker that opened it before the removal finds it is not the file
    /// at the path any more.
    fn release(&self, lease: &Lease) -> Result<()> {
        let path = self.path(&lease.key);
        let held = self
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&lease.key);
        let Some(f) = held else {
            return Ok(());
        };
        let removed = match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("remove {}", path.display()))
            }
            _ => Ok(()),
        };
        let unlocked = f
            .unlock()
            .with_context(|| format!("unlock {}", path.display()));
        removed.and(unlocked)
    }

    /// Remove the lock file, unless an apply holds its lock (whatever pid
    /// the file names).
    fn break_lease(&self, key: &str, stack: &str) -> Result<String> {
        let path = self.path(key);
        let f = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(format!("stack {stack} is not locked"));
            }
            Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
        };
        let pid = lock_holder(&path).unwrap_or_else(|| "unknown".into());
        match f.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => bail!(
                "stack {stack} is locked by a running apply (pid {pid}): {}; stop it first",
                path.display()
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("lock {}", path.display()));
            }
        }
        if same_file(&f, &path) {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
        f.unlock()
            .with_context(|| format!("unlock {}", path.display()))?;
        Ok(format!(
            "stack {stack} unlocked (no apply held the lock of pid {pid}): {}",
            path.display()
        ))
    }

    fn appends_in_place(&self) -> bool {
        true
    }

    /// The file is locked (`flock`) while it is read and written, so two
    /// processes appending (a `plan --out` beside an apply) do not fork an
    /// audit log's chain; and synced before the lock is let go. Only its
    /// last lines are read, and not even those when it is as long as this
    /// store's last append left it (R-146): an apply's appends cost what
    /// they write, not the log's length.
    fn append(&self, key: &str, line: &mut dyn FnMut(&[u8]) -> Vec<u8>) -> Result<()> {
        use std::io::Write;
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
        let len = f
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        let mut tails = self.tails.lock().expect("tails");
        let tail = match tails.remove(key) {
            Some((at, tail)) if at == len => tail,
            _ => read_tail(&mut f, len).with_context(|| format!("read {}", path.display()))?,
        };
        let add = line(&tail);
        f.write_all(&add)
            .with_context(|| format!("write {}", path.display()))?;
        // Durable before it returns: the log is the state's (`wal`).
        f.sync_data()
            .with_context(|| format!("sync {}", path.display()))?;
        let mut tail = tail;
        tail.extend_from_slice(&add);
        let keep = last_lines(&tail, TAIL_LINES);
        tails.insert(
            key.to_string(),
            (len + add.len() as u64, tail[keep..].to_vec()),
        );
        f.unlock()?;
        Ok(())
    }
}

/// How many of an object's last lines an append in place is given
/// ([`Store::append`]): the last entry, and the one before it when the
/// last is a line a crash cut short.
pub const TAIL_LINES: usize = 2;

/// Where in `bytes` its last `n` non-empty lines start (0 when it has no
/// more).
fn last_lines(bytes: &[u8], n: usize) -> usize {
    let mut seen = 0;
    let mut end = bytes.len();
    while end > 0 {
        let start = bytes[..end]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |i| i + 1);
        if bytes[start..end].iter().any(|b| !b.is_ascii_whitespace()) {
            seen += 1;
            if seen == n {
                return start;
            }
        }
        end = start.saturating_sub(1);
        if start == 0 {
            break;
        }
    }
    0
}

/// The end of file `f` (`len` bytes long) from the start of its last
/// [`TAIL_LINES`] lines, read backwards in blocks.
fn read_tail(f: &mut std::fs::File, len: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    const BLOCK: u64 = 64 * 1024;
    let mut from = len;
    let mut tail: Vec<u8> = Vec::new();
    while from > 0 {
        let start = from.saturating_sub(BLOCK);
        let mut block = vec![0; (from - start) as usize];
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut block)?;
        block.extend_from_slice(&tail);
        tail = block;
        from = start;
        // A line is whole once a newline before it is in hand.
        let at = last_lines(&tail, TAIL_LINES);
        if at > 0 {
            return Ok(tail[at..].to_vec());
        }
    }
    Ok(tail)
}

/// Write `bytes` to `path` whole (R-138): a temporary file beside it
/// (the same directory, so the same filesystem), written, fsynced and
/// renamed over it, and the directory fsynced. A crash, a kill or a full
/// disk leaves the old content or the new. The temporary is removed on
/// every error path.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_at(path, bytes, None, None)
}

/// [`write_atomic`]; `mode` the new file's permissions, `chaos` the point
/// tests may stop the process at between the temporary and the rename
/// ([`abort_at`]).
fn write_atomic_at(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    chaos: Option<&str>,
) -> Result<()> {
    let tmp = Temporary::write(path, bytes, mode)?;
    if let Some(point) = chaos {
        abort_at(point);
    }
    std::fs::rename(&tmp.0, path)
        .with_context(|| format!("rename {} to {}", tmp.0.display(), path.display()))?;
    tmp.keep();
    sync_dir(path)
}

/// Create `path` with `bytes` whole unless there is a file there: the
/// temporary is linked in place, which fails when one exists. `false`
/// when one did.
fn create_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<bool> {
    let tmp = Temporary::write(path, bytes, Some(mode))?;
    match std::fs::hard_link(&tmp.0, path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("write {}", path.display())),
    }
    drop(tmp);
    sync_dir(path)?;
    Ok(true)
}

/// Is a directory entry's name a write's temporary?
fn is_temporary(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".tmp")
}

/// A write's temporary file, removed when dropped unless kept (its path
/// emptied).
struct Temporary(PathBuf);

impl Temporary {
    /// `bytes` in a new temporary beside `path`, synced.
    fn write(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<Temporary> {
        use std::io::Write;
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = path.parent().unwrap_or(Path::new(""));
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = dir.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        let t = Temporary(tmp);
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .with_context(|| format!("write {}", t.0.display()))?;
        Ok(t)
    }

    /// It was renamed: nothing to remove.
    fn keep(mut self) {
        self.0 = PathBuf::new();
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// Make a rename or a new name in `path`'s directory durable.
fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("sync {}", dir.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// The pid a lock file names, for messages.
fn lock_holder(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let pid = text.trim();
    (!pid.is_empty()).then(|| pid.to_string())
}

/// Is the open file `f` the one at `path` now (not removed or replaced
/// since it was opened)?
fn same_file(f: &std::fs::File, path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (f.metadata(), std::fs::metadata(path)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = f;
        path.exists()
    }
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
    /// The thread renewing the lease, while a [`Guard`] holds it.
    renewer: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The state as this run last read or logged it (`wal`).
    wal: Mutex<Wal>,
    /// The audit log's sink, which the state's entries go to as well.
    sink: Mutex<(Option<String>, Duration, bool)>,
    writes: AtomicUsize,
    submits: AtomicUsize,
}

/// The state as this run last read, logged or checkpointed it: what the
/// next `state` entry is a change from.
#[derive(Default)]
struct Wal {
    /// As JSON; `None` before it is read.
    state: Option<serde_json::Value>,
    /// The last log entry it includes.
    pos: Option<crate::audit::Pos>,
    /// The log reaches it: the next entry may be a change, else it is
    /// written whole.
    chained: bool,
}

/// The state a read found: the checkpoint with the log's entries after it
/// replayed.
struct Read {
    state: serde_json::Value,
    /// The checkpoint's ETag; `None` when there is none.
    etag: Option<String>,
    replayed: crate::wal::Replayed,
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
                renewer: Mutex::new(None),
                wal: Mutex::new(Wal::default()),
                sink: Mutex::new((None, crate::audit::SINK_TIMEOUT, false)),
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

    /// Is there state: a checkpoint, or the log's entries of one a run
    /// that died before its first checkpoint wrote?
    pub fn has_state(&self) -> Result<bool> {
        if self.inner.store.get(STATE)?.is_some() {
            return Ok(true);
        }
        Ok(self.read()?.replayed.entries > 0)
    }

    /// The deployment's state, empty when there is none: its checkpoint
    /// with the log's `state` entries after it replayed (`wal`).
    pub fn load_state(&self) -> Result<State> {
        let read = self.read()?;
        let st = serde_json::from_value(read.state.clone()).context("parse state")?;
        *self.inner.etag.lock().expect("etag") = Some(read.etag);
        *self.inner.wal.lock().expect("wal") = Wal {
            state: Some(read.state),
            pos: read.replayed.pos,
            chained: read.replayed.chained,
        };
        Ok(st)
    }

    /// `dform state show --from-log`: the state the log alone makes, from
    /// its last whole `state` entry; `None` when it has none.
    pub fn state_from_log(&self) -> Result<Option<(State, usize)>> {
        let (tail, _) = self.wal_log().after(None)?;
        let r = crate::wal::replay(serde_json::json!({}), None, 0, &tail, false);
        if !r.chained {
            return Ok(None);
        }
        let st = serde_json::from_value(r.state).context("parse state")?;
        Ok(Some((st, r.entries)))
    }

    /// The checkpoint and the log after it, replayed.
    fn read(&self) -> Result<Read> {
        let empty = || {
            serde_json::to_value(State {
                version: 1,
                ..State::default()
            })
        };
        let (mut v, etag) = match self.inner.store.get(STATE)? {
            None => (empty()?, None),
            Some(o) => (
                serde_json::from_slice::<serde_json::Value>(&o.bytes).context("parse state")?,
                Some(o.etag),
            ),
        };
        // The checkpoint's own fields, not the state's.
        let (fence, pos) = match v.as_object_mut() {
            Some(m) => (
                m.remove("fence").and_then(|f| f.as_u64()).unwrap_or(0),
                m.remove("log")
                    .and_then(|p| serde_json::from_value::<crate::audit::Pos>(p).ok()),
            ),
            None => (0, None),
        };
        // A checkpoint the log does not reach (none, one from before the
        // state had a log, or the log started again): its own, unless the
        // log has a whole entry since.
        let (tail, found) = self.wal_log().after(pos.as_ref())?;
        let replayed = crate::wal::replay(v, pos, fence, &tail, found);
        Ok(Read {
            state: replayed.state.clone(),
            etag,
            replayed,
        })
    }

    /// The log the state's entries go to: the audit log, its sink as this
    /// run's ([`Deployment::audit`]).
    fn wal_log(&self) -> crate::audit::Log {
        let (sink, timeout, all) = self.inner.sink.lock().expect("sink").clone();
        crate::audit::Log::new(self.inner.store.clone(), sink)
            .with_sink_timeout(timeout)
            .with_sink_entries(all)
    }

    /// Log what changed in the state since this run last read or logged
    /// it, durably, before anything else is done (`wal`): after every Apply
    /// call that answers. In a store that fences, only under this run's
    /// lease, checked first, and with its fence in the entry. Nothing
    /// changed, nothing is written.
    pub fn record(&self, st: &State) -> Result<()> {
        stall_at("DFORM_TEST_STALL_AT_WRITE", &self.inner.writes);
        self.log_state(st)
    }

    fn log_state(&self, st: &State) -> Result<()> {
        let inner = &self.inner;
        let v = serde_json::to_value(st)?;
        let mut wal = inner.wal.lock().expect("wal");
        let fields = match (&wal.state, wal.chained) {
            (Some(was), true) => {
                let changes = crate::wal::diff(was, &v);
                if changes.is_empty() {
                    return Ok(());
                }
                serde_json::json!({ "changes": changes })
            }
            _ => serde_json::json!({ "full": v }),
        };
        let fence = match inner.store.fenced() {
            true => self.check_lease()?,
            false => 0,
        };
        let mut fields = fields;
        fields["fence"] = fence.into();
        let pos = self.wal_log().append("state", fields).with_context(|| {
            format!(
                "stack {}: the change of state was not logged, and nothing after it is done",
                inner.name
            )
        })?;
        abort_at("logged");
        *wal = Wal {
            state: Some(v),
            pos: Some(pos),
            chained: true,
        };
        Ok(())
    }

    /// Write the deployment's state: its change logged ([`Deployment::record`]),
    /// then the checkpoint, written whole, saying the last log entry it
    /// includes. In a store that fences, only under its lease
    /// ([`Deployment::lock`]), with the lease's fencing counter in it, and
    /// only over the version this run last read or wrote.
    pub fn save_state(&self, st: &State) -> Result<()> {
        let inner = &self.inner;
        stall_at("DFORM_TEST_STALL_AT_WRITE", &inner.writes);
        self.log_state(st)?;
        let mut v = serde_json::to_value(st)?;
        if let (Some(m), Some(pos)) = (v.as_object_mut(), &inner.wal.lock().expect("wal").pos) {
            m.insert("log".into(), serde_json::to_value(pos)?);
        }
        if inner.store.fenced() {
            self.save_fenced(v)?;
        } else {
            inner
                .store
                .put(STATE, &serde_json::to_vec_pretty(&v)?, &Cond::Any)?;
        }
        Ok(())
    }

    fn save_fenced(&self, mut v: serde_json::Value) -> Result<()> {
        let inner = &self.inner;
        let fence = self.check_lease()?;
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

    /// Write one of the deployment's own objects beside the state (the
    /// controller's [`MEMO`], its [`PENDING`] digest), `what` in messages.
    /// In a store that fences, only under this run's lease, checked
    /// first, so a stale controller cannot overwrite a newer one's: a run
    /// that holds no lease (its apply ended, or never took it) writes
    /// nothing (`Ok(false)`), and one whose lease was taken over is
    /// refused.
    pub fn put_fenced(&self, key: &str, bytes: &[u8], what: &str) -> Result<bool> {
        if !self.fence_for(what)? {
            return Ok(false);
        }
        self.inner.store.put(key, bytes, &Cond::Any)?;
        Ok(true)
    }

    /// Remove one of the deployment's own objects, as
    /// [`Deployment::put_fenced`] writes one.
    pub fn delete_fenced(&self, key: &str, what: &str) -> Result<bool> {
        if !self.fence_for(what)? {
            return Ok(false);
        }
        self.inner.store.delete(key)?;
        Ok(true)
    }

    /// May this run write its objects now? Always where the store does not
    /// fence; else only under its lease, checked.
    fn fence_for(&self, what: &str) -> Result<bool> {
        if !self.inner.store.fenced() {
            return Ok(true);
        }
        if self.inner.lease().is_none() {
            return Ok(false);
        }
        self.check_lease()
            .with_context(|| format!("{what} was not written"))?;
        Ok(true)
    }

    /// The fencing counter of this run's lease, once the lease object says
    /// it is still this run's (renewed first when it lapsed without being
    /// taken over).
    fn check_lease(&self) -> Result<u64> {
        let inner = &self.inner;
        if let Err(why) = inner.held() {
            bail!(
                "stack {}: state write refused by fencing: {why}",
                inner.name
            );
        }
        // Copied out: the lock is not held across the store's calls.
        let Some(mut lease) = inner.lease().clone() else {
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
                        .renew(&mut lease, inner.times.duration)
                        .with_context(|| {
                            format!("stack {}: state write refused by fencing", inner.name)
                        })?;
                    inner.renewed(&lease);
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

    /// Take the deployment's lock: one apply at a time. Held until
    /// [`Guard::release`], or the guard drops; in a store that fences,
    /// renewed until then.
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
        *inner.lease() = Some(lease);
        *inner.lost.lock().expect("lost") = None;
        let mut guard = Guard {
            inner: self.inner.clone(),
            stop: None,
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

    /// Mark the taking of the lease in the log (a `lease` entry, its
    /// fence: what a stale holder logs after it is skipped on replay),
    /// then write the state once with the new lease's fencing counter in
    /// it, so its ETag moves and a write of an earlier holder's is
    /// refused: a checkpoint of the log up to the `lease` entry. The state
    /// must be the version this run read, if it read one, its log's
    /// entries included.
    fn fence_state(&self) -> Result<()> {
        let inner = &self.inner;
        let (fence, holder) = inner
            .lease()
            .as_ref()
            .map_or((0, String::new()), |l| (l.fence, l.holder.clone()));
        let lease = self.wal_log().append(
            "lease",
            serde_json::json!({ "fence": fence, "holder": holder }),
        )?;
        let mut etag = inner.etag.lock().expect("etag");
        let mut wal = inner.wal.lock().expect("wal");
        let changed = || {
            anyhow!(
                "stack {}: its state {} changed since this run read it (another apply wrote \
                 it); run again",
                inner.name,
                inner.store.locate(STATE)
            )
        };
        let now = self.read()?;
        if let Some(read) = &*etag
            && (read != &now.etag || wal.state.as_ref() != Some(&now.state))
        {
            return Err(changed());
        }
        // Nothing to fence: no checkpoint, and no entry in the log.
        if now.etag.is_none() && now.replayed.entries == 0 {
            return Ok(());
        }
        let mut v = now.state.clone();
        if let Some(m) = v.as_object_mut() {
            m.insert("fence".into(), fence.into());
            m.insert("log".into(), serde_json::to_value(&lease)?);
        }
        let cond = match &now.etag {
            Some(e) => Cond::IfMatch(e.clone()),
            None => Cond::IfAbsent,
        };
        match inner
            .store
            .put(STATE, &serde_json::to_vec_pretty(&v)?, &cond)?
        {
            Some(e) => {
                if etag.is_some() {
                    *etag = Some(Some(e));
                    *wal = Wal {
                        state: Some(now.state),
                        pos: Some(lease),
                        chained: true,
                    };
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

    /// The deployment's audit log; each entry also to `sink`, given
    /// `timeout` for each, the state's own entries only when `all`. The
    /// log the state is written to carries the same sink.
    pub fn audit(&self, sink: Option<String>, timeout: Duration, all: bool) -> crate::audit::Log {
        *self.inner.sink.lock().expect("sink") = (sink, timeout, all);
        self.wal_log()
    }

    /// The deployment's master (`custody`): its key file read, one made
    /// only for a deployment with no state (`want`), or on `--new-master`.
    pub fn master(
        &self,
        mixing: &crate::custody::Mixing,
        want: crate::custody::Want,
    ) -> Result<crate::custody::Master> {
        crate::custody::resolve(
            self.inner.store.as_ref(),
            &self.inner.name,
            &|| self.applied(),
            mixing,
            want,
        )
    }

    /// Was the deployment ever applied with a master: its state records
    /// one (R-163), or its log an apply from before states did. A state
    /// written by hand (a test's fixture) was not.
    fn applied(&self) -> Result<bool> {
        if let Some(o) = self.inner.store.get(STATE)?
            && serde_json::from_slice::<serde_json::Value>(&o.bytes)
                .is_ok_and(|s| s["master"].is_string())
        {
            return Ok(true);
        }
        Ok(self
            .wal_log()
            .entries()?
            .iter()
            .any(|e| e["kind"] == "apply_start" || e["master"].is_string()))
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

/// Tests kill the process at a point of a state write
/// (`DFORM_TEST_ABORT_AT=POINT:N`), as a `kill -9` would: `logged`, its Nth
/// `state` entry durable in the log and nothing after it (between the log
/// and the checkpoint, or mid-tick); `renamed`, its Nth checkpoint's
/// temporary written and synced, not yet renamed over the state. The
/// process is killed (SIGKILL to itself): no destructor runs, nothing is
/// flushed.
fn abort_at(point: &str) {
    static COUNTS: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());
    let Some(spec) = std::env::var_os("DFORM_TEST_ABORT_AT") else {
        return;
    };
    let spec = spec.to_string_lossy();
    let Some((at, n)) = spec.split_once(':') else {
        return;
    };
    if at != point {
        return;
    }
    let mut counts = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let count = counts.entry(point.to_string()).or_default();
    *count += 1;
    if n.parse() == Ok(*count) {
        #[cfg(unix)]
        // SAFETY: kill takes a pid and a signal number and touches no
        // memory.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
        std::process::abort();
    }
}

impl Inner {
    /// This run's lease. A renewer that panicked holding it leaves it as
    /// it was, so a poisoned lock still holds the lease.
    fn lease(&self) -> std::sync::MutexGuard<'_, Option<Lease>> {
        self.lease.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keep `l`'s renewal, made on a copy of the lease: its ETag and
    /// expiry, while the lease is still that one.
    fn renewed(&self, l: &Lease) {
        if let Some(cur) = self.lease().as_mut()
            && cur.fence == l.fence
        {
            cur.etag = l.etag.clone();
            cur.expires_ms = l.expires_ms;
        }
    }

    /// Is the lease still this run's as far as this process knows: not
    /// found lost, and its renewer running? The renewer returns only once
    /// it recorded the lease lost, or when its guard stops it: finished
    /// otherwise, it panicked, and nothing renews the lease any more.
    fn held(&self) -> std::result::Result<(), String> {
        let mut lost = self.lost.lock().expect("lost");
        if let Some(why) = &*lost {
            return Err(why.clone());
        }
        if self
            .renewer
            .lock()
            .expect("renewer")
            .as_ref()
            .is_some_and(|r| r.is_finished())
        {
            let why = format!(
                "the lease {} is no longer renewed: its renewer stopped",
                self.store.locate(LOCK)
            );
            *lost = Some(why.clone());
            return Err(why);
        }
        Ok(())
    }
}

/// A deployment's lock, held until [`Guard::release`]. In a store that
/// fences, a thread renews the lease every `lease_renewal` until then;
/// releasing stops the thread (at once: it waits on a channel the guard
/// closes), joins it and gives the lease up. A guard dropped unreleased
/// (a panic, an early return) releases in its drop, and says so when that
/// fails.
pub struct Guard {
    inner: Arc<Inner>,
    stop: Option<mpsc::Sender<()>>,
}

impl Guard {
    /// Is the lease still this run's, as far as this process knows (not
    /// found lost by its renewer, the renewer running)? Asked before a
    /// question that would hold the lease while a person reads the plan.
    pub fn check(&self) -> Result<()> {
        self.inner
            .held()
            .map_err(|why| anyhow!("stack {}: {why}", self.inner.name))
    }

    /// Give the lock up: stop the renewer and release the lease (a lease
    /// another holder took is left alone).
    pub fn release(mut self) -> Result<()> {
        self.give_up()
    }

    fn give_up(&mut self) -> Result<()> {
        let inner = &self.inner;
        drop(self.stop.take());
        let renewer = inner
            .renewer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(r) = renewer
            && r.join().is_err()
        {
            eprintln!(
                "warning: stack {}: the lease renewer panicked; releasing the lease",
                inner.name
            );
        }
        // Released whether or not the lease was found lost: a release
        // writes only over a lease that still names this holder, and one
        // whose renewer died is still this run's until it expires.
        let lease = inner.lease().take();
        match lease {
            Some(l) => inner.store.release(&l).with_context(|| {
                format!(
                    "stack {}: release the lease {}; it is held until it expires, or \
                     `dform stack unlock {}`",
                    inner.name,
                    inner.store.locate(LOCK),
                    inner.name
                )
            }),
            None => Ok(()),
        }
    }

    fn renew_every(&mut self, times: LeaseTimes) {
        let (tx, rx) = mpsc::channel::<()>();
        let inner = self.inner.clone();
        let renewer = std::thread::Builder::new()
            .name("dform-lease".into())
            .spawn(move || {
                // A renewal that fails to reach the store says nothing of
                // who holds the lease: it is tried again sooner, backing
                // off to `lease_renewal`, and one past the expiry still
                // renews it if no one took it over. Only a lease found
                // another's (or gone) stops the renewer.
                let mut wait = times.renewal;
                let mut failing = false;
                while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(wait) {
                    // The lease is copied out and its renewal written
                    // back: the lock is not held across the store's call.
                    let Some(mut l) = inner.lease().clone() else {
                        return;
                    };
                    match inner.store.renew(&mut l, times.duration) {
                        Ok(()) => {
                            inner.renewed(&l);
                            if failing {
                                eprintln!("note: stack {}: the lease is renewed again", inner.name);
                            }
                            failing = false;
                            wait = times.renewal;
                        }
                        Err(e) if e.is::<Lost>() => {
                            eprintln!("warning: stack {}: {e:#}", inner.name);
                            *inner.lost.lock().expect("lost") = Some(format!("{e:#}"));
                            return;
                        }
                        Err(e) => {
                            if !failing {
                                eprintln!(
                                    "warning: stack {}: the lease {} was not renewed ({e:#}); \
                                     trying again ({}s of it left)",
                                    inner.name,
                                    inner.store.locate(LOCK),
                                    l.expires_ms.saturating_sub(now_ms()) / 1000
                                );
                                wait = times.renewal / 8;
                            } else {
                                wait = (wait * 2).min(times.renewal);
                            }
                            failing = true;
                        }
                    }
                }
            })
            .expect("spawn the lease renewer");
        self.stop = Some(tx);
        *self.inner.renewer.lock().expect("renewer") = Some(renewer);
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(e) = self.give_up() {
            eprintln!("warning: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An append in place is given the file's last lines, never the
    /// whole of it (R-146): from memory when the file is as this store
    /// left it, else read from the end; what another writer (another
    /// store on the same file) appended is seen.
    #[test]
    fn an_append_reads_the_last_lines_not_the_log() {
        let dir = std::env::temp_dir().join(format!("dform-store-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = LocalStore::beside(&dir.join(STATE));
        let b = LocalStore::beside(&dir.join(STATE));
        let mut given = Vec::new();
        for n in 0..200 {
            let store = if n % 50 == 49 { &b } else { &a };
            store
                .append("log", &mut |text| {
                    given.push(text.len());
                    let last = String::from_utf8_lossy(text)
                        .lines()
                        .last()
                        .map(|l| l.trim().parse::<usize>().unwrap());
                    assert_eq!(last, (n > 0).then(|| n - 1), "append {n}");
                    // Long lines: more than one block apart.
                    format!("{n}{}\n", " ".repeat(if n % 7 == 0 { 70_000 } else { 10 }))
                        .into_bytes()
                })
                .unwrap();
        }
        let whole = std::fs::metadata(a.path("log")).unwrap().len() as usize;
        assert!(whole > 1_000_000);
        // At most two lines' worth, however long the log.
        assert!(given.iter().all(|g| *g <= 2 * 70_100), "{given:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        ga.inner
            .renewer
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .unwrap();
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

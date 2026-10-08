//! Reading a document (R-153): one model. A loader (`text`, `yaml`,
//! `toml`, `json`, `csv`) takes a location, and a location is a `uri`
//! whose scheme selects the transport, or a path from the project root,
//! which is `file:`. Emacs TRAMP's model: one location syntax carrying the
//! method, the user and the host; nothing remote is a separate thing to
//! configure, and a read is the same whatever the scheme.
//!
//! The scheme rule: a scheme is dform's when the host already has its
//! transport, and a provider's when its manifest declares it.
//!
//! | scheme                    | the transport                                              |
//! |---------------------------|------------------------------------------------------------|
//! | `file:`, a bare path      | the project's files                                        |
//! | `data:`                   | the bytes in the location itself (RFC 2397)                |
//! | `ssh://USER@HOST/PATH`    | SFTP, the host's SSH client ([`ssh`])                      |
//! | `https://`                | dform's HTTP client (`crate::http`)                        |
//! | `git+https://`, `git+ssh://` | a remote repository's mirror (`crate::git`), `?ref=`    |
//! | `git+file:`               | a local repository, in place, `?ref=`                      |
//! | `s3://BUCKET/KEY`         | the S3 client (crates/dform-s3, [`register`]ed by the CLI) |
//! | a provider's (`gs`, ..)   | the provider that declares it, through the host            |
//!
//! [`Files`] is one run's reader: the program's loaders call it, and so
//! does the host for a provider (`dform:host/io`), so there is one
//! mirror cache, one known-hosts store and one timeout policy. A provider
//! reads only what dform.toml grants it (`[providers.NAME] reads`, by
//! scheme and host pattern); the program reads what it names, as it reads
//! its own files. Credentials are by name (`[io] credentials`, a
//! location pattern to a credential), never in the program.
//!
//! A read the world has not reached yet is "not yet", not an error: an
//! `ssh://` host that does not answer or a file it has not written, an
//! `https://` or `s3://` object that is not there yet. The program's own
//! (`file:`, `data:`, a repository's file) is there or is an error.

#[cfg(not(target_family = "wasm"))]
pub mod ssh;

use crate::git::{Fetched, Git, Via};
use crate::plugin::host::{Error, Failure, Grants};
use crate::state::{KnownHost, State};
use crate::uri::Uri;
use crate::watch::Source;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// What a read that is "not yet" waits under, as its null's label
/// (`read.location/LOCATION#1`): the plan prints it as the location, and
/// `[io] wait` bounds the wait (`Manifest::provider_waits`' `read`).
pub const READ: &str = "read.location";

/// The schemes dform reads itself.
pub const SCHEMES: [&str; 7] = [
    "file",
    "data",
    "ssh",
    "https",
    "git+https",
    "git+ssh",
    "git+file",
];

/// What a source answers of a location: its bytes, and the version it
/// names them by when it keeps versions (a secret manager's version id,
/// R-172), which `?version=` on the same location reads again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    pub bytes: Vec<u8>,
    pub version: Option<String>,
}

impl Document {
    /// Bytes with no version.
    pub fn new(bytes: Vec<u8>) -> Document {
        Document {
            bytes,
            version: None,
        }
    }
}

/// A transport a scheme selects, beside dform's own: the S3 client's,
/// which the CLI registers ([`register`]), or a provider's.
pub trait Transport: Send + Sync {
    /// The bytes at `at`; a thing not there yet is `Failure::NotYet`.
    fn read(&self, at: &Uri, files: &Files) -> Result<Vec<u8>, Failure>;

    /// The bytes at `at` and their version, when the source keeps
    /// versions; by default [`Transport::read`]'s, unversioned.
    fn read_document(&self, at: &Uri, files: &Files) -> Result<Document, Failure> {
        self.read(at, files).map(Document::new)
    }
}

/// The version query a location pinned to a version carries
/// (`vault://kv/app?version=3#key`): what [`pinned`] writes.
pub const VERSION: &str = "version";

/// `at` pinned to `version`: its `?version=` set, so a read of it reads
/// that version again. How a versioned read's rows name where they are,
/// as a repository's name the commit.
pub fn pinned(at: &Uri, version: &str) -> String {
    let mut u = at.clone();
    let mut q: Vec<String> = u
        .query
        .as_deref()
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty() && p.split('=').next() != Some(VERSION))
        .map(str::to_string)
        .collect();
    q.push(format!(
        "{VERSION}={}",
        percent_encoding::utf8_percent_encode(version, percent_encoding::NON_ALPHANUMERIC)
    ));
    u.query = Some(q.join("&"));
    u.to_string()
}

/// The version a pinned location names (`?version=`), if it does.
pub fn version_of(at: &str) -> Option<String> {
    match location(at).ok()? {
        Location::Uri(u) => u.query_pairs().remove(VERSION),
        Location::Path(_) => None,
    }
}

/// Transports registered for the process, by scheme: `s3`.
fn registry() -> &'static Mutex<BTreeMap<String, Arc<dyn Transport>>> {
    static R: OnceLock<Mutex<BTreeMap<String, Arc<dyn Transport>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Read `scheme` with `t` in every run of this process (the CLI's `s3`).
pub fn register(scheme: &str, t: Arc<dyn Transport>) {
    let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
    r.insert(scheme.to_string(), t);
}

/// A location: a uri, or a path from the project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Path(String),
    Uri(Uri),
}

/// `text` as a location: `SCHEME:..` for a scheme dform or a provider
/// reads (or any `SCHEME://`), else a path.
pub fn location(text: &str) -> Result<Location, String> {
    let scheme = text.split_once(':').map(|(s, _)| s).filter(|s| {
        s.len() > 1
            && s.bytes().next().is_some_and(|c| c.is_ascii_alphabetic())
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
    });
    let Some(scheme) = scheme else {
        return Ok(Location::Path(text.to_string()));
    };
    let known = SCHEMES.contains(&scheme)
        || scheme == "http"
        || registry()
            .lock()
            .map(|r| r.contains_key(scheme))
            .unwrap_or(false);
    if !known && !text[scheme.len()..].starts_with("://") {
        return Ok(Location::Path(text.to_string()));
    }
    if scheme == "data" {
        // RFC 2397's data is not a uri's path: kept as written.
        return Ok(Location::Uri(Uri {
            scheme: "data".into(),
            user: None,
            password: None,
            host: None,
            port: None,
            path: text["data:".len()..].to_string(),
            query: None,
            fragment: None,
        }));
    }
    Uri::parse(text).map(Location::Uri)
}

/// What a read read.
#[derive(Debug, Clone)]
pub struct Read {
    pub bytes: Vec<u8>,
    /// How a row names where it is: the path as written, the location, or
    /// a repository's `REPO@COMMIT:PATH`, the whole commit (shown by its
    /// first seven digits, `tables::shown_at`).
    pub shown: String,
    /// What the controller watches.
    pub source: Source,
    /// The commit a repository was read at.
    pub commit: Option<String>,
    /// The version a source that keeps versions answered (R-172); `shown`
    /// is then the location pinned to it.
    pub version: Option<String>,
}

/// What a read came to.
#[derive(Debug, Clone)]
pub enum Outcome {
    Read(Read),
    /// Not there yet, and why.
    NotYet(String),
}

/// What dform.toml says of reading: `[io]`, and the s3 backends.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// `[io] credentials`: a location pattern and the credential its
    /// reads use, the longest pattern first.
    pub credentials: Vec<(String, String)>,
    /// The s3 backends' buckets, by name: their endpoint and region.
    pub buckets: BTreeMap<String, crate::store::S3Spec>,
}

impl Settings {
    pub fn of(m: Option<&crate::project::Manifest>) -> Settings {
        let Some(m) = m else {
            return Settings::default();
        };
        let mut credentials: Vec<(String, String)> =
            m.io.credentials
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
        credentials.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        Settings {
            credentials,
            buckets: m.buckets(),
        }
    }
}

/// `*` matches any run of characters; every other character itself.
pub fn matches(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < t.len() {
        if i < p.len() && p[i] == b'*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if i < p.len() && p[i] == t[j] {
            i += 1;
            j += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == b'*')
}

/// A location as a pattern matches it: scheme, host (and port), path; no
/// user, query or fragment.
pub fn pattern_text(u: &Uri) -> String {
    let host = u
        .host_ascii()
        .or_else(|| u.host.clone())
        .unwrap_or_default();
    let port = u.port.map(|p| format!(":{p}")).unwrap_or_default();
    format!("{}://{host}{port}{}", u.scheme, u.path)
}

/// The run's reader as a provider's host holds it (in its `Grants`):
/// none outside a run, where the host reads with a reader of its own.
#[derive(Clone, Default)]
pub struct Shared(pub Option<Arc<Files>>);

impl Shared {
    /// The run's reader, else a fresh one.
    pub fn get(&self) -> Arc<Files> {
        self.0.clone().unwrap_or_default()
    }
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Shared(run)"
        } else {
            "Shared(none)"
        })
    }
}

/// Every run's reader is the same as far as grants compare.
impl PartialEq for Shared {
    fn eq(&self, _: &Shared) -> bool {
        true
    }
}

impl Eq for Shared {}

/// A scheme a provider declares: the provider, and its reader.
type Declared = (String, Arc<dyn Transport>);

/// One run's reader.
pub struct Files {
    settings: Settings,
    /// SSH host keys: state's, and those met this run.
    known: Mutex<BTreeMap<String, KnownHost>>,
    git: Git,
    /// The schemes the run's providers declare.
    declared: Mutex<BTreeMap<String, Declared>>,
}

impl Default for Files {
    fn default() -> Files {
        Files::new(Settings::default(), BTreeMap::new())
    }
}

impl std::fmt::Debug for Files {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Files")
    }
}

impl Files {
    pub fn new(settings: Settings, known: BTreeMap<String, KnownHost>) -> Files {
        Files {
            settings,
            known: Mutex::new(known),
            git: Git::cache(),
            declared: Mutex::new(BTreeMap::new()),
        }
    }

    /// The host keys state knows, read once state is.
    pub fn know(&self, known: &BTreeMap<String, KnownHost>) {
        let mut k = self.known.lock().unwrap_or_else(|e| e.into_inner());
        for (h, v) in known {
            k.entry(h.clone()).or_insert_with(|| v.clone());
        }
    }

    /// Keep in `st` each host key met this run that it does not know yet.
    pub fn keep(&self, st: &mut State) {
        let k = self.known.lock().unwrap_or_else(|e| e.into_inner());
        for (h, v) in k.iter() {
            st.known_hosts.entry(h.clone()).or_insert_with(|| v.clone());
        }
    }

    /// `scheme` is read by the provider `provider` through `t`.
    pub fn declare(&self, scheme: &str, provider: &str, t: Arc<dyn Transport>) {
        let mut d = self.declared.lock().unwrap_or_else(|e| e.into_inner());
        d.insert(scheme.to_string(), (provider.to_string(), t));
    }

    /// The s3 bucket `name`'s backend settings, when dform.toml has one.
    pub fn bucket(&self, name: &str) -> Option<&crate::store::S3Spec> {
        self.settings.buckets.get(name)
    }

    /// The credential `[io] credentials` gives `u`: the longest pattern
    /// that matches.
    pub fn credential_for(&self, u: &Uri) -> Option<&str> {
        let text = pattern_text(u);
        self.settings
            .credentials
            .iter()
            .find(|(p, _)| matches(p, &text))
            .map(|(_, c)| c.as_str())
    }

    /// The program's read of `text`, a path from `base` or a uri.
    pub fn read(&self, text: &str, base: &Path) -> anyhow::Result<Outcome> {
        let loc = location(text).map_err(|e| anyhow::anyhow!(e))?;
        let u = match loc {
            Location::Path(p) => {
                let path = base.join(&p);
                let bytes = std::fs::read(&path).map_err(|e| anyhow::anyhow!("read {p}: {e}"))?;
                return Ok(Outcome::Read(Read {
                    bytes,
                    shown: p,
                    source: Source::File(path),
                    commit: None,
                    version: None,
                }));
            }
            Location::Uri(u) => u,
        };
        let shown = match u.scheme.as_str() {
            "data" => format!("data:{}", short_data(&u.path)),
            _ => u.to_string(),
        };
        match u.scheme.as_str() {
            "file" => {
                let p = decoded(&u.path);
                let path = match Path::new(&p).is_absolute() {
                    true => PathBuf::from(&p),
                    false => base.join(&p),
                };
                let bytes =
                    std::fs::read(&path).map_err(|e| anyhow::anyhow!("read {text}: {e}"))?;
                Ok(Outcome::Read(Read {
                    bytes,
                    shown: p,
                    source: Source::File(path),
                    commit: None,
                    version: None,
                }))
            }
            "git+https" | "git+ssh" | "git+file" => self.git_read(&u, base).map(Outcome::Read),
            _ => match self.transport(&u) {
                Ok(Document { bytes, version }) => Ok(Outcome::Read(Read {
                    bytes,
                    source: Source::Location(u.to_string()),
                    shown: match &version {
                        Some(v) => pinned(&u, v),
                        None => shown,
                    },
                    commit: None,
                    version,
                })),
                Err(Failure::NotYet(why)) => Ok(Outcome::NotYet(why)),
                Err(Failure::Error(e)) => Err(anyhow::anyhow!("read {shown}: {}", e.message)),
            },
        }
    }

    /// A provider's read of `text` (`dform:host/io`): a uri its grants
    /// name by scheme and host pattern, through the same transports.
    pub fn read_for(&self, grants: &Grants, text: &str) -> Result<Vec<u8>, Failure> {
        self.read_for_document(grants, text).map(|d| d.bytes)
    }

    /// [`Files::read_for`], with the version a source that keeps versions
    /// answers (`dform:host/io`'s `read-versioned`).
    pub fn read_for_document(&self, grants: &Grants, text: &str) -> Result<Document, Failure> {
        let u = match location(text) {
            Ok(Location::Uri(u)) => u,
            Ok(Location::Path(p)) => {
                return Err(Error::fatal(format!(
                    "provider {}: {p:?} is not a location: a provider reads a uri \
                     (`https://..`, `ssh://USER@HOST/PATH`, `s3://BUCKET/KEY`)",
                    grants.provider
                ))
                .into());
            }
            Err(e) => return Err(Error::fatal(e).into()),
        };
        let what = pattern_text(&u);
        if !grants.reads.iter().any(|p| matches(p, &what)) {
            return Err(Error::fatal(format!(
                "provider {} is not granted a read of {what}: add a pattern that matches it \
                 to [providers.{}] reads in dform.toml (`reads = [\"{}://{}/*\"]`)",
                grants.provider,
                grants.provider,
                u.scheme,
                u.host.as_deref().unwrap_or_default()
            ))
            .into());
        }
        match u.scheme.as_str() {
            "file" | "git+file" => Err(Error::fatal(format!(
                "provider {}: {} is the project's: a provider reads no local file through the \
                 host",
                grants.provider, u
            ))
            .into()),
            "git+https" | "git+ssh" => self
                .git_read(&u, Path::new(""))
                .map(|r| Document::new(r.bytes))
                .map_err(|e| Error::fatal(format!("{e:#}")).into()),
            _ => self.transport(&u),
        }
    }

    /// `u` through the transport its scheme selects (not git's).
    fn transport(&self, u: &Uri) -> Result<Document, Failure> {
        match u.scheme.as_str() {
            "data" => data(&u.path)
                .map(Document::new)
                .map_err(|e| Error::fatal(e).into()),
            "http" => Err(Error::fatal(format!(
                "{u}: `http:` is not read (no TLS): write `https:`"
            ))
            .into()),
            #[cfg(not(target_family = "wasm"))]
            "ssh" => self.ssh(u).map(Document::new),
            #[cfg(not(target_family = "wasm"))]
            "https" => self.https(u).map(Document::new),
            // dform's own transports first (the CLI's `s3`), then the one a
            // provider declares.
            s => {
                let registered = registry()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(s)
                    .cloned();
                let declared = self
                    .declared
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(s)
                    .map(|(_, t)| t.clone());
                match registered.or(declared) {
                    Some(t) => t.read_document(u, self),
                    None => Err(Error::fatal(format!(
                        "{u}: no transport reads `{s}:` (dform reads {}, and a scheme a \
                         provider declares)",
                        SCHEMES
                            .iter()
                            .chain(["s3"].iter())
                            .map(|s| format!("{s}:"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                    .into()),
                }
            }
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn ssh(&self, u: &Uri) -> Result<Vec<u8>, Failure> {
        let t = ssh::Target::of(u)?;
        let named = self.ssh_key(u)?;
        ssh::read(&self.known, &t, named.as_deref(), &decoded(&u.path))
    }

    /// The key `[io] credentials` names for an ssh location: `ssh:NAME`
    /// is the key NAME (the agent's by comment or fingerprint, else the
    /// credential's file).
    #[cfg(not(target_family = "wasm"))]
    fn ssh_key(&self, u: &Uri) -> Result<Option<String>, Failure> {
        match self.credential_for(u) {
            None => Ok(None),
            Some(c) => match c.split_once(':') {
                Some(("ssh", n)) => Ok(Some(n.to_string())),
                _ => Err(Error::fatal(format!(
                    "{u}: [io] credentials names {c} for it, which is no SSH key: write \
                     ssh:NAME"
                ))
                .into()),
            },
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn https(&self, u: &Uri) -> Result<Vec<u8>, Failure> {
        let cred = match self.credential_for(u) {
            None => None,
            Some(c) => Some(
                crate::plugin::credentials::load(c)
                    .map_err(|e| Error::fatal(format!("{u}: the credential {c}: {e:#}")))?,
            ),
        };
        let url = u.ascii();
        let r = crate::http::send(
            crate::plugin::host::HttpRequest {
                method: "GET".into(),
                url: url.clone(),
                ..Default::default()
            },
            cred.as_ref(),
            None,
        )
        .map_err(Failure::Error)?;
        match r.status {
            200..=299 => Ok(r.body),
            404 | 410 => Err(Failure::NotYet(format!(
                "{u}: not there yet ({})",
                r.status
            ))),
            s => Err(Error::fatal(format!("{u}: the server answered {s}")).into()),
        }
    }

    /// A repository's file: `git+https://HOST/OWNER/REPO/PATH?ref=REF`,
    /// `git+ssh://USER@HOST/..`, `git+file:REPO/PATH?ref=REF`.
    fn git_read(&self, u: &Uri, base: &Path) -> anyhow::Result<Read> {
        let rev = u.query_pairs().remove("ref").ok_or_else(|| {
            anyhow::anyhow!("{u}: a repository's file is read at a ref: add `?ref=TAG` (a tag, a branch or a commit)")
        })?;
        let path = decoded(&u.path);
        if u.scheme == "git+file" {
            let (repo, file) = local_repo(base, &path)
                .ok_or_else(|| anyhow::anyhow!("{u}: no git repository holds {path}"))?;
            let shown_repo = repo
                .strip_prefix(base)
                .unwrap_or(&repo)
                .display()
                .to_string();
            let dir = repo.display().to_string();
            let commit = crate::git::resolve(&repo, &rev).map_err(|e| {
                anyhow::anyhow!(
                    "git repository {shown_repo}: ref {rev} does not name a commit ({e})"
                )
            })?;
            let bytes = self
                .git
                .read(&dir, &commit, &file)
                .map_err(|e| anyhow::anyhow!(e.message))?;
            return Ok(Read {
                version: None,
                bytes,
                shown: format!("{shown_repo}@{commit}:{file}"),
                source: Source::Git {
                    repo,
                    rev,
                    path: file,
                },
                commit: Some(commit),
            });
        }
        let (repo, file) = split_repo(&path)
            .ok_or_else(|| anyhow::anyhow!("{u}: no repository and file in {path:?}: write HOST/OWNER/REPO/PATH, or end the repository with `.git` or `//`"))?;
        let host = u
            .host_ascii()
            .or_else(|| u.host.clone())
            .unwrap_or_default();
        let port = u.port.map(|p| format!(":{p}")).unwrap_or_default();
        let shown_repo = format!("{host}{port}/{repo}");
        let Fetched { commit, data } = match u.scheme.as_str() {
            "git+https" => {
                let headers = match self.credential_for(u) {
                    None => Vec::new(),
                    Some(c) => crate::plugin::credentials::load(c)
                        .map_err(|e| anyhow::anyhow!("{u}: the credential {c}: {e:#}"))?
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v.expose()).into_owned()))
                        .collect(),
                };
                let url = format!("https://{host}{port}/{repo}");
                self.git
                    .read_remote(&url, &rev, &file, Via::Https { headers })
                    .map_err(|e| anyhow::anyhow!(e.message))?
            }
            _ => self.git_ssh(u, &host, &repo, &rev, &file)?,
        };
        let mirror = self
            .git
            .mirror(&format!("https://{host}/{repo}"))
            .unwrap_or_default();
        Ok(Read {
            version: None,
            bytes: data,
            shown: format!("{shown_repo}@{commit}:{file}"),
            source: Source::Git {
                repo: mirror,
                rev,
                path: file,
            },
            commit: Some(commit),
        })
    }

    #[cfg(not(target_family = "wasm"))]
    fn git_ssh(
        &self,
        u: &Uri,
        host: &str,
        repo: &str,
        rev: &str,
        file: &str,
    ) -> anyhow::Result<Fetched> {
        let t = ssh::Target::of(u).map_err(|e| anyhow::anyhow!("{e}"))?;
        let named = self.ssh_key(u).map_err(|e| anyhow::anyhow!("{e}"))?;
        let open = |cmd: &str| ssh::pipe(&self.known, &t, named.as_deref(), cmd);
        let port = u.port.map(|p| format!(":{p}")).unwrap_or_default();
        let user = u.user.clone().unwrap_or_else(ssh::local_user);
        let url = format!("ssh://{user}@{host}{port}/{repo}");
        self.git
            .read_remote(&url, rev, file, Via::Ssh(&open))
            .map_err(|e| anyhow::anyhow!(e.message))
    }

    #[cfg(target_family = "wasm")]
    fn git_ssh(&self, u: &Uri, _: &str, _: &str, _: &str, _: &str) -> anyhow::Result<Fetched> {
        anyhow::bail!("{u}: no SSH client in a wasm build of dform-core")
    }
}

/// A location's path, percent-decoded.
fn decoded(path: &str) -> String {
    percent_encoding::percent_decode_str(path)
        .decode_utf8_lossy()
        .into_owned()
}

/// A `data:` location's bytes (RFC 2397): `[MEDIATYPE][;base64],DATA`.
fn data(rest: &str) -> Result<Vec<u8>, String> {
    let (meta, body) = rest
        .split_once(',')
        .ok_or_else(|| "a data: location is data:[MEDIATYPE][;base64],DATA".to_string())?;
    match meta.ends_with(";base64") {
        true => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(body)
                .map_err(|e| format!("data: not base64: {e}"))
        }
        false => Ok(percent_encoding::percent_decode_str(body).collect()),
    }
}

/// A `data:` location as a row names it: its first bytes.
fn short_data(rest: &str) -> String {
    match rest.chars().count() > 24 {
        true => format!("{}..", rest.chars().take(24).collect::<String>()),
        false => rest.to_string(),
    }
}

/// A remote repository's path and the file's in `path` (`/OWNER/REPO/..`):
/// the repository ends at a segment ending `.git`, else at `//`, else
/// after two segments (`OWNER/REPO`, the forges' shape).
pub fn split_repo(path: &str) -> Option<(String, String)> {
    let path = path.trim_start_matches('/');
    if let Some((repo, file)) = path.split_once("//") {
        return Some((repo.to_string(), file.to_string()))
            .filter(|(r, f)| !r.is_empty() && !f.is_empty());
    }
    let segs: Vec<&str> = path.split('/').collect();
    let end = segs
        .iter()
        .position(|s| s.ends_with(".git"))
        .map(|i| i + 1)
        .unwrap_or(2);
    if segs.len() <= end {
        return None;
    }
    Some((segs[..end].join("/"), segs[end..].join("/")))
}

/// A local repository and the file in it: the shortest prefix of `path`
/// (from `base`) that is a repository.
fn local_repo(base: &Path, path: &str) -> Option<(PathBuf, String)> {
    let (repo, file) = match path.split_once("//") {
        Some((r, f)) => (r.to_string(), f.to_string()),
        None => {
            let segs: Vec<&str> = path.split('/').collect();
            let i = (1..segs.len()).find(|&i| {
                let p = base.join(segs[..i].join("/"));
                p.join(".git").exists() || (p.join("HEAD").is_file() && p.join("objects").is_dir())
            })?;
            (segs[..i].join("/"), segs[i..].join("/"))
        }
    };
    let repo = match Path::new(&repo).is_absolute() {
        true => PathBuf::from(&repo),
        false => base.join(&repo),
    };
    Some((repo, file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_location_is_a_uri_or_a_path() {
        assert_eq!(
            location("data/net.toml").unwrap(),
            Location::Path("data/net.toml".into())
        );
        assert_eq!(location("c:x").unwrap(), Location::Path("c:x".into()));
        let Location::Uri(u) = location("ssh://ubuntu@10.0.0.5/etc/k3s.yaml").unwrap() else {
            panic!()
        };
        assert_eq!(
            (u.scheme.as_str(), u.user.as_deref()),
            ("ssh", Some("ubuntu"))
        );
        let Location::Uri(u) = location("data:,a%20b").unwrap() else {
            panic!()
        };
        assert_eq!(data(&u.path).unwrap(), b"a b");
        assert_eq!(data(";base64,aGk=").unwrap(), b"hi");
    }

    #[test]
    fn a_repository_ends_at_git_or_a_double_slash_or_two_segments() {
        let s = |p| split_repo(p).unwrap();
        assert_eq!(
            s("/traefik/traefik/docs/content/crd.yml"),
            ("traefik/traefik".into(), "docs/content/crd.yml".into())
        );
        assert_eq!(
            s("/group/sub/ops.git/net.csv"),
            ("group/sub/ops.git".into(), "net.csv".into())
        );
        assert_eq!(
            s("/group/sub/ops//a/b.yml"),
            ("group/sub/ops".into(), "a/b.yml".into())
        );
        assert_eq!(split_repo("/acme/ops"), None);
    }

    #[test]
    fn a_pattern_matches_scheme_host_and_path() {
        assert!(matches("https://github.com/*", "https://github.com/a/b"));
        assert!(matches("ssh://*", "ssh://10.0.0.5/etc/x"));
        assert!(matches("s3://*.example/*", "s3://b.example/k"));
        assert!(!matches("https://github.com/*", "https://gitlab.com/a"));
        assert!(!matches("s3://bucket/*", "s3://bucket2/k"));
    }
}

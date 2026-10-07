//! Git in process, through gix (R-103; house rules: no shell-outs). No
//! `git`, `tar` or `ssh` is run, and the operator's git configuration is
//! not consulted (`open::Options::isolated`): a run reads the same thing
//! on every machine.
//!
//! Two uses:
//!
//! - The program's own repository: the commit it is at ([`head`]), the
//!   tracked files modified since ([`modified`]), whether nothing under a
//!   directory moved ([`clean`]), a file at a commit ([`show`]), a ref
//!   resolved ([`resolve`]), and the tree at a commit written into a
//!   scratch directory for `diff --since` ([`checkout`]).
//! - A remote repository a location names (`git+https://..`,
//!   `git+ssh://..`, `crate::files`): read from its mirror,
//!   `$XDG_CACHE_HOME/dform/git/<host>-<owner>/<repo>.git/` ([`mirror_dir`],
//!   R-103's layout: every segment before the repository joins the host
//!   with `-`), cloned once as a bare mirror and fetched (never re-cloned)
//!   on a later read that needs it, shared across projects and runs; a run
//!   holds a file lock on the mirror while it fetches and reads. A ref
//!   that is a commit the mirror holds, or a tag it holds, is read with no
//!   fetch (a tag does not move, as Go's module cache has it); any other
//!   ref is fetched first. Over https the transport is dform's one HTTP
//!   client (`crate::http`, through gix-transport's `Http` trait); over
//!   ssh it is the host's SSH client running `git-upload-pack` (a
//!   [`Pipe`] the caller opens). A local path is read in place, no copy.
//!
//! `commit` writes files onto a branch of a local repository. Pushing to
//! a remote is not done: gitoxide has no push yet (WORK.org R-13b).

use crate::plugin::host::{Error, GitFile};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn fail<E: std::fmt::Display>(what: impl std::fmt::Display) -> impl FnOnce(E) -> Error {
    move |e| Error::fatal(format!("{what}: {e}"))
}

/// The repository holding `dir`, opened isolated from the operator's git
/// configuration.
fn discover(dir: &Path) -> Option<gix::Repository> {
    let isolated = gix::open::Options::isolated();
    gix::ThreadSafeRepository::discover_opts(
        dir,
        Default::default(),
        gix::sec::trust::Mapping {
            full: isolated.clone(),
            reduced: isolated,
        },
    )
    .ok()
    .map(|r| r.to_thread_local())
}

/// `dir` relative to the work tree of `r`, `/`-separated (empty at its
/// root).
fn prefix(r: &gix::Repository, dir: &Path) -> String {
    let (Some(wd), Ok(dir)) = (r.workdir(), dir.canonicalize()) else {
        return String::new();
    };
    let wd = wd.canonicalize().unwrap_or_else(|_| wd.to_path_buf());
    dir.strip_prefix(&wd)
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default()
}

fn under(prefix: &str, path: &str) -> bool {
    prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// The commit `rev` names in `r`: a branch, a tag, an id, `HEAD~n`.
fn commit_of<'r>(r: &'r gix::Repository, rev: &str) -> Result<gix::Commit<'r>, String> {
    r.rev_parse_single(rev)
        .map_err(|e| e.to_string())?
        .object()
        .map_err(|e| e.to_string())?
        .peel_to_commit()
        .map_err(|e| e.to_string())
}

/// The commit the directory `dir` is at, when it is in a repository.
pub fn head(dir: &Path) -> Option<String> {
    let r = discover(dir)?;
    let id = r.head_id().ok()?;
    Some(id.to_string())
}

/// What `git status` says under `dir`: each changed path, from the
/// repository's root; untracked files too when `untracked`.
fn status(dir: &Path, untracked: bool) -> Option<Vec<String>> {
    let r = discover(dir)?;
    let prefix = prefix(&r, dir);
    let mode = match untracked {
        true => gix::status::UntrackedFiles::Files,
        false => gix::status::UntrackedFiles::None,
    };
    let iter = r
        .status(gix::progress::Discard)
        .ok()?
        .untracked_files(mode)
        .into_iter(None)
        .ok()?;
    let mut out = Vec::new();
    for item in iter {
        let item = item.ok()?;
        let path = item.location().to_string();
        if under(&prefix, &path) && !out.contains(&path) {
            out.push(path);
        }
    }
    out.sort();
    Some(out)
}

/// The tracked files under `dir` modified since the commit, from the
/// repository's root (`git status --untracked-files=no`): empty when the
/// tree is clean or not a repository.
pub fn modified(dir: &Path) -> Vec<String> {
    status(dir, false).unwrap_or_default()
}

/// Whether nothing under `dir` differs from the commit, untracked files
/// included (`git status -- .` is empty). False outside a repository.
pub fn clean(dir: &Path) -> bool {
    status(dir, true).is_some_and(|s| s.is_empty())
}

/// The commit `rev` names in the repository holding `dir`.
pub fn resolve(dir: &Path, rev: &str) -> anyhow::Result<String> {
    let r = discover(dir)
        .ok_or_else(|| anyhow::anyhow!("{} is not in a git repository", dir.display()))?;
    commit_of(&r, rev)
        .map(|c| c.id.to_string())
        .map_err(|e| anyhow::anyhow!("git {}: {rev}: {e}", dir.display()))
}

/// The file `rel` (relative to `dir`) at `commit` of the repository
/// holding `dir`.
pub fn show(dir: &Path, commit: &str, rel: &str) -> anyhow::Result<Vec<u8>> {
    let r = discover(dir)
        .ok_or_else(|| anyhow::anyhow!("{} is not in a git repository", dir.display()))?;
    let at = format!("git {} {commit}:{rel}", dir.display());
    let path = match prefix(&r, dir) {
        p if p.is_empty() => rel.to_string(),
        p => format!("{p}/{rel}"),
    };
    let tree = commit_of(&r, commit)
        .and_then(|c| c.tree().map_err(|e| e.to_string()))
        .map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
    blob(&tree, &path).map_err(|e| anyhow::anyhow!("{at}: {}", e.message))
}

/// The blob at `path` in `tree`.
fn blob(tree: &gix::Tree<'_>, path: &str) -> Result<Vec<u8>, Error> {
    let entry = tree
        .lookup_entry_by_path(path)
        .map_err(fail(path))?
        .ok_or_else(|| Error::fatal("no such file"))?;
    let obj = entry.object().map_err(fail(path))?;
    if obj.kind != gix::object::Kind::Blob {
        return Err(Error::fatal("not a file"));
    }
    Ok(obj.data.clone())
}

/// Write the tree under `dir` at `commit` into `dest`: what `git archive
/// COMMIT:./ | tar -x` did, with no archive and no tar. Files keep their
/// executable bit; a link is a link; a submodule is skipped.
pub fn checkout(dir: &Path, commit: &str, dest: &Path) -> anyhow::Result<()> {
    let r = discover(dir)
        .ok_or_else(|| anyhow::anyhow!("{} is not in a git repository", dir.display()))?;
    let at = format!("read the project at {commit}");
    let mut tree = commit_of(&r, commit)
        .and_then(|c| c.tree().map_err(|e| e.to_string()))
        .map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
    let p = prefix(&r, dir);
    if !p.is_empty() {
        let entry = tree
            .lookup_entry_by_path(&p)
            .map_err(|e| anyhow::anyhow!("{at}: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("{at}: {p} is not in that commit"))?;
        tree = entry
            .object()
            .map_err(|e| anyhow::anyhow!("{at}: {e}"))?
            .peel_to_tree()
            .map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
    }
    write_tree(&r, &tree, dest).map_err(|e| anyhow::anyhow!("{at}: {e}"))
}

fn write_tree(r: &gix::Repository, tree: &gix::Tree<'_>, dest: &Path) -> Result<(), String> {
    use gix::object::tree::EntryKind;
    std::fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    for e in tree.iter() {
        let e = e.map_err(|e| e.to_string())?;
        let to = dest.join(gix::path::from_bstr(e.filename()).as_ref());
        let obj = || r.find_object(e.oid()).map_err(|x| x.to_string());
        match e.mode().kind() {
            EntryKind::Tree => {
                let sub = obj()?.peel_to_tree().map_err(|x| x.to_string())?;
                write_tree(r, &sub, &to)?;
            }
            EntryKind::Blob | EntryKind::BlobExecutable => {
                std::fs::write(&to, &obj()?.data)
                    .map_err(|x| format!("write {}: {x}", to.display()))?;
                #[cfg(unix)]
                if e.mode().kind() == EntryKind::BlobExecutable {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755));
                }
            }
            EntryKind::Link => {
                let target = String::from_utf8_lossy(&obj()?.data).into_owned();
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &to)
                    .map_err(|x| format!("link {}: {x}", to.display()))?;
                #[cfg(not(unix))]
                let _ = target;
            }
            EntryKind::Commit => {}
        }
    }
    Ok(())
}

/// The mirror directory of `url` under the cache: `github.com-acme/ops.git`.
/// The scheme and the user go; an ssh url's `host:path` is `host/path`;
/// every segment before the repository joins the host with `-`.
pub fn mirror_dir(url: &str) -> Option<PathBuf> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    // `git@host:path` and `user@host/path`: the user goes, `:` is `/`.
    let rest = match rest.split_once('/') {
        Some((auth, path)) => format!("{}/{path}", auth.rsplit_once('@').map_or(auth, |(_, h)| h)),
        None => rest.rsplit_once('@').map_or(rest, |(_, r)| r).to_string(),
    };
    let rest = rest.replacen(':', "/", 1);
    let segs: Vec<&str> = rest
        .split('/')
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .collect();
    let (repo, owner) = segs.split_last()?;
    if owner.is_empty() {
        return None;
    }
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    Some(PathBuf::from(owner.join("-")).join(format!("{repo}.git")))
}

/// A URL (`https://github.com/acme/ops.git`, `git@github.com:acme/ops`),
/// else a path.
fn remote(repo: &str) -> bool {
    repo.contains("://") || (repo.contains('@') && repo.contains(':') && !Path::new(repo).exists())
}

/// A remote's reader and writer: `git-upload-pack` running on an SSH
/// host, its stdout and stdin.
pub struct Pipe {
    pub read: Box<dyn Read + Send>,
    pub write: Box<dyn Write + Send>,
}

/// How a remote repository is reached.
pub enum Via<'a> {
    /// Over https, with these headers (a credential's) on every request.
    Https { headers: Vec<(String, String)> },
    /// Over ssh: opens `git-upload-pack 'PATH'` on the host.
    Ssh(&'a dyn Fn(&str) -> Result<Pipe, Error>),
}

/// What a read of a remote repository read: the commit, and the blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub commit: String,
    pub data: Vec<u8>,
}

pub struct Git {
    /// Where mirrors live.
    cache: PathBuf,
}

impl Git {
    /// Mirrors under `$XDG_CACHE_HOME/dform/git` (`~/.cache/dform/git`).
    pub fn cache() -> Git {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .unwrap_or_else(std::env::temp_dir);
        Git::at(base.join("dform").join("git"))
    }

    pub fn at(cache: PathBuf) -> Git {
        Git { cache }
    }

    /// Where `url`'s mirror is.
    pub fn mirror(&self, url: &str) -> Result<PathBuf, Error> {
        Ok(self.cache.join(
            mirror_dir(url)
                .ok_or_else(|| Error::fatal(format!("git {url}: not a repository URL")))?,
        ))
    }

    /// A local repository, in place: the one holding the path.
    fn open_local(repo: &str) -> Result<gix::Repository, Error> {
        discover(Path::new(repo))
            .ok_or_else(|| Error::fatal(format!("git {repo}: not a git repository")))
    }

    /// The commit `rev` names: a branch, a tag or an id, in the repository
    /// or (a mirror's) as the remote's branch.
    fn resolve<'r>(
        r: &'r gix::Repository,
        repo: &str,
        rev: &str,
    ) -> Result<gix::Commit<'r>, Error> {
        commit_of(r, rev)
            .or_else(|_| commit_of(r, &format!("origin/{rev}")))
            .map_err(|e| Error::fatal(format!("git {repo}: no revision {rev:?}: {e}")))
    }

    /// The blob at `path` in the local repository `repo` at `rev`.
    pub fn read(&self, repo: &str, rev: &str, path: &str) -> Result<Vec<u8>, Error> {
        if remote(repo) {
            return Err(Error::fatal(format!(
                "git {repo}: a remote repository is read as a location, \
                 `git+https://HOST/OWNER/REPO/PATH?ref=REF` (dform:host/files)"
            )));
        }
        let r = Self::open_local(repo)?;
        let at = format!("git {repo} {rev}:{path}");
        let tree = Self::resolve(&r, repo, rev)?.tree().map_err(fail(&at))?;
        blob(&tree, path).map_err(|e| Error::fatal(format!("{at}: {}", e.message)))
    }

    /// The blob at `path` of the remote `url` at `rev`, through its mirror,
    /// fetched first unless `rev` is a commit or a tag the mirror holds.
    pub fn read_remote(
        &self,
        url: &str,
        rev: &str,
        path: &str,
        via: Via<'_>,
    ) -> Result<Fetched, Error> {
        let dir = self.mirror(url)?;
        let at = format!("git {url} {rev}:{path}");
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)
                .map_err(fail(format!("{at}: create {}", parent.display())))?;
        }
        // One run fetches a mirror at a time; a reader waits for it.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.with_extension("git.lock"))
            .map_err(fail(format!("{at}: lock the mirror")))?;
        lock.lock()
            .map_err(fail(format!("{at}: lock the mirror")))?;
        let r = match dir.exists() {
            true => gix::open_opts(&dir, gix::open::Options::isolated())
                .map_err(fail(format!("{at}: {}", dir.display())))?,
            false => gix::init_bare(&dir).map_err(fail(format!("{at}: {}", dir.display())))?,
        };
        let held = |r: &gix::Repository| {
            let commit = rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit());
            let name = match commit {
                true => rev.to_string(),
                false => format!("refs/tags/{rev}"),
            };
            commit_of(r, &name).ok().map(|c| c.id)
        };
        let id = match held(&r) {
            Some(id) => id,
            None => {
                fetch(&r, url, via).map_err(|e| Error::fatal(format!("{at}: fetch: {e}")))?;
                let r = gix::open_opts(&dir, gix::open::Options::isolated())
                    .map_err(fail(format!("{at}: {}", dir.display())))?;
                let found = [
                    format!("refs/tags/{rev}"),
                    format!("refs/heads/{rev}"),
                    rev.to_string(),
                ]
                .iter()
                .find_map(|n| commit_of(&r, n).ok().map(|c| c.id));
                found.ok_or_else(|| {
                    Error::fatal(format!(
                        "{at}: the repository has no branch, tag or commit {rev:?}"
                    ))
                })?
            }
        };
        let r = gix::open_opts(&dir, gix::open::Options::isolated())
            .map_err(fail(format!("{at}: {}", dir.display())))?;
        let tree = r
            .find_object(id)
            .map_err(fail(&at))?
            .peel_to_commit()
            .map_err(fail(&at))?
            .tree()
            .map_err(fail(&at))?;
        let data = blob(&tree, path).map_err(|e| Error::fatal(format!("{at}: {}", e.message)))?;
        drop(lock);
        Ok(Fetched {
            commit: id.to_string(),
            data,
        })
    }

    pub fn commit(
        &self,
        repo: &str,
        branch: &str,
        files: Vec<GitFile>,
        message: &str,
    ) -> Result<String, Error> {
        if remote(repo) {
            return Err(Error::fatal(format!(
                "git commit to {repo}: pushing is not available (gitoxide has no push yet): \
                 commit to a local repository path"
            )));
        }
        let r = Self::open_local(repo)?;
        let at = format!("git commit {repo} {branch}");
        let refname = format!("refs/heads/{branch}");
        let parent = Self::resolve(&r, repo, &refname)?;
        let mut ed = r
            .edit_tree(parent.tree_id().map_err(fail(&at))?)
            .map_err(fail(&at))?;
        for f in files {
            let blob = r.write_blob(&f.data).map_err(fail(&at))?;
            ed.upsert(f.path.as_str(), gix::object::tree::EntryKind::Blob, blob)
                .map_err(fail(format!("{at}: {}", f.path)))?;
        }
        let tree = ed.write().map_err(fail(&at))?;
        let time = gix::date::Time::now_utc();
        let mut buf = gix::date::parse::TimeBuf::default();
        let sig = gix::actor::SignatureRef {
            name: "dform".into(),
            email: "dform@localhost".into(),
            time: time.to_str(&mut buf),
        };
        let id = r
            .commit_as(sig, sig, refname.as_str(), message, tree, [parent.id])
            .map_err(fail(&at))?;
        Ok(id.to_string())
    }
}

/// The refs a mirror keeps: every branch and tag, as the remote has them.
const MIRROR: [&str; 2] = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];

/// Fetch every branch and tag of `url` into the mirror `r`.
fn fetch(r: &gix::Repository, url: &str, via: Via<'_>) -> Result<(), String> {
    use gix::protocol::transport::Protocol;
    use gix::protocol::transport::client::git::ConnectMode;
    let remote = r
        .remote_at(url)
        .map_err(|e| e.to_string())?
        .with_refspecs(MIRROR, gix::remote::Direction::Fetch)
        .map_err(|e| e.to_string())?;
    let parsed = gix::url::parse(gix::bstr::BStr::new(url)).map_err(|e| e.to_string())?;
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    // A remote asking for a password is refused, never prompted for: the
    // credential is the operator's, by name, in dform.toml.
    let refuse =
        |_: gix::credentials::helper::Action| -> gix::credentials::protocol::Result { Ok(None) };
    macro_rules! go {
        ($transport:expr) => {{
            remote
                .to_connection_with_transport($transport)
                .with_credentials(refuse)
                .prepare_fetch(gix::progress::Discard, Default::default())
                .map_err(|e| e.to_string())?
                .receive(gix::progress::Discard, &interrupt)
                .map_err(|e| e.to_string())?;
        }};
    }
    match via {
        Via::Https { headers } => {
            let client = Client {
                agent: crate::http::agent(None, None, crate::http::TIMEOUT)
                    .map_err(|e| e.message)?,
                headers,
            };
            go!(
                gix::protocol::transport::client::blocking_io::http::connect_http(
                    client,
                    parsed,
                    Protocol::V2,
                    false,
                )
            )
        }
        Via::Ssh(open) => {
            let path = parsed.path.to_string();
            let pipe = open(&format!(
                "git-upload-pack '{}'",
                path.replace('\'', "'\\''")
            ))
            .map_err(|e| e.message)?;
            go!(
                gix::protocol::transport::client::git::blocking_io::Connection::new(
                    std::io::BufReader::new(pipe.read),
                    pipe.write,
                    Protocol::V1,
                    path,
                    None::<(String, Option<u16>)>,
                    ConnectMode::Process,
                    false,
                )
            )
        }
    }
    Ok(())
}

/// gix-transport's HTTP over dform's client (`crate::http`): a GET at
/// once, a POST once its body is written (gix drops the body's writer
/// before it reads the answer). A 401 is `PermissionDenied`, as the trait
/// asks; any other status that is no success is an error naming it.
struct Client {
    agent: ureq::Agent,
    headers: Vec<(String, String)>,
}

/// An answer's headers, as lines, and its body.
type Answer = (Vec<u8>, Box<dyn Read + Send>);

/// A request, its body as written so far, and its answer once sent.
#[derive(Default)]
struct Pending {
    request: Option<ureq::http::request::Builder>,
    body: Vec<u8>,
    answer: Option<std::io::Result<Answer>>,
    agent: Option<ureq::Agent>,
}

type Shared = Arc<Mutex<Pending>>;

impl Pending {
    /// Send the request, once.
    fn send(&mut self) {
        if self.answer.is_some() {
            return;
        }
        let (Some(b), Some(agent)) = (self.request.take(), self.agent.take()) else {
            self.answer = Some(Err(std::io::Error::other("internal: no request")));
            return;
        };
        let body = std::mem::take(&mut self.body);
        let answer = b
            .body(body)
            .map_err(std::io::Error::other)
            .and_then(|req| agent.run(req).map_err(std::io::Error::other))
            .and_then(|resp| {
                let status = resp.status();
                let header_lines = |resp: &ureq::http::Response<ureq::Body>| {
                    let mut h = Vec::new();
                    for (k, v) in resp.headers() {
                        h.extend_from_slice(k.as_str().as_bytes());
                        h.push(b':');
                        h.extend_from_slice(v.as_bytes());
                        h.push(b'\n');
                    }
                    h
                };
                if status.as_u16() == 401 {
                    let www_authenticate = resp
                        .headers()
                        .get_all("www-authenticate")
                        .iter()
                        .map(|v| v.as_bytes().into())
                        .collect();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        gix::protocol::transport::client::AuthenticationRequired {
                            www_authenticate,
                        },
                    ));
                }
                if !status.is_success() {
                    return Err(std::io::Error::other(format!(
                        "Received HTTP status {}",
                        status.as_u16()
                    )));
                }
                let headers = header_lines(&resp);
                let body: Box<dyn Read + Send> = Box::new(resp.into_body().into_reader());
                Ok((headers, body))
            });
        self.answer = Some(answer);
    }
}

/// A part of an answer, read once the request is sent.
struct Part {
    shared: Shared,
    headers: bool,
    inner: Option<std::io::BufReader<Box<dyn Read + Send>>>,
}

impl Part {
    fn inner(&mut self) -> std::io::Result<&mut std::io::BufReader<Box<dyn Read + Send>>> {
        if self.inner.is_none() {
            let mut p = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            p.send();
            let r: Box<dyn Read + Send> = match p.answer.as_mut() {
                Some(Ok((h, b))) => match self.headers {
                    true => Box::new(std::io::Cursor::new(std::mem::take(h))),
                    false => std::mem::replace(b, Box::new(std::io::empty())),
                },
                Some(Err(e)) => return Err(std::io::Error::new(e.kind(), e.to_string())),
                None => return Err(std::io::Error::other("internal: no answer")),
            };
            self.inner = Some(std::io::BufReader::new(r));
        }
        Ok(self.inner.as_mut().expect("set above"))
    }
}

impl Read for Part {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner()?.read(buf)
    }
}

impl BufRead for Part {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.inner()?.fill_buf()
    }

    fn consume(&mut self, n: usize) {
        if let Some(i) = self.inner.as_mut() {
            i.consume(n);
        }
    }
}

/// A POST's body, written before its answer is read.
struct Body(Shared);

impl Write for Body {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut p = self.0.lock().unwrap_or_else(|e| e.into_inner());
        p.body.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Client {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Shared {
        let mut b = ureq::http::Request::builder().method(method).uri(url);
        for line in headers {
            if let Some((k, v)) = line.as_ref().split_once(':') {
                b = b.header(k.trim(), v.trim());
            }
        }
        for (k, v) in &self.headers {
            b = b.header(k.as_str(), v.as_str());
        }
        Arc::new(Mutex::new(Pending {
            request: Some(b),
            agent: Some(self.agent.clone()),
            ..Pending::default()
        }))
    }

    fn parts(shared: &Shared) -> (Part, Part) {
        let part = |headers| Part {
            shared: shared.clone(),
            headers,
            inner: None,
        };
        (part(true), part(false))
    }
}

use gix::protocol::transport::client::blocking_io::http as gh;

impl gh::Http for Client {
    type Headers = Part;
    type ResponseBody = Part;
    type PostBody = Body;

    fn get(
        &mut self,
        url: &str,
        _base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> gix::ExnMessageResult<gh::GetResponse<Part, Part>> {
        let shared = self.request("GET", url, headers);
        let (headers, body) = Client::parts(&shared);
        Ok(gh::GetResponse { headers, body })
    }

    fn post(
        &mut self,
        url: &str,
        _base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        _body: gh::PostBodyDataKind,
    ) -> gix::ExnMessageResult<gh::PostResponse<Part, Part, Body>> {
        let shared = self.request("POST", url, headers);
        let (headers, body) = Client::parts(&shared);
        Ok(gh::PostResponse {
            post_body: Body(shared),
            headers,
            body,
        })
    }

    fn configure(&mut self, _config: &dyn std::any::Any) -> gix::ExnResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_are_host_owner_then_repo() {
        let m = |u| mirror_dir(u).map(|p| p.display().to_string());
        assert_eq!(
            m("https://github.com/acme/ops.git").as_deref(),
            Some("github.com-acme/ops.git")
        );
        assert_eq!(
            m("git@gitlab.com:group/sub/ops").as_deref(),
            Some("gitlab.com-group-sub/ops.git")
        );
        assert_eq!(
            m("ssh://git@host.example:2222/a/b.git").as_deref(),
            Some("host.example-2222-a/b.git")
        );
        assert_eq!(m("https://host"), None);
    }

    fn sig_commit(
        r: &gix::Repository,
        tree: gix::ObjectId,
        parents: Vec<gix::ObjectId>,
    ) -> gix::ObjectId {
        let time = gix::date::Time::now_utc();
        let mut buf = gix::date::parse::TimeBuf::default();
        let sig = gix::actor::SignatureRef {
            name: "t".into(),
            email: "t@t".into(),
            time: time.to_str(&mut buf),
        };
        r.commit_as(sig, sig, "refs/heads/main", "c", tree, parents)
            .unwrap()
            .detach()
    }

    /// A commit onto a local repository's branch, read back at the branch.
    #[test]
    fn a_local_repository_commits_and_reads() {
        let dir = std::env::temp_dir().join(format!("dform-core-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let r = gix::init_bare(&dir).unwrap();
        let empty = gix::ObjectId::empty_tree(gix::hash::Kind::Sha1);
        r.write_object(gix::objs::Tree::empty()).unwrap();
        sig_commit(&r, empty, vec![]);
        let g = Git::at(dir.join("cache"));
        let repo = dir.display().to_string();
        let id = g
            .commit(
                &repo,
                "main",
                vec![GitFile {
                    path: "a/b.txt".into(),
                    data: b"hello".to_vec(),
                }],
                "add b",
            )
            .unwrap();
        assert_eq!(id.len(), 40);
        assert_eq!(g.read(&repo, "main", "a/b.txt").unwrap(), b"hello");
        assert_eq!(g.read(&repo, &id, "a/b.txt").unwrap(), b"hello");
        let e = g.read(&repo, "main", "nope").unwrap_err();
        assert!(e.message.contains("no such file"), "{}", e.message);
        let e = g
            .commit("https://example.com/a/b.git", "main", vec![], "x")
            .unwrap_err();
        assert!(e.message.contains("no push"), "{}", e.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tag the mirror holds is read with no fetch (it does not move); a
    /// branch is fetched first, and a host that is not there fails the
    /// fetch, naming it.
    #[test]
    fn a_held_tag_reads_without_a_fetch() {
        let dir = std::env::temp_dir().join(format!("dform-core-mirror-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let g = Git::at(dir.clone());
        let url = "https://git.invalid/acme/ops";
        let mirror = g.mirror(url).unwrap();
        assert!(mirror.ends_with("git.invalid-acme/ops.git"));
        std::fs::create_dir_all(&mirror).unwrap();
        let r = gix::init_bare(&mirror).unwrap();
        let blob = r.write_blob(b"kind: X\n").unwrap().detach();
        let mut ed = r
            .edit_tree(gix::ObjectId::empty_tree(gix::hash::Kind::Sha1))
            .unwrap();
        ed.upsert("docs/x.yml", gix::object::tree::EntryKind::Blob, blob)
            .unwrap();
        let tree = ed.write().unwrap().detach();
        let c = sig_commit(&r, tree, vec![]);
        r.reference(
            "refs/tags/v1.0.0",
            c,
            gix::refs::transaction::PreviousValue::Any,
            "tag",
        )
        .unwrap();
        let read = g
            .read_remote(url, "v1.0.0", "docs/x.yml", Via::Https { headers: vec![] })
            .unwrap();
        assert_eq!(read.data, b"kind: X\n");
        assert_eq!(read.commit, c.to_string());
        let e = g
            .read_remote(url, "main", "docs/x.yml", Via::Https { headers: vec![] })
            .unwrap_err();
        assert!(e.message.contains("fetch"), "{}", e.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A smart-HTTP git server on the loopback for one repository root:
    /// each request handed to `git http-backend` (a test may run git; the
    /// product never does). Serves until the test ends.
    fn http_backend(root: &Path) -> Option<u16> {
        use std::io::{BufRead, BufReader, Read, Write};
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .ok()?;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(s) = s else { return };
                let root = root.clone();
                std::thread::spawn(move || {
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let target = parts.next().unwrap_or_default().to_string();
                    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                    let mut headers = Vec::new();
                    loop {
                        let mut h = String::new();
                        r.read_line(&mut h).unwrap();
                        if h.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':') {
                            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                        }
                    }
                    let get = |k: &str| {
                        headers
                            .iter()
                            .find(|(n, _)| n == k)
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default()
                    };
                    let len: usize = get("content-length").parse().unwrap_or(0);
                    let mut body = vec![0; len];
                    r.read_exact(&mut body).unwrap();
                    let mut c = std::process::Command::new("git")
                        .arg("http-backend")
                        .env("GIT_PROJECT_ROOT", &root)
                        .env("GIT_HTTP_EXPORT_ALL", "1")
                        .env("REQUEST_METHOD", &method)
                        .env("PATH_INFO", path)
                        .env("QUERY_STRING", query)
                        .env("CONTENT_TYPE", get("content-type"))
                        .env("CONTENT_LENGTH", len.to_string())
                        .env("GIT_PROTOCOL", get("git-protocol"))
                        .env("REMOTE_ADDR", "127.0.0.1")
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .spawn()
                        .unwrap();
                    c.stdin.take().unwrap().write_all(&body).unwrap();
                    let out = c.wait_with_output().unwrap().stdout;
                    let split = out
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|i| (i, 4));
                    let split =
                        split.or_else(|| out.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)));
                    let (i, n) = split.unwrap_or((0, 0));
                    let head = String::from_utf8_lossy(&out[..i]).into_owned();
                    let rest = &out[i + n..];
                    let mut status = "200 OK".to_string();
                    let mut s = s;
                    let mut lines = Vec::new();
                    for h in head.lines() {
                        match h.strip_prefix("Status: ") {
                            Some(st) => status = st.to_string(),
                            None => lines.push(h.to_string()),
                        }
                    }
                    write!(s, "HTTP/1.1 {status}\r\n").unwrap();
                    for h in lines {
                        write!(s, "{h}\r\n").unwrap();
                    }
                    write!(
                        s,
                        "content-length: {}\r\nconnection: close\r\n\r\n",
                        rest.len()
                    )
                    .unwrap();
                    s.write_all(rest).unwrap();
                });
            }
        });
        Some(port)
    }

    /// A branch is fetched into the mirror over HTTP (dform's client as
    /// gix's transport), and a later read of a moved branch fetches again.
    #[test]
    fn a_branch_is_fetched_over_http() {
        let dir = std::env::temp_dir().join(format!("dform-core-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let served = dir.join("served");
        std::fs::create_dir_all(served.join("acme")).unwrap();
        let repo = served.join("acme/ops.git");
        let r = gix::init_bare(&repo).unwrap();
        let empty = gix::ObjectId::empty_tree(gix::hash::Kind::Sha1);
        r.write_object(gix::objs::Tree::empty()).unwrap();
        sig_commit(&r, empty, vec![]);
        let g = Git::at(dir.join("cache"));
        let local = repo.display().to_string();
        let file = |text: &str| GitFile {
            path: "docs/x.yml".into(),
            data: text.as_bytes().to_vec(),
        };
        let first = g.commit(&local, "main", vec![file("one")], "1").unwrap();
        let Some(port) = http_backend(&served) else {
            eprintln!("skipped: no git to serve with");
            return;
        };
        let url = format!("http://127.0.0.1:{port}/acme/ops.git");
        let via = || Via::Https { headers: vec![] };
        let read = g.read_remote(&url, "main", "docs/x.yml", via()).unwrap();
        assert_eq!(
            (read.commit.as_str(), read.data.as_slice()),
            (first.as_str(), &b"one"[..])
        );
        assert!(g.mirror(&url).unwrap().join("refs").exists());
        let second = g.commit(&local, "main", vec![file("two")], "2").unwrap();
        let read = g.read_remote(&url, "main", "docs/x.yml", via()).unwrap();
        assert_eq!(
            (read.commit.as_str(), read.data.as_slice()),
            (second.as_str(), &b"two"[..])
        );
        // The first commit, held, is read without asking.
        let read = g.read_remote(&url, &first, "docs/x.yml", via()).unwrap();
        assert_eq!(read.data, b"one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The program's repository: its head, what is modified and whether
    /// it is clean (untracked files count), a file at a commit, and the
    /// tree under a directory written out at a commit (`diff --since`).
    #[test]
    fn a_work_tree_is_read_in_process() {
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
        };
        let dir = std::env::temp_dir().join(format!("dform-core-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj/data")).unwrap();
        if git(&dir, &["init", "-q", "-b", "main"]).is_none() {
            eprintln!("skipped: no git to make the fixture with");
            return;
        }
        std::fs::write(dir.join("proj/a.df"), "a\n").unwrap();
        std::fs::write(dir.join("proj/data/b.yaml"), "b: 1\n").unwrap();
        std::fs::write(dir.join("top.txt"), "t\n").unwrap();
        git(&dir, &["add", "."]).unwrap();
        git(&dir, &["commit", "-q", "-m", "one"]).unwrap();
        let proj = dir.join("proj");
        let c = head(&proj).unwrap();
        assert_eq!(c, resolve(&proj, "HEAD").unwrap());
        assert!(clean(&proj) && modified(&proj).is_empty());
        std::fs::write(dir.join("proj/a.df"), "a2\n").unwrap();
        std::fs::write(dir.join("top.txt"), "t2\n").unwrap();
        assert_eq!(modified(&proj), vec!["proj/a.df".to_string()]);
        assert!(!clean(&proj));
        git(&dir, &["checkout", "-q", "--", "."]).unwrap();
        std::fs::write(dir.join("proj/new.df"), "n\n").unwrap();
        assert!(modified(&proj).is_empty() && !clean(&proj));
        assert_eq!(show(&proj, &c, "data/b.yaml").unwrap(), b"b: 1\n");
        let out = dir.join("out");
        checkout(&proj, &c, &out).unwrap();
        assert_eq!(std::fs::read(out.join("a.df")).unwrap(), b"a\n");
        assert_eq!(std::fs::read(out.join("data/b.yaml")).unwrap(), b"b: 1\n");
        assert!(!out.join("top.txt").exists() && !out.join("new.df").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

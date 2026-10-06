//! `dform:host/git`, through gix (no shell-outs). A remote repository is
//! read from its mirror, `$XDG_CACHE_HOME/dform/git/<host>-<owner>/<repo>.git/`
//! (R-103's layout: every segment before the repository joins the host
//! with `-`); a local path is read in place. Cloning and fetching the
//! mirror is not done here yet: gix's HTTP transport (reqwest) links a
//! second rustls crypto provider (aws-lc-rs) into every binary of the
//! workspace, and kube then cannot pick one. R-103 brings the fetch, over
//! a transport on the host's own HTTP client.
//! Configuration is isolated: the operator's git installation is not
//! consulted, and no `git` is run.
//!
//! `commit` writes the files onto the branch of a local repository.
//! Pushing to a remote is not done: gitoxide has no push yet, so a commit
//! to a remote repository is refused naming that (WORK.org R-13b's report).

use dform_core::plugin::host::{Error, GitFile};
use std::path::{Path, PathBuf};

pub struct Git {
    /// Where mirrors live.
    cache: PathBuf,
}

fn fail<E: std::fmt::Display>(what: impl std::fmt::Display) -> impl FnOnce(E) -> Error {
    move |e| Error::fatal(format!("{what}: {e}"))
}

/// A URL (`https://github.com/acme/ops.git`, `git@github.com:acme/ops`),
/// else a path.
fn remote(repo: &str) -> bool {
    repo.contains("://") || (repo.contains('@') && repo.contains(':') && !Path::new(repo).exists())
}

/// The mirror directory of `url` under the cache: `github.com-acme/ops.git`.
pub fn mirror_dir(url: &str) -> Option<PathBuf> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    // `git@host:path` and `user@host/path`: the user goes, `:` is `/`.
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
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

    /// `repo` opened: in place, or its mirror.
    fn open(&self, repo: &str) -> Result<gix::Repository, Error> {
        if !remote(repo) {
            return gix::open_opts(repo, gix::open::Options::isolated())
                .map_err(|e| Error::fatal(format!("git {repo}: {e}")));
        }
        let dir = self.cache.join(
            mirror_dir(repo)
                .ok_or_else(|| Error::fatal(format!("git {repo}: not a repository URL")))?,
        );
        if !dir.exists() {
            return Err(Error::fatal(format!(
                "git {repo}: a remote repository is read from its mirror, {}, which is not \
                 there: the host does not fetch yet (gix's HTTP transport links a TLS stack of \
                 its own; the mirror and its fetch are R-103's)",
                dir.display()
            )));
        }
        gix::open_opts(&dir, gix::open::Options::isolated())
            .map_err(|e| Error::fatal(format!("git {repo}: {}: {e}", dir.display())))
    }

    /// The commit `rev` names: a branch, a tag or an id, in the repository
    /// or (a mirror's) as the remote's branch.
    fn resolve<'r>(
        r: &'r gix::Repository,
        repo: &str,
        rev: &str,
    ) -> Result<gix::Commit<'r>, Error> {
        let id = r
            .rev_parse_single(rev)
            .or_else(|_| r.rev_parse_single(format!("origin/{rev}").as_str()))
            .map_err(fail(format!("git {repo}: no revision {rev:?}")))?;
        id.object()
            .map_err(fail(format!("git {repo} {rev}")))?
            .peel_to_commit()
            .map_err(fail(format!("git {repo} {rev}")))
    }

    pub fn read(&self, repo: &str, rev: &str, path: &str) -> Result<Vec<u8>, Error> {
        let r = self.open(repo)?;
        let at = format!("git {repo} {rev}:{path}");
        let tree = Self::resolve(&r, repo, rev)?.tree().map_err(fail(&at))?;
        let entry = tree
            .lookup_entry_by_path(path)
            .map_err(fail(&at))?
            .ok_or_else(|| Error::fatal(format!("{at}: no such file")))?;
        let obj = entry.object().map_err(fail(&at))?;
        if obj.kind != gix::object::Kind::Blob {
            return Err(Error::fatal(format!("{at}: not a file")));
        }
        Ok(obj.data.clone())
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
        let r = self.open(repo)?;
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
        assert_eq!(m("https://host"), None);
    }

    /// A commit onto a local repository's branch, read back at the branch.
    #[test]
    fn a_local_repository_commits_and_reads() {
        let dir = std::env::temp_dir().join(format!("dform-host-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let r = gix::init_bare(&dir).unwrap();
        // An empty first commit on main.
        let empty = gix::ObjectId::empty_tree(gix::hash::Kind::Sha1);
        r.write_object(gix::objs::Tree::empty()).unwrap();
        let time = gix::date::Time::now_utc();
        let mut buf = gix::date::parse::TimeBuf::default();
        let sig = gix::actor::SignatureRef {
            name: "t".into(),
            email: "t@t".into(),
            time: time.to_str(&mut buf),
        };
        r.commit_as(
            sig,
            sig,
            "refs/heads/main",
            "init",
            empty,
            None::<gix::ObjectId>,
        )
        .unwrap();
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
    }
}

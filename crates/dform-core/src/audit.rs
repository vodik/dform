//! The audit log (E DR-16, "the log is audit", made real): one JSON-lines
//! file per stack deployment beside its state, `state.audit.jsonl`
//! (`<stem>.state.audit.jsonl` beside a `--world` file), moving with the
//! state on a rekey or a handover.
//!
//! Every entry is one line of canonical JSON: `seq` (from 1), `time` (RFC
//! 3339, UTC), `kind`, `prev` (the previous entry's `hash`, "" for the
//! first), the kind's fields, and `hash`, sha256 over the entry's canonical
//! JSON without `hash`. Editing an entry breaks its own hash; removing or
//! reordering one breaks the next one's `prev`; [`verify`] names the first
//! broken link.
//!
//! Kinds: `plan` (digest, inputs, git commits, who), `approval` (the
//! verified statement and the token, or `not required`, or why it was
//! refused), `apply_start` (who, dform's version, the providers),
//! `action` (kind, address, result, remote id, a digest of the redacted
//! diff), `tick` (a digest of the world as the executor saw it),
//! `apply_end`, `controller` (events, holds, releases), `rekey` and
//! `handover`. Values are never written: a diff is a digest of its redacted
//! form, where a sensitive leaf is already the stack's HMAC of it.
//!
//! Nothing reads the log as truth (DR-16): plan and apply never look at it.
//! A sink (`--audit-sink CMD`, or the stack's `audit_sink = "CMD"`) gets
//! each entry as a JSON line on its stdin (`sh -c CMD`, once per entry);
//! a sink that fails is a warning, and the local log stays authoritative.

use crate::approval::{canonical_json, digest_of, now, rfc3339};
use anyhow::{Context, Result};
use serde_json::{Map, Value as Json};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

/// A deployment's audit log.
#[derive(Debug, Clone)]
pub struct Log {
    path: PathBuf,
    sink: Option<String>,
}

/// The log beside the state file `state`.
pub fn path_beside(state: &Path) -> PathBuf {
    state.with_extension("audit.jsonl")
}

impl Log {
    /// The log of the deployment whose state file is `state`; each entry
    /// also goes to `sink` when there is one.
    pub fn beside(state: &Path, sink: Option<String>) -> Log {
        Log {
            path: path_beside(state),
            sink,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append an entry of `kind` with `fields` (an object), chained to the
    /// last one. The file is locked while it is read and written, so two
    /// processes appending (a `plan --out` beside an apply) do not fork
    /// the chain.
    pub fn append(&self, kind: &str, fields: Json) -> Result<()> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))?;
        f.lock()
            .with_context(|| format!("lock {}", self.path.display()))?;
        let mut text = String::new();
        f.seek(std::io::SeekFrom::Start(0))?;
        f.read_to_string(&mut text)
            .with_context(|| format!("read {}", self.path.display()))?;
        let (seq, prev) = match text.lines().rev().find(|l| !l.trim().is_empty()) {
            None => (1, String::new()),
            Some(last) => match serde_json::from_str::<Json>(last) {
                Ok(e) => (
                    e["seq"].as_u64().unwrap_or(0) + 1,
                    e["hash"].as_str().unwrap_or_default().to_string(),
                ),
                Err(_) => (
                    text.lines().filter(|l| !l.trim().is_empty()).count() as u64 + 1,
                    format!("sha256:{}", crate::approval::sha256_hex(last.as_bytes())),
                ),
            },
        };
        let mut entry = Map::new();
        entry.insert("seq".into(), seq.into());
        entry.insert("time".into(), rfc3339(now()).into());
        entry.insert("kind".into(), kind.into());
        entry.insert("prev".into(), prev.into());
        if let Json::Object(m) = fields {
            for (k, v) in m {
                entry.entry(k).or_insert(v);
            }
        }
        let hash = digest_of(&Json::Object(entry.clone()));
        entry.insert("hash".into(), hash.into());
        let line = canonical_json(&Json::Object(entry));
        f.write_all(format!("{line}\n").as_bytes())
            .with_context(|| format!("write {}", self.path.display()))?;
        f.unlock()?;
        drop(f);
        if let Some(cmd) = &self.sink
            && let Err(e) = send(cmd, &line)
        {
            eprintln!("warning: audit sink `{cmd}`: {e:#}; the local log has the entry");
        }
        Ok(())
    }
}

/// Pipe `line` to `sh -c CMD`.
fn send(cmd: &str, line: &str) -> Result<()> {
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .context("start it")?;
    if let Some(mut stdin) = child.stdin.take() {
        // A sink that exits without reading is judged by its status.
        let _ = stdin.write_all(format!("{line}\n").as_bytes());
    }
    let status = child.wait().context("wait for it")?;
    if !status.success() {
        anyhow::bail!("it exited with {status}");
    }
    Ok(())
}

/// Who is acting: `DFORM_ACTOR` when the environment gives it (a CI job's
/// OIDC subject), else `user@host`.
pub fn who() -> String {
    if let Ok(a) = std::env::var("DFORM_ACTOR")
        && !a.is_empty()
    {
        return a;
    }
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".into());
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    format!("{user}@{host}")
}

/// The entries of the log at `path`, in order (none when there is no log).
pub fn read(path: &Path) -> Result<Vec<Json>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l)
                .with_context(|| format!("{}: entry {} is not JSON", path.display(), i + 1))
        })
        .collect()
}

/// The first broken link of a chain: the entry (1-based, by position) and
/// what is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broken {
    pub entry: usize,
    pub why: String,
}

/// Check the chain of the log at `path`: every entry's hash is its
/// content's, its `prev` is the entry before's hash, and `seq` counts from
/// 1. Returns how many entries there are, and the first broken link.
pub fn verify(path: &Path) -> Result<(usize, Option<Broken>)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut prev = String::new();
    for (i, l) in lines.iter().enumerate() {
        let n = i + 1;
        let broken = |why: String| Ok((lines.len(), Some(Broken { entry: n, why })));
        let Ok(Json::Object(mut e)) = serde_json::from_str::<Json>(l) else {
            return broken(format!("entry {n} is not a JSON object"));
        };
        let kind = e
            .get("kind")
            .and_then(Json::as_str)
            .unwrap_or("?")
            .to_string();
        let Some(Json::String(hash)) = e.remove("hash") else {
            return broken(format!("entry {n} ({kind}) has no hash"));
        };
        if digest_of(&Json::Object(e.clone())) != hash {
            return broken(format!(
                "entry {n} ({kind}) was altered: its hash does not match its content"
            ));
        }
        if e.get("prev").and_then(Json::as_str) != Some(prev.as_str()) {
            return broken(format!(
                "entry {n} ({kind}): its prev is not entry {}'s hash; an entry before it was \
                 removed, reordered or altered",
                n - 1
            ));
        }
        if e.get("seq").and_then(Json::as_u64) != Some(n as u64) {
            return broken(format!("entry {n} ({kind}): its seq is not {n}"));
        }
        prev = hash;
    }
    Ok((lines.len(), None))
}

/// One entry as `dform log` prints it: `SEQ TIME KIND k=v ...`, a long
/// value cut short.
pub fn line(e: &Json) -> String {
    let s = |k: &str| match &e[k] {
        Json::String(s) => s.clone(),
        v => v.to_string(),
    };
    let mut out = format!("{:>4} {} {}", s("seq"), s("time"), s("kind"));
    if let Json::Object(m) = e {
        for (k, v) in m {
            if matches!(k.as_str(), "seq" | "time" | "kind" | "prev" | "hash") {
                continue;
            }
            let mut v = match v {
                Json::String(s) => s.clone(),
                v => canonical_json(v),
            };
            if v.chars().count() > 72 {
                v = format!("{}...", v.chars().take(69).collect::<String>());
            }
            out.push_str(&format!(" {k}={v}"));
        }
    }
    out
}

/// Keep the entries at or after `since`: a `seq` (a number) or a time
/// (RFC 3339, or a prefix of one such as a date).
pub fn since(entries: Vec<Json>, since: &str) -> Vec<Json> {
    match since.parse::<u64>() {
        Ok(n) => entries
            .into_iter()
            .filter(|e| e["seq"].as_u64().is_some_and(|s| s >= n))
            .collect(),
        Err(_) => entries
            .into_iter()
            .filter(|e| e["time"].as_str().is_some_and(|t| t >= since))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_chain_verifies_and_an_edit_is_named() {
        let dir = std::env::temp_dir().join(format!("dform-audit-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = Log::beside(&dir.join("state.json"), None);
        for i in 0..3 {
            log.append("tick", json!({ "tick": i })).unwrap();
        }
        assert_eq!(verify(log.path()).unwrap(), (3, None));
        let text = std::fs::read_to_string(log.path()).unwrap();
        std::fs::write(log.path(), text.replacen("\"tick\":1", "\"tick\":7", 1)).unwrap();
        let (_, broken) = verify(log.path()).unwrap();
        assert_eq!(broken.unwrap().entry, 2);
        let lines: Vec<&str> = text.lines().collect();
        std::fs::write(log.path(), format!("{}\n{}\n", lines[0], lines[2])).unwrap();
        let (_, broken) = verify(log.path()).unwrap();
        assert!(broken.unwrap().why.starts_with("entry 2 (tick): its prev"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

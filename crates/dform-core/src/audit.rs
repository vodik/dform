//! The audit log (E DR-16, "the log is audit", made real): one JSON-lines
//! object per stack deployment in its backend beside its state,
//! `state.audit.jsonl` (`<stem>.state.audit.jsonl` beside a `--world`
//! file; under the prefix of an `s3` backend), moving with the state on a
//! rekey or a handover. A store that cannot append in place (a bucket:
//! an object is written whole) keeps the log in segments of
//! [`SEGMENT`] entries, `state.audit/000001.jsonl`, ...: an entry
//! rewrites only the last one. The log is the segments in order, after
//! `state.audit.jsonl` when a log from before segments has one.
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
//! diff), `tick` (a digest of the world as the executor saw it), `retry`
//! (a provider call sent again: the call, the attempt and its budget, the
//! delay, why the last one failed; [`retry`]), `wait` (a tick waiting on
//! open nulls: what, since when, how long, and how it ended; [`wait`]),
//! `apply_end`, `controller` (events, holds, releases), `rekey` and
//! `handover`. Values are never written: a diff is a digest of its redacted
//! form, where a sensitive leaf is already the stack's HMAC of it.
//!
//! Nothing reads the log as truth (DR-16): plan and apply never look at it.
//! A sink (`--audit-sink CMD`, or the stack's `audit_sink = "CMD"`) gets
//! each entry as a JSON line on its stdin (`sh -c CMD`, once per entry);
//! a sink that fails is a warning, and the local log stays authoritative.

use crate::approval::{canonical_json, digest_of, now, rfc3339};
use crate::store::{AUDIT, AUDIT_SEGMENTS, Cond, LocalStore, Store};
use anyhow::{Context, Result};
use serde_json::{Map, Value as Json};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// The entries of one segment of a log kept in segments.
pub const SEGMENT: usize = 100;

/// A deployment's audit log: the object `state.audit.jsonl` of its store.
#[derive(Clone)]
pub struct Log {
    store: Arc<dyn Store>,
    sink: Option<String>,
}

impl Log {
    /// The log in `store`; each entry also goes to `sink` when there is
    /// one.
    pub fn new(store: Arc<dyn Store>, sink: Option<String>) -> Log {
        Log { store, sink }
    }

    /// The log of the local deployment whose state file is `state`.
    pub fn beside(state: &Path, sink: Option<String>) -> Log {
        Log::new(Arc::new(LocalStore::beside(state)), sink)
    }

    /// Where the log is, as messages name it.
    pub fn locate(&self) -> String {
        self.store.locate(AUDIT)
    }

    /// The log's text; `None` when there is no log.
    pub fn text(&self) -> Result<Option<String>> {
        let mut parts = Vec::new();
        if let Some(o) = self.store.get(AUDIT)? {
            parts.push((self.locate(), o.bytes));
        }
        if !self.store.appends_in_place() {
            for k in self.store.list(AUDIT_SEGMENTS)? {
                if let Some(o) = self.store.get(&k)? {
                    parts.push((self.store.locate(&k), o.bytes));
                }
            }
        }
        if parts.is_empty() {
            return Ok(None);
        }
        let mut text = String::new();
        for (at, bytes) in parts {
            let t = String::from_utf8(bytes).with_context(|| format!("{at}: not UTF-8"))?;
            text.push_str(&t);
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
        }
        Ok(Some(text))
    }

    /// The entries, in order (none when there is no log).
    pub fn entries(&self) -> Result<Vec<Json>> {
        entries(&self.text()?.unwrap_or_default(), &self.locate())
    }

    /// Append an entry of `kind` with `fields` (an object), chained to the
    /// last one. The store appends without losing a concurrent entry (a
    /// `plan --out` beside an apply), so the chain does not fork.
    pub fn append(&self, kind: &str, fields: Json) -> Result<()> {
        let mut written = String::new();
        let mut line = |text: &[u8]| {
            let text = String::from_utf8_lossy(text);
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
            if let Json::Object(m) = &fields {
                for (k, v) in m {
                    entry.entry(k.clone()).or_insert(v.clone());
                }
            }
            let hash = digest_of(&Json::Object(entry.clone()));
            entry.insert("hash".into(), hash.into());
            written = canonical_json(&Json::Object(entry));
            format!("{written}\n").into_bytes()
        };
        if self.store.appends_in_place() {
            self.store.append(AUDIT, &mut line)?;
        } else {
            self.append_segment(&mut line)?;
        }
        if let Some(cmd) = &self.sink
            && let Err(e) = send(cmd, &written)
        {
            eprintln!("warning: audit sink `{cmd}`: {e:#}; the local log has the entry");
        }
        Ok(())
    }
}

impl Log {
    /// Append to the last segment what `line` makes of it (a new segment
    /// once it holds [`SEGMENT`] entries; the first one continues a log
    /// from before segments), without losing a concurrent append: every
    /// write is conditional on what was read.
    fn append_segment(&self, line: &mut dyn FnMut(&[u8]) -> Vec<u8>) -> Result<()> {
        let segment = |n: usize| format!("{AUDIT_SEGMENTS}{n:06}.jsonl");
        for _ in 0..20 {
            let last = self.store.list(AUDIT_SEGMENTS)?.pop();
            let n = last
                .as_deref()
                .and_then(|k| k.strip_prefix(AUDIT_SEGMENTS)?.strip_suffix(".jsonl"))
                .and_then(|n| n.parse::<usize>().ok());
            let got = match &last {
                Some(k) => self.store.get(k)?,
                None => None,
            };
            let (key, bytes, cond) = match (n, got) {
                (Some(n), Some(o)) => {
                    let entries = o
                        .bytes
                        .split(|b| *b == b'\n')
                        .filter(|l| !l.is_empty())
                        .count();
                    if entries < SEGMENT {
                        let mut bytes = o.bytes.clone();
                        bytes.extend(line(&o.bytes));
                        (segment(n), bytes, Cond::IfMatch(o.etag))
                    } else {
                        (segment(n + 1), line(&o.bytes), Cond::IfAbsent)
                    }
                }
                // Gone since the listing: look again.
                (Some(_), None) => continue,
                (None, _) => {
                    let before = self.store.get(AUDIT)?.map(|o| o.bytes).unwrap_or_default();
                    (segment(1), line(&before), Cond::IfAbsent)
                }
            };
            if self.store.put(&key, &bytes, &cond)?.is_some() {
                return Ok(());
            }
        }
        anyhow::bail!(
            "append to {}: it changed under every attempt",
            self.store.locate(AUDIT_SEGMENTS)
        )
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

/// A `retry` entry's fields: a provider call that failed in a way worth
/// trying again, sent again after `delay` (R-81). `error` is why the last
/// attempt failed, as the caller redacts it.
pub fn retry(tick: usize, r: &crate::plugin::link::Retry, error: String) -> Json {
    serde_json::json!({
        "tick": tick,
        "provider": r.provider,
        "call": r.call,
        "attempt": r.attempt,
        "of": r.of,
        "delay_ms": r.delay.as_millis() as u64,
        "error": error,
    })
}

/// A `wait` entry's fields: a tick waiting on the open nulls `on` (as
/// plan prints them) since `since`, for `waited`; `result` is how it
/// ended: `resolved`, or `expired` once the budget is spent.
pub fn wait(
    tick: usize,
    on: &[String],
    since: &str,
    waited: std::time::Duration,
    result: &str,
) -> Json {
    serde_json::json!({
        "tick": tick,
        "on": on,
        "since": since,
        "waited_ms": waited.as_millis() as u64,
        "result": result,
    })
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

/// The entries of a log's `text`; `at` names it.
fn entries(text: &str, at: &str) -> Result<Vec<Json>> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l).with_context(|| format!("{at}: entry {} is not JSON", i + 1))
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

/// Check the chain of a log's `text`: every entry's hash is its
/// content's, its `prev` is the entry before's hash, and `seq` counts from
/// 1. Returns how many entries there are, and the first broken link.
pub fn verify(text: &str) -> (usize, Option<Broken>) {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut prev = String::new();
    for (i, l) in lines.iter().enumerate() {
        let n = i + 1;
        let broken = |why: String| (lines.len(), Some(Broken { entry: n, why }));
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
    (lines.len(), None)
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
        let text = log.text().unwrap().unwrap();
        assert_eq!(verify(&text), (3, None));
        let (_, broken) = verify(&text.replacen("\"tick\":1", "\"tick\":7", 1));
        assert_eq!(broken.unwrap().entry, 2);
        let lines: Vec<&str> = text.lines().collect();
        let (_, broken) = verify(&format!("{}\n{}\n", lines[0], lines[2]));
        assert!(broken.unwrap().why.starts_with("entry 2 (tick): its prev"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_in_segments_continues_an_old_one_and_rolls_over() {
        use crate::store::MemoryStore;
        let store = Arc::new(MemoryStore::new());
        let log = Log::new(store.clone(), None);
        // A log from before segments: one entry in the one object.
        log.append("tick", json!({ "tick": 0 })).unwrap();
        let first = format!("{AUDIT_SEGMENTS}000001.jsonl");
        let old = store.get(&first).unwrap().unwrap().bytes;
        store.delete(&first).unwrap();
        store.put(AUDIT, &old, &Cond::Any).unwrap();
        for i in 1..=SEGMENT + 5 {
            log.append("tick", json!({ "tick": i })).unwrap();
        }
        assert_eq!(
            store.list(AUDIT_SEGMENTS).unwrap(),
            [first.clone(), format!("{AUDIT_SEGMENTS}000002.jsonl")]
        );
        let second = store
            .get(&format!("{AUDIT_SEGMENTS}000002.jsonl"))
            .unwrap()
            .unwrap();
        assert_eq!(String::from_utf8(second.bytes).unwrap().lines().count(), 5);
        let text = log.text().unwrap().unwrap();
        assert_eq!(verify(&text), (SEGMENT + 6, None));
        assert_eq!(log.entries().unwrap().last().unwrap()["seq"], SEGMENT + 6);
    }
}

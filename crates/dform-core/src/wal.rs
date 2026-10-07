//! The state's write-ahead log (R-146): the audit log is the log, and the
//! state object (`state.json`) a checkpoint of it.
//!
//! Every change of state is first appended to the audit log as a `state`
//! entry, and the append is durable (fsynced, or a conditional PUT) before
//! anything else happens: a call's answer is in the log before the next
//! call is made. A `state` entry is either the whole state (`full`: the
//! first of a deployment, or one whose checkpoint the log does not reach)
//! or what changed since the entry before (`changes`, each `{at: PATH,
//! to: VALUE}` or `{at: PATH, gone: true}`, PATH the keys down to it), and
//! the fence of the lease it was written under (0 where the store does
//! not fence). The checkpoint, written per tick by a whole-object write
//! (on the local backend: a temporary file, fsynced, renamed over), says
//! the last entry it includes (`log`: its `seq` and `hash`, and in a store
//! kept in segments its `part`).
//!
//! Recovery: a run reads the checkpoint and replays the `state` entries
//! after its position. A checkpoint the log does not reach (none yet: the
//! first apply died before it; or the log was started again) replays from
//! the log's last `full` entry. An entry written under a lower fence than
//! one already seen (a `lease` entry, written when a lease is taken, or a
//! `state` entry) is a stale holder's, written after its lease was taken
//! over, and is skipped.

use crate::audit::Pos;
use serde_json::{Map, Value as Json, json};

/// What changed from `old` to `new`, as `state` entry changes: objects
/// are compared key by key, anything else whole.
pub fn diff(old: &Json, new: &Json) -> Vec<Json> {
    let mut out = Vec::new();
    walk(&mut Vec::new(), old, new, &mut out);
    out
}

fn walk(at: &mut Vec<String>, old: &Json, new: &Json, out: &mut Vec<Json>) {
    match (old, new) {
        (Json::Object(o), Json::Object(n)) => {
            for (k, v) in n {
                at.push(k.clone());
                match o.get(k) {
                    Some(was) => walk(at, was, v, out),
                    None => out.push(json!({ "at": at, "to": v })),
                }
                at.pop();
            }
            for k in o.keys().filter(|k| !n.contains_key(*k)) {
                at.push(k.clone());
                out.push(json!({ "at": at, "gone": true }));
                at.pop();
            }
        }
        _ if old != new => out.push(json!({ "at": at, "to": new })),
        _ => {}
    }
}

/// `changes` applied to `v`; `false` when one does not fit it (a path
/// through something that is not an object).
pub fn apply(v: &mut Json, changes: &[Json]) -> bool {
    for c in changes {
        let Some(at) = c["at"].as_array() else {
            return false;
        };
        let path: Vec<&str> = at.iter().filter_map(Json::as_str).collect();
        if path.len() != at.len() {
            return false;
        }
        let Some((last, parents)) = path.split_last() else {
            // The whole value.
            if let Some(to) = c.get("to") {
                *v = to.clone();
            }
            continue;
        };
        let mut here = &mut *v;
        for k in parents {
            let Json::Object(m) = here else {
                return false;
            };
            here = m
                .entry(k.to_string())
                .or_insert_with(|| Json::Object(Map::new()));
        }
        let Json::Object(m) = here else {
            return false;
        };
        match c.get("to") {
            Some(to) => {
                m.insert(last.to_string(), to.clone());
            }
            None => {
                m.remove(*last);
            }
        }
    }
    true
}

/// What a replay made of a checkpoint and the log after it.
#[derive(Debug, Clone)]
pub struct Replayed {
    /// The state, as JSON.
    pub state: Json,
    /// The position of the last entry it includes.
    pub pos: Option<Pos>,
    /// The `state` entries replayed.
    pub entries: usize,
    /// The log reaches it: the next entry may be a change (else it must be
    /// `full`).
    pub chained: bool,
}

/// Replay `tail` (the log's entries after the checkpoint's position, or
/// every entry when `found` is false: the log does not reach it) over the
/// checkpoint `base` (its position `pos`, its `fence`).
pub fn replay(base: Json, pos: Option<Pos>, fence: u64, tail: &[Json], found: bool) -> Replayed {
    let mut start = 0;
    let mut chained = found;
    if !found {
        match tail
            .iter()
            .rposition(|e| e["kind"] == "state" && e.get("full").is_some())
        {
            Some(i) => {
                start = i;
                chained = true;
            }
            // Nothing to start from: the checkpoint as it is, and the next
            // entry a whole one.
            None => {
                return Replayed {
                    state: base,
                    pos,
                    entries: 0,
                    chained: false,
                };
            }
        }
    }
    let mut out = Replayed {
        state: base,
        pos,
        entries: 0,
        chained,
    };
    let mut seen = if found { fence } else { 0 };
    for e in &tail[start..] {
        let f = e["fence"].as_u64().unwrap_or(0);
        match e["kind"].as_str() {
            Some("lease") => seen = seen.max(f),
            Some("state") if f >= seen => {
                seen = f;
                let ok = match e.get("full") {
                    Some(full) => {
                        out.state = full.clone();
                        true
                    }
                    None => e["changes"]
                        .as_array()
                        .is_some_and(|c| apply(&mut out.state, c)),
                };
                if !ok {
                    // A change that does not fit: what follows cannot be
                    // trusted either; the next entry is a whole one.
                    out.chained = false;
                    break;
                }
                out.entries += 1;
                out.pos = Pos::of(e);
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diff_applied_is_the_new_value() {
        let old = json!({"version": 1, "resources": {"a": {"remote": "1"}, "b": {"remote": "2"}},
                         "in_flight": {"tick": 1, "remaining": {"a": null, "b": {"x": 1}}}});
        let new = json!({"version": 1, "resources": {"a": {"remote": "9"}, "c": {"remote": "3"}},
                         "keys": 4});
        let d = diff(&old, &new);
        let mut v = old.clone();
        assert!(apply(&mut v, &d));
        assert_eq!(v, new);
        assert!(diff(&new, &new).is_empty());
        // A null is a value, not a removal.
        let with_null = json!({"in_flight": {"remaining": {"a": null}}});
        let mut v = json!({"in_flight": {"remaining": {}}});
        let d = diff(&v, &with_null);
        assert!(apply(&mut v, &d));
        assert_eq!(v, with_null);
    }

    fn entry(seq: u64, kind: &str, fence: u64, body: Json) -> Json {
        let mut e = json!({"seq": seq, "hash": format!("h{seq}"), "kind": kind, "fence": fence});
        if let (Json::Object(e), Json::Object(b)) = (&mut e, body) {
            e.extend(b);
        }
        e
    }

    #[test]
    fn a_stale_holders_entry_after_a_takeover_is_skipped() {
        let base = json!({"n": 0});
        let tail = [
            entry(2, "state", 1, json!({"changes": [{"at": ["n"], "to": 1}]})),
            entry(3, "lease", 2, json!({})),
            entry(4, "state", 1, json!({"changes": [{"at": ["n"], "to": 7}]})),
            entry(5, "state", 2, json!({"changes": [{"at": ["m"], "to": 2}]})),
        ];
        let r = replay(base, None, 1, &tail, true);
        assert_eq!(r.state, json!({"n": 1, "m": 2}));
        assert_eq!(r.entries, 2);
        assert_eq!(r.pos.unwrap().seq, 5);
    }

    #[test]
    fn a_checkpoint_the_log_does_not_reach_replays_from_the_last_whole_entry() {
        let tail = [
            entry(1, "state", 0, json!({"full": {"n": 1}})),
            entry(2, "state", 0, json!({"changes": [{"at": ["n"], "to": 2}]})),
            entry(3, "state", 0, json!({"full": {"n": 5}})),
            entry(4, "state", 0, json!({"changes": [{"at": ["k"], "to": 1}]})),
        ];
        let r = replay(json!({"old": true}), None, 0, &tail, false);
        assert_eq!(r.state, json!({"n": 5, "k": 1}));
        assert!(r.chained);
        let r = replay(json!({"old": true}), None, 0, &tail[1..2], false);
        assert_eq!(r.state, json!({"old": true}));
        assert!(!r.chained);
    }
}

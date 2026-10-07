//! `memo.first(+key: string, +candidate, -value)` (R-60): the one way a
//! value is kept across runs. The first candidate ever given for a key is
//! kept in the deployment's state and is the value on every later run,
//! whatever the candidate then; `dform state taint memo KEY` forgets it.
//!
//! ```text
//! memo.first("db-created", time.now(), created)      # a creation time
//! let pw = memo.first("db-pw", random.base64("db-pw", 32))
//! ```
//!
//! It is a built-in extern, answered by dform ([`Memos::answer`]) and in
//! scope with no provider's `use`. Within a run the first call of a key
//! answers every later one, so two sites agree. What a run answered is kept
//! when an apply completes a tick ([`keep`]); a plan keeps nothing.
//!
//! A secret candidate (the secrets pass says which sites:
//! `secrets::secret_memos`) is kept sealed with a key derived from the
//! deployment's master (`custody`, `secrets::seal`): state holds the seal,
//! never the value, and the run that reads it opens it in memory; a run
//! that does not hold the master answers its stand-in.

use crate::ast::ExternFn;
use crate::state::State;
use crate::value::Value;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;

/// The relation.
pub const FIRST: &str = "memo.first";

/// A kept value, in state under its key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    /// When it was kept: RFC 3339, UTC.
    pub kept: String,
    /// A plain value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// A secret: its seal (`secrets::seal`, bound to the key).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sealed: String,
}

thread_local! {
    /// When each key the run reads was kept, for `why` (`engine`'s leaf of
    /// a `memo.first` fact): the last [`Memos`] made on this thread.
    static KEPT: RefCell<BTreeMap<String, String>> = const { RefCell::new(BTreeMap::new()) };
}

/// What `why` says of the `memo.first` answer for `key`: kept when, or
/// given by this run.
pub fn provenance(key: &str) -> String {
    KEPT.with(|k| match k.borrow().get(key) {
        Some(when) => format!("{FIRST}(\"{key}\"): memo, first kept {when}"),
        None => format!("{FIRST}(\"{key}\"): memo, first given by this run"),
    })
}

/// How `why` names where an extern's fact came from (its leaf's `call`):
/// a memo's provenance, else "answered" (R-129: never `extern`).
pub fn source(call: &str) -> String {
    match call.split_once("): memo, ") {
        Some((head, rest)) if head.starts_with(FIRST) => format!("memo, {rest}"),
        _ => "answered".into(),
    }
}

/// The answers of `memo.first` for one run, from the state it starts with.
pub struct Memos {
    kept: BTreeMap<String, Kept>,
    /// The deployment's master, which opens a sealed one; `None` in a run
    /// that does not hold it, where a sealed one answers its stand-in
    /// (`secrets::standin`).
    key: Option<crate::zset::file::Key>,
    /// The first candidate of each key not kept, as this run answered it.
    given: RefCell<BTreeMap<String, Value>>,
}

impl Memos {
    pub fn new(st: &State, key: Option<crate::zset::file::Key>) -> Memos {
        let when = st
            .memo
            .iter()
            .map(|(k, m)| (k.clone(), m.kept.clone()))
            .collect();
        KEPT.with(|k| *k.borrow_mut() = when);
        Memos {
            kept: st.memo.clone(),
            key,
            given: RefCell::new(BTreeMap::new()),
        }
    }

    /// The answer to `memo.first(key, candidate, -value)`: the kept value,
    /// else the first candidate this run was given for the key. `None`
    /// for another extern.
    pub fn answer(&self, f: &ExternFn, inputs: &[Value]) -> Option<Result<Vec<Vec<Value>>>> {
        if f.name != FIRST {
            return None;
        }
        let [Value::Str(k), candidate] = inputs else {
            return Some(Err(anyhow!(
                "memo.first takes a key, a string, and a candidate"
            )));
        };
        let value = match self.kept.get(k) {
            Some(m) => match self.value(k, m) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            },
            None => self
                .given
                .borrow_mut()
                .entry(k.clone())
                .or_insert_with(|| candidate.clone())
                .clone(),
        };
        Some(Ok(vec![vec![
            Value::Str(k.clone()),
            candidate.clone(),
            value,
        ]]))
    }

    fn value(&self, k: &str, m: &Kept) -> Result<Value> {
        if let Some(v) = &m.value {
            return Ok(v.clone());
        }
        // Its stand-in (R-164): a function of the key and the seal, which
        // stay until the memo is tainted.
        let standin = {
            use sha2::Digest;
            let d = sha2::Sha256::new()
                .chain_update(k.as_bytes())
                .chain_update([0])
                .chain_update(m.sealed.as_bytes())
                .finalize();
            let hex: String = d.iter().take(16).map(|b| format!("{b:02x}")).collect();
            format!("memo-{hex}")
        };
        let label = format!("{FIRST}({k:?})");
        let Some(key) = &self.key else {
            // A run that does not hold the master: the stand-in.
            crate::secrets::standin::register(&standin, &label, &standin);
            return Ok(Value::Str(standin));
        };
        let plain = crate::secrets::open(key, k, &m.sealed)?;
        let v: Value = serde_json::from_slice(&plain)
            .with_context(|| format!("memo {k}: the opened value"))?;
        if let Value::Str(t) = &v {
            crate::secrets::standin::register(t, &label, &standin);
        }
        Ok(v)
    }
}

/// Keep in `st` each memo the run answered that it does not have yet:
/// `(key, value, secret)` ([`crate::externs::Externs::memos`]), a secret
/// one sealed with the deployment's master `key`. A run that does not
/// hold it keeps no secret one, nor one that holds a stand-in: the next
/// apply with the master does. `now`: when, RFC 3339.
pub fn keep(
    st: &mut State,
    memos: Vec<(String, Value, bool)>,
    key: Option<&crate::zset::file::Key>,
    now: &str,
) -> Result<()> {
    for (k, v, secret) in memos {
        if st.memo.contains_key(&k) {
            continue;
        }
        let kept = match (secret, key) {
            (true, None) => continue,
            (false, None)
                if crate::secrets::standin::carries(&crate::engine::value_to_json(&v)) =>
            {
                continue;
            }
            (true, Some(key)) => Kept {
                kept: now.to_string(),
                value: None,
                sealed: crate::secrets::seal(key, &k, &serde_json::to_vec(&v)?)?,
            },
            (false, _) => Kept {
                kept: now.to_string(),
                value: Some(v),
                sealed: String::new(),
            },
        };
        st.memo.insert(k, kept);
    }
    Ok(())
}

/// The time now, RFC 3339 in UTC to the second (what a memo is kept at
/// and `time.now` answers): `DFORM_TEST_NOW` in tests, else the clock.
pub fn now() -> String {
    match std::env::var("DFORM_TEST_NOW") {
        Ok(t) => t,
        Err(_) => jiff::Timestamp::now()
            .round(jiff::Unit::Second)
            .map(|t| t.to_string())
            .unwrap_or_default(),
    }
}

//! The providers of a tree of deployments planned or applied as one run
//! (one plan over the dependency tree): one link per provider for the
//! tree, its process started, its handshake and its schema asked once,
//! whichever deployment's providers reach it.
//!
//! Each deployment's providers open a session of a link with their own
//! configuration (the deployment, its world, the program's settings), and
//! every call they make names it ([`super::backend::Session`]): the
//! provider answers it with that configuration, so the deployments'
//! calls may be in flight on one link together, each landing in its own
//! world.

use super::link::Link;
use super::pb;
use anyhow::Result;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

thread_local! {
    /// The pool of the run in progress on this thread, if it is a tree's.
    static POOL: RefCell<Option<Rc<Pool>>> = const { RefCell::new(None) };
}

/// The run's pool, while its guard lives ([`share`]).
pub struct Pool {
    entries: RefCell<Vec<Rc<Entry>>>,
    /// The types the tree's programs name: what a link's one Schema call
    /// asks for, beside the types the first deployment to ask names.
    hint: Option<BTreeSet<String>>,
}

/// What a link is started for: the mock (which plays every mock schema no
/// `use` renames), or a provider by its spec and the name a `use .. as`
/// gives it (R-115).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Mock,
    Spec {
        spec: String,
        rename: Option<String>,
    },
}

/// One link of the pool.
pub struct Entry {
    key: Key,
    pub link: Rc<RefCell<Link>>,
    /// Its Schema answer, as it was configured then and asked for which
    /// types (`None`: all).
    schema: RefCell<Option<Asked>>,
}

/// A Schema answer of a link, and what it was asked under.
struct Asked {
    /// The link's configuration but the deployment's run (its world, its
    /// name, ..): what else configures a provider may change its schema
    /// (the mock's schemas, a cluster's settings).
    basis: serde_json::Value,
    types: Option<BTreeSet<String>>,
    resp: pb::SchemaResponse,
}

/// The keys of a link's configuration that are a deployment's run, not
/// the provider's: the same provider serves the same schema whatever they
/// are ([`super::providers`]' `base_config`).
const RUN_KEYS: [&str; 7] = [
    "world",
    "worlds",
    "inventory",
    "chaos",
    "stack",
    "cache",
    "digest_key",
];

/// A link's configuration `config` but its run's keys.
pub(super) fn basis(config: Option<&serde_json::Value>) -> serde_json::Value {
    let mut b = config.cloned().unwrap_or_default();
    if let Some(o) = b.as_object_mut() {
        for k in RUN_KEYS {
            o.remove(k);
        }
    }
    b
}

/// The pool is the run's while this lives; the one before it, if any,
/// after.
pub struct Shared(Option<Rc<Pool>>);

impl Drop for Shared {
    fn drop(&mut self) {
        let before = self.0.take();
        POOL.with(|p| *p.borrow_mut() = before);
    }
}

/// Share the providers of the runs that follow, on this thread, until the
/// guard is dropped: a tree's. `hint`: the types its programs name.
pub fn share(hint: Option<BTreeSet<String>>) -> Shared {
    let pool = Rc::new(Pool {
        entries: RefCell::new(Vec::new()),
        hint,
    });
    Shared(POOL.with(|p| p.borrow_mut().replace(pool)))
}

/// The types a Schema answer says its provider serves: every one, a
/// scoped answer too (each `type_provider` and `type_alias` row is in
/// it), so a name it does not list is no type of this provider's.
fn served(resp: &pb::SchemaResponse) -> BTreeSet<String> {
    resp.facts
        .iter()
        .filter(|f| f.pred == "type_provider" || f.pred == "type_alias")
        .filter_map(|f| crate::ast::Atom::try_from(f).ok())
        .filter_map(|a| a.args.first()?.as_str().map(str::to_string))
        .collect()
}

/// The pool of the run in progress, if it is a tree's.
pub(super) fn current() -> Option<Rc<Pool>> {
    POOL.with(|p| p.borrow().clone())
}

/// Whether the run in progress on this thread is a tree's.
pub fn active() -> bool {
    POOL.with(|p| p.borrow().is_some())
}

impl Pool {
    /// The link of `key`: the pool's, else started now (`start`) and
    /// kept.
    pub(super) fn link(&self, key: Key, start: impl FnOnce() -> Result<Link>) -> Result<Rc<Entry>> {
        if let Some(e) = self.entries.borrow().iter().find(|e| e.key == key) {
            return Ok(e.clone());
        }
        let e = Rc::new(Entry {
            key,
            link: Rc::new(RefCell::new(start()?)),
            schema: RefCell::new(None),
        });
        self.entries.borrow_mut().push(e.clone());
        Ok(e)
    }
}

impl Entry {
    /// The link's Schema answer for the types `types` (`None`: all),
    /// configured as `basis` says: the one it gave so configured, when it
    /// was asked for each of them it serves; else asked now, for them,
    /// those asked before and the tree's (`hint`), and kept.
    pub(super) fn schema(
        &self,
        pool: &Pool,
        basis: serde_json::Value,
        types: Option<&BTreeSet<String>>,
        ask: impl FnOnce(Option<Vec<String>>) -> Result<pb::SchemaResponse>,
    ) -> Result<pb::SchemaResponse> {
        let before = self.schema.borrow_mut().take().filter(|a| a.basis == basis);
        if let Some(a) = before.as_ref().filter(|a| a.covers(types)) {
            let resp = a.resp.clone();
            *self.schema.borrow_mut() = before;
            return Ok(resp);
        }
        let types = types.map(|t| {
            let mut all = t.clone();
            if let Some(Asked { types: Some(a), .. }) = &before {
                all.extend(a.iter().cloned());
            }
            all.extend(pool.hint.iter().flatten().cloned());
            all
        });
        let resp = ask(types.as_ref().map(|a| a.iter().cloned().collect()))?;
        *self.schema.borrow_mut() = Some(Asked {
            basis,
            types,
            resp: resp.clone(),
        });
        Ok(resp)
    }
}

impl Asked {
    /// Whether the answer has what an ask for `types` would: it was asked
    /// for every one, or for each of them its provider serves.
    fn covers(&self, types: Option<&BTreeSet<String>>) -> bool {
        match (&self.types, types) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(a), Some(t)) => {
                let served = served(&self.resp);
                t.iter().all(|n| a.contains(n) || !served.contains(n))
            }
        }
    }
}

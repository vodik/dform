//! The providers of a tree of deployments planned or applied as one run
//! (one plan over the dependency tree): one process per provider for the
//! tree, started, its handshake and its schema asked once, whichever
//! deployment's providers reach it.
//!
//! Each deployment's providers open a session of a link with their own
//! configuration (the deployment, its world, the program's settings), and
//! every call they make names it ([`super::backend::Session`]): the
//! provider answers it with that configuration, so the deployments'
//! calls may be in flight on one process together, each landing in its
//! own world. The deployments of a tree's plan run each on a thread of
//! its own ([`join`]); a thread's link of a provider is its own, attached
//! to the process the first started ([`Link::attach`]), so its calls,
//! answers and waits are its own. The deployments one thread runs share
//! that thread's link (a tree's apply walks them all on one).

use super::link::Link;
use super::pb;
use anyhow::Result;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};

thread_local! {
    /// The pool of the run in progress on this thread, if it is a tree's,
    /// and this thread's links of it.
    static ENTERED: RefCell<Option<Entered>> = const { RefCell::new(None) };
}

/// A thread in a pool: its links of the pool's providers.
struct Entered {
    pool: Arc<Pool>,
    links: Vec<(Arc<Entry>, Rc<RefCell<Link>>)>,
}

/// The run's pool, while a guard of it lives ([`share`], [`join`]).
pub struct Pool {
    entries: Mutex<Vec<Arc<Entry>>>,
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

/// One provider of the pool.
pub struct Entry {
    key: Key,
    /// The link it was started as, which calls nothing: what each
    /// thread's link of it is attached from.
    started: Mutex<Option<Link>>,
    /// Its Schema answer, as it was configured then and asked for which
    /// types (`None`: all).
    schema: Mutex<Option<Asked>>,
}

/// `m` locked: what it guards is whole whatever panicked holding it (a
/// value set or taken).
fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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

/// The pool is this thread's while this lives, its links the thread's;
/// the one before it, if any, after.
pub struct Shared(Option<Entered>);

impl Drop for Shared {
    fn drop(&mut self) {
        let before = self.0.take();
        ENTERED.with(|p| *p.borrow_mut() = before);
    }
}

/// Share the providers of the runs that follow, on this thread, until the
/// guard is dropped: a tree's. `hint`: the types its programs name.
pub fn share(hint: Option<BTreeSet<String>>) -> Shared {
    join(Arc::new(Pool {
        entries: Mutex::new(Vec::new()),
        hint,
    }))
}

/// This thread in `pool`, another thread's ([`current`]), until the guard
/// is dropped: the runs that follow on it share the pool's providers,
/// through links of this thread's own.
pub fn join(pool: Arc<Pool>) -> Shared {
    let entered = Entered {
        pool,
        links: Vec::new(),
    };
    Shared(ENTERED.with(|p| p.borrow_mut().replace(entered)))
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

/// The pool of the run in progress on this thread, if it is a tree's.
pub fn current() -> Option<Arc<Pool>> {
    ENTERED.with(|p| p.borrow().as_ref().map(|e| e.pool.clone()))
}

/// Whether the run in progress on this thread is a tree's.
pub fn active() -> bool {
    ENTERED.with(|p| p.borrow().is_some())
}

impl Pool {
    /// This thread's link of `key`, and its entry: the thread's, else
    /// attached to the provider as started, else started now (`start`)
    /// and kept. A thread that needs a provider another is starting waits
    /// for it.
    pub(super) fn link(
        &self,
        key: Key,
        start: impl FnOnce() -> Result<Link>,
    ) -> Result<(Rc<RefCell<Link>>, Arc<Entry>)> {
        let entry = self.entry(key);
        if let Some(link) = mine(&entry) {
            return Ok((link, entry));
        }
        let link = Rc::new(RefCell::new(entry.attach(start)?));
        ENTERED.with(|p| {
            if let Some(e) = p.borrow_mut().as_mut() {
                e.links.push((entry.clone(), link.clone()));
            }
        });
        Ok((link, entry))
    }

    /// The entry of `key`, made now if there is none.
    fn entry(&self, key: Key) -> Arc<Entry> {
        let mut entries = locked(&self.entries);
        if let Some(e) = entries.iter().find(|e| e.key == key) {
            return e.clone();
        }
        let e = Arc::new(Entry {
            key,
            started: Mutex::new(None),
            schema: Mutex::new(None),
        });
        entries.push(e.clone());
        e
    }
}

/// This thread's link of `entry`, if it has one.
fn mine(entry: &Arc<Entry>) -> Option<Rc<RefCell<Link>>> {
    ENTERED.with(|p| {
        p.borrow().as_ref().and_then(|e| {
            e.links
                .iter()
                .find(|(x, _)| Arc::ptr_eq(x, entry))
                .map(|(_, l)| l.clone())
        })
    })
}

impl Entry {
    /// A link of the calling thread's own to the provider: attached to it
    /// as started, else started now (`start`).
    fn attach(&self, start: impl FnOnce() -> Result<Link>) -> Result<Link> {
        let mut started = locked(&self.started);
        if let Some(s) = started.as_ref() {
            return Ok(s.attach());
        }
        let link = start()?;
        *started = Some(link.attach());
        Ok(link)
    }

    /// The link's Schema answer for the types `types` (`None`: all),
    /// configured as `basis` says: the one it gave so configured, when it
    /// was asked for each of them it serves; else asked now, for them,
    /// those asked before and the tree's (`hint`), and kept. A deployment
    /// that needs it while another asks waits for that answer.
    pub(super) fn schema(
        &self,
        pool: &Pool,
        basis: serde_json::Value,
        types: Option<&BTreeSet<String>>,
        ask: impl FnOnce(Option<Vec<String>>) -> Result<pb::SchemaResponse>,
    ) -> Result<pb::SchemaResponse> {
        let mut held = locked(&self.schema);
        let before = held.take().filter(|a| a.basis == basis);
        if let Some(a) = before.as_ref().filter(|a| a.covers(types)) {
            let resp = a.resp.clone();
            *held = before;
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
        *held = Some(Asked {
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

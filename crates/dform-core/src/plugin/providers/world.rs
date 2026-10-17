//! The world as the providers answer it: Refresh (each object state
//! maps, Read once between two writes), Import, Health, and the world as
//! facts for round 0.

use super::{Object, Providers, World, key};
use crate::address::Address;
use crate::ast::{Atom, Term};
use crate::plugin::pb;
use crate::plugin::wire;
use crate::provider::{self, get_path};
use crate::state::{self, State, StateEntry};
use crate::value::{NullClass, Value};
use anyhow::{Context, Result, bail};
use serde_json::Value as Json;
use std::collections::BTreeMap;

/// An object state maps, as Refresh reads it: its address, its entry and
/// the link that serves it.
struct Mapped<'a> {
    addr: Address,
    entry: &'a StateEntry,
    link: usize,
}

/// The Read of `remote`, the object at `addr`, told what state remembers
/// of it (`prior`, absent when state holds nothing).
fn read_request(addr: &Address, remote: &str, prior: Option<&Json>) -> pb::ReadRequest {
    pb::ReadRequest {
        r#type: addr.typ.clone(),
        remote: remote.to_string(),
        name: addr.name.clone(),
        prior: prior.map(wire::doc),
        ..Default::default()
    }
}

impl Providers {
    /// Read the world again at the next refresh, though nothing was
    /// written: a tick waits on what the world has not reached yet (R-81).
    pub fn reread(&self) {
        self.invalidate();
    }

    /// A write happened: the next refresh Reads again.
    pub(super) fn invalidate(&self) {
        self.refreshed.borrow_mut().clear();
        self.imported.borrow_mut().clear();
    }

    /// Link `i` was configured again: what it answered may differ now, so
    /// its objects are read again; every other link's Reads stand.
    pub(super) fn forget(&self, i: usize) {
        self.refreshed.borrow_mut().retain(|_, (j, _)| *j != i);
        self.imported.borrow_mut().retain(|_, (j, _)| *j != i);
    }

    /// Whether the object `remote` of `addr`'s type is there: one Read.
    pub fn exists(&self, addr: &Address, remote: &str) -> Result<bool> {
        Ok(self
            .read(self.route(&addr.typ), addr, remote, None)?
            .is_some())
    }

    pub(super) fn object(
        attrs: Option<&pb::Value>,
        computed: Option<&pb::Value>,
    ) -> Result<Object> {
        Ok(Object {
            attrs: wire::from_doc_or_empty(attrs)?,
            computed: wire::from_doc_or_empty(computed)?,
        })
    }

    /// Read one object, told what state remembers of it (`prior`, its
    /// computed values); `None` when Read returns nothing (the provider
    /// retries a type's `type_retry` attempts before it says so).
    pub(super) fn read(
        &self,
        i: usize,
        addr: &Address,
        remote: &str,
        prior: Option<&Json>,
    ) -> Result<Option<Object>> {
        let req = read_request(addr, remote, prior);
        let r: pb::ReadResponse = self.call(i, req)?;
        if !r.found {
            return Ok(None);
        }
        Self::object(r.attrs.as_ref(), r.computed.as_ref()).map(Some)
    }

    /// Import: an object by remote id, whether dform manages it or not.
    pub(super) fn import(&self, typ: &str, remote: &str) -> Result<Option<Object>> {
        let k = key(typ, remote);
        let i = self.route(typ);
        if let Some((j, o)) = self.imported.borrow().get(&k)
            && *j == i
        {
            return Ok(o.clone());
        }
        let req = pb::ImportRequest {
            r#type: typ.to_string(),
            remote: remote.to_string(),
            ..Default::default()
        };
        let r: pb::ImportResponse = self.call(i, req)?;
        let o = match r.found {
            true => Some(Self::object(r.attrs.as_ref(), r.computed.as_ref())?),
            false => None,
        };
        self.imported.borrow_mut().insert(k, (i, o.clone()));
        Ok(o)
    }

    /// Refresh: every object state maps (deposed ones included), as Read
    /// returns it. An object Read does not return is gone; one state does
    /// not map is not read. Each object is read once between two writes
    /// ([`Providers::refreshed`]): only those not read yet are asked.
    pub(super) fn refresh(&self, state: &State) -> Result<World> {
        let mut mapped: BTreeMap<String, Mapped> = BTreeMap::new();
        for (addr, e) in self
            .entries(&state.resources)
            .chain(self.entries(&state.deposed))
        {
            let link = self
                .link_serving(&addr.typ, &e.provider)
                .expect("entries are served");
            mapped.insert(
                key(&addr.typ, &e.remote),
                Mapped {
                    addr,
                    entry: e,
                    link,
                },
            );
        }
        let unread: Vec<(&String, &Mapped)> = {
            let read = self.refreshed.borrow();
            mapped
                .iter()
                .filter(|(k, m)| read.get(*k).is_none_or(|(j, _)| *j != m.link))
                .collect()
        };
        if !unread.is_empty() {
            self.read_all(unread)?;
        }
        let read = self.refreshed.borrow();
        Ok(mapped
            .keys()
            .filter_map(|k| Some((k.clone(), read.get(k)?.1.clone()?)))
            .collect())
    }

    /// What Read answered of each object state maps that `world` holds,
    /// its computed values, kept in state for its next Read
    /// (`StateEntry::computed`).
    pub(super) fn remember(&self, world: &World, state: &mut State) {
        let read: Vec<(Address, Json)> = self
            .entries(&state.resources)
            .filter_map(|(addr, e)| {
                let o = world.get(&key(&addr.typ, &e.remote))?;
                Some((addr, o.computed.clone()))
            })
            .collect();
        for (addr, computed) in read {
            state.set_computed(&addr, computed);
        }
    }

    /// Read `objects`, each its key and what state maps, into
    /// [`Providers::refreshed`]. Every Read is sent before any answer is
    /// taken, to every provider, so a provider that can answers them at
    /// once: a plan waits one round trip to its API, not one per object.
    /// A Read that fails is sent again alone, under the provider's retry
    /// policy; one that fails then is the refresh's error, and not kept.
    #[tracing::instrument(
        target = "dform",
        level = "trace",
        name = "plan.phase",
        skip_all,
        fields(otel.name = "refresh", count = objects.len())
    )]
    fn read_all(&self, objects: Vec<(&String, &Mapped)>) -> Result<()> {
        let mut sent = Vec::new();
        for (k, m) in objects {
            let req = read_request(&m.addr, &m.entry.remote, m.entry.prior());
            let t = self.submit(m.link, req)?;
            sent.push((k, m, t));
        }
        for l in &self.links {
            l.borrow_mut().flush();
        }
        for (k, m, t) in sent {
            let answer = {
                let mut link = self.links[m.link].borrow_mut();
                link.wait(t)
                    .and_then(|r| link.expect::<pb::ReadResponse>("Read", r))
            };
            let found = match answer {
                Ok(r) if !r.found => None,
                Ok(r) => Some(Self::object(r.attrs.as_ref(), r.computed.as_ref())?),
                Err(_) => self.read(m.link, &m.addr, &m.entry.remote, m.entry.prior())?,
            };
            self.refreshed
                .borrow_mut()
                .insert(k.clone(), (m.link, found));
        }
        Ok(())
    }

    /// Health (R-203): each object state maps (deposed ones not) whose
    /// provider's handshake says it answers Health for its type, as that
    /// provider judges it now; one call to each provider. An object of a
    /// type nobody answers for is not asked, and not in the answer; one
    /// whose provider waits on the program's settings is `unknown`. For
    /// `dform status`, and asked at no other time: nothing of it reaches
    /// the plan, the apply or state.
    pub fn health(&self, state: &State) -> Result<BTreeMap<Address, pb::Health>> {
        let mut out = BTreeMap::new();
        let mut asked: BTreeMap<usize, Vec<Address>> = BTreeMap::new();
        let mut objects: BTreeMap<usize, Vec<pb::Identity>> = BTreeMap::new();
        for (k, e) in &state.resources {
            let Some(addr) = state::parse_key(k) else {
                continue;
            };
            let Some(i) = self.link_serving(&addr.typ, &e.provider) else {
                continue;
            };
            if !self.links[i].borrow().answers_health(&addr.typ) {
                continue;
            }
            if self.awaiting.borrow().contains(&i) {
                let why = format!(
                    "provider {} waits on the program's settings",
                    self.block_name(i)
                );
                out.insert(
                    addr,
                    crate::plugin::backend::health(pb::HealthState::Unknown, why),
                );
                continue;
            }
            objects.entry(i).or_default().push(pb::Identity {
                r#type: addr.typ.clone(),
                name: addr.name.clone(),
                remote: e.remote.clone(),
            });
            asked.entry(i).or_default().push(addr);
        }
        for (i, objects) in objects {
            let n = objects.len();
            let r: pb::HealthResponse = self
                .call(
                    i,
                    pb::HealthRequest {
                        objects,
                        ..Default::default()
                    },
                )
                .with_context(|| format!("provider {}: Health", self.block_name(i)))?;
            if r.answers.len() != n {
                bail!(
                    "provider {}: Health answered {} objects of the {n} asked",
                    self.block_name(i),
                    r.answers.len()
                );
            }
            out.extend(
                asked
                    .remove(&i)
                    .unwrap_or_default()
                    .into_iter()
                    .zip(r.answers),
            );
        }
        Ok(out)
    }

    /// Refresh as facts, for round-0 resolution (E Rule 4, F DR-11 revised):
    /// `identity(T, A, Rid)` for every address state maps to an object Read
    /// returns, `remote_name(T, A, Program, Name)` for one dform named
    /// (`StateEntry::name`), and `world_attr(T, Rid, P, V)` for every schema-computed or
    /// Optional+Computed path the world holds a value for. Secrets are never
    /// handed to the evaluator.
    pub fn world_facts(&self, state: &State) -> Result<Vec<Atom>> {
        let world = self.refresh(state)?;
        let s = |x: &str| Term::Val(Value::Str(x.to_string()));
        let mut out = Vec::new();
        for (addr, e) in self.entries(&state.resources) {
            let Some(o) = world.get(&key(&addr.typ, &e.remote)) else {
                continue;
            };
            out.push(Atom {
                pred: crate::ast::Name::user("identity"),
                args: vec![s(&addr.typ), s(&addr.name), s(&e.remote)],
                record: None,
                span: Default::default(),
            });
            // The name dform gave it, for the program's (R-189): a read of
            // the name answers it (`transform::remote_name_prelude`).
            if let Some(name) = &e.name
                && let Some((program, _)) = state::generation(name)
            {
                out.push(Atom {
                    pred: crate::ast::Name::user(crate::transform::REMOTE_NAME),
                    args: vec![s(&addr.typ), s(&addr.name), s(program), s(name)],
                    record: None,
                    span: Default::default(),
                });
            }
            let paths = self
                .schema()
                .computed_of(&addr.typ)
                .into_iter()
                .chain(self.schema().optional_computed_of(&addr.typ));
            for (path, class) in paths {
                if class == NullClass::Secret {
                    continue;
                }
                if let Some(v) = get_path(&o.computed, &path)
                    && provider::marker(v).is_none()
                {
                    // A quantity or a time is read at the edge (R-66).
                    let mut v = provider::json_to_value(v);
                    if let Some(r) = self
                        .schema()
                        .attr(&addr.typ, &path)
                        .and_then(|a| a.render())
                    {
                        v = r.read_back(v);
                    }
                    out.push(Atom {
                        pred: crate::ast::Name::user("world_attr"),
                        args: vec![s(&addr.typ), s(&e.remote), s(&path), Term::Val(v)],
                        record: None,
                        span: Default::default(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// The world's configured attributes for every address state maps to an
    /// object Read returns: what the executor compares to see whether the
    /// world moved under a deformation.
    pub fn observe(&self, state: &State) -> Result<BTreeMap<Address, Json>> {
        let world = self.refresh(state)?;
        Ok(self
            .entries(&state.resources)
            .filter_map(|(addr, e)| {
                let o = world.get(&key(&addr.typ, &e.remote))?;
                Some((addr, o.attrs.clone()))
            })
            .collect())
    }
}

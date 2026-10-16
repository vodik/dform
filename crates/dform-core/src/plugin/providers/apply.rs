//! The tick: per-resource Apply calls in flight together, across the
//! parts of the deployments it applies, each answer recorded in its
//! part's world and state.

use super::planning::Ctx;
use super::written::Holders;
use super::{Object, Providers, World, key, not_sent};
use crate::address::Address;
use crate::plugin::backend::{Call, CallError, Reply, Ticket};
use crate::plugin::link::{Link, Retry};
use crate::plugin::pb;
use crate::plugin::policy::{self, Class};
use crate::plugin::timed::timed_out;
use crate::plugin::wire;
use crate::provider::{self, Action, ActionKind, get_path, remove_path, set_path};
use crate::resources::{Adopt, Resource};
use crate::state::{self, State};
use crate::zset::Lifecycle;
use anyhow::Result;
use serde_json::{Value as Json, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

impl Providers {
    /// These providers' part of a tick ([`Tick`]): the executor's
    /// per-action Apply calls of the deployment (`executor::run_tick`).
    pub fn tick_part<'a>(
        &'a self,
        desired: &'a [Resource],
        adopts: &[Adopt],
        lifecycle: &'a Lifecycle,
    ) -> Part<'a> {
        Part {
            lifecycle,
            cloud: self,
            world: None,
            adopt_map: state::adopt_map(adopts),
            desired: desired.iter().map(|r| (r.addr.clone(), r)).collect(),
            resolved: BTreeMap::new(),
            timeline: Vec::new(),
            returned: BTreeMap::new(),
            elapsed: BTreeMap::new(),
        }
    }
}

/// One deployment's part of a tick ([`Tick`]): its providers, what its
/// plan wants, and the world its Apply calls change, as the tick's view
/// of it for the calls submitted after each answer.
pub struct Part<'a> {
    cloud: &'a Providers,
    lifecycle: &'a Lifecycle,
    /// The refresh the tick starts from, overlaid with what its calls
    /// returned; read at the first call.
    world: Option<World>,
    adopt_map: BTreeMap<Address, String>,
    desired: BTreeMap<Address, &'a Resource>,
    /// Documents applied so far this tick, for configured-attribute refs.
    resolved: BTreeMap<Address, Json>,
    /// The tick's Apply calls on the executor's clock.
    timeline: Vec<pb::Span>,
    /// What each answered Apply call returned: the object's configured
    /// attributes, `None` for a delete.
    returned: BTreeMap<Address, Option<Json>>,
    /// How long each call took on its provider's clock.
    elapsed: BTreeMap<Address, u64>,
}

/// One tick, open for Apply calls: the parts of the deployments it
/// applies (a deployment's apply has one; a tree's, one each), and their
/// calls in flight together. The executor decides the order; each call
/// is one provider Apply, submitted (`submit`) and answered later
/// (`next_answer`, then `answered`), and what it returns is its part's
/// view of the world for the calls submitted after it. The parts' calls
/// share a link where their deployments share a provider (a tree's pool):
/// each names its deployment's session, and an answer goes back to its
/// part by its link and ticket.
pub struct Tick<'a> {
    parts: Vec<Part<'a>>,
    /// The Apply calls in flight, in the order they were submitted.
    in_flight: Vec<InFlight>,
}

/// An Apply call's answer, taken ([`Tick::next_answer`]) and not yet
/// recorded ([`Tick::answered`]).
pub struct Answer {
    f: InFlight,
    answer: Result<Reply, CallError>,
}

impl Answer {
    /// The part whose call it answers.
    pub fn part(&self) -> usize {
        self.f.part
    }
}

/// One submitted Apply call, and what its answer is recorded against.
struct InFlight {
    /// The part it is of.
    part: usize,
    /// The executor's name for the call.
    id: usize,
    link: usize,
    ticket: Ticket,
    kind: ActionKind,
    addr: Address,
    /// The object's remote id before the call; empty for a create.
    remote: String,
    /// The remote name of the object the call makes or updates, when it
    /// is not the program's (`StateEntry::name`).
    named: Option<String>,
    /// The call, to send again when it failed in a way worth retrying.
    req: pb::ApplyRequest,
    /// How many times it has been sent again.
    retried: u32,
}

/// The strings inside a document value.
fn strings(v: &Json) -> Vec<&str> {
    match v {
        Json::String(s) => vec![s.as_str()],
        Json::Array(xs) => xs.iter().flat_map(strings).collect(),
        Json::Object(m) => m.values().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

/// `now`'s entries at `path` or under it as `was` has them: what an
/// update that did not send `path` leaves recorded of it.
fn keep_under(now: &mut BTreeMap<String, String>, was: &BTreeMap<String, String>, path: &str) {
    let under = |k: &str| {
        k.strip_prefix(path)
            .is_some_and(|r| r.is_empty() || r.starts_with('.') || r.starts_with('['))
    };
    now.retain(|k, _| !under(k));
    now.extend(
        was.iter()
            .filter(|(k, _)| under(k))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
}

/// What an Apply call sends ([`Tick::submit`]): its op, the object's
/// remote id, its document, whether a replacement is made first, and the
/// paths an update asks its provider to leave as they are.
struct Sending {
    op: pb::Op,
    remote: String,
    config: Option<Json>,
    create_first: bool,
    keep: Vec<String>,
}

/// An update's document as it is made ready ([`Part::update`]), the paths
/// it asks its provider to leave as the object has them beside it; its
/// provider `keeps` (it can leave a path as it is).
struct Updating {
    doc: Json,
    keep: Vec<String>,
    keeps: bool,
}

impl Updating {
    /// What is given at creation only (its `paths`): the world's value
    /// (`cur`), or its absence; a write-only one, which the world never
    /// answers, its provider leaves as the object has it when it can
    /// (R-198).
    fn at_create<'p>(
        &mut self,
        paths: impl Iterator<Item = &'p String>,
        cur: &Object,
        write_only: &[&str],
    ) {
        for p in paths {
            match get_path(&cur.attrs, p) {
                Some(v) => set_path(&mut self.doc, p, v.clone()),
                None if self.keeps
                    && write_only.contains(&p.as_str())
                    && get_path(&self.doc, p).is_some() =>
                {
                    self.leave(p)
                }
                None => remove_path(&mut self.doc, p),
            }
        }
    }

    /// A run that does not hold the master (R-164): a leaf of `derived` it
    /// proved unchanged holds a stand-in; the world's own value (`cur`'s)
    /// goes in its place, as if only the changes were sent. One the world
    /// does not answer (a write-only one) is kept by a provider that can
    /// leave it as it is, else the call is not sent.
    fn standins<'p>(
        &mut self,
        addr: &Address,
        derived: impl Iterator<Item = &'p String>,
        cur: Option<&Object>,
        standins: &crate::secrets::standin::Registry,
    ) -> Result<()> {
        for p in derived {
            if !get_path(&self.doc, p).is_some_and(|v| standins.carries(v)) {
                continue;
            }
            match cur.and_then(|c| get_path(&c.attrs, p)) {
                Some(v) if provider::marker(v).is_none() => set_path(&mut self.doc, p, v.clone()),
                _ if self.keeps => self.leave(p),
                _ => {
                    return Err(not_sent(
                        addr,
                        &format!(
                            "{} holds a value only the deployment's master derives, and the \
                             world does not answer it",
                            crate::report::attribute(addr, p)
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// The path `p` left as the object has it.
    fn leave(&mut self, p: &str) {
        remove_path(&mut self.doc, p);
        self.keep.push(p.to_string());
    }
}

impl<'a> Tick<'a> {
    /// A tick of `parts`, none of their calls made yet.
    pub fn new(parts: Vec<Part<'a>>) -> Tick<'a> {
        Tick {
            parts,
            in_flight: Vec::new(),
        }
    }

    /// Submit the Apply call for `a` of part `part`, as the executor's
    /// call `id`. Returns whether a call is in flight; an action that
    /// needs none (the delete of an object state no longer maps) is
    /// answered at once.
    pub fn submit(
        &mut self,
        part: usize,
        id: usize,
        a: &Action,
        state: &mut State,
    ) -> Result<bool> {
        let call = self.parts[part].submit((part, id), a, state)?;
        let sent = call.is_some();
        self.in_flight.extend(call);
        Ok(sent)
    }

    pub fn busy(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// The next answered Apply call, of whichever part: an answer a link
    /// already holds comes first; else the link of the oldest call in
    /// flight is waited on. A call that failed in a way worth trying
    /// again is sent again ([`Part::again`]) and waited on with the
    /// rest. What a call in flight says meanwhile goes to `events` with
    /// its part and its call's id (R-130).
    pub fn next_answer(&mut self, events: &mut dyn FnMut(usize, usize, pb::Event)) -> Answer {
        loop {
            let (mut f, answer) = self.take_answer(events);
            match self.parts[f.part].again(&mut f, answer) {
                None => self.in_flight.push(f),
                Some(answer) => return Answer { f, answer },
            }
        }
    }

    /// Record `answer` in its part's world and in `state`, its part's:
    /// identity when the call answered; a call that may have taken effect
    /// without answering (a timeout) records none. Returns the call's id
    /// and its outcome.
    pub fn answered(&mut self, answer: Answer, state: &mut State) -> (usize, Result<()>) {
        let Answer { f, answer } = answer;
        (f.id, self.parts[f.part].answered(f, answer, state))
    }

    /// The next answer, out of `in_flight` ([`Tick::next_answer`]).
    fn take_answer(
        &mut self,
        events: &mut dyn FnMut(usize, usize, pb::Event),
    ) -> (InFlight, Result<Reply, CallError>) {
        assert!(self.busy(), "internal: no Apply call in flight");
        let (parts, in_flight) = (&self.parts, &self.in_flight);
        let link_of = |f: &InFlight| &parts[f.part].cloud.links[f.link];
        let call_of = |l: &Rc<RefCell<Link>>, t: Ticket| {
            in_flight
                .iter()
                .position(|f| f.ticket == t && Rc::ptr_eq(link_of(f), l))
        };
        // The call an event is of, by its link and ticket; an event of a
        // call no longer in flight is dropped.
        let mut tell = |l: &Rc<RefCell<Link>>, t: Ticket, e: pb::Event| {
            if let Some(f) = call_of(l, t).map(|k| &in_flight[k]) {
                // Its link is in a call: named by the address alone.
                tracing::debug!(
                    target: crate::timing::TARGET,
                    "apply {}: {}",
                    crate::report::address(&f.addr),
                    says(&e)
                );
                events(f.part, f.id, e);
            }
        };
        // Those kept while a link waited for another call come first.
        let mut links: Vec<&Rc<RefCell<Link>>> = Vec::new();
        for l in in_flight.iter().map(link_of) {
            if !links.iter().any(|x| Rc::ptr_eq(x, l)) {
                links.push(l);
            }
        }
        for l in links {
            for (t, e) in l.borrow_mut().take_events() {
                tell(l, t, e);
            }
        }
        let held = in_flight
            .iter()
            .position(|f| link_of(f).borrow().has_answer(f.ticket));
        let (k, answer) = match held {
            Some(k) => {
                let f = &in_flight[k];
                let r = link_of(f).borrow_mut().take_answer(f.ticket);
                (k, r.expect("the link has the answer it holds"))
            }
            None => {
                let l = link_of(&in_flight[0]);
                let (t, r) = l.borrow_mut().next_completed(&mut |t, e| tell(l, t, e));
                let k = call_of(l, t).expect("internal: an answer to a call nobody made");
                (k, r)
            }
        };
        (self.in_flight.remove(k), answer)
    }

    /// How long part `part`'s Apply call for `addr` took on its
    /// provider's clock.
    pub fn latency(&self, part: usize, addr: &Address) -> u64 {
        self.parts[part].latency(addr)
    }

    /// Record part `part`'s Apply call's span on the executor's clock.
    pub fn span(&mut self, part: usize, addr: &Address, start_ms: u64, end_ms: u64) {
        self.parts[part].record(addr, start_ms, end_ms);
    }

    /// The tick's parts, every call answered: each ends with its state
    /// ([`Part::end`]).
    pub fn into_parts(self) -> Vec<Part<'a>> {
        assert!(!self.busy(), "internal: the tick ends with calls in flight");
        self.parts
    }
}

impl Part<'_> {
    /// The Apply call for `a`, submitted as the tick's call `id` of this,
    /// its part `part`; `None` when it needs none (the delete of an object
    /// state no longer maps), answered at once.
    fn submit(
        &mut self,
        (part, id): (usize, usize),
        a: &Action,
        state: &mut State,
    ) -> Result<Option<InFlight>> {
        let addr = &a.addr;
        if matches!(a.kind, ActionKind::Noop | ActionKind::Pending) {
            return Ok(None);
        }
        if self.world.is_none() {
            let world = self.cloud.refresh(state)?;
            self.cloud.remember(&world, state);
            self.world = Some(world);
        }
        let (doc, generated) = self.desired_doc(a, state)?;
        let named = self.named(a, &doc, generated, state);
        let Some(sending) = self.sending(a, doc, state)? else {
            return Ok(None);
        };
        let remote = sending.remote.clone();
        let req = self.request(a, sending, named.clone(), state)?;
        let link = self.cloud.route(&addr.typ);
        let ticket = self.cloud.submit(link, req.clone())?;
        self.cloud.invalidate();
        Ok(Some(InFlight {
            part,
            id,
            link,
            ticket,
            kind: a.kind.clone(),
            addr: addr.clone(),
            remote,
            named,
            req,
            retried: 0,
        }))
    }

    /// The Apply request for `a` that `sending` says, under the remote
    /// name `named`: its document's held secrets revealed into it, its
    /// idempotency key in `state`. A stand-in never leaves the process
    /// (R-164).
    fn request(
        &self,
        a: &Action,
        sending: Sending,
        named: Option<String>,
        state: &mut State,
    ) -> Result<pb::ApplyRequest> {
        let addr = &a.addr;
        let assertions = match sending.config {
            None => Vec::new(),
            Some(_) => self.assertions(addr),
        };
        // A stand-in never leaves the process (R-164): the executor is not
        // given an action that would send one ([`Providers::needs_master`]).
        if sending
            .config
            .as_ref()
            .is_some_and(|c| self.cloud.secrets.standins().carries(c))
        {
            return Err(not_sent(
                addr,
                "its document holds a value only the deployment's master derives, which this \
                 run does not hold",
            ));
        }
        let Sending {
            op,
            remote,
            config,
            create_first,
            keep,
        } = sending;
        let config = match config {
            Some(mut doc) => {
                self.cloud.reveal_into(addr, &mut doc, state)?;
                Some(doc)
            }
            None => None,
        };
        let idempotency_key = uncertain_from_here(a, addr, &remote, named, state);
        Ok(pb::ApplyRequest {
            op: op as i32,
            r#type: addr.typ.clone(),
            name: addr.name.clone(),
            remote: remote.clone(),
            config: config.as_ref().map(wire::doc),
            create_first,
            assertions,
            spans: Vec::new(),
            idempotency_key,
            keep,
            prior: state.prior(addr, &remote).map(wire::doc),
            ..Default::default()
        })
    }

    /// The world the tick's calls are made against: read at its first.
    fn world(&self) -> &World {
        self.world
            .as_ref()
            .expect("the world is read at the tick's first call")
    }

    /// The world, as the tick's calls change it.
    fn world_mut(&mut self) -> &mut World {
        self.world
            .as_mut()
            .expect("the world is read at the tick's first call")
    }

    /// The document `a` sends (none for a delete), resolved, and the
    /// remote name dform gives it when it is a create-first replacement.
    pub(super) fn desired_doc(
        &mut self,
        a: &Action,
        state: &State,
    ) -> Result<(Json, Option<String>)> {
        let addr = &a.addr;
        if matches!(a.kind, ActionKind::Delete | ActionKind::DeleteDeposed) {
            return Ok((Json::Null, None));
        }
        let r = self
            .desired
            .get(addr)
            .ok_or_else(|| not_sent(addr, "no desired resource"))?;
        let ctx = Ctx {
            cloud: self.cloud,
            world: self.world(),
            state,
            adopts: &self.adopt_map,
            resolved: &self.resolved,
            strict: Some(addr),
            retracted: &BTreeSet::new(),
            renaming: &BTreeSet::new(),
        };
        let mut doc = self.cloud.desired_doc(&ctx, r)?;
        let generated = match a.kind {
            ActionKind::Replace { create_first: true } => {
                self.name_replacement(addr, &mut doc, state)
            }
            _ => None,
        };
        self.resolved.insert(addr.clone(), doc.clone());
        Ok((doc, generated))
    }

    /// The name dform gave the object the call of `a` makes, which state
    /// records (`StateEntry::name`): `generated` for a create-first
    /// replacement, else the one `doc` keeps.
    fn named(
        &self,
        a: &Action,
        doc: &Json,
        generated: Option<String>,
        state: &State,
    ) -> Option<String> {
        match a.kind {
            ActionKind::Create | ActionKind::Replace { .. } => {
                generated.or_else(|| self.cloud.kept_name(&a.addr, doc, state))
            }
            ActionKind::Update | ActionKind::Drift => self.cloud.kept_name(&a.addr, doc, state),
            _ => None,
        }
    }

    /// What the call of `a` sends, `doc` its document; `None` when `a`
    /// needs no call (a delete of what state no longer maps), answered at
    /// once.
    fn sending(&mut self, a: &Action, doc: Json, state: &mut State) -> Result<Option<Sending>> {
        let addr = &a.addr;
        let sent = |op, remote: &String, config| Sending {
            op,
            remote: remote.clone(),
            config,
            create_first: false,
            keep: Vec::new(),
        };
        Ok(Some(match a.kind {
            ActionKind::Noop | ActionKind::Pending | ActionKind::Forget => {
                unreachable!("not submitted")
            }
            ActionKind::Delete | ActionKind::DeleteDeposed => {
                let entry = match a.kind {
                    ActionKind::Delete => state.get(addr),
                    _ => state.deposed.get(&state::key(addr)),
                };
                match entry {
                    Some(e) => sent(pb::Op::Delete, &e.remote, None),
                    None => {
                        self.recorded(a.kind.clone(), addr, &Ok(None), state)?;
                        return Ok(None);
                    }
                }
            }
            ActionKind::Create => sent(pb::Op::Create, &String::new(), Some(doc)),
            ActionKind::Replace { create_first } => {
                let Some(old) = state.get(addr).map(|e| &e.remote) else {
                    return Err(not_sent(addr, "replace without a state entry"));
                };
                Sending {
                    create_first,
                    ..sent(pb::Op::Replace, old, Some(doc))
                }
            }
            ActionKind::Adopt => {
                let Some(remote_name) = self.adopt_map.get(addr) else {
                    return Err(not_sent(addr, "adopt action missing adopt mapping"));
                };
                sent(pb::Op::Adopt, remote_name, Some(doc))
            }
            ActionKind::Update | ActionKind::Drift => self.update(addr, doc, state)?,
        }))
    }

    /// The update of `addr` to `doc`, of its object as the world has it:
    /// what is given at creation only, and what a run without the master
    /// holds a stand-in for, as the object has it.
    fn update(&self, addr: &Address, doc: Json, state: &State) -> Result<Sending> {
        let Some(e) = state.get(addr) else {
            return Err(not_sent(addr, "update without a state entry"));
        };
        let cur = self.world().get(&key(&addr.typ, &e.remote));
        let mut updating = Updating {
            doc,
            keep: Vec::new(),
            keeps: self.cloud.keeps(&addr.typ),
        };
        if let Some(cur) = cur {
            let write_only = self.cloud.schema().write_only_of(&addr.typ);
            let at_create = self.lifecycle.at_create_of(addr).map(|(p, _)| p);
            updating.at_create(at_create, cur, &write_only);
        }
        // A run that does not hold the master (R-164).
        let standins = self.cloud.secrets.standins();
        if standins.active() {
            updating.standins(addr, e.derived.keys(), cur, &standins)?;
        }
        Ok(Sending {
            op: pb::Op::Update,
            remote: e.remote.clone(),
            config: Some(updating.doc),
            create_first: false,
            keep: updating.keep,
        })
    }

    /// The refinements on `addr`'s sensitive paths: the provider checks
    /// each after materializing the secret (F DR-13 revised).
    fn assertions(&self, addr: &Address) -> Vec<pb::Assertion> {
        self.lifecycle
            .assertions
            .get(addr)
            .into_iter()
            .flatten()
            .map(|(path, c)| {
                let (op, value) = crate::refine::to_assertion(c);
                pb::Assertion {
                    path: path.clone(),
                    op,
                    value: Some(wire::doc(&value)),
                    message: format!(
                        "{} fails its refinement {c}",
                        crate::report::attribute(addr, path)
                    ),
                }
            })
            .collect()
    }

    /// Give a create-first replacement of `addr` the next generation of
    /// its remote name in `doc`, where the program's would be the old
    /// object's (`Providers::renamed`): the two exist at once until the
    /// old one is deleted, a tick later. Returns the name given.
    fn name_replacement(&self, addr: &Address, doc: &mut Json, state: &State) -> Option<String> {
        let entry = state.get(addr);
        let was = entry
            .and_then(|e| self.world().get(&key(&addr.typ, &e.remote)))
            .map(|o| &o.attrs);
        let (_, new) = self.cloud.renamed(addr, doc, was, entry)?;
        let path = self.cloud.schema().remote_name_of(&addr.typ)?;
        set_path(doc, path, json!(new));
        Some(new)
    }

    /// Record the answer to `f` in the world and in `state`: identity when
    /// the call answered; a call that may have taken effect without
    /// answering (a timeout) records none. Its outcome.
    fn answered(
        &mut self,
        f: InFlight,
        answer: Result<Reply, CallError>,
        state: &mut State,
    ) -> Result<()> {
        let cloud = self.cloud;
        cloud.invalidate();
        let result = answer.and_then(|r| {
            cloud.links[f.link]
                .borrow()
                .expect::<pb::ApplyResponse>("Apply", r)
        });
        let result = match result.map(|resp| self.record_object(&f, resp, state)) {
            Ok(Ok(resp)) => Ok(Some(resp)),
            Ok(Err(e)) => return Err(e),
            Err(e) => Err(e),
        };
        self.forget_object(&f, &result, state);
        if !matches!(
            result,
            Err(CallError::MaybeApplied(_) | CallError::Crashed(_))
        ) {
            state.uncertain.remove(&uncertain_key(&f.kind, &f.addr));
        }
        self.recorded(f.kind, &f.addr, &result, state)
    }

    /// A call that failed in a way worth trying again (a transient
    /// refusal: nothing changed, R-81) is sent again after its backoff,
    /// while its provider's budget lasts: `None`, `f` in flight again.
    /// Anything else is its answer, the budget's end saying so.
    fn again(
        &mut self,
        f: &mut InFlight,
        answer: Result<Reply, CallError>,
    ) -> Option<Result<Reply, CallError>> {
        let e = match answer {
            Err(e) if policy::class(&e) == Class::Retryable => e,
            // No answer within its timeout: what the call did is looked up
            // before it is sent again (R-81).
            Err(e) if timed_out(&e) => match self.look(f) {
                Looked::Made(reply) => return Some(Ok(*reply)),
                Looked::NotMade => e,
                Looked::Unknown(why) => {
                    return Some(Err(CallError::MaybeApplied(format!("{e}; {why}"))));
                }
            },
            answer => return Some(answer),
        };
        let mut link = self.cloud.links[f.link].borrow_mut();
        let budget = link.policy().retries;
        if f.retried >= budget {
            return Some(Err(crate::plugin::link::gave_up(e, budget)));
        }
        f.retried += 1;
        let delay = link.policy().delay(f.retried);
        let retry = Retry {
            provider: link.name_or_program().to_string(),
            call: policy::describe(&Call::Apply(f.req.clone())),
            attempt: f.retried,
            of: budget,
            delay,
            // The provider's own naming of the change dropped (R-109).
            error: crate::report::said_of(&f.addr, &e.to_string()),
        };
        link.retried(self.cloud.sessions[f.link], retry);
        std::thread::sleep(delay);
        f.ticket = link.submit(self.cloud.sessions[f.link], f.req.clone());
        None
    }

    /// What an Apply call that timed out did (R-81), so that it is sent
    /// again only when that is safe: a Create is looked up by its
    /// idempotency key (`created`), and the object it made is the call's
    /// answer, adopted, never made twice; a Delete by a Read. An Update
    /// sends the same document again, an Adopt names the same object. A
    /// Replace, or a Create whose provider cannot say what a key made, is
    /// not sent again: the next apply resolves it (`State::uncertain`).
    fn look(&self, f: &InFlight) -> Looked {
        let cloud = self.cloud;
        let at = &f.addr;
        match f.kind {
            ActionKind::Update | ActionKind::Drift | ActionKind::Adopt => Looked::NotMade,
            ActionKind::Create => {
                let key = &f.req.idempotency_key;
                let unknown = || {
                    Looked::Unknown(format!(
                        "the provider {} cannot say what its idempotency key made, so it is \
                         not sent again: the next apply looks",
                        cloud.names[f.link]
                    ))
                };
                if key.is_empty() || !cloud.links[f.link].borrow().has("managed") {
                    return unknown();
                }
                let remote = match cloud.created(at, key) {
                    Ok(Some(remote)) => remote,
                    Ok(None) => return Looked::NotMade,
                    Err(e) => return Looked::Unknown(format!("looking it up failed: {e:#}")),
                };
                match cloud.read(f.link, at, &remote, None) {
                    Ok(Some(o)) => {
                        crate::progress::line(&format!(
                            "apply {}: the Create that timed out made {remote}; it is \
                             adopted, not made again",
                            crate::report::address(at)
                        ));
                        Looked::Made(Box::new(Reply::Apply(pb::ApplyResponse {
                            remote,
                            attrs: Some(wire::doc(&o.attrs)),
                            computed: Some(wire::doc(&o.computed)),
                            ..Default::default()
                        })))
                    }
                    Ok(None) => unknown(),
                    Err(e) => Looked::Unknown(format!("reading it failed: {e:#}")),
                }
            }
            ActionKind::Delete | ActionKind::DeleteDeposed => {
                match cloud.read(f.link, at, &f.remote, None) {
                    Ok(None) => {
                        crate::progress::line(&format!(
                            "apply {}: the Delete that timed out took effect; {} is gone",
                            crate::report::address(at),
                            f.remote
                        ));
                        Looked::Made(Box::new(Reply::Apply(pb::ApplyResponse::default())))
                    }
                    Ok(Some(_)) => Looked::NotMade,
                    Err(e) => Looked::Unknown(format!("reading it failed: {e:#}")),
                }
            }
            ActionKind::Replace { .. }
            | ActionKind::Noop
            | ActionKind::Pending
            | ActionKind::Forget => {
                Looked::Unknown("a replace is not sent again: the next apply looks".into())
            }
        }
    }

    /// An answered call's object, in the tick's world and in state.
    fn record_object(
        &mut self,
        f: &InFlight,
        resp: pb::ApplyResponse,
        state: &mut State,
    ) -> Result<pb::ApplyResponse> {
        let addr = &f.addr;
        let cloud = self.cloud;
        let provider = &cloud.names[f.link];
        let world = self.world_mut();
        match f.kind {
            ActionKind::Create | ActionKind::Adopt | ActionKind::Replace { .. } => {
                if let ActionKind::Replace { .. } = f.kind {
                    self.forget_old(f, state);
                }
                let o = Providers::object(resp.attrs.as_ref(), resp.computed.as_ref())?;
                state.set(addr, provider.clone(), resp.remote.clone());
                state.set_name(addr, f.named.clone());
                state.set_computed(addr, o.computed.clone());
                self.world_mut().insert(key(&addr.typ, &resp.remote), o);
                self.record_written(addr, state, true);
            }
            ActionKind::Update | ActionKind::Drift => {
                let o = Providers::object(resp.attrs.as_ref(), resp.computed.as_ref())?;
                // A program that names it another way now: the name is its.
                state.set_name(addr, f.named.clone());
                state.set_computed(addr, o.computed.clone());
                world.insert(key(&addr.typ, &f.remote), o);
                self.record_written(addr, state, false);
            }
            ActionKind::Delete => {
                world.remove(&key(&addr.typ, &f.remote));
                state.remove(addr);
            }
            ActionKind::DeleteDeposed => {
                world.remove(&key(&addr.typ, &f.remote));
                state.deposed.remove(&state::key(addr));
            }
            ActionKind::Noop | ActionKind::Pending | ActionKind::Forget => {}
        }
        Ok(resp)
    }

    /// The digests of the write-only attributes `addr`'s object was just
    /// applied with (R-106), in state beside it; of one just `made`, the
    /// secret generations its values given at creation only read
    /// (R-198). An update sent none of those: what state had of them
    /// stays.
    fn record_written(&self, addr: &Address, state: &mut State, made: bool) {
        let Some(doc) = self.resolved.get(addr) else {
            return;
        };
        let was = state.get(addr).cloned();
        let before = was.as_ref().map(|e| e.written.clone()).unwrap_or_default();
        let holders = Holders::of(state, self.world.as_ref());
        let mut written = self.cloud.written(&addr.typ, doc, &before, &holders);
        let mut derived = self.cloud.derivations(doc);
        let given: Vec<&String> = self.lifecycle.at_create_of(addr).map(|(p, _)| p).collect();
        match (made, was) {
            (true, _) => {
                let made_with = given
                    .iter()
                    .filter_map(|p| {
                        let mut gens = BTreeMap::new();
                        for text in get_path(doc, p).map(strings).unwrap_or_default() {
                            gens.extend(self.cloud.secrets.random().generations_in(text));
                        }
                        (!gens.is_empty()).then(|| ((*p).clone(), gens))
                    })
                    .collect();
                state.set_made_with(addr, made_with);
            }
            (false, Some(was)) => {
                for p in given {
                    keep_under(&mut written, &was.written, p);
                    keep_under(&mut derived, &was.derived, p);
                }
            }
            (false, None) => {}
        }
        state.set_written(addr, written, derived);
    }

    /// A call that may have taken effect without answering: what it
    /// removes from the world is gone, and a replacement's old object is
    /// deposed or gone.
    fn forget_object(
        &mut self,
        f: &InFlight,
        result: &std::result::Result<Option<pb::ApplyResponse>, CallError>,
        state: &mut State,
    ) {
        if !matches!(result, Err(CallError::MaybeApplied(_))) {
            return;
        }
        match f.kind {
            ActionKind::Delete | ActionKind::DeleteDeposed => {
                let world = self.world_mut();
                world.remove(&key(&f.addr.typ, &f.remote));
            }
            ActionKind::Replace { .. } => self.forget_old(f, state),
            _ => {}
        }
    }

    /// The old object of a replacement: it stays, deposed, until it is
    /// deleted after what depends on it has moved to the new one; or it
    /// went first.
    fn forget_old(&mut self, f: &InFlight, state: &mut State) {
        let ActionKind::Replace { create_first } = f.kind else {
            return;
        };
        if create_first {
            state.depose(&f.addr);
        } else {
            let world = self.world_mut();
            world.remove(&key(&f.addr.typ, &f.remote));
            state.remove(&f.addr);
        }
    }

    /// What every answered call records: its time, its notes, the
    /// dependencies of what it applied, and what it returned.
    fn recorded(
        &mut self,
        kind: ActionKind,
        addr: &Address,
        result: &std::result::Result<Option<pb::ApplyResponse>, CallError>,
        state: &mut State,
    ) -> Result<()> {
        if let Ok(Some(resp)) = result {
            self.elapsed.insert(addr.clone(), resp.elapsed_ms);
            self.cloud
                .notes
                .borrow_mut()
                .extend(resp.notes.iter().cloned());
        }
        let deps = self.desired.get(addr).map(|r| r.deps.clone());
        if let (Some(deps), Ok(_) | Err(CallError::MaybeApplied(_))) = (deps, result) {
            state.set_deps(addr, deps);
        }
        if result.is_ok() && !matches!(kind, ActionKind::DeleteDeposed) {
            let world = self.world();
            let now = state
                .get(addr)
                .and_then(|e| world.get(&key(&addr.typ, &e.remote)))
                .map(|o| o.attrs.clone());
            self.returned.insert(addr.clone(), now);
        }
        // In one shape (R-109): the change as the plan prints it and
        // what happened, then the provider's message, its own naming of
        // the change dropped.
        let happened = match result {
            Ok(_) => return Ok(()),
            Err(CallError::Refused(_)) => "refused, nothing changed",
            Err(CallError::MaybeApplied(_)) => "no answer; the change may have taken effect",
            Err(CallError::Crashed(_)) => "the provider died; the change may have taken effect",
        };
        let m = result
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        Err(crate::report::Failure::of("apply", addr, happened, &m).into())
    }

    /// How long the Apply call for `addr` took on its provider's clock
    /// (the mock's chaos `latency`, else no time).
    pub fn latency(&self, addr: &Address) -> u64 {
        self.elapsed.get(addr).copied().unwrap_or(0)
    }

    /// Record an Apply call's span on the executor's clock.
    pub fn record(&mut self, addr: &Address, start_ms: u64, end_ms: u64) {
        self.timeline.push(pb::Span {
            addr: addr.to_string(),
            start_ms,
            end_ms,
        });
    }

    /// The part's tick ends: state records every desired object's
    /// dependencies, and every provider still running hears the boundary
    /// in the deployment's session (the mock's clock advances and its
    /// chaos mutations land). Returns what the part's answered Apply calls
    /// returned. Every call is answered by now ([`Tick::into_parts`]).
    pub fn end(self, state: &mut State) -> Result<BTreeMap<Address, Option<Json>>> {
        // What each object depends on, for ordering its delete later.
        for (addr, r) in &self.desired {
            state.set_deps(addr, r.deps.iter().cloned());
        }
        for (link, &session) in self.cloud.links.iter().zip(&self.cloud.sessions) {
            let mut link = link.borrow_mut();
            if link.is_dead() {
                continue;
            }
            let req = pb::ApplyRequest {
                op: pb::Op::EndTick as i32,
                spans: self.timeline.clone(),
                ..Default::default()
            };
            let r: pb::ApplyResponse = link.call(session, req)?;
            self.cloud.notes.borrow_mut().extend(r.notes);
        }
        self.cloud.invalidate();
        Ok(self.returned)
    }
}

/// What an Apply call that timed out did ([`Part::look`]).
enum Looked {
    /// It took effect: this is its answer.
    Made(Box<Reply>),
    /// It did not: send it again.
    NotMade,
    /// Nobody can say; why.
    Unknown(String),
}

/// What a provider's event of a call in flight says: its status and its
/// message, joined.
fn says(e: &pb::Event) -> String {
    [&e.status, &e.message]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(": ")
}

/// `State::uncertain`'s key for a call of `kind` on `addr`.
fn uncertain_key(kind: &ActionKind, addr: &Address) -> String {
    match kind {
        ActionKind::DeleteDeposed => state::deposed_key(addr),
        _ => state::key(addr),
    }
}

/// A call is uncertain from its submission until it answers: record it,
/// and return the idempotency key a create or a replace carries (the one
/// `executor::mark_creates` gave it before the tick, else a new one).
fn uncertain_from_here(
    a: &Action,
    addr: &Address,
    remote: &str,
    name: Option<String>,
    state: &mut State,
) -> String {
    use state::UncertainOp as Op;
    let op = match a.kind {
        ActionKind::Create => Op::Create,
        ActionKind::Replace { create_first } => Op::Replace { create_first },
        ActionKind::Update | ActionKind::Drift => Op::Update,
        ActionKind::Delete => Op::Delete,
        ActionKind::DeleteDeposed => Op::DeleteDeposed,
        ActionKind::Adopt | ActionKind::Noop | ActionKind::Pending | ActionKind::Forget => {
            return String::new();
        }
    };
    let k = uncertain_key(&a.kind, addr);
    let key = match op {
        Op::Create | Op::Replace { .. } => state
            .uncertain
            .get(&k)
            .map(|u| u.key.clone())
            .filter(|key| !key.is_empty())
            .unwrap_or_else(|| state.new_idempotency_key("", addr, a)),
        _ => String::new(),
    };
    state.uncertain.insert(
        k,
        state::Uncertain {
            op,
            remote: remote.to_string(),
            key: key.clone(),
            name,
        },
    );
    key
}

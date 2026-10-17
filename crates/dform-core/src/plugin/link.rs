//! One started provider: its handshake, its sessions' Configures, and calls
//! to it, each in the session it names ([`Session`]: the configured
//! instance it is made in; one link serves several). A blocking call
//! is a submit and a wait for its own ticket; an answer to another call
//! that arrives meanwhile (an Apply in flight) is kept for whoever waits
//! for it, and so is an event a call in flight sends meanwhile
//! ([`Link::take_events`]). Every call has the link's timeout ([`Timed`]), and a blocking
//! call that failed in a way worth trying again is retried with backoff
//! (R-81, [`super::policy`]); an Apply is the executor's to retry
//! (`providers::Tick`). A link configured from what is not settled yet is
//! provisional ([`Link::set_provisional`]).

use super::backend::{BUILD, BUILT_IN, Call, CallError, Provider, Reply, Session, Ticket, VERSION};
use super::credentials::{self, Holding, Kind, Secret};
use super::pb;
use super::policy::{self, Class, Policy, Unsettled};
use super::timed::Timed;
use super::wire;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub struct Link {
    /// The name the provider's handshake gave: what state records.
    pub name: String,
    pub capabilities: Vec<String>,
    /// The settings its handshake declares, each with whether its value is
    /// sensitive ([`crate::schema::Schema::settings`]).
    pub settings: Vec<(String, bool)>,
    /// The settings its handshake declares a credential (or a part of
    /// one), each with the credential's kind: what a Configure has held by
    /// the host in their place ([`Link::configure`]).
    credentials: BTreeMap<String, Kind>,
    /// The credentials each session's last Configure had held, until it
    /// is configured again or the link stops.
    holding: BTreeMap<Session, Vec<Holding>>,
    /// The types its handshake says it answers Health for (R-203), as
    /// the provider names them.
    pub health: Vec<String>,
    /// Its build, as its handshake gave it.
    pub version: String,
    /// What was started, for messages.
    pub program: String,
    /// How the launcher hosted it, for `provider check` (R-13b).
    pub hosting: Option<super::host::Hosting>,
    /// The location schemes its manifest declares (R-153), and its
    /// reader of them (its `Io` service): the run's reader routes a
    /// read of one to it.
    pub schemes: Vec<String>,
    pub reader: Option<std::sync::Arc<dyn crate::files::Transport>>,
    backend: Timed,
    /// Its timeout, retries and backoff.
    policy: Policy,
    /// Answers taken while waiting for another call.
    done: BTreeMap<Ticket, Result<Reply, CallError>>,
    /// The retries since they were last taken, each of the session its
    /// call was made in ([`Link::take_retries`]).
    retries: Vec<(Session, Retry)>,
    /// Events of calls in flight taken while waiting for another call.
    events: Vec<(Ticket, pb::Event)>,
    /// The provider's types under the name a `use .. as` gives it
    /// (R-115): what renames a call, and what renames its answer back.
    rename: Option<(super::wire::Rename, super::wire::Rename)>,
    /// The Queries in flight whose answer's first column is a type.
    typed: std::collections::BTreeSet<Ticket>,
    /// The provider's attribute names where the program's differ: what
    /// renames a call, and what renames its answer back.
    names: (super::wire::Names, super::wire::Names),
    /// What each call in flight whose answer is renamed named
    /// ([`super::wire::Names::answered_by`]).
    answers: BTreeMap<Ticket, String>,
    /// The sessions configured from what is not settled yet.
    provisional: BTreeMap<Session, Provisional>,
    /// The calls in flight of those sessions, each with its session.
    unsettled: BTreeMap<Ticket, Session>,
}

/// A session configured from what is not settled yet
/// ([`Link::set_provisional`]).
struct Provisional {
    unsettled: Arc<dyn Unsettled>,
    /// A call of it was answered: the first contact is made.
    contacted: bool,
    /// A call of it failed: nothing more is sent until it settles.
    failed: bool,
}

/// One failed call sent again: for the progress line and the audit log.
#[derive(Debug, Clone, PartialEq)]
pub struct Retry {
    /// The provider, as its handshake named it.
    pub provider: String,
    /// The call (`Read net.vpc["main"]`).
    pub call: String,
    /// This retry, from 1, and the budget.
    pub attempt: u32,
    pub of: u32,
    /// How long dform waits before sending it.
    pub delay: Duration,
    /// Why the last attempt failed.
    pub error: String,
}

impl Retry {
    /// `retrying the Read net.vpc["main"] call in 1.2s (retry 2 of 5): ERROR`.
    pub fn line(&self) -> String {
        format!(
            "retrying the {} call in {} (retry {} of {}): {}",
            self.call,
            policy::show(self.delay),
            self.attempt,
            self.of,
            self.error
        )
    }
}

/// A provider built with dform (`BUILT_IN`) must be this build: one built
/// from another commit, or before handshakes carried a version, plays by
/// other rules with no error saying so.
fn check_build(program: &str, hs: &pb::HandshakeResponse) -> Result<()> {
    if BUILT_IN.contains(&hs.name.as_str()) && hs.version != BUILD {
        let build = if hs.version.is_empty() {
            "a build with no version".to_string()
        } else {
            format!("version {}", hs.version)
        };
        bail!(
            "provider {program} ({}) is {build}; this dform is {BUILD}: rebuild: cargo build \
             --workspace",
            hs.name
        );
    }
    Ok(())
}

impl Link {
    /// Shake hands with the provider `backend` reaches; `program` names it
    /// in messages until it has a name.
    pub fn start(program: impl Into<String>, backend: Box<dyn Provider + Send>) -> Result<Link> {
        let program = program.into();
        let policy = Policy::default();
        let backend = Timed::new(program.clone(), backend, policy.timeout)
            .with_context(|| format!("start the thread calls to provider {program} wait on"))?;
        let mut link = Link {
            name: String::new(),
            capabilities: Vec::new(),
            settings: Vec::new(),
            credentials: BTreeMap::new(),
            holding: BTreeMap::new(),
            health: Vec::new(),
            version: String::new(),
            backend,
            program,
            hosting: None,
            schemes: Vec::new(),
            reader: None,
            policy,
            done: BTreeMap::new(),
            retries: Vec::new(),
            events: Vec::new(),
            rename: None,
            typed: Default::default(),
            names: Default::default(),
            answers: BTreeMap::new(),
            provisional: BTreeMap::new(),
            unsettled: BTreeMap::new(),
        };
        let hs: pb::HandshakeResponse = link.call(
            Session::NONE,
            pb::HandshakeRequest {
                protocol_version: VERSION,
            },
        )?;
        if hs.protocol_version != VERSION {
            bail!(
                "provider {} speaks protocol version {}; this dform speaks {VERSION}",
                link.program,
                hs.protocol_version,
            );
        }
        check_build(&link.program, &hs)?;
        link.backend.set_name(&hs.name);
        link.name = hs.name;
        link.capabilities = hs.capabilities;
        link.health = hs.health;
        link.version = hs.version;
        for s in &hs.settings {
            let Some(kind) = &s.credential else { continue };
            let kind = Kind::named(kind).ok_or_else(|| {
                anyhow!(
                    "provider {} declares the setting {} a credential of the kind {kind:?}: \
                     expected one of {}",
                    link.program,
                    s.name,
                    credentials::KINDS.join(", ")
                )
            })?;
            link.credentials.insert(s.name.clone(), kind);
        }
        link.settings = hs
            .settings
            .into_iter()
            .map(|s| (s.name, s.sensitive))
            .collect();
        Ok(link)
    }

    /// Another link to the same provider, as its handshake answered: its
    /// calls, answers and waits its own, its process the same
    /// ([`Timed::another`]). A tree's deployments planned at once, each
    /// on a thread of its own, reach a provider so (`plugin::pool`).
    pub fn attach(&self) -> Link {
        Link {
            name: self.name.clone(),
            capabilities: self.capabilities.clone(),
            settings: self.settings.clone(),
            credentials: self.credentials.clone(),
            holding: BTreeMap::new(),
            health: self.health.clone(),
            version: self.version.clone(),
            program: self.program.clone(),
            hosting: self.hosting.clone(),
            schemes: self.schemes.clone(),
            reader: self.reader.clone(),
            backend: self.backend.another(),
            policy: self.policy,
            done: BTreeMap::new(),
            retries: Vec::new(),
            events: Vec::new(),
            rename: self.rename.clone(),
            typed: Default::default(),
            names: self.names.clone(),
            answers: BTreeMap::new(),
            provisional: BTreeMap::new(),
            unsettled: BTreeMap::new(),
        }
    }

    /// Its call policy (`[providers.NAME]` in dform.toml).
    pub fn set_policy(&mut self, policy: Policy) {
        self.policy = policy;
        self.backend.set_timeout(policy.timeout);
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// `session` configured from what is not settled yet (`unsettled`: a
    /// reader's provider, from the output its upstream last published), or
    /// from what is (`None`). While it is not, its calls have
    /// [`policy::FIRST_CONTACT`] at most until one is answered, and once
    /// one failed nothing more is sent in it until it settles: as read,
    /// the failed call is sent again under the link's policy; otherwise
    /// every call fails, as the run is planned again.
    pub fn set_provisional(&mut self, session: Session, unsettled: Option<Arc<dyn Unsettled>>) {
        match unsettled {
            Some(unsettled) => self.provisional.insert(
                session,
                Provisional {
                    unsettled,
                    contacted: false,
                    failed: false,
                },
            ),
            None => self.provisional.remove(&session),
        };
        self.time_out();
    }

    /// The link's timeout: [`policy::FIRST_CONTACT`] at most while a
    /// provisional session has had no answer, else its policy's.
    fn time_out(&mut self) {
        let timeout = match self.provisional.values().any(|p| !p.contacted) {
            true => self.policy.timeout.min(policy::FIRST_CONTACT),
            false => self.policy.timeout,
        };
        self.backend.set_timeout(timeout);
    }

    /// Whether a call may be sent in `session` now: always, but in a
    /// provisional session once one of its calls failed, when it settles
    /// as read, waited for (it is settled from now on, as it is once
    /// settled anyway). An error when it settled otherwise.
    pub fn settled(&mut self, session: Session) -> Result<(), CallError> {
        let Some(p) = self.provisional.get(&session) else {
            return Ok(());
        };
        let settled = match p.failed {
            true => p.unsettled.settle().map(|()| true),
            false => p.unsettled.settled(),
        };
        match settled {
            Ok(false) => Ok(()),
            Ok(true) => {
                self.set_provisional(session, None);
                Ok(())
            }
            Err(e) => Err(CallError::Refused(format!("{e:#}"))),
        }
    }

    /// The answer `r` to a call of the provisional `session`: one answered
    /// is the first contact made; one that failed stops the session until
    /// it settles.
    fn contacted(&mut self, session: Session, r: &Result<Reply, CallError>) {
        let Some(p) = self.provisional.get_mut(&session) else {
            return;
        };
        match r {
            Ok(_) => p.contacted = true,
            Err(_) if !p.failed => {
                p.failed = true;
                p.unsettled.failed();
            }
            Err(_) => {}
        }
        self.time_out();
    }

    /// Record a retry of a call to this provider made in `session`, and
    /// say so on stderr.
    pub fn retried(&mut self, session: Session, r: Retry) {
        crate::progress::line(&r.line());
        self.retries.push((session, r));
    }

    /// The retries of `session`'s calls since the last time they were
    /// taken: a tree's deployments share the link, each its session.
    pub fn take_retries(&mut self, session: Session) -> Vec<Retry> {
        let (mine, others) = std::mem::take(&mut self.retries)
            .into_iter()
            .partition(|(s, _)| *s == session);
        self.retries = others;
        mine.into_iter().map(|(_, r)| r).collect()
    }

    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    /// Whether it answers Health for `typ`, as the program names the type
    /// (R-203).
    pub fn answers_health(&self, typ: &str) -> bool {
        let typ = match &self.rename {
            Some((_, out)) => out.name(typ),
            None => typ.to_string(),
        };
        self.health.contains(&typ)
    }

    /// The provider's name, else what was started.
    pub fn name_or_program(&self) -> &str {
        if self.name.is_empty() {
            &self.program
        } else {
            &self.name
        }
    }

    /// Whether the provider is gone.
    pub fn is_dead(&mut self) -> bool {
        self.backend.is_dead()
    }

    /// Serve the provider's types under another name (R-115): `out`
    /// renames each call's (`ca.instance` to `ovh.instance`), its inverse
    /// each answer's.
    pub fn rename(&mut self, out: super::wire::Rename) {
        self.rename = Some((out.inverse(), out));
    }

    /// Send the provider's attributes under its own names where the
    /// program's differ (`Schema::wire`), and take its answers back under
    /// the program's.
    pub fn names(&mut self, out: super::wire::Names) {
        self.names = (out.inverse(), out);
    }

    /// Start `call` in `session`.
    pub fn submit(&mut self, session: Session, call: impl Into<Call>) -> Ticket {
        let call = call.into().in_session(session);
        let answered = match self.names.1.is_empty() {
            true => None,
            false => super::wire::Names::answered_by(&call),
        };
        let call = self.names.1.call(call);
        let t = match &self.rename {
            None => self.backend.submit(call),
            Some((_, out)) => {
                let typed = matches!(&call, Call::Query(q) if super::wire::Rename::typed(&q.pred));
                let t = self.backend.submit(out.call(call));
                if typed {
                    self.typed.insert(t);
                }
                t
            }
        };
        if let Some(a) = answered {
            self.answers.insert(t, a);
        }
        if self.provisional.contains_key(&session) {
            self.unsettled.insert(t, session);
        }
        t
    }

    /// An answer from the backend, its types and attributes as the
    /// program names them.
    fn back(&mut self, t: Ticket, r: Result<Reply, CallError>) -> Result<Reply, CallError> {
        if let Some(session) = self.unsettled.remove(&t) {
            self.contacted(session, &r);
        }
        let typed = self.typed.remove(&t);
        let answered = self.answers.remove(&t);
        let r = match &self.rename {
            Some((back, _)) => r.map(|r| back.reply(r, typed)),
            None => r,
        };
        r.map(|r| self.names.0.reply(r, answered.as_deref()))
    }

    /// Send the calls submitted so far ([`Timed::flush`]).
    pub fn flush(&mut self) {
        self.backend.flush();
    }

    /// Whether the answer to `t` is in.
    pub fn has_answer(&self, t: Ticket) -> bool {
        self.done.contains_key(&t)
    }

    /// The answer to `t`, if it is in.
    pub fn take_answer(&mut self, t: Ticket) -> Option<Result<Reply, CallError>> {
        self.done.remove(&t)
    }

    /// The next answer: one already taken, else the backend's next. The
    /// events of calls in flight, those kept first, go to `events` as they
    /// come.
    pub fn next_completed(
        &mut self,
        events: &mut dyn FnMut(Ticket, pb::Event),
    ) -> (Ticket, Result<Reply, CallError>) {
        self.next_completed_by(None, events)
            .expect("with no deadline, an answer")
    }

    /// [`Link::next_completed`], waited for no later than `deadline`:
    /// `None` when it passes first.
    pub fn next_completed_by(
        &mut self,
        deadline: Option<std::time::Instant>,
        events: &mut dyn FnMut(Ticket, pb::Event),
    ) -> Option<(Ticket, Result<Reply, CallError>)> {
        for (t, e) in self.take_events() {
            events(t, e);
        }
        if let Some((t, r)) = self.done.pop_first() {
            return Some((t, r));
        }
        let (t, r) = self.backend.next_completed_by(deadline, events)?;
        Some((t, self.back(t, r)))
    }

    /// The events of calls in flight kept while waiting for another.
    pub fn take_events(&mut self) -> Vec<(Ticket, pb::Event)> {
        std::mem::take(&mut self.events)
    }

    /// Block until `t` is answered, keeping the answers to other calls and
    /// their events.
    pub fn wait(&mut self, t: Ticket) -> Result<Reply, CallError> {
        if let Some(r) = self.done.remove(&t) {
            return r;
        }
        loop {
            let kept = &mut self.events;
            let (got, r) = self.backend.next_completed(&mut |t, e| kept.push((t, e)));
            let r = self.back(got, r);
            if got == t {
                return r;
            }
            self.done.insert(got, r);
        }
    }

    /// One call, its failure classified. A failure worth trying again is
    /// retried with backoff, up to the policy's budget: a transient
    /// refusal, and a timeout of a call that changes nothing (not an
    /// Apply, whose retries are the executor's). A provisional link's
    /// failure is sent again once it settles, not before
    /// ([`Link::settled`]).
    pub fn try_call<R>(&mut self, session: Session, call: impl Into<Call>) -> Result<R, CallError>
    where
        R: TryFrom<Reply, Error = Reply>,
    {
        let call = call.into().in_session(session);
        let method = call.method();
        let mut attempt = 0;
        loop {
            self.settled(session)?;
            let t = self.submit(session, call.clone());
            let e = match self.wait(t) {
                Ok(reply) => return self.expect(method, reply),
                Err(e) => e,
            };
            let again = match policy::class(&e) {
                Class::Retryable => true,
                Class::MaybeApplied => !matches!(call, Call::Apply(_)),
                Class::Final => false,
            };
            if !again {
                return Err(e);
            }
            if self.provisional.contains_key(&session) {
                continue;
            }
            attempt += 1;
            if attempt > self.policy.retries {
                return Err(gave_up(e, self.policy.retries));
            }
            let delay = self.policy.delay(attempt);
            self.retried(
                session,
                Retry {
                    provider: self.name_or_program().to_string(),
                    call: policy::describe(&call),
                    attempt,
                    of: self.policy.retries,
                    delay,
                    // The provider's own naming of the resource as the plan
                    // prints it (R-109).
                    error: match policy::address_of(&call) {
                        Some(a) => crate::report::said_of(&a, &e.to_string()),
                        None => e.to_string(),
                    },
                },
            );
            std::thread::sleep(delay);
        }
    }

    /// One call; any failure is an error carrying the provider's message.
    pub fn call<R>(&mut self, session: Session, call: impl Into<Call>) -> Result<R>
    where
        R: TryFrom<Reply, Error = Reply>,
    {
        self.try_call(session, call).map_err(anyhow::Error::new)
    }

    /// Configure `session` (none: open one) with `config`: its answer, which
    /// names the session configured. A provider that answers none, or
    /// another than the one named, is refused, naming it and its build.
    /// A setting it declares a credential is held by the host
    /// ([`Link::hold`]) and sent as the name it is held under, never its
    /// value; the operator's credentials it may open are named.
    pub fn configure(
        &mut self,
        session: Session,
        config: pb::Value,
    ) -> Result<pb::ConfigureResponse> {
        let granted = self.hosting.as_ref().map(|h| &h.grants.credentials);
        let (config, held) =
            match self.credentials.is_empty() && granted.is_none_or(|g| g.is_empty()) {
                true => (config, Vec::new()),
                false => {
                    let mut doc = wire::from_doc(&config)?;
                    let held = self.hold(&mut doc)?;
                    (wire::doc(&doc), held)
                }
            };
        let r: pb::ConfigureResponse = self.call(
            session,
            pb::ConfigureRequest {
                config: Some(config),
                session: session.0,
            },
        )?;
        let answered = Session(r.session);
        if answered == Session::NONE || (session != Session::NONE && answered != session) {
            let said = match answered {
                Session::NONE => "no session".to_string(),
                s => format!("session {s} for session {session}"),
            };
            bail!(
                "provider {} ({} {}) answered its Configure with {said}: it was built \
                 before sessions; rebuild it",
                self.program,
                self.name_or_program(),
                match self.version.as_str() {
                    "" => "of no version",
                    v => v,
                },
            );
        }
        // The session's credentials of its last Configure are released.
        self.holding.insert(answered, held);
        Ok(r)
    }

    /// The credentials the Configure document `config` gives: each setting
    /// this provider declares a credential, its settings of one kind held
    /// by the host as one credential for it (`credentials::hold`) and each
    /// replaced by that credential's name. A Configure that is not
    /// deferred names, in its `credentials`, the operator's this provider
    /// may open ([`Link::operators`]).
    fn hold(&self, config: &mut Json) -> Result<Vec<Holding>> {
        if config.get("deferred") == Some(&Json::Bool(true)) {
            return Ok(Vec::new());
        }
        let configured = config.get("settings").is_some_and(Json::is_object);
        let named = self.operators(configured);
        if !named.is_empty() {
            config["credentials"] = Json::Object(named);
        }
        let Some(settings) = config.get_mut("settings").and_then(Json::as_object_mut) else {
            return Ok(Vec::new());
        };
        // The provider's name in dform.toml: what its host opens by.
        let to = self
            .hosting
            .as_ref()
            .map_or(self.name.as_str(), |h| h.grants.provider.as_str());
        let mut held = Vec::new();
        let mut by_kind: BTreeMap<Kind, BTreeMap<String, Secret>> = BTreeMap::new();
        for (setting, kind) in &self.credentials {
            match settings.get(setting) {
                None | Some(Json::Null) => {}
                Some(Json::String(v)) => {
                    by_kind
                        .entry(*kind)
                        .or_default()
                        .insert(setting.clone(), Secret::new(v.as_bytes().to_vec()));
                }
                Some(_) => bail!(
                    "provider {}: the setting {setting} is a credential: a string",
                    self.name_or_program()
                ),
            }
        }
        for (kind, parts) in by_kind {
            let given: Vec<String> = parts.keys().cloned().collect();
            let h = credentials::hold(kind, to, parts);
            for setting in given {
                settings.insert(setting, json!(h.name()));
            }
            held.push(h);
        }
        Ok(held)
    }

    /// The operator's credentials a Configure names, by kind, which the
    /// host opens only where dform.toml grants them, nothing held: the one
    /// dform.toml grants this provider of each kind, when it grants one
    /// (`{"signed": "signed:lab"}`; several of a kind name
    /// none); and, for a provider the program does not configure
    /// (`configured` false), the environment's of each kind it declares a
    /// setting of that has one (`{"kubeconfig": "kubeconfig:environment"}`).
    fn operators(&self, configured: bool) -> serde_json::Map<String, Json> {
        let mut granted: BTreeMap<Kind, Vec<&str>> = BTreeMap::new();
        for name in self.hosting.iter().flat_map(|h| &h.grants.credentials) {
            if let Ok((kind, _)) = credentials::parse_name(name) {
                granted.entry(kind).or_default().push(name);
            }
        }
        let mut named: serde_json::Map<String, Json> = granted
            .into_iter()
            .filter_map(|(kind, names)| match names[..] {
                [one] => Some((kind.as_str().to_string(), json!(one))),
                _ => None,
            })
            .collect();
        let environment = self.credentials.values().filter(|k| k.made_by_signer());
        for k in environment.filter(|_| !configured) {
            let name = format!("{}:{}", k.as_str(), credentials::ENVIRONMENT);
            named.entry(k.as_str()).or_insert_with(|| json!(name));
        }
        named
    }

    /// `reply`, as the answer to a `method` call.
    pub fn expect<R>(&self, method: &str, reply: Reply) -> Result<R, CallError>
    where
        R: TryFrom<Reply, Error = Reply>,
    {
        R::try_from(reply).map_err(|other| {
            CallError::Refused(format!(
                "the provider {} answered a {method} call with a {} reply",
                self.name_or_program(),
                other.method()
            ))
        })
    }
}

/// `e`, saying the budget of `retries` is spent (none: `e` as it is).
pub fn gave_up(e: CallError, retries: u32) -> CallError {
    if retries == 0 {
        return e;
    }
    let say = |m: String| format!("{m} (gave up after {retries} retries)");
    match e {
        CallError::Refused(m) => CallError::Refused(say(m)),
        CallError::MaybeApplied(m) => CallError::MaybeApplied(say(m)),
        CallError::Crashed(m) => CallError::Crashed(say(m)),
    }
}

#[cfg(test)]
mod tests {
    use super::super::backend::{FAKECLOUD, Handler, KUBERNETES, Progress, Session};
    use super::super::queue::{Order, Queue};
    use super::*;

    /// Answers the handshake as `name` at `version`.
    struct Hello(&'static str, &'static str);

    impl Handler for Hello {
        fn handle(&self, _: Session, _: Call, _: Progress) -> Result<Reply, CallError> {
            Ok(Reply::Handshake(pb::HandshakeResponse {
                protocol_version: VERSION,
                name: self.0.into(),
                capabilities: vec!["resource".into()],
                version: self.1.into(),
                settings: Vec::new(),
                health: Vec::new(),
            }))
        }
    }

    fn start(name: &'static str, version: &'static str) -> Result<Link> {
        let q = Queue::new(Hello(name, version), Order::Clock, false);
        Link::start("prov", Box::new(q))
    }

    #[test]
    fn a_built_in_provider_of_another_build_is_refused_with_the_rebuild_hint() {
        assert_eq!(start(FAKECLOUD, BUILD).unwrap().name, FAKECLOUD);
        for (name, version) in [(FAKECLOUD, "0.1.0+0000000"), (KUBERNETES, "")] {
            let e = start(name, version).err().expect("refused").to_string();
            assert!(
                e.contains(&format!("provider prov ({name})"))
                    && e.contains(BUILD)
                    && e.ends_with("rebuild: cargo build --workspace"),
                "{e}"
            );
        }
        // A provider not built with dform versions itself.
        assert_eq!(start("acme", "2.0.0").unwrap().name, "acme");
    }

    /// Refuses the first Reads as busy (503), then answers.
    struct Busy(std::sync::atomic::AtomicU32);

    impl Handler for Busy {
        fn handle(&self, s: Session, call: Call, progress: Progress) -> Result<Reply, CallError> {
            use std::sync::atomic::Ordering;
            match call {
                Call::Handshake(_) => Hello(FAKECLOUD, BUILD).handle(s, call, progress),
                Call::Read(_)
                    if self
                        .0
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                        .is_ok() =>
                {
                    Err(CallError::Refused("read x: Too Many Requests (429)".into()))
                }
                _ => Ok(Reply::Read(pb::ReadResponse::default())),
            }
        }
    }

    /// A blocking call refused as transient is sent again after its
    /// backoff, each retry recorded; past the budget the last error says
    /// the budget is spent (R-81).
    #[test]
    fn a_transient_refusal_of_a_read_is_retried_up_to_the_budget() {
        use std::time::Duration;
        let link = |busy: u32| {
            let q = Queue::new(Busy(busy.into()), Order::Clock, false);
            let mut l = Link::start("prov", Box::new(q)).unwrap();
            l.set_policy(Policy {
                retries: 2,
                backoff: Duration::from_millis(1),
                ..Policy::default()
            });
            l
        };
        let mut l = link(2);
        let r: Result<pb::ReadResponse, _> = l.try_call(Session(1), pb::ReadRequest::default());
        assert!(r.is_ok(), "{r:?}");
        let retries = l.take_retries(Session(1));
        assert_eq!(retries.len(), 2);
        assert_eq!((retries[1].attempt, retries[1].of), (2, 2));
        assert_eq!(retries[1].call, "Read");
        let mut l = link(3);
        let r: Result<pb::ReadResponse, _> = l.try_call(Session(1), pb::ReadRequest::default());
        assert_eq!(
            r.unwrap_err().to_string(),
            "read x: Too Many Requests (429) (gave up after 2 retries)"
        );
    }

    /// A provider built before sessions: its Configure answers none.
    struct Old(Vec<(Ticket, Call)>);

    impl Provider for Old {
        fn submit(&mut self, call: Call) -> Ticket {
            let t = Ticket(self.0.len() as u64);
            self.0.push((t, call));
            t
        }

        fn next_completed(
            &mut self,
            _: &mut dyn FnMut(Ticket, pb::Event),
        ) -> (Ticket, Result<Reply, CallError>) {
            let (t, call) = self.0.pop().expect("a call in flight");
            let reply = match call {
                Call::Handshake(_) => Reply::Handshake(pb::HandshakeResponse {
                    protocol_version: VERSION,
                    name: "acme".into(),
                    version: "1.0.0".into(),
                    ..Default::default()
                }),
                _ => Reply::Configure(pb::ConfigureResponse::default()),
            };
            (t, Ok(reply))
        }
    }

    /// Declares `kubeconfig` and `token` a kubeconfig's parts, and keeps
    /// each Configure it is sent.
    #[derive(Default)]
    struct Holder(std::sync::Mutex<Vec<Json>>);

    impl Handler for Holder {
        fn handle(&self, _: Session, call: Call, _: Progress) -> Result<Reply, CallError> {
            let decl = |name: &str| pb::SettingDecl {
                name: name.into(),
                sensitive: true,
                credential: Some("kubeconfig".into()),
            };
            Ok(match call {
                Call::Handshake(_) => Reply::Handshake(pb::HandshakeResponse {
                    protocol_version: VERSION,
                    name: "kube".into(),
                    settings: vec![decl("kubeconfig"), decl("token")],
                    ..Default::default()
                }),
                Call::Configure(r) => {
                    let c = wire::from_doc_or_empty(r.config.as_ref()).unwrap();
                    self.0.lock().unwrap().push(c);
                    Reply::Configure(pb::ConfigureResponse {
                        session: r.session.max(1),
                        ..Default::default()
                    })
                }
                _ => Reply::Read(pb::ReadResponse::default()),
            })
        }
    }

    /// A setting the provider declares a credential reaches it as the
    /// name the host holds it under, never its value, and only that
    /// provider opens it; a Configure with no settings names the
    /// environment's, which only a grant opens; configuring the session
    /// again lets the last one go.
    #[test]
    fn a_credential_setting_reaches_the_provider_as_a_name() {
        let holder = std::sync::Arc::new(Holder::default());
        struct Shared(std::sync::Arc<Holder>);
        impl Handler for Shared {
            fn handle(&self, s: Session, c: Call, p: Progress) -> Result<Reply, CallError> {
                self.0.handle(s, c, p)
            }
        }
        let q = Queue::new(Shared(holder.clone()), Order::Clock, false);
        let mut l = Link::start("kube", Box::new(q)).unwrap();
        const KC: &str = "current-context: c\ncontexts: [{name: c, context: {cluster: k}}]\n\
                          clusters: [{name: k, cluster: {server: 'https://k'}}]\n";
        let config = json!({"settings": {"kubeconfig": KC, "namespace": "shop"}});
        l.configure(Session::NONE, wire::doc(&config)).unwrap();
        let sent = holder.0.lock().unwrap()[0].clone();
        let name = sent["settings"]["kubeconfig"].as_str().unwrap().to_string();
        assert!(name.starts_with("kubeconfig:kube."), "{sent}");
        assert_eq!(sent["settings"]["namespace"], "shop");
        assert!(!sent.to_string().contains("https://k"), "{sent}");
        let given = credentials::held_settings(&name, "kube").unwrap();
        assert_eq!(given["kubeconfig"].expose(), KC.as_bytes());
        assert!(credentials::held_settings(&name, "other").is_none());
        // Its session again, with no settings: the environment's, by the
        // name a grant gives it, held for no one.
        l.configure(Session(1), wire::doc(&json!({}))).unwrap();
        let sent = holder.0.lock().unwrap()[1].clone();
        let env = sent["credentials"]["kubeconfig"].as_str().unwrap();
        assert_eq!(env, "kubeconfig:environment", "{sent}");
        assert!(credentials::held_settings(env, "kube").is_none(), "{sent}");
        assert!(
            credentials::held_settings(&name, "kube").is_none(),
            "let go"
        );
        // A deferred Configure holds nothing.
        l.configure(Session(1), wire::doc(&json!({"deferred": true})))
            .unwrap();
        assert_eq!(holder.0.lock().unwrap()[2], json!({"deferred": true}));
    }

    /// The operator's credential dform.toml grants a provider is named in
    /// each Configure, by its kind, with the program's settings or
    /// without, in place of the environment's: one of each kind, none of a
    /// kind granted twice, nothing held.
    #[test]
    fn a_granted_credential_is_named_by_its_kind() {
        let holder = std::sync::Arc::new(Holder::default());
        struct Shared(std::sync::Arc<Holder>);
        impl Handler for Shared {
            fn handle(&self, s: Session, c: Call, p: Progress) -> Result<Reply, CallError> {
                self.0.handle(s, c, p)
            }
        }
        let q = Queue::new(Shared(holder.clone()), Order::Clock, false);
        let mut l = Link::start("kube", Box::new(q)).unwrap();
        let granted = ["signed:lab", "bearer:a", "bearer:b", "kubeconfig:prod"];
        l.hosting = Some(super::super::host::Hosting {
            host: "native",
            manifest: None,
            grants: super::super::host::Grants {
                provider: "kube".into(),
                credentials: granted.map(String::from).into(),
                ..Default::default()
            },
        });
        let named = json!({"kubeconfig": "kubeconfig:prod", "signed": "signed:lab"});
        let config = json!({"settings": {"namespace": "shop"}});
        l.configure(Session::NONE, wire::doc(&config)).unwrap();
        l.configure(Session(1), wire::doc(&json!({}))).unwrap();
        let sent = holder.0.lock().unwrap().clone();
        assert_eq!(sent[0]["credentials"], named, "{sent:?}");
        assert_eq!(sent[0]["settings"], json!({"namespace": "shop"}));
        assert_eq!(sent[1]["credentials"], named, "{sent:?}");
        assert!(credentials::held("signed:lab", "kube").is_none());
    }

    /// A provider whose Configure answers no session is refused, naming
    /// it and its build: no call of a deployment goes to it unnamed.
    #[test]
    fn a_provider_that_answers_no_session_is_refused() {
        let mut l = Link::start("prov", Box::new(Old(Vec::new()))).unwrap();
        let e = l
            .configure(Session::NONE, pb::Value::default())
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "provider prov (acme 1.0.0) answered its Configure with no session: it was built \
             before sessions; rebuild it"
        );
    }
}

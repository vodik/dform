//! One started provider: its handshake, and calls to it. A blocking call
//! is a submit and a wait for its own ticket; an answer to another call
//! that arrives meanwhile (an Apply in flight) is kept for whoever waits
//! for it, and so is an event a call in flight sends meanwhile
//! ([`Link::take_events`]). Every call has the link's timeout ([`Timed`]), and a blocking
//! call that failed in a way worth trying again is retried with backoff
//! (R-81, [`super::policy`]); an Apply is the executor's to retry
//! (`providers::Tick`).

use super::backend::{BUILD, BUILT_IN, Call, CallError, Provider, Reply, Ticket, VERSION};
use super::pb;
use super::policy::{self, Class, Policy};
use super::timed::Timed;
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::time::Duration;

pub struct Link {
    /// The name the provider's handshake gave: what state records.
    pub name: String,
    pub capabilities: Vec<String>,
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
    /// The retries since they were last taken ([`Link::take_retries`]).
    retries: Vec<Retry>,
    /// Events of calls in flight taken while waiting for another call.
    events: Vec<(Ticket, pb::Event)>,
    /// The provider's types under the name a `use .. as` gives it
    /// (R-115): what renames a call, and what renames its answer back.
    rename: Option<(super::wire::Rename, super::wire::Rename)>,
    /// The Queries in flight whose answer's first column is a type.
    typed: std::collections::BTreeSet<Ticket>,
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
        let mut link = Link {
            name: String::new(),
            capabilities: Vec::new(),
            backend: Timed::new(program.clone(), backend, policy.timeout),
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
        };
        let hs: pb::HandshakeResponse = link.call(pb::HandshakeRequest {
            protocol_version: VERSION,
        })?;
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
        Ok(link)
    }

    /// Its call policy (`[providers.NAME]` in dform.toml).
    pub fn set_policy(&mut self, policy: Policy) {
        self.policy = policy;
        self.backend.set_timeout(policy.timeout);
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// Record a retry of a call to this provider, and say so on stderr.
    pub fn retried(&mut self, r: Retry) {
        crate::progress::line(&r.line());
        self.retries.push(r);
    }

    /// The retries since the last time they were taken.
    pub fn take_retries(&mut self) -> Vec<Retry> {
        std::mem::take(&mut self.retries)
    }

    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
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

    pub fn submit(&mut self, call: impl Into<Call>) -> Ticket {
        let call = call.into();
        let Some((_, out)) = &self.rename else {
            return self.backend.submit(call);
        };
        let typed = matches!(&call, Call::Query(q) if super::wire::Rename::typed(&q.pred));
        let t = self.backend.submit(out.call(call));
        if typed {
            self.typed.insert(t);
        }
        t
    }

    /// An answer from the backend, its types as the program names them.
    fn back(&mut self, t: Ticket, r: Result<Reply, CallError>) -> Result<Reply, CallError> {
        let typed = self.typed.remove(&t);
        match &self.rename {
            Some((back, _)) => r.map(|r| back.reply(r, typed)),
            None => r,
        }
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
        for (t, e) in self.take_events() {
            events(t, e);
        }
        if let Some(t) = self.done.keys().next().copied() {
            let r = self.done.remove(&t).expect("present");
            return (t, r);
        }
        let (t, r) = self.backend.next_completed(events);
        (t, self.back(t, r))
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
    /// Apply, whose retries are the executor's).
    pub fn try_call<R>(&mut self, call: impl Into<Call>) -> Result<R, CallError>
    where
        R: TryFrom<Reply, Error = Reply>,
    {
        let call = call.into();
        let method = call.method();
        let mut attempt = 0;
        loop {
            let t = self.submit(call.clone());
            let e = match self.wait(t) {
                Ok(reply) => return self.expect(method, reply),
                Err(e) => e,
            };
            let again = match policy::class(&e) {
                Class::Retryable => true,
                Class::MaybeApplied => !matches!(call, Call::Apply(_)),
                Class::Final => false,
            };
            attempt += 1;
            if !again {
                return Err(e);
            }
            if attempt > self.policy.retries {
                return Err(gave_up(e, self.policy.retries));
            }
            let delay = self.policy.delay(attempt);
            self.retried(Retry {
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
            });
            std::thread::sleep(delay);
        }
    }

    /// One call; any failure is an error carrying the provider's message.
    pub fn call<R>(&mut self, call: impl Into<Call>) -> Result<R>
    where
        R: TryFrom<Reply, Error = Reply>,
    {
        self.try_call(call).map_err(anyhow::Error::new)
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
    use super::super::backend::{FAKECLOUD, Handler, KUBERNETES, Progress};
    use super::super::queue::{Order, Queue};
    use super::*;

    /// Answers the handshake as `name` at `version`.
    struct Hello(&'static str, &'static str);

    impl Handler for Hello {
        fn handle(&self, _: Call, _: Progress) -> Result<Reply, CallError> {
            Ok(Reply::Handshake(pb::HandshakeResponse {
                protocol_version: VERSION,
                name: self.0.into(),
                capabilities: vec!["resource".into()],
                version: self.1.into(),
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
        fn handle(&self, call: Call, progress: Progress) -> Result<Reply, CallError> {
            use std::sync::atomic::Ordering;
            match call {
                Call::Handshake(_) => Hello(FAKECLOUD, BUILD).handle(call, progress),
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
        let r: Result<pb::ReadResponse, _> = l.try_call(pb::ReadRequest::default());
        assert!(r.is_ok(), "{r:?}");
        let retries = l.take_retries();
        assert_eq!(retries.len(), 2);
        assert_eq!((retries[1].attempt, retries[1].of), (2, 2));
        assert_eq!(retries[1].call, "Read");
        let mut l = link(3);
        let r: Result<pb::ReadResponse, _> = l.try_call(pb::ReadRequest::default());
        assert_eq!(
            r.unwrap_err().to_string(),
            "read x: Too Many Requests (429) (gave up after 2 retries)"
        );
    }
}

//! One started provider: its handshake, and calls to it. A blocking call
//! is a submit and a wait for its own ticket; an answer to another call
//! that arrives meanwhile (an Apply in flight) is kept for whoever waits
//! for it. Every call has the link's timeout ([`Timed`], R-81).

use super::backend::{BUILD, BUILT_IN, Call, CallError, Provider, Reply, Ticket, VERSION};
use super::pb;
use super::policy::Policy;
use super::timed::Timed;
use anyhow::{Result, bail};
use std::collections::BTreeMap;

pub struct Link {
    /// The name the provider's handshake gave: what state records.
    pub name: String,
    pub capabilities: Vec<String>,
    /// What was started, for messages.
    pub program: String,
    backend: Timed,
    /// Its timeout, retries and backoff.
    policy: Policy,
    /// Answers taken while waiting for another call.
    done: BTreeMap<Ticket, Result<Reply, CallError>>,
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
            policy,
            done: BTreeMap::new(),
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

    pub fn submit(&mut self, call: impl Into<Call>) -> Ticket {
        self.backend.submit(call.into())
    }

    /// Whether the answer to `t` is in.
    pub fn has_answer(&self, t: Ticket) -> bool {
        self.done.contains_key(&t)
    }

    /// The answer to `t`, if it is in.
    pub fn take_answer(&mut self, t: Ticket) -> Option<Result<Reply, CallError>> {
        self.done.remove(&t)
    }

    /// The next answer: one already taken, else the backend's next.
    pub fn next_completed(&mut self) -> (Ticket, Result<Reply, CallError>) {
        if let Some(t) = self.done.keys().next().copied() {
            let r = self.done.remove(&t).expect("present");
            return (t, r);
        }
        self.backend.next_completed()
    }

    /// Block until `t` is answered, keeping the answers to other calls.
    pub fn wait(&mut self, t: Ticket) -> Result<Reply, CallError> {
        if let Some(r) = self.done.remove(&t) {
            return r;
        }
        loop {
            let (got, r) = self.backend.next_completed();
            if got == t {
                return r;
            }
            self.done.insert(got, r);
        }
    }

    /// One call, its failure classified.
    pub fn try_call<R>(&mut self, call: impl Into<Call>) -> Result<R, CallError>
    where
        R: TryFrom<Reply, Error = Reply>,
    {
        let call = call.into();
        let method = call.method();
        let t = self.submit(call);
        let reply = self.wait(t)?;
        self.expect(method, reply)
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

#[cfg(test)]
mod tests {
    use super::super::backend::{FAKECLOUD, Handler, KUBERNETES};
    use super::super::queue::{Order, Queue};
    use super::*;

    /// Answers the handshake as `name` at `version`.
    struct Hello(&'static str, &'static str);

    impl Handler for Hello {
        fn handle(&self, _: Call) -> Result<Reply, CallError> {
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
}

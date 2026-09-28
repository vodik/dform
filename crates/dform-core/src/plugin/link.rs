//! One started provider: its handshake, and calls to it. A blocking call
//! is a submit and a wait for its own ticket; an answer to another call
//! that arrives meanwhile (an Apply in flight) is kept for whoever waits
//! for it.

use super::backend::{Call, CallError, Provider, Reply, Ticket, VERSION};
use super::pb;
use anyhow::{Result, bail};
use std::collections::BTreeMap;

pub struct Link {
    /// The name the provider's handshake gave: what state records.
    pub name: String,
    pub capabilities: Vec<String>,
    /// What was started, for messages.
    pub program: String,
    backend: Box<dyn Provider>,
    /// Answers taken while waiting for another call.
    done: BTreeMap<Ticket, Result<Reply, CallError>>,
}

impl Link {
    /// Shake hands with the provider `backend` reaches; `program` names it
    /// in messages until it has a name.
    pub fn start(program: impl Into<String>, backend: Box<dyn Provider>) -> Result<Link> {
        let mut link = Link {
            name: String::new(),
            capabilities: Vec::new(),
            program: program.into(),
            backend,
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
        link.name = hs.name;
        link.capabilities = hs.capabilities;
        Ok(link)
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

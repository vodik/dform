//! The direct and wire backends: a provider linked in (a [`Handler`]),
//! its calls queued. `submit` queues a call; `next_completed` answers one.
//! Which one is the queue's [`Order`]:
//!
//! * [`Order::Clock`]: simulated time, what `--parallel` overlaps (the
//!   mock's chaos `latency`). A call starts when it is submitted, at the
//!   queue's clock, and is run then, in submit order; it ends its answer's
//!   `elapsed_ms` later, and one that fails takes no time. The next answer
//!   is the call that ends first (a failure before a success at the same
//!   time, then submit order), and the clock moves to its end. With one
//!   call in flight at a time this is submit order.
//! * [`Order::Seed`]: the next answer is a queued call picked by a seeded
//!   generator, and it is run as it is picked: property tests explore
//!   interleavings with it.
//!
//! The wire backend encodes every call and every answer through prost and
//! decodes it again, so what crosses is exactly what the protocol can
//! carry; the direct backend hands the messages over as they are.

use super::backend::{Call, CallError, Handler, Provider, Reply, Ticket};
use prost::Message;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

/// Which queued call answers next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Simulated time: the call that ends first.
    Clock,
    /// A call picked by a generator seeded with this.
    Seed(u64),
}

pub struct Queue<H> {
    handler: H,
    order: Order,
    /// Encode and decode every message (the wire backend).
    wire: bool,
    next: u64,
    /// The clock (`Order::Clock`), in ms.
    now: u64,
    /// The generator's state (`Order::Seed`).
    rng: u64,
    /// Submitted calls not yet run: ticket, call, start.
    queued: Vec<(Ticket, Call, u64)>,
    /// Calls run, not yet answered, the next first: end, whether it
    /// succeeded, ticket.
    ran: BinaryHeap<Reverse<(u64, bool, Ticket)>>,
    answers: HashMap<Ticket, Result<Reply, CallError>>,
}

impl<H: Handler> Queue<H> {
    pub fn new(handler: H, order: Order, wire: bool) -> Queue<H> {
        let rng = match order {
            Order::Seed(s) => s,
            Order::Clock => 0,
        };
        Queue {
            handler,
            order,
            wire,
            next: 0,
            now: 0,
            rng,
            queued: Vec::new(),
            ran: BinaryHeap::new(),
            answers: HashMap::new(),
        }
    }

    pub fn handler(&self) -> &H {
        &self.handler
    }

    /// splitmix64.
    fn random(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    fn run(&self, call: Call) -> Result<Reply, CallError> {
        if !self.wire {
            return self.handler.handle(call);
        }
        let answer = self.handler.handle(across_call(&call)?);
        answer.and_then(|r| across_reply(&r))
    }
}

impl<H: Handler> Provider for Queue<H> {
    fn submit(&mut self, call: Call) -> Ticket {
        let t = Ticket(self.next);
        self.next += 1;
        self.queued.push((t, call, self.now));
        t
    }

    fn next_completed(&mut self) -> (Ticket, Result<Reply, CallError>) {
        match self.order {
            Order::Seed(_) => {
                assert!(!self.queued.is_empty(), "internal: no call in flight");
                let k = (self.random() % self.queued.len() as u64) as usize;
                let (t, call, _) = self.queued.swap_remove(k);
                (t, self.run(call))
            }
            Order::Clock => {
                for (t, call, start) in std::mem::take(&mut self.queued) {
                    let answer = self.run(call);
                    let took = match &answer {
                        Ok(Reply::Apply(r)) => r.elapsed_ms,
                        _ => 0,
                    };
                    self.ran.push(Reverse((start + took, answer.is_ok(), t)));
                    self.answers.insert(t, answer);
                }
                let Reverse((end, _, t)) = self.ran.pop().expect("internal: no call in flight");
                self.now = self.now.max(end);
                (t, self.answers.remove(&t).expect("run"))
            }
        }
    }

    fn is_dead(&mut self) -> bool {
        self.handler.is_dead()
    }
}

fn across<M: Message + Default>(m: &M) -> Result<M, CallError> {
    M::decode(m.encode_to_vec().as_slice())
        .map_err(|e| CallError::Refused(format!("the wire does not carry the message: {e}")))
}

/// A call as the provider receives it across the wire.
fn across_call(c: &Call) -> Result<Call, CallError> {
    Ok(match c {
        Call::Handshake(r) => Call::Handshake(across(r)?),
        Call::Configure(r) => Call::Configure(across(r)?),
        Call::Schema(r) => Call::Schema(across(r)?),
        Call::Query(r) => Call::Query(across(r)?),
        Call::Read(r) => Call::Read(across(r)?),
        Call::Plan(r) => Call::Plan(across(r)?),
        Call::Apply(r) => Call::Apply(across(r)?),
        Call::Import(r) => Call::Import(across(r)?),
    })
}

/// An answer as dform receives it across the wire (a Query's rows one by
/// one, as the stream carries them).
fn across_reply(r: &Reply) -> Result<Reply, CallError> {
    Ok(match r {
        Reply::Handshake(r) => Reply::Handshake(across(r)?),
        Reply::Configure(r) => Reply::Configure(across(r)?),
        Reply::Schema(r) => Reply::Schema(across(r)?),
        Reply::Query(rows) => Reply::Query(rows.iter().map(across).collect::<Result<_, _>>()?),
        Reply::Read(r) => Reply::Read(across(r)?),
        Reply::Plan(r) => Reply::Plan(across(r)?),
        Reply::Apply(r) => Reply::Apply(across(r)?),
        Reply::Import(r) => Reply::Import(across(r)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::pb;
    use std::cell::RefCell;

    /// Answers every Apply with the latency its name asks for, and fails
    /// the one named `fail`; records the order it ran them in.
    #[derive(Default)]
    struct Clocked {
        ran: RefCell<Vec<String>>,
    }

    impl Handler for Clocked {
        fn handle(&self, call: Call) -> Result<Reply, CallError> {
            let Call::Apply(r) = call else {
                return Err(CallError::Refused("apply only".into()));
            };
            self.ran.borrow_mut().push(r.name.clone());
            if r.name == "fail" {
                return Err(CallError::Refused("failed".into()));
            }
            Ok(Reply::Apply(pb::ApplyResponse {
                elapsed_ms: r.name.trim_start_matches('t').parse().unwrap_or(0),
                ..Default::default()
            }))
        }
    }

    fn apply(name: &str) -> Call {
        Call::Apply(pb::ApplyRequest {
            name: name.into(),
            ..Default::default()
        })
    }

    fn answers(q: &mut Queue<Clocked>, n: usize) -> Vec<Ticket> {
        (0..n).map(|_| q.next_completed().0).collect()
    }

    /// On the clock, the call that ends first answers first; a failure,
    /// which takes no time, before a success ending at the same time.
    #[test]
    fn the_clock_answers_the_call_that_ends_first() {
        let mut q = Queue::new(Clocked::default(), Order::Clock, false);
        let slow = q.submit(apply("t100"));
        let fast = q.submit(apply("t50"));
        let now = q.submit(apply("t0"));
        let fail = q.submit(apply("fail"));
        assert_eq!(answers(&mut q, 4), [fail, now, fast, slow]);
        // Run in submit order, when the first answer was asked for.
        assert_eq!(*q.handler().ran.borrow(), ["t100", "t50", "t0", "fail"]);
        // A call submitted now starts at the clock: 100.
        let late = q.submit(apply("t10"));
        let early = q.submit(apply("t0"));
        assert_eq!(answers(&mut q, 2), [early, late]);
    }

    /// A seed picks an interleaving; the same seed the same one, and every
    /// call is answered once.
    #[test]
    fn a_seed_picks_an_interleaving() {
        let order = |seed| {
            let mut q = Queue::new(Clocked::default(), Order::Seed(seed), false);
            let ts: Vec<Ticket> = (0..6).map(|i| q.submit(apply(&format!("t{i}")))).collect();
            let got = answers(&mut q, 6);
            let mut sorted = got.clone();
            sorted.sort();
            assert_eq!(sorted, ts);
            got
        };
        assert_eq!(order(7), order(7));
        assert!((0..8).any(|s| order(s) != order(0)));
    }

    /// Across the wire a message is what prost makes of it.
    #[test]
    fn the_wire_encodes_and_decodes() {
        let doc = crate::plugin::wire::doc(&serde_json::json!({"a": [1, 0.5, "x"]}));
        let call = Call::Apply(pb::ApplyRequest {
            config: Some(doc),
            ..Default::default()
        });
        assert_eq!(across_call(&call).unwrap(), call);
    }
}

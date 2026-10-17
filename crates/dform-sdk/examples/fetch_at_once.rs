//! A provider whose Read sends two requests at once (two
//! [`dform_sdk::Http::request`]s awaited together, [`dform_sdk::join`]),
//! the probe of the async ABI: built as a component, both are subtasks of
//! the host's `send` in flight from the one call, with no thread in the
//! component (tests/host_wasm.rs,
//! `a_components_read_keeps_two_requests_in_flight`).
//!
//!   cargo build -p dform-sdk --example fetch_at_once --target wasm32-wasip3 --features component
//!
//! The Read's `remote` is the base URL; it fetches `/a` and `/b` under it
//! and is found when both answered 200. A Read of `web.one` fetches its
//! `remote` alone, awaiting it, and one of `web.blocked` the same waiting
//! on this task ([`dform_sdk::block_on`], a synchronous handler's way):
//! two of them at once say whether the instance answers its other calls
//! while one waits. A Read of `web.locked` fetches its `remote` holding
//! the provider's one lock ([`dform_sdk::sync::Mutex`]): two at once, the
//! second awaits the lock and is woken by the first's task letting it go.
//! Its requests are sent once ([`once`]): a 503 is what it measures. A
//! Read of `web.later` answers not yet the first two times it is asked:
//! the host asks it again.

use dform_core::plugin::backend::{Call, Progress, Reply, Session, VERSION};
use dform_sdk::{CallError, Handler, Request, Unanswered, host, pb};
use std::sync::OnceLock;

/// The lock a `web.locked` Read holds across its request.
static LOCK: OnceLock<dform_sdk::sync::Mutex<()>> = OnceLock::new();

struct FetchAtOnce;

impl Handler for FetchAtOnce {
    async fn handle(&self, _: Session, call: Call, _: Progress<'_>) -> Result<Reply, Unanswered> {
        match call {
            Call::Handshake(_) => Ok(Reply::Handshake(pb::HandshakeResponse {
                protocol_version: VERSION,
                name: "fetch-at-once".into(),
                capabilities: vec!["resource".into()],
                ..Default::default()
            })),
            Call::Read(r) if r.r#type == "web.one" => {
                let answer = host().http.request(once(&r.remote)).await;
                found(answer.map_err(|e| CallError::Refused(e.message))?.status == 200)
            }
            Call::Read(r) if r.r#type == "web.locked" => {
                let _held = LOCK.get_or_init(Default::default).lock().await;
                let answer = host().http.request(once(&r.remote)).await;
                found(answer.map_err(|e| CallError::Refused(e.message))?.status == 200)
            }
            Call::Read(r) if r.r#type == "web.blocked" => {
                let answer = dform_sdk::block_on(host().http.request(once(&r.remote)));
                found(answer.map_err(|e| CallError::Refused(e.message))?.status == 200)
            }
            Call::Read(r) if r.r#type == "web.later" => {
                // Not there the first two times it is asked: an answer,
                // the host's to ask again.
                let n = ASKED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match n < 2 {
                    true => Err(dform_sdk::not_yet(
                        std::time::Duration::from_millis(200),
                        format!("web.later {}: not there yet", r.name),
                    )),
                    false => found(true),
                }
            }
            Call::Read(r) => {
                let get = |p: &str| host().http.request(once(format!("{}/{p}", r.remote)));
                let (a, b) = dform_sdk::join(get("a"), get("b")).await;
                let mut both = true;
                for answer in [a, b] {
                    both &= answer.map_err(|e| CallError::Refused(e.message))?.status == 200;
                }
                found(both)
            }
            other => Err(CallError::Refused(format!("{} only", other.method())).into()),
        }
    }
}

/// How many times a `web.later` Read was asked.
static ASKED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn found(found: bool) -> Result<Reply, Unanswered> {
    Ok(Reply::Read(pb::ReadResponse {
        found,
        ..Default::default()
    }))
}

dform_sdk::provider!(FetchAtOnce);

/// A GET the host sends once: not idempotent, so a 503 (the probe's
/// server saying a request came alone) is the answer, never retried.
fn once(url: impl Into<String>) -> Request {
    Request::get(url).idempotent(false)
}

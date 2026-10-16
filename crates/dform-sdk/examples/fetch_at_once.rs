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
//! while one waits.

use dform_core::plugin::backend::{Call, Progress, Reply, Session, VERSION};
use dform_sdk::{CallError, Handler, Request, host, pb};

struct FetchAtOnce;

impl Handler for FetchAtOnce {
    async fn handle(&self, _: Session, call: Call, _: Progress<'_>) -> Result<Reply, CallError> {
        match call {
            Call::Handshake(_) => Ok(Reply::Handshake(pb::HandshakeResponse {
                protocol_version: VERSION,
                name: "fetch-at-once".into(),
                capabilities: vec!["resource".into()],
                ..Default::default()
            })),
            Call::Read(r) if r.r#type == "web.one" => {
                let answer = host().http.request(Request::get(&r.remote)).await;
                found(answer.map_err(|e| CallError::Refused(e.message))?.status == 200)
            }
            Call::Read(r) if r.r#type == "web.blocked" => {
                let answer = dform_sdk::block_on(host().http.request(Request::get(&r.remote)));
                found(answer.map_err(|e| CallError::Refused(e.message))?.status == 200)
            }
            Call::Read(r) => {
                let get = |p: &str| {
                    host()
                        .http
                        .request(Request::get(format!("{}/{p}", r.remote)))
                };
                let (a, b) = dform_sdk::join(get("a"), get("b")).await;
                let mut both = true;
                for answer in [a, b] {
                    both &= answer.map_err(|e| CallError::Refused(e.message))?.status == 200;
                }
                found(both)
            }
            other => Err(CallError::Refused(format!("{} only", other.method()))),
        }
    }
}

fn found(found: bool) -> Result<Reply, CallError> {
    Ok(Reply::Read(pb::ReadResponse {
        found,
        ..Default::default()
    }))
}

dform_sdk::provider!(FetchAtOnce);

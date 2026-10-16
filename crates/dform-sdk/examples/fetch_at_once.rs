//! A provider whose Read sends two requests at once
//! ([`dform_sdk::Http::request_all`]), the probe of the async ABI: built
//! as a component, both are subtasks of the host's `send` in flight from
//! the one call, with no thread in the component (tests/host_wasm.rs,
//! `a_components_read_keeps_two_requests_in_flight`).
//!
//!   cargo build -p dform-sdk --example fetch_at_once --target wasm32-wasip3 --features component
//!
//! The Read's `remote` is the base URL; it fetches `/a` and `/b` under it
//! and is found when both answered 200.

use dform_core::plugin::backend::{Call, Progress, Reply, Session, VERSION};
use dform_sdk::{CallError, Handler, Request, pb};

struct FetchAtOnce;

impl Handler for FetchAtOnce {
    fn handle(&self, _: Session, call: Call, _: Progress) -> Result<Reply, CallError> {
        match call {
            Call::Handshake(_) => Ok(Reply::Handshake(pb::HandshakeResponse {
                protocol_version: VERSION,
                name: "fetch-at-once".into(),
                capabilities: vec!["resource".into()],
                ..Default::default()
            })),
            Call::Read(r) => {
                let answers = dform_sdk::host()
                    .http
                    .request_all(["a", "b"].map(|p| Request::get(format!("{}/{p}", r.remote))));
                let mut found = true;
                for a in answers {
                    found &= a.map_err(|e| CallError::Refused(e.message))?.status == 200;
                }
                Ok(Reply::Read(pb::ReadResponse {
                    found,
                    ..Default::default()
                }))
            }
            other => Err(CallError::Refused(format!("{} only", other.method()))),
        }
    }
}

dform_sdk::provider!(FetchAtOnce);

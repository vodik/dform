//! Retries are the host's (`Services::request`): a test of its own
//! process, so its retries' pauses never share the process's connection
//! pool with the unit tests' timings.

use dform_core::http::fake::Server;
use dform_core::plugin::host::{Calls, Grants, HttpRequest};
use dform_host::services::{RETRIES, Services};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// An idempotent GET the server answers 503 once is sent again and
/// answered; a POST is answered the 503, for the provider to classify; a
/// GET that keeps failing is sent `1 + RETRIES` times in all.
#[test]
fn the_host_sends_an_idempotent_request_again_and_no_other() {
    let server = Server::http1();
    let seen = Arc::new(Mutex::new(BTreeMap::<String, u32>::new()));
    let counts = seen.clone();
    server.answer(move |s| {
        let mut c = counts.lock().unwrap();
        let n = c.entry(format!("{} {}", s.method, s.path)).or_default();
        *n += 1;
        match (*n, s.path.as_str()) {
            (1, _) | (_, "/down") => (503, Vec::new(), b"busy".to_vec()),
            _ => (200, Vec::new(), b"ok".to_vec()),
        }
    });
    let host = Services::new(Grants::none("p"));
    let send = |method: &str, path: &str| {
        let req = HttpRequest {
            method: method.into(),
            url: server.url(path),
            idempotent: HttpRequest::idempotent_method(method),
            ..HttpRequest::default()
        };
        host.send(req, None, None).unwrap().status
    };
    assert_eq!(send("GET", "/a"), 200);
    assert_eq!(send("POST", "/b"), 503);
    assert_eq!(send("GET", "/down"), 503);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        [("GET /a", 2), ("GET /down", 1 + RETRIES), ("POST /b", 1)]
            .map(|(k, n)| (k.to_string(), n))
            .into()
    );
}

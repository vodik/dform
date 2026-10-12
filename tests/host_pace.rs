//! The host paces a provider's requests (`dform_host::ratelimit`): a
//! window per bucket that the server's answers size, a 429's
//! `Retry-After` pausing its bucket, and no rate declared anywhere. Driven
//! as a native SDK provider sends (`dform_sdk::native::Grpc`, the host's
//! `send`), against a server that takes one request a second.

use dform::plugin::host::{Calls, Grants, HttpRequest};
use dform_host::services::Services;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A server on the loopback that answers one request a second: a request
/// sooner than a second after the last it answered is a 429 with
/// `Retry-After: 1`. Returns its address and how many 429s it answered.
fn one_a_second() -> (SocketAddr, Arc<Mutex<usize>>) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let last: Arc<Mutex<Option<Instant>>> = Arc::default();
    let refused: Arc<Mutex<usize>> = Arc::default();
    let seen = refused.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let (last, refused) = (last.clone(), refused.clone());
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let reply = {
                    let mut last = last.lock().unwrap();
                    match *last {
                        Some(t) if t.elapsed() < Duration::from_secs(1) => {
                            *refused.lock().unwrap() += 1;
                            "HTTP/1.1 429 Too Many Requests\r\nretry-after: 1\r\n"
                        }
                        _ => {
                            *last = Some(Instant::now());
                            "HTTP/1.1 200 OK\r\n"
                        }
                    }
                };
                let mut s = s;
                let _ = write!(s, "{reply}content-length: 0\r\nconnection: close\r\n\r\n");
            });
        }
    });
    (addr, seen)
}

/// Three requests at once to a server that takes one a second, each
/// retried by its provider on a 429 as the SDK's retry would: the first
/// 429's `Retry-After` pauses the bucket and halves its window, so the
/// three are answered a second apart and the server refuses a couple,
/// not every retry a provider could make in two seconds.
#[test]
fn a_bucket_is_paced_by_what_the_server_answers() {
    let services = Services::new(Grants {
        provider: "pace".into(),
        ..Grants::default()
    });
    let served = dform_host::grpc::serve(services).unwrap();
    let c = Arc::new(dform_sdk::native::Grpc::dial(&served.address).unwrap());
    let (addr, refused) = one_a_second();
    let start = Instant::now();
    let calls: Vec<_> = (0..3)
        .map(|_| {
            let c = c.clone();
            std::thread::spawn(move || {
                loop {
                    let r = c
                        .send(
                            HttpRequest {
                                method: "GET".into(),
                                url: format!("http://{addr}/"),
                                bucket: Some("things.list".into()),
                                ..HttpRequest::default()
                            },
                            None,
                            None,
                        )
                        .unwrap();
                    if r.status == 200 {
                        return start.elapsed();
                    }
                    assert_eq!(r.status, 429, "answered as the server answered");
                }
            })
        })
        .collect();
    let mut answered: Vec<Duration> = calls.into_iter().map(|c| c.join().unwrap()).collect();
    answered.sort();
    for pair in answered.windows(2) {
        assert!(
            pair[1] - pair[0] >= Duration::from_millis(900),
            "a second apart: {answered:?}"
        );
    }
    let refused = *refused.lock().unwrap();
    assert!(
        (1..=4).contains(&refused),
        "a 429 paused the bucket: {refused} refused, {answered:?}"
    );
}

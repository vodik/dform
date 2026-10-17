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

/// A bucket whose server speaks HTTP/2 grows its window no wider than
/// the streams the server takes at once on its connection, which each
/// answer tells; its requests at once are streams of that one
/// connection, made with the credential's CA.
#[test]
fn a_window_keeps_under_the_servers_stream_ceiling() {
    use base64::Engine;
    use dform::http::fake::{CA, Server};
    use dform::plugin::backend::Session;
    use dform::plugin::credentials::{self, Kind, Secret};
    use dform_host::ratelimit::Limiter;
    let server = Arc::new(Server::h2(3));
    let ca = base64::engine::general_purpose::STANDARD.encode(CA);
    let kubeconfig = Secret::new(
        format!(
            "current-context: c\ncontexts: [{{name: c, context: {{cluster: k, user: u}}}}]\n\
             clusters: [{{name: k, cluster: {{server: '{}', certificate-authority-data: {ca}}}}}]\n\
             users: [{{name: u, user: {{token: t}}}}]\n",
            server.url("")
        )
        .into_bytes(),
    );
    let held = credentials::hold(
        Kind::Kubeconfig,
        "streams",
        [("kubeconfig".to_string(), kubeconfig)].into(),
    );
    let name = held.name();
    let limiter = Arc::new(Limiter::new("streams"));
    let services = Services::paced(Grants::none("streams"), limiter.clone())
        .with_signer(Arc::new(dform_provider_k8s_signer::Kube));
    let services = Arc::new(services);
    let auth = services.open(name).unwrap().handle;
    let get = |i: usize| HttpRequest {
        method: "GET".into(),
        url: server.url(&format!("/{i}")),
        bucket: Some("things.get".into()),
        ..HttpRequest::default()
    };
    // Ten answers would open a window of twelve.
    for i in 0..10 {
        assert_eq!(services.send(get(i), Some(auth), None).unwrap().status, 200);
    }
    assert_eq!(limiter.window(Session::NONE, Some("things.get")).0, 3);
    server.slow(Duration::from_millis(100));
    let calls: Vec<_> = (0..9)
        .map(|i| {
            let (services, get) = (services.clone(), get(i));
            std::thread::spawn(move || services.send(get, Some(auth), None).unwrap().status)
        })
        .collect();
    for c in calls {
        assert_eq!(c.join().unwrap(), 200);
    }
    assert_eq!(server.connections(), 1, "one TLS handshake");
    assert_eq!(server.most_at_once(), 3);
}

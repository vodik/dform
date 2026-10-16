use super::*;
use crate::plugin::credentials::Secret;
use crate::plugin::host::Class;
use fake::Server;

fn get(url: String) -> HttpRequest {
    HttpRequest {
        method: "GET".into(),
        url,
        timeout: Some(Duration::from_secs(5)),
        ..HttpRequest::default()
    }
}

/// The host applies a credential's header; the provider names the
/// credential and never sees the value. A tunnel is the URL's host
/// reached at the tunnel's end.
#[test]
fn a_credential_is_applied_and_a_tunnel_routes() {
    let server = Server::http1();
    server.answer(|s| {
        let auth = format!(
            "{} {}",
            s.header("host").unwrap_or_default(),
            s.header("authorization").unwrap_or_default()
        );
        (200, Vec::new(), auth.into_bytes())
    });
    let cred = Credential {
        name: "bearer:t".into(),
        headers: vec![("Authorization".into(), Secret::new(b"Bearer tok".to_vec()))],
        ..Credential::default()
    };
    let r = send(
        get("http://api.internal.example/v1".into()),
        Some(&cred),
        Some(server.addr),
    )
    .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"api.internal.example Bearer tok");
}

/// Nobody listening is a failure worth retrying: nothing was sent.
#[test]
fn a_refused_connection_is_retryable() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let e = send(get(format!("http://127.0.0.1:{port}/")), None, None).unwrap_err();
    assert_eq!(e.class, Class::Retryable, "{}", e.message);
}

/// No answer in time may have taken effect.
#[test]
fn a_request_with_no_answer_in_time_may_have_taken_effect() {
    let server = Server::http1();
    server.slow(Duration::from_secs(2));
    let mut req = get(server.url("/slow"));
    req.timeout = Some(Duration::from_millis(200));
    let e = send(req, None, None).unwrap_err();
    assert_eq!(e.class, Class::MaybeApplied, "{}", e.message);
}

/// One endpoint over HTTP/1.1: a request after another goes over the
/// connection the first one kept alive.
#[test]
fn requests_one_after_another_share_a_kept_alive_connection() {
    let server = Server::http1();
    for path in ["/a", "/b", "/c"] {
        let r = send(get(server.url(path)), None, None).unwrap();
        assert_eq!(r.body, path.as_bytes());
    }
    assert_eq!(server.connections(), 1);
}

/// One endpoint over HTTP/2: requests at once are streams of one
/// connection, one TLS handshake; no more of them at once than the
/// server's ceiling, which each answer tells.
#[test]
fn requests_at_once_are_streams_of_one_connection() {
    let server = Arc::new(Server::h2(2));
    server.slow(Duration::from_millis(200));
    let cred = Arc::new(Server::credential());
    let sent: Vec<_> = (0..6)
        .map(|i| {
            let (server, cred) = (server.clone(), cred.clone());
            std::thread::spawn(move || {
                let mut req = get(server.url(&format!("/{i}")));
                req.method = "PUT".into();
                req.body = vec![b'x'; 100];
                exchange(req, Some(&cred), None)
            })
        })
        .collect();
    for (i, s) in sent.into_iter().enumerate() {
        let x = s.join().unwrap();
        let r = x.answer.unwrap();
        assert_eq!(r.body, format!("/{i}").as_bytes());
        assert_eq!(x.pace.streams, Some(2));
    }
    assert_eq!(server.connections(), 1);
    assert_eq!(server.requests(), 6);
    assert_eq!(server.most_at_once(), 2);
}

/// A body read as it comes.
#[test]
fn a_streamed_answer_is_read_as_it_comes() {
    let server = Server::http1();
    server.answer(|_| {
        (
            200,
            vec![("x-part".into(), "1".into())],
            vec![b'z'; 100_000],
        )
    });
    let mut s = stream(get(server.url("/pack")), None, None).unwrap();
    assert_eq!(s.status, 200);
    assert!(s.headers.iter().any(|(k, v)| k == "x-part" && v == "1"));
    let mut body = Vec::new();
    s.body.read_to_end(&mut body).unwrap();
    assert_eq!(body.len(), 100_000);
    // Read to its end, the connection is kept for the next request.
    send(get(server.url("/again")), None, None).unwrap();
    assert_eq!(server.connections(), 1);
}

/// Over HTTP/2, a request whose body cannot start because the server
/// has not taken the bodies before it (the connection's window spent)
/// stalled; one whose body went at once did not.
#[test]
fn a_body_waiting_on_a_spent_window_is_a_stall() {
    let server = Arc::new(Server::h2(10));
    let cred = Arc::new(Server::credential());
    // The connection made, its settings known.
    send(get(server.url("/")), Some(&cred), None).unwrap();
    server.slow(Duration::from_millis(300));
    let sent: Vec<_> = (0..3)
        .map(|i| {
            let (server, cred) = (server.clone(), cred.clone());
            std::thread::sleep(Duration::from_millis(30));
            std::thread::spawn(move || {
                let mut req = get(server.url(&format!("/{i}")));
                req.method = "PUT".into();
                req.body = vec![b'x'; 40_000];
                exchange(req, Some(&cred), None).pace.stalled
            })
        })
        .collect();
    let stalled: Vec<bool> = sent.into_iter().map(|s| s.join().unwrap()).collect();
    // 64 KiB of window: the first body goes whole, the second starts,
    // the third waits for the first two to be read.
    assert_eq!(stalled, [false, false, true]);
}

/// An HTTP/2 connection lets the server send 2 MiB on a stream and 5 MiB
/// in all before the client reads (hyper's defaults): HTTP/2's own 64 KiB
/// held a megabytes answer (an API server's OpenAPI document) to 64 KiB a
/// round trip.
#[test]
fn an_http2_connection_takes_2_mib_a_stream_and_5_mib_in_all() {
    let server = Server::h2(10);
    send(get(server.url("/")), Some(&Server::credential()), None).unwrap();
    assert_eq!(
        server.windows(),
        fake::Windows {
            stream: 2 * 1024 * 1024,
            connection: 5 * 1024 * 1024,
        }
    );
}

/// A burst of requests to an HTTP/1.1 endpoint opens a connection each;
/// the endpoint keeps eight of them idle for what comes next.
#[test]
fn an_endpoint_keeps_eight_connections_idle() {
    let server = Arc::new(Server::http1());
    server.slow(Duration::from_millis(300));
    let burst = || {
        let sent: Vec<_> = (0..12)
            .map(|i| {
                let server = server.clone();
                std::thread::spawn(move || send(get(server.url(&format!("/{i}"))), None, None))
            })
            .collect();
        for s in sent {
            assert_eq!(s.join().unwrap().unwrap().status, 200);
        }
    };
    burst();
    assert_eq!(server.connections(), 12);
    burst();
    assert_eq!(server.connections(), 12 + 4, "eight kept, four more");
}

/// A credential's server name is what TLS verifies (a kubeconfig's
/// `tls-server-name`): a server reached at a name its certificate is not
/// for is accepted by the name it is for, and refused without it.
#[test]
fn a_credentials_server_name_is_what_tls_verifies() {
    let server = Server::h2(10);
    let url = format!("https://k8s.internal.example:{}/", server.addr.port());
    let named = Credential {
        server_name: Some("localhost".into()),
        ..Server::credential()
    };
    let r = send(get(url.clone()), Some(&named), Some(server.addr)).unwrap();
    assert_eq!(r.status, 200);
    let e = send(get(url), Some(&Server::credential()), Some(server.addr)).unwrap_err();
    assert!(e.message.contains("TLS"), "{}", e.message);
}

/// A proxy that tunnels each connection (HTTP `CONNECT`) to where it is
/// asked, keeping each request line and `Proxy-Authorization` it took.
fn proxy() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut ask = Vec::new();
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                ask.push(line.trim_end().to_string());
            }
            let to = ask[0].split_whitespace().nth(1).unwrap().to_string();
            kept.lock().unwrap().extend(ask);
            let up = std::net::TcpStream::connect(to).unwrap();
            s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .unwrap();
            let (mut a, mut b) = (up.try_clone().unwrap(), s.try_clone().unwrap());
            std::thread::spawn(move || std::io::copy(&mut r, &mut a));
            std::thread::spawn(move || {
                let mut up = up;
                std::io::copy(&mut up, &mut b)
            });
        }
    });
    (url, seen)
}

/// A credential's proxy is the one its requests go through (a
/// kubeconfig's `proxy-url`), with the user and password its URL carries,
/// never shown.
#[test]
fn a_credentials_proxy_carries_its_requests() {
    let server = Server::http1();
    let (at, seen) = proxy();
    let cred = Credential {
        name: "kubeconfig:p".into(),
        proxy: Some(at.replace("http://", "http://u:p%40ss@")),
        ..Credential::default()
    };
    let r = send(get(server.url("/through")), Some(&cred), None).unwrap();
    assert_eq!((r.status, r.body.as_slice()), (200, &b"/through"[..]));
    let port = server.addr.port();
    assert_eq!(
        *seen.lock().unwrap(),
        [
            format!("CONNECT localhost:{port} HTTP/1.1"),
            format!("Host: localhost:{port}"),
            // u:p@ss
            "Proxy-Authorization: Basic dTpwQHNz".to_string(),
        ]
    );
}

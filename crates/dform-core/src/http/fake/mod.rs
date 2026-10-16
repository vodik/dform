//! A local HTTP server, for tests of what goes over the network: HTTP/1.1
//! in the clear, or HTTP/2 over TLS under a test CA ([`CA`]) with a
//! ceiling on the requests it takes at once. It counts the connections
//! it accepts and the requests it holds at once, and answers each
//! request with what [`Server::answer`] says, after [`Server::slow`]'s
//! delay (over HTTP/2, before it reads the request's body, whose
//! flow-control window is held meanwhile).

use crate::plugin::credentials::Credential;
use bytes::Bytes;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The CA the HTTP/2 server's certificate (for `localhost` and
/// `127.0.0.1`) is issued by.
pub const CA: &str = include_str!("ca.pem");
const CHAIN: &str = include_str!("server.pem");
const KEY: &str = include_str!("server.key");

/// A request as the server took it.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// An answer: its status, headers and body.
pub type Answer = (u16, Vec<(String, String)>, Vec<u8>);

type Answering = Arc<dyn Fn(&Seen) -> Answer + Send + Sync>;

#[derive(Default)]
struct Counts {
    connections: AtomicUsize,
    requests: AtomicUsize,
    now: AtomicUsize,
    most: AtomicUsize,
}

impl Counts {
    fn begin(&self) {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
    }

    fn end(&self) {
        self.now.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How the server answers, shared with its threads.
struct Behaviour {
    answer: Mutex<Answering>,
    delay: Mutex<Duration>,
    counts: Counts,
}

impl Behaviour {
    fn delay(&self) -> Duration {
        *self.delay.lock().unwrap()
    }

    /// The answer to `seen`, at once.
    fn answer(&self, seen: &Seen) -> Answer {
        let answer = self.answer.lock().unwrap().clone();
        answer(seen)
    }
}

pub struct Server {
    pub addr: SocketAddr,
    https: bool,
    behaviour: Arc<Behaviour>,
}

impl Server {
    fn behaviour() -> Arc<Behaviour> {
        let path: Answering = Arc::new(|s: &Seen| (200, Vec::new(), s.path.clone().into_bytes()));
        Arc::new(Behaviour {
            answer: Mutex::new(path),
            delay: Mutex::new(Duration::ZERO),
            counts: Counts::default(),
        })
    }

    /// HTTP/1.1 in the clear, each connection kept alive and served by a
    /// thread of its own.
    pub fn http1() -> Server {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let behaviour = Server::behaviour();
        let b = behaviour.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(s) = s else { return };
                b.counts.connections.fetch_add(1, Ordering::SeqCst);
                let b = b.clone();
                std::thread::spawn(move || serve_http1(s, &b));
            }
        });
        Server {
            addr,
            https: false,
            behaviour,
        }
    }

    /// HTTP/2 over TLS (ALPN `h2` only), taking at most `streams`
    /// requests at once on a connection.
    pub fn h2(streams: u32) -> Server {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let addr = l.local_addr().unwrap();
        let behaviour = Server::behaviour();
        let b = behaviour.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(serve_h2(l, streams, b));
        });
        Server {
            addr,
            https: true,
            behaviour,
        }
    }

    /// Answer with `f` (the default: 200, the path as the body).
    pub fn answer(&self, f: impl Fn(&Seen) -> Answer + Send + Sync + 'static) {
        *self.behaviour.answer.lock().unwrap() = Arc::new(f);
    }

    /// Take `d` over each answer.
    pub fn slow(&self, d: Duration) {
        *self.behaviour.delay.lock().unwrap() = d;
    }

    /// `path` on it, by the name its certificate is for.
    pub fn url(&self, path: &str) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://localhost:{}{path}", self.addr.port())
    }

    pub fn connections(&self) -> usize {
        self.behaviour.counts.connections.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> usize {
        self.behaviour.counts.requests.load(Ordering::SeqCst)
    }

    /// The most requests it held at once.
    pub fn most_at_once(&self) -> usize {
        self.behaviour.counts.most.load(Ordering::SeqCst)
    }

    /// A credential that trusts its CA.
    pub fn credential() -> Credential {
        Credential {
            name: "ca:fake".into(),
            ca: Some(CA.as_bytes().to_vec()),
            ..Credential::default()
        }
    }
}

fn serve_http1(s: TcpStream, b: &Behaviour) {
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut w = s;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut words = line.split_whitespace();
        let mut seen = Seen {
            method: words.next().unwrap_or_default().to_string(),
            path: words.next().unwrap_or_default().to_string(),
            ..Seen::default()
        };
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                seen.headers
                    .push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let len = seen
            .header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        seen.body = vec![0; len];
        if r.read_exact(&mut seen.body).is_err() {
            return;
        }
        b.counts.begin();
        std::thread::sleep(b.delay());
        let (status, headers, body) = b.answer(&seen);
        b.counts.end();
        let mut out = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\n", body.len());
        for (k, v) in headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        let mut out = out.into_bytes();
        out.extend(body);
        if w.write_all(&out).is_err() {
            return;
        }
    }
}

async fn serve_h2(l: TcpListener, streams: u32, b: Arc<Behaviour>) {
    let l = tokio::net::TcpListener::from_std(l).unwrap();
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        {
            use rustls_pki_types::pem::PemObject;
            rustls_pki_types::CertificateDer::pem_slice_iter(CHAIN.as_bytes())
                .collect::<Result<_, _>>()
                .unwrap()
        },
        {
            use rustls_pki_types::pem::PemObject;
            rustls_pki_types::PrivateKeyDer::from_pem_slice(KEY.as_bytes()).unwrap()
        },
    )
    .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let tls = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    loop {
        let Ok((tcp, _)) = l.accept().await else {
            return;
        };
        b.counts.connections.fetch_add(1, Ordering::SeqCst);
        let (tls, b) = (tls.clone(), b.clone());
        tokio::spawn(async move {
            let Ok(io) = tls.accept(tcp).await else {
                return;
            };
            let Ok(mut conn) = h2::server::Builder::new()
                .max_concurrent_streams(streams)
                .handshake::<_, Bytes>(io)
                .await
            else {
                return;
            };
            while let Some(Ok((req, respond))) = conn.accept().await {
                tokio::spawn(answer_h2(req, respond, b.clone()));
            }
        });
    }
}

async fn answer_h2(
    req: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    b: Arc<Behaviour>,
) {
    let (head, mut body) = req.into_parts();
    let mut seen = Seen {
        method: head.method.to_string(),
        path: head
            .uri
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_string(),
        headers: super::headers(&head.headers),
        body: Vec::new(),
    };
    // The delay before the body is read: the window it holds is spent
    // meanwhile.
    b.counts.begin();
    tokio::time::sleep(b.delay()).await;
    while let Some(Ok(data)) = body.data().await {
        let _ = body.flow_control().release_capacity(data.len());
        seen.body.extend_from_slice(&data);
    }
    let (status, headers, out) = b.answer(&seen);
    b.counts.end();
    let mut resp = http::Response::builder().status(status);
    for (k, v) in headers {
        resp = resp.header(k, v);
    }
    let Ok(mut send) = respond.send_response(resp.body(()).unwrap(), false) else {
        return;
    };
    let _ = send.send_data(Bytes::from(out), true);
}

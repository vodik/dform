//! A local HTTP server, for tests of what goes over the network: HTTP/1.1
//! in the clear, or HTTP/2 over TLS under a test CA ([`CA`]) with a
//! ceiling on the requests it takes at once, optionally asking each
//! client for a certificate the test client CA issued
//! ([`Server::h2_mutual`]). It counts the connections
//! it accepts and the requests it holds at once, and answers each
//! request with what [`Server::answer`] says, after [`Server::slow`]'s
//! delay (over HTTP/2, before it reads the request's body, whose
//! flow-control window is held meanwhile). Over HTTP/2 it keeps what a
//! client's first connection sent before its first request, for the
//! windows the client asked for ([`Server::windows`]).

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
/// The CA a client's certificate is issued by, which
/// [`Server::h2_mutual`] trusts.
const CLIENT_CA: &str = include_str!("client-ca.pem");
/// A client certificate (`CN=dform-k8s-test`) and its key.
pub const CLIENT_CHAIN: &str = include_str!("client.pem");
pub const CLIENT_KEY: &str = include_str!("client.key");

/// A request as the server took it.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The certificate the client presented, DER ([`Server::h2_mutual`]).
    pub peer: Option<Vec<u8>>,
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
    /// The first bytes a client sent on the first HTTP/2 connection, up
    /// to [`OPENING`]: its preface, settings and window updates.
    opening: Mutex<Vec<u8>>,
}

/// How much of a connection's first bytes [`Behaviour::opening`] keeps.
const OPENING: usize = 4096;

/// The flow-control windows an HTTP/2 client gives the server: what it may
/// send on a stream, and on the connection, before the client reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Windows {
    pub stream: u32,
    pub connection: u32,
}

impl Windows {
    /// HTTP/2's own, before a SETTINGS frame or a WINDOW_UPDATE says
    /// otherwise.
    const DEFAULT: u32 = 65_535;

    /// The windows the frames after the client's preface in `opening`
    /// set: SETTINGS_INITIAL_WINDOW_SIZE, and the connection's own (its
    /// WINDOW_UPDATEs on stream 0).
    fn of(opening: &[u8]) -> Windows {
        const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        let mut w = Windows {
            stream: Windows::DEFAULT,
            connection: Windows::DEFAULT,
        };
        let mut rest = opening.strip_prefix(PREFACE).unwrap_or_default();
        let word = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        while rest.len() >= 9 {
            let len =
                (usize::from(rest[0]) << 16) | (usize::from(rest[1]) << 8) | usize::from(rest[2]);
            let (kind, flags, stream) = (rest[3], rest[4], word(&rest[5..9]) & 0x7fff_ffff);
            let Some(payload) = rest.get(9..9 + len) else {
                break;
            };
            match (kind, stream) {
                // SETTINGS, not its ACK: six bytes a setting.
                (0x4, 0) if flags & 0x1 == 0 => {
                    for s in payload.as_chunks::<6>().0 {
                        if u16::from_be_bytes([s[0], s[1]]) == 0x4 {
                            w.stream = word(&s[2..]);
                        }
                    }
                }
                (0x8, 0) if len == 4 => w.connection += word(payload) & 0x7fff_ffff,
                _ => {}
            }
            rest = &rest[9 + len..];
        }
        w
    }
}

/// A connection whose first bytes from the client are kept
/// ([`Behaviour::opening`]) when it is the server's first.
struct Tee<S> {
    io: S,
    b: Arc<Behaviour>,
    first: bool,
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Tee<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut self.io).poll_read(cx, buf);
        if !self.first {
            return r;
        }
        let mut kept = self.b.opening.lock().unwrap();
        let room = OPENING.saturating_sub(kept.len());
        let new = &buf.filled()[before..];
        kept.extend_from_slice(&new[..new.len().min(room)]);
        r
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Tee<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
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
            opening: Mutex::new(Vec::new()),
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
        Server::https(streams, false)
    }

    /// HTTP/2 over TLS, as [`Server::h2`], refusing a client that presents
    /// no certificate the test client CA issued; each request's
    /// [`Seen::peer`] is the one it presented.
    pub fn h2_mutual(streams: u32) -> Server {
        Server::https(streams, true)
    }

    fn https(streams: u32, mutual: bool) -> Server {
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
            rt.block_on(serve_h2(l, streams, mutual, b));
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

    /// The windows the client gave it on its first HTTP/2 connection, as
    /// they stood when its first request came.
    pub fn windows(&self) -> Windows {
        Windows::of(&self.behaviour.opening.lock().unwrap())
    }

    /// A credential that trusts its CA.
    pub fn credential() -> Credential {
        Credential {
            name: "ca:fake".into(),
            ca: Some(CA.as_bytes().to_vec()),
            ..Credential::default()
        }
    }

    /// The DER of [`CLIENT_CHAIN`]'s certificate: what [`Seen::peer`] is
    /// when the client presented it.
    pub fn client_der() -> Vec<u8> {
        use rustls_pki_types::pem::PemObject;
        rustls_pki_types::CertificateDer::from_pem_slice(CLIENT_CHAIN.as_bytes())
            .unwrap()
            .to_vec()
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

/// The certificates in `pem`.
fn certs(pem: &str) -> Vec<rustls_pki_types::CertificateDer<'static>> {
    use rustls_pki_types::pem::PemObject;
    rustls_pki_types::CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap()
}

async fn serve_h2(l: TcpListener, streams: u32, mutual: bool, b: Arc<Behaviour>) {
    let l = tokio::net::TcpListener::from_std(l).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap();
    let builder = match mutual {
        false => builder.with_no_client_auth(),
        true => {
            let mut roots = rustls::RootCertStore::empty();
            for c in certs(CLIENT_CA) {
                roots.add(c).unwrap();
            }
            let verifier =
                rustls::server::WebPkiClientVerifier::builder_with_provider(roots.into(), provider)
                    .build()
                    .unwrap();
            builder.with_client_cert_verifier(verifier)
        }
    };
    let mut config = builder
        .with_single_cert(certs(CHAIN), {
            use rustls_pki_types::pem::PemObject;
            rustls_pki_types::PrivateKeyDer::from_pem_slice(KEY.as_bytes()).unwrap()
        })
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let tls = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    loop {
        let Ok((tcp, _)) = l.accept().await else {
            return;
        };
        let first = b.counts.connections.fetch_add(1, Ordering::SeqCst) == 0;
        let (tls, b) = (tls.clone(), b.clone());
        tokio::spawn(async move {
            let Ok(io) = tls.accept(tcp).await else {
                return;
            };
            let peer = io
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .map(|c| c.to_vec());
            let io = Tee {
                io,
                b: b.clone(),
                first,
            };
            let Ok(mut conn) = h2::server::Builder::new()
                .max_concurrent_streams(streams)
                .handshake::<_, Bytes>(io)
                .await
            else {
                return;
            };
            while let Some(Ok((req, respond))) = conn.accept().await {
                tokio::spawn(answer_h2(req, respond, peer.clone(), b.clone()));
            }
        });
    }
}

async fn answer_h2(
    req: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    peer: Option<Vec<u8>>,
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
        peer,
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

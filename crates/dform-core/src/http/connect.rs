//! Opening a connection to an endpoint: TCP to the URL's host, or to a
//! tunnel's local end, or through the environment's proxy (`HTTPS_PROXY`,
//! `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`, by HTTP `CONNECT`); TLS for
//! `https`, offering HTTP/2 and HTTP/1.1; then the protocol the server
//! chose, its connection driven by a task of the pool's runtime until
//! either side closes it.

use super::Route;
use super::tls;
use crate::plugin::host::Error;
use bytes::Bytes;
use http_body_util::Full;
use hyper::client::conn::http1;
use hyper_util::client::proxy::matcher::Matcher;
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// A connection, ready for a request.
pub(super) enum Conn {
    /// One request at a time, kept alive between them.
    H1(http1::SendRequest<Full<Bytes>>),
    /// Requests multiplexed as streams.
    H2(h2::client::SendRequest<Bytes>),
}

/// The environment's proxy settings, read once.
fn proxies() -> &'static Matcher {
    static PROXIES: OnceLock<Matcher> = OnceLock::new();
    PROXIES.get_or_init(Matcher::from_env)
}

/// Why a connection could not be made: nothing was sent, so it is worth
/// trying again.
fn unreachable(route: &Route, what: &str, e: impl std::fmt::Display) -> Error {
    Error::retryable(format!("{}: {what}: {e}", route.origin()))
}

/// A connection to `route`, with `tls` for an `https` one.
pub(super) async fn dial(route: &Route, tls: Option<Arc<ClientConfig>>) -> Result<Conn, Error> {
    let tcp = tcp(route).await?;
    let _ = tcp.set_nodelay(true);
    match tls {
        None => handshake(route, tcp, false).await,
        Some(config) => {
            let name = tls::server_name(&route.host)?;
            let stream = tokio_rustls::TlsConnector::from(config)
                .connect(name, tcp)
                .await
                .map_err(|e| unreachable(route, "TLS", e))?;
            let h2 = stream.get_ref().1.alpn_protocol() == Some(tls::H2);
            handshake(route, stream, h2).await
        }
    }
}

/// The TCP connection a request to `route` goes over: to the tunnel's
/// end, through the proxy, or to the host.
async fn tcp(route: &Route) -> Result<TcpStream, Error> {
    if let Some(at) = route.via {
        return TcpStream::connect(at)
            .await
            .map_err(|e| unreachable(route, &format!("the tunnel at {at}"), e));
    }
    let uri: http::Uri = route
        .origin()
        .parse()
        .map_err(|e| Error::fatal(format!("{}: {e}", route.origin())))?;
    let Some(proxy) = proxies().intercept(&uri) else {
        return TcpStream::connect((route.connect_host(), route.port))
            .await
            .map_err(|e| unreachable(route, "connect", e));
    };
    let at = proxy.uri();
    if at.scheme_str() != Some("http") {
        return Err(Error::fatal(format!(
            "{}: the proxy {at}: only an http:// proxy is supported",
            route.origin()
        )));
    }
    let host = at.host().unwrap_or_default();
    let mut tcp = TcpStream::connect((host, at.port_u16().unwrap_or(80)))
        .await
        .map_err(|e| unreachable(route, &format!("the proxy {at}"), e))?;
    tunnel(&mut tcp, route, proxy.basic_auth())
        .await
        .map_err(|e| unreachable(route, &format!("the proxy {at}"), e))?;
    Ok(tcp)
}

/// Ask the proxy on `tcp` for a tunnel to `route`'s host (HTTP
/// `CONNECT`); the connection is then the host's.
async fn tunnel(
    tcp: &mut TcpStream,
    route: &Route,
    auth: Option<&http::HeaderValue>,
) -> std::io::Result<()> {
    let to = format!("{}:{}", route.host, route.port);
    let mut ask = format!("CONNECT {to} HTTP/1.1\r\nHost: {to}\r\n");
    if let Some(a) = auth.and_then(|a| a.to_str().ok()) {
        ask.push_str(&format!("Proxy-Authorization: {a}\r\n"));
    }
    ask.push_str("\r\n");
    tcp.write_all(ask.as_bytes()).await?;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 || tcp.read_u8().await.map(|b| head.push(b)).is_err() {
            return Err(std::io::Error::other("no answer to CONNECT"));
        }
    }
    let status = String::from_utf8_lossy(&head);
    let status = status.lines().next().unwrap_or_default();
    match status.split_whitespace().nth(1) {
        Some(code) if code.starts_with('2') => Ok(()),
        _ => Err(std::io::Error::other(format!("CONNECT refused: {status}"))),
    }
}

/// The protocol's handshake over `io`: HTTP/2 when ALPN chose it, else
/// HTTP/1.1. The connection is driven by a task of its own.
async fn handshake<Io>(route: &Route, io: Io, h2: bool) -> Result<Conn, Error>
where
    Io: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    if h2 {
        // One stream until the server's SETTINGS say how many: a request
        // past its ceiling would be refused unprocessed.
        let (send, conn) = h2::client::Builder::new()
            .initial_max_send_streams(1)
            .handshake(io)
            .await
            .map_err(|e| unreachable(route, "HTTP/2", e))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        return Ok(Conn::H2(send));
    }
    let (send, conn) = http1::handshake(TokioIo::new(io))
        .await
        .map_err(|e| unreachable(route, "HTTP/1.1", e))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(Conn::H1(send))
}

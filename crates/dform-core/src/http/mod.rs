//! dform's one HTTP client (R-103): the host's `http.send` for every
//! provider, the state backend's requests (`dform-s3`), an `https://`
//! location's read (`crate::files`), git over https (`crate::git`), an
//! image's digest (`crate::files::oci`) and the JWKS an approval is
//! verified against (`crate::approval`). One process, one pool
//! (`pool`): a connection per endpoint, which is the scheme, host and
//! port, the tunnel it goes through and the TLS of the credential it
//! is sent with; HTTP/2 where the server offers it by ALPN, every
//! request a stream of one connection and one TLS handshake for all;
//! HTTP/1.1 kept alive otherwise. TLS with the machine's CA (the platform
//! verifier) and proxy (`HTTPS_PROXY`, `NO_PROXY`); a credential applied
//! by its parts (headers, a client certificate, its own CA, the name TLS
//! verifies, its own proxy); a tunnel by
//! connecting to the tunnel's local end, so TLS still verifies the name
//! the URL gives. No `curl`, no second TLS stack.
//!
//! The callers block; the pool's connections are driven by a runtime on
//! a thread of its own. Each answer says what its connection told of the
//! server's pace ([`Pace`]), which the host's limiter
//! (`dform_host::ratelimit`) takes as its window's ceiling and as
//! backpressure.

mod connect;
mod exchange;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
mod pool;
mod tls;

use crate::plugin::credentials::Credential;
use crate::plugin::host::{Error, HttpRequest, HttpResponse};
use bytes::Bytes;
use exchange::{Answered, Prepared};
use pool::{Endpoint, pool};
use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// How long a request may take when it does not say.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// What a request's connection told of the server's pace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pace {
    /// The most requests the server takes at once on the connection
    /// (HTTP/2's `SETTINGS_MAX_CONCURRENT_STREAMS`); none over HTTP/1.1,
    /// where another request opens another connection.
    pub streams: Option<usize>,
    /// The request's body waited on the server before its first byte
    /// went: the connection's flow-control window (HTTP/2) spent.
    /// Backpressure, before any 429.
    pub stalled: bool,
}

/// A request's answer, and what its connection told of the pace.
#[derive(Debug)]
pub struct Exchange {
    pub answer: Result<HttpResponse, Error>,
    pub pace: Pace,
}

/// An answer whose body is read as it comes (a git pack).
pub struct Streamed {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

/// Send `req` with `cred` applied, through `via` when given. A status the
/// server answered is a response, not an error.
pub fn send(
    req: HttpRequest,
    cred: Option<&Credential>,
    via: Option<SocketAddr>,
) -> Result<HttpResponse, Error> {
    exchange(req, cred, via).answer
}

/// [`send`], with what the connection told of the server's pace.
pub fn exchange(req: HttpRequest, cred: Option<&Credential>, via: Option<SocketAddr>) -> Exchange {
    let sent = Request::new(req, cred, via).and_then(|r| Ok((pool()?, r)));
    let (pool, r) = match sent {
        Ok(p) => p,
        Err(e) => {
            return Exchange {
                answer: Err(e),
                pace: Pace::default(),
            };
        }
    };
    pool.run(r.exchange())
}

/// Send `req` as [`send`] does, its answer's body read as it comes, each
/// read waiting for the next part: the request's timeout bounds the
/// whole of it.
pub fn stream(
    req: HttpRequest,
    cred: Option<&Credential>,
    via: Option<SocketAddr>,
) -> Result<Streamed, Error> {
    let r = Request::new(req, cred, via)?;
    let pool = pool()?;
    let deadline = r.deadline();
    let uri = r.prepared.uri.clone();
    let Answered { head, mut body, .. } = pool.run(r.answered(deadline))?;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    pool.spawn(async move {
        loop {
            let part = match tokio::time::timeout_at(deadline, body.next(&uri)).await {
                Ok(Some(part)) => part,
                Ok(None) => return,
                Err(_) => Err(timed_out(&uri)),
            };
            let last = part.is_err();
            if tx.send(part).await.is_err() || last {
                return;
            }
        }
    });
    Ok(Streamed {
        status: head.status.as_u16(),
        headers: headers(&head.headers),
        body: Box::new(Reader {
            parts: rx,
            part: Bytes::new(),
        }),
    })
}

/// Where a request goes: what one connection serves.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Route {
    https: bool,
    /// As the URL gives it (an IPv6 address in brackets).
    host: String,
    port: u16,
    /// A tunnel's local end, connected to in place of the host.
    via: Option<SocketAddr>,
    tls: tls::Profile,
    /// The name TLS verifies in place of the host (a credential's).
    server_name: Option<String>,
    /// The proxy it goes through in place of the environment's (a
    /// credential's).
    proxy: Option<String>,
}

impl Route {
    /// `scheme://host:port`, for messages and the proxy's choice.
    fn origin(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}:{}", self.host, self.port)
    }

    /// The `Host` header: the host, and its port unless the scheme's.
    fn authority(&self) -> String {
        match (self.https, self.port) {
            (true, 443) | (false, 80) => self.host.clone(),
            _ => format!("{}:{}", self.host, self.port),
        }
    }

    /// The host to connect to: an IPv6 address without its brackets.
    fn connect_host(&self) -> &str {
        self.host.trim_start_matches('[').trim_end_matches(']')
    }
}

/// A request on its endpoint.
struct Request {
    prepared: Prepared,
    endpoint: Arc<Endpoint>,
    timeout: Duration,
}

impl Request {
    /// `req` with `cred`'s headers applied, on its endpoint: refused when
    /// its method, URL or a header cannot be sent (a header is named,
    /// never its value: a credential's may be secret).
    fn new(
        req: HttpRequest,
        cred: Option<&Credential>,
        via: Option<SocketAddr>,
    ) -> Result<Request, Error> {
        let url = req.url.as_str();
        let refused = |what: String| Error::fatal(format!("{url}: {what}"));
        let method = http::Method::from_bytes(req.method.to_ascii_uppercase().as_bytes())
            .map_err(|_| refused(format!("no HTTP method {:?}", req.method)))?;
        let uri: http::Uri = url.parse().map_err(|e| refused(format!("{e}")))?;
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err(refused("not an http:// or https:// URL".into())),
        };
        let host = uri
            .host()
            .ok_or_else(|| refused("no host".into()))?
            .to_string();
        let route = Route {
            https,
            port: uri.port_u16().unwrap_or(if https { 443 } else { 80 }),
            host,
            via,
            tls: tls::Profile::of(cred),
            server_name: cred.and_then(|c| c.server_name.clone()),
            proxy: cred.and_then(|c| c.proxy.clone()),
        };
        let mut headers = http::HeaderMap::new();
        let given = req.headers.iter().map(|(k, v)| (k.as_str(), v.as_bytes()));
        let held = cred
            .iter()
            .flat_map(|c| &c.headers)
            .map(|(k, v)| (k.as_str(), v.expose()));
        for (k, v) in given.chain(held) {
            let name = http::HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| refused(format!("no header named {k:?}")))?;
            let value = http::HeaderValue::from_bytes(v)
                .map_err(|_| refused(format!("the header {k}'s value cannot be sent")))?;
            headers.append(name, value);
        }
        Ok(Request {
            endpoint: pool()?.endpoint(route, cred)?,
            prepared: Prepared {
                method,
                uri,
                headers,
                body: Bytes::from(req.body),
            },
            timeout: req.timeout.unwrap_or(TIMEOUT),
        })
    }

    fn deadline(&self) -> Instant {
        Instant::now() + self.timeout
    }

    /// Its answer's head, by `deadline`.
    async fn answered(self, deadline: Instant) -> Result<Answered, Error> {
        let uri = self.prepared.uri.clone();
        tokio::time::timeout_at(deadline, self.prepared.send(&self.endpoint))
            .await
            .unwrap_or_else(|_| Err(timed_out(&uri)))
    }

    /// Its answer, the body read to its end, by its deadline.
    async fn exchange(self) -> Exchange {
        let deadline = self.deadline();
        let uri = self.prepared.uri.clone();
        let read = async {
            let Answered {
                head,
                mut body,
                pace,
            } = self.answered(deadline).await?;
            let mut bytes = Vec::new();
            while let Some(part) = body.next(&uri).await {
                bytes.extend_from_slice(&part?);
            }
            let resp = HttpResponse {
                status: head.status.as_u16(),
                headers: headers(&head.headers),
                body: bytes,
            };
            Ok::<_, Error>((resp, pace))
        };
        match tokio::time::timeout_at(deadline, read).await {
            Ok(Ok((resp, pace))) => Exchange {
                answer: Ok(resp),
                pace,
            },
            Ok(Err(e)) => Exchange {
                answer: Err(e),
                pace: Pace::default(),
            },
            Err(_) => Exchange {
                answer: Err(timed_out(&uri)),
                pace: Pace::default(),
            },
        }
    }
}

/// No answer by `deadline`: the request may have taken effect.
fn timed_out(uri: &http::Uri) -> Error {
    Error::maybe_applied(format!("{uri}: no answer in time (timed out)"))
}

fn headers(h: &http::HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// A streamed body, read on the caller's thread as its parts come.
struct Reader {
    parts: tokio::sync::mpsc::Receiver<Result<Bytes, Error>>,
    part: Bytes,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.part.is_empty() {
            match self.parts.blocking_recv() {
                Some(Ok(part)) => self.part = part,
                Some(Err(e)) => return Err(std::io::Error::other(e.message)),
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.part.len());
        buf[..n].copy_from_slice(&self.part.split_to(n));
        Ok(n)
    }
}

#[cfg(test)]
mod tests;

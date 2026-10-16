//! One request on an endpoint's connection, and its answer's body as it
//! comes. Over HTTP/2 a request waits for a stream when the server's
//! ceiling is reached (which the host's window keeps it under: the
//! ceiling is the window's, [`super::Pace::streams`]), and its body for
//! the server's flow-control window: a wait before its first byte is a
//! stall ([`super::Pace::stalled`]), the server holding the whole
//! connection back. A wait in the middle of a body is the ordinary pace
//! of a large upload, and is not.

use super::Pace;
use super::pool::{Endpoint, Lease};
use crate::plugin::host::Error;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use std::future::poll_fn;
use std::sync::Arc;

/// A request, ready to send as many times as a lost connection asks.
pub(super) struct Prepared {
    pub(super) method: http::Method,
    pub(super) uri: http::Uri,
    pub(super) headers: http::HeaderMap,
    pub(super) body: Bytes,
}

/// What a request's answer came with.
pub(super) struct Answered {
    pub(super) head: http::response::Parts,
    pub(super) body: Incoming,
    pub(super) pace: Pace,
}

/// The headers HTTP/2 carries in its own frames, or forbids.
const HOP_BY_HOP: [&str; 6] = [
    "connection",
    "host",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
];

impl Prepared {
    /// The method is one a lost connection may be sent again for even
    /// after the request went out: it changes nothing.
    fn safe(&self) -> bool {
        matches!(self.method, http::Method::GET | http::Method::HEAD)
    }

    /// The request as HTTP/1.1 sends it: the path, and the host in its
    /// header.
    fn http1(&self, route: &super::Route) -> Result<http::Request<Full<Bytes>>, Error> {
        let mut b = http::Request::builder()
            .method(self.method.clone())
            .uri(self.uri.path_and_query().map_or("/", |p| p.as_str()));
        if !self.headers.contains_key(http::header::HOST) {
            b = b.header(http::header::HOST, route.authority());
        }
        let mut req = b
            .body(Full::new(self.body.clone()))
            .map_err(|e| Error::fatal(format!("{}: {e}", self.uri)))?;
        req.headers_mut().extend(self.headers.clone());
        Ok(req)
    }

    /// The request's head as HTTP/2 sends it: the whole URL, no
    /// connection headers.
    fn h2(&self) -> Result<http::Request<()>, Error> {
        let mut req = http::Request::builder()
            .method(self.method.clone())
            .uri(self.uri.clone())
            .body(())
            .map_err(|e| Error::fatal(format!("{}: {e}", self.uri)))?;
        let headers = req.headers_mut();
        headers.extend(self.headers.clone());
        for h in HOP_BY_HOP {
            headers.remove(h);
        }
        Ok(req)
    }

    /// Send it on `endpoint`: once, and once more on a new connection
    /// when the one it went on was gone before it was taken (a kept-alive
    /// connection the server closed, an HTTP/2 connection that went
    /// away), or, for a GET, whenever the connection failed.
    pub(super) async fn send(&self, endpoint: &Arc<Endpoint>) -> Result<Answered, Error> {
        match self.send_once(endpoint).await {
            Err(Failed { again: true, .. }) => self.send_once(endpoint).await,
            r => r,
        }
        .map_err(|f| f.error)
    }

    async fn send_once(&self, endpoint: &Arc<Endpoint>) -> Result<Answered, Failed> {
        match endpoint.lease().await.map_err(Failed::not_again)? {
            Lease::H1 { send, reused } => self.send_http1(endpoint, send, reused).await,
            Lease::H2 { send, which } => {
                let r = self.send_h2(send).await;
                if r.as_ref().is_err_and(|f| f.again) {
                    endpoint.lost(which);
                }
                r
            }
        }
    }

    async fn send_http1(
        &self,
        endpoint: &Arc<Endpoint>,
        mut send: hyper::client::conn::http1::SendRequest<Full<Bytes>>,
        reused: bool,
    ) -> Result<Answered, Failed> {
        let req = self.http1(&endpoint.route).map_err(Failed::not_again)?;
        // Gone before the request was taken: send it on another. Failed
        // after: again only if it changes nothing, on a connection the
        // server may have closed while it was idle.
        let unsent = |e: hyper::Error| Failed::again(self.transport(&e));
        let lost = |e: hyper::Error| Failed {
            again: e.is_canceled() || reused && self.safe(),
            error: self.transport(&e),
        };
        send.ready().await.map_err(unsent)?;
        let resp = send.send_request(req).await.map_err(lost)?;
        let (head, body) = resp.into_parts();
        Ok(Answered {
            head,
            body: Incoming::H1 {
                body,
                conn: Some((send, endpoint.clone())),
            },
            pace: Pace::default(),
        })
    }

    async fn send_h2(&self, send: h2::client::SendRequest<Bytes>) -> Result<Answered, Failed> {
        let req = self.h2().map_err(Failed::not_again)?;
        // The connection gone before the stream opened, or the server
        // refusing the stream unprocessed (a GOAWAY past it, REFUSED_STREAM):
        // send it on another. Failed after: again only if it changes nothing.
        let unsent = |e: h2::Error| Failed::again(self.transport(&e));
        let lost = |e: h2::Error| Failed {
            again: e.is_go_away()
                || e.reason() == Some(h2::Reason::REFUSED_STREAM)
                || e.is_io() && self.safe(),
            error: self.transport(&e),
        };
        let mut send = send.ready().await.map_err(unsent)?;
        let mut stalled = false;
        let end = self.body.is_empty();
        let (answer, mut stream) = send.send_request(req, end).map_err(lost)?;
        if !end {
            self.send_body(&mut stream, &mut stalled)
                .await
                .map_err(lost)?;
        }
        let resp = answer.await.map_err(lost)?;
        let (head, body) = resp.into_parts();
        Ok(Answered {
            head,
            body: Incoming::H2(body),
            pace: Pace {
                streams: Some(send.current_max_send_streams()),
                stalled,
            },
        })
    }

    /// The body, as the server's window lets it go. The stream is let
    /// open first (its connection's task, which the request woke, runs
    /// before this one again: a stream not yet open is given no window);
    /// then none to start with is a stall.
    async fn send_body(
        &self,
        stream: &mut h2::SendStream<Bytes>,
        stalled: &mut bool,
    ) -> Result<(), h2::Error> {
        tokio::task::yield_now().await;
        let mut rest = self.body.clone();
        stream.reserve_capacity(rest.len());
        *stalled = stream.capacity() == 0;
        while !rest.is_empty() {
            let window = poll_fn(|cx| stream.poll_capacity(cx)).await;
            let n = match window {
                Some(n) => n?,
                None => return Err(h2::Error::from(h2::Reason::STREAM_CLOSED)),
            };
            if n == 0 {
                continue;
            }
            let chunk = rest.split_to(n.min(rest.len()));
            stream.send_data(chunk, rest.is_empty())?;
            stream.reserve_capacity(rest.len());
        }
        Ok(())
    }

    /// A failure of the connection: nothing came back, and the request
    /// may have been sent; worth sending again by what the caller knows.
    fn transport(&self, e: &dyn std::fmt::Display) -> Error {
        Error::retryable(format!("{}: {e}", self.uri))
    }
}

/// A request that failed, and whether it is sent again on a new
/// connection.
struct Failed {
    again: bool,
    error: Error,
}

impl Failed {
    fn not_again(error: Error) -> Failed {
        Failed {
            again: false,
            error,
        }
    }

    fn again(error: Error) -> Failed {
        Failed { again: true, error }
    }
}

/// An answer's body as it comes.
pub(super) enum Incoming {
    /// The connection goes back to its endpoint once the body is read to
    /// its end.
    H1 {
        body: hyper::body::Incoming,
        conn: Option<(
            hyper::client::conn::http1::SendRequest<Full<Bytes>>,
            Arc<Endpoint>,
        )>,
    },
    H2(h2::RecvStream),
}

impl Incoming {
    /// The next part of the body; `None` at its end.
    pub(super) async fn next(&mut self, url: &http::Uri) -> Option<Result<Bytes, Error>> {
        let failed =
            |e: &dyn std::fmt::Display| Error::retryable(format!("{url}: reading the answer: {e}"));
        match self {
            Incoming::H1 { body, conn } => loop {
                match body.frame().await {
                    Some(Ok(f)) => match f.into_data() {
                        Ok(data) => return Some(Ok(data)),
                        Err(_trailers) => continue,
                    },
                    Some(Err(e)) => return Some(Err(failed(&e))),
                    None => {
                        if let Some((send, endpoint)) = conn.take() {
                            endpoint.give_back(send);
                        }
                        return None;
                    }
                }
            },
            Incoming::H2(body) => {
                let data = body.data().await?;
                Some(match data {
                    Ok(data) => {
                        let _ = body.flow_control().release_capacity(data.len());
                        Ok(data)
                    }
                    Err(e) => Err(failed(&e)),
                })
            }
        }
    }
}

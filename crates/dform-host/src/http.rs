//! `http.send`: HTTP with TLS by the host. The machine's CA (the platform
//! verifier) and proxy (`HTTPS_PROXY`, `NO_PROXY`); a credential applied
//! by its parts (headers, a client certificate, its own CA); a tunnel by
//! resolving the URL's host to the tunnel's local end, so TLS still
//! verifies the name the URL gives.

use dform_core::plugin::credentials::Credential;
use dform_core::plugin::host::{Error, HttpRequest, HttpResponse};
use std::net::SocketAddr;
use std::time::Duration;
use ureq::tls::{Certificate, ClientCert, PemItem, PrivateKey, RootCerts, TlsConfig};

/// How long a request may take when it does not say.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// Resolves every host to the tunnel's local end.
#[derive(Debug)]
struct Through(SocketAddr);

impl ureq::unversioned::resolver::Resolver for Through {
    fn resolve(
        &self,
        _uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let mut v = self.empty();
        v.push(self.0);
        Ok(v)
    }
}

fn certs(pem: &[u8], what: &str) -> Result<Vec<Certificate<'static>>, Error> {
    let mut out = Vec::new();
    for item in ureq::tls::parse_pem(pem) {
        match item.map_err(|e| Error::fatal(format!("{what}: {e}")))? {
            PemItem::Certificate(c) => out.push(c),
            PemItem::PrivateKey(_) => {}
            _ => {}
        }
    }
    if out.is_empty() {
        return Err(Error::fatal(format!("{what}: no certificate")));
    }
    Ok(out)
}

fn tls(cred: Option<&Credential>) -> Result<TlsConfig, Error> {
    let mut tls = TlsConfig::builder().root_certs(RootCerts::PlatformVerifier);
    let Some(c) = cred else {
        return Ok(tls.build());
    };
    if let Some(ca) = &c.ca {
        tls = tls.root_certs(RootCerts::new_with_certs(&certs(
            ca,
            &format!("the credential {}'s CA", c.name),
        )?));
    }
    if let Some((chain, key)) = &c.client_cert {
        let what = format!("the credential {}'s client certificate", c.name);
        let chain = certs(chain.expose(), &what)?;
        let key = PrivateKey::from_pem(key.expose())
            .map_err(|e| Error::fatal(format!("{what}: its key: {e}")))?;
        tls = tls.client_cert(Some(ClientCert::new_with_certs(&chain, key)));
    }
    if c.insecure {
        tls = tls.disable_verification(true);
    }
    Ok(tls.build())
}

/// The class of a transport failure: one before the request was sent
/// changed nothing and is worth sending again; a timeout after may have
/// taken effect.
fn failure(url: &str, e: ureq::Error) -> Error {
    use ureq::Error as E;
    let m = format!("{url}: {e}");
    match e {
        E::Timeout(_) => Error::maybe_applied(m),
        E::HostNotFound | E::ConnectionFailed | E::Io(_) | E::Tls(_) | E::ConnectProxyFailed(_) => {
            Error::retryable(m)
        }
        _ => Error::fatal(m),
    }
}

/// Send `req` with `cred` applied, through `via` when given.
pub fn send(
    req: HttpRequest,
    cred: Option<&Credential>,
    via: Option<SocketAddr>,
) -> Result<HttpResponse, Error> {
    let config = ureq::Agent::config_builder()
        .tls_config(tls(cred)?)
        .proxy(if via.is_some() {
            None
        } else {
            ureq::Proxy::try_from_env()
        })
        .http_status_as_error(false)
        .timeout_global(Some(req.timeout.unwrap_or(TIMEOUT)))
        .build();
    let agent = match via {
        None => ureq::Agent::new_with_config(config),
        Some(addr) => ureq::Agent::with_parts(
            config,
            ureq::unversioned::transport::DefaultConnector::new(),
            Through(addr),
        ),
    };
    let method = ureq::http::Method::from_bytes(req.method.to_ascii_uppercase().as_bytes())
        .map_err(|_| Error::fatal(format!("{}: no HTTP method {:?}", req.url, req.method)))?;
    let mut b = ureq::http::Request::builder()
        .method(method)
        .uri(req.url.as_str());
    for (k, v) in &req.headers {
        b = b.header(k.as_str(), v.as_str());
    }
    for (k, v) in cred.iter().flat_map(|c| &c.headers) {
        b = b.header(k.as_str(), v.expose());
    }
    let request = b
        .body(req.body)
        .map_err(|e| Error::fatal(format!("{}: {e}", req.url)))?;
    let resp = agent.run(request).map_err(|e| failure(&req.url, e))?;
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = resp
        .into_body()
        .with_config()
        .limit(u64::MAX)
        .read_to_vec()
        .map_err(|e| failure(&req.url, e))?;
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dform_core::plugin::credentials::Secret;
    use std::io::{BufRead, BufReader, Write};

    /// A one-request HTTP server on the loopback: answers 200 with the
    /// request's Authorization header as the body.
    fn echo_auth() -> (SocketAddr, std::thread::JoinHandle<()>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut auth = String::new();
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("authorization: ") {
                    auth = line[line.len() - v.len()..].trim().to_string();
                }
            }
            let mut s = s;
            write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{auth}",
                auth.len()
            )
            .unwrap();
        });
        (addr, t)
    }

    /// The host applies a credential's header; the provider names the
    /// credential and never sees the value. A tunnel is the URL's host
    /// resolved to the tunnel's end.
    #[test]
    fn a_credential_is_applied_and_a_tunnel_routes() {
        let (addr, t) = echo_auth();
        let cred = Credential {
            name: "bearer:t".into(),
            headers: vec![("Authorization".into(), Secret::new(b"Bearer tok".to_vec()))],
            ..Credential::default()
        };
        let r = send(
            HttpRequest {
                method: "get".into(),
                url: "http://api.internal.example/v1".into(),
                timeout: Some(Duration::from_secs(3)),
                ..HttpRequest::default()
            },
            Some(&cred),
            Some(addr),
        )
        .unwrap();
        t.join().unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"Bearer tok");
    }

    /// Nobody listening is a failure worth retrying: nothing was sent.
    #[test]
    fn a_refused_connection_is_retryable() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let e = send(
            HttpRequest {
                method: "GET".into(),
                url: format!("http://127.0.0.1:{port}/"),
                ..HttpRequest::default()
            },
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(
            e.class,
            dform_core::plugin::host::Class::Retryable,
            "{}",
            e.message
        );
    }
}

//! TLS for tokio-postgres over rustls (ring), as `sslmode` says: `require`
//! encrypts and checks the handshake's signatures but not the
//! certificate (libpq's `require`), `verify-ca` checks the chain against
//! the roots, `verify-full` the chain and the host's name. The roots are
//! `root_cert`'s PEM when written, else the machine's.

use crate::config::SslMode;
use anyhow::{Context, Result, anyhow};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as Cx, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::tls::{ChannelBinding, TlsConnect};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The connector for one server: its config and the name it is checked
/// against.
#[derive(Clone)]
pub struct Tls {
    config: Arc<ClientConfig>,
    server: ServerName<'static>,
}

impl Tls {
    pub fn new(mode: SslMode, host: &str, root_cert: Option<&str>) -> Result<Tls> {
        let server = ServerName::try_from(host.to_string())
            .map_err(|_| anyhow!("{host:?} is not a host name TLS can check"))?;
        let builder = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .context("TLS versions")?;
        let verifier: Arc<dyn ServerCertVerifier> = match mode {
            SslMode::Disable | SslMode::Require => Arc::new(Unchecked(provider())),
            SslMode::VerifyCa | SslMode::VerifyFull => {
                let roots = Arc::new(roots(root_cert)?);
                let webpki = WebPkiServerVerifier::builder_with_provider(roots, provider())
                    .build()
                    .context("a certificate verifier")?;
                match mode {
                    SslMode::VerifyCa => Arc::new(AnyName(webpki)),
                    _ => webpki,
                }
            }
        };
        let config = builder
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        Ok(Tls {
            config: Arc::new(config),
            server,
        })
    }
}

fn roots(pem: Option<&str>) -> Result<RootCertStore> {
    let mut store = RootCertStore::empty();
    match pem {
        Some(pem) => {
            for c in CertificateDer::pem_slice_iter(pem.as_bytes()) {
                let c = c.map_err(|e| anyhow!("root_cert: {e}"))?;
                store.add(c).map_err(|e| anyhow!("root_cert: {e}"))?;
            }
            if store.is_empty() {
                anyhow::bail!("root_cert holds no PEM certificate");
            }
        }
        None => {
            let found = rustls_native_certs::load_native_certs();
            for c in found.certs {
                let _ = store.add(c);
            }
            if store.is_empty() {
                anyhow::bail!(
                    "no root certificate on this machine to check the server's against: \
                     write root_cert"
                );
            }
        }
    }
    Ok(store)
}

/// `require`: any certificate, its handshake's signatures checked.
#[derive(Debug)]
struct Unchecked(Arc<CryptoProvider>);

impl ServerCertVerifier for Unchecked {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// `verify-ca`: the chain checked, whatever name it is for.
#[derive(Debug)]
struct AnyName(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for AnyName {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        match self
            .0
            .verify_server_cert(end, intermediates, name, ocsp, now)
        {
            Err(Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            r => r,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// A TLS session over `S`, for tokio-postgres.
pub struct Stream<S>(tokio_rustls::client::TlsStream<S>);

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for Stream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Cx<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for Stream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Cx<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Cx<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Cx<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> tokio_postgres::tls::TlsStream for Stream<S> {
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

impl<S> TlsConnect<S> for Tls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = Stream<S>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Stream<S>>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            tokio_rustls::TlsConnector::from(self.config)
                .connect(self.server, stream)
                .await
                .map(Stream)
        })
    }
}

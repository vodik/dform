//! TLS for the pool: one rustls configuration per credential's TLS (the
//! machine's CA through the platform verifier, a credential's own CA
//! beside it, its client certificate, or no verification when it says
//! so), built once and shared by every connection made with it, so a
//! second connection to an endpoint resumes the first one's session.
//! Both protocols are offered by ALPN, HTTP/2 first.

use crate::plugin::credentials::Credential;
use crate::plugin::host::Error;
use rustls::ClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// What ALPN offers, in order of preference.
pub(super) const H2: &[u8] = b"h2";
const HTTP1: &[u8] = b"http/1.1";

/// Which TLS a credential asks for: none of its own (the machine's), or
/// its parts' digest, so two credentials with the same parts share a
/// configuration and a configuration is never another's by name alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Profile {
    Machine,
    Own([u8; 32]),
}

impl Profile {
    pub(super) fn of(cred: Option<&Credential>) -> Profile {
        let Some(c) = cred.filter(|c| c.ca.is_some() || c.client_cert.is_some() || c.insecure)
        else {
            return Profile::Machine;
        };
        let mut h = Sha256::new();
        for part in [
            c.ca.as_deref(),
            c.client_cert.as_ref().map(|(chain, _)| chain.expose()),
        ] {
            h.update(part.map_or(0, <[u8]>::len).to_be_bytes());
            h.update(part.unwrap_or_default());
        }
        if let Some((_, key)) = &c.client_cert {
            h.update(Sha256::digest(key.expose()));
        }
        h.update([u8::from(c.insecure)]);
        Profile::Own(h.finalize().into())
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The certificates in `pem`; `what` names it in an error.
fn certs(pem: &[u8], what: &str) -> Result<Vec<CertificateDer<'static>>, Error> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::fatal(format!("{what}: {e}")))?;
    if certs.is_empty() {
        return Err(Error::fatal(format!("{what}: no certificate")));
    }
    Ok(certs)
}

/// The configuration for `cred`'s TLS: the machine's CA, with the
/// credential's own beside it; its client certificate; no verification
/// when it asks for none (a kubeconfig's `insecure-skip-tls-verify`).
pub(super) fn config(cred: Option<&Credential>) -> Result<Arc<ClientConfig>, Error> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::fatal(format!("TLS: {e}")))?;
    let verifier: Arc<dyn ServerCertVerifier> = match cred {
        Some(c) if c.insecure => Arc::new(Unverified(provider())),
        Some(Credential {
            ca: Some(ca), name, ..
        }) => {
            let extra = certs(ca, &format!("the credential {name}'s CA"))?;
            Arc::new(platform(
                rustls_platform_verifier::Verifier::new_with_extra_roots(extra, provider()),
            )?)
        }
        _ => Arc::new(platform(rustls_platform_verifier::Verifier::new(
            provider(),
        ))?),
    };
    let builder = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let mut config = match cred.and_then(|c| c.client_cert.as_ref().map(|cc| (c, cc))) {
        None => builder.with_no_client_auth(),
        Some((c, (chain, key))) => {
            let what = format!("the credential {}'s client certificate", c.name);
            let chain = certs(chain.expose(), &what)?;
            let key = PrivateKeyDer::from_pem_slice(key.expose())
                .map_err(|e| Error::fatal(format!("{what}: its key: {e}")))?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| Error::fatal(format!("{what}: {e}")))?
        }
    };
    config.alpn_protocols = vec![H2.to_vec(), HTTP1.to_vec()];
    Ok(Arc::new(config))
}

fn platform<V>(v: Result<V, rustls::Error>) -> Result<V, Error> {
    v.map_err(|e| Error::fatal(format!("TLS: the machine's certificate store: {e}")))
}

/// The name TLS verifies: the URL's host, whatever address it is reached
/// at (a tunnel's end, a proxy).
pub(super) fn server_name(host: &str) -> Result<ServerName<'static>, Error> {
    ServerName::try_from(
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
    )
    .map_err(|e| Error::fatal(format!("TLS: {host}: {e}")))
}

/// A server not verified: what a credential that says so asks for. The
/// handshake's signatures are still checked.
#[derive(Debug)]
struct Unverified(Arc<CryptoProvider>);

impl ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

//! TLS 1.2/1.3 client & server configuration: rustls with the aws-lc-rs provider — the same
//! crypto OpenDAL's default reqwest client uses, so TLS CPU cost matches production.

use std::sync::{Arc, OnceLock};

#[cfg(feature = "__internal")]
use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::{DigitallySignedStruct, SignatureScheme};
#[cfg(feature = "__internal")]
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
#[cfg(feature = "__internal")]
use tokio_rustls::TlsAcceptor;

static CRYPTO_PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();

/// Return the shared `rustls::crypto::CryptoProvider` (aws-lc-rs).
pub fn crypto_provider() -> Arc<CryptoProvider> {
    CRYPTO_PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .clone()
}

/// TLS client negotiation & trust options.
#[derive(Clone, Debug)]
pub struct TlsClientOptions {
    /// ALPN protocols advertised during ClientHello (e.g., `["h2", "http/1.1"]`).
    pub alpn_protocols: Vec<Vec<u8>>,
    /// Optional extra PEM certificate bundle (for private S3 / MinIO / Ceph / testbed CAs).
    pub extra_ca_pem: Option<String>,
    /// Explicitly bypass X.509 certificate verification (for local self-signed dev endpoints).
    pub insecure_skip_verify: bool,
}

impl Default for TlsClientOptions {
    fn default() -> Self {
        Self {
            alpn_protocols: vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            extra_ca_pem: None,
            insecure_skip_verify: false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct InsecureCertVerifier {
    pub(crate) provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for InsecureCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build a `tokio_rustls::TlsAcceptor` (the lab server) from PEM-encoded certificate chain and PKCS#8 private key.
#[cfg(feature = "__internal")]
pub fn build_tls_acceptor(cert_pem: &str, key_pem: &str, enable_h2: bool) -> Result<TlsAcceptor> {
    let alpn: &[&[u8]] = if enable_h2 {
        &[b"h2", b"http/1.1"]
    } else {
        &[b"http/1.1"]
    };
    build_tls_acceptor_alpn(cert_pem, key_pem, alpn)
}

/// A TLS acceptor offering exactly the `alpn` protocols (`[b"h2"]`: HTTP/2 only).
#[cfg(feature = "__internal")]
pub fn build_tls_acceptor_alpn(
    cert_pem: &str,
    key_pem: &str,
    alpn: &[&[u8]],
) -> Result<TlsAcceptor> {
    let provider = crypto_provider();
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse server certificate PEM")?;
    let key = rustls_pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .context("failed to parse server private key PEM")?;

    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("failed to configure server TLS versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("invalid server TLS certificate or private key")?;

    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();

    Ok(TlsAcceptor::from(Arc::new(cfg)))
}

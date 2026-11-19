//! Pure-Rust TLS 1.2/1.3 client & server configuration (rustls + RustCrypto provider, no C deps).

use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio_rustls::{TlsAcceptor, TlsConnector};

static CRYPTO_PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
static SYSTEM_ROOT_CERTS: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();

/// Return the shared pure-Rust `rustls::crypto::CryptoProvider`.
pub fn pure_rust_crypto_provider() -> Arc<CryptoProvider> {
    CRYPTO_PROVIDER
        .get_or_init(|| Arc::new(oxitls_rustcrypto_provider::provider()))
        .clone()
}

fn cached_system_certs() -> &'static [CertificateDer<'static>] {
    SYSTEM_ROOT_CERTS
        .get_or_init(|| rustls_native_certs::load_native_certs().certs)
        .as_slice()
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

/// Build a `RootCertStore` combining Mozilla WebPKI roots, OS native roots, and optional custom PEMs.
pub fn build_root_store(extra_ca_pem: Option<&str>) -> Result<RootCertStore> {
    let mut store = RootCertStore::empty();
    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in cached_system_certs() {
        let _ = store.add(cert.clone());
    }
    if let Some(pem) = extra_ca_pem {
        for item in CertificateDer::pem_slice_iter(pem.as_bytes()) {
            let cert = item.context("failed to parse custom CA PEM certificate")?;
            store
                .add(cert)
                .context("failed to add custom CA certificate to RootCertStore")?;
        }
    }
    Ok(store)
}

#[derive(Debug)]
struct InsecureCertVerifier {
    provider: Arc<CryptoProvider>,
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

/// Build a `tokio_rustls::TlsConnector` from [`TlsClientOptions`].
pub fn build_tls_connector(opts: &TlsClientOptions) -> Result<TlsConnector> {
    let provider = pure_rust_crypto_provider();
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("failed to configure TLS 1.2/1.3 protocol versions")?;

    let mut cfg = if opts.insecure_skip_verify {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(InsecureCertVerifier { provider }))
            .with_no_client_auth()
    } else {
        let roots = build_root_store(opts.extra_ca_pem.as_deref())?;
        builder.with_root_certificates(roots).with_no_client_auth()
    };

    cfg.alpn_protocols.clone_from(&opts.alpn_protocols);
    Ok(TlsConnector::from(Arc::new(cfg)))
}

/// Build a `tokio_rustls::TlsAcceptor` from PEM-encoded certificate chain and PKCS#8 private key.
pub fn build_tls_acceptor(cert_pem: &str, key_pem: &str, enable_h2: bool) -> Result<TlsAcceptor> {
    let provider = pure_rust_crypto_provider();
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse server certificate PEM")?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .context("failed to parse server private key PEM")?;

    let mut cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("failed to configure server TLS versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("invalid server TLS certificate or private key")?;

    cfg.alpn_protocols = if enable_h2 {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };

    Ok(TlsAcceptor::from(Arc::new(cfg)))
}

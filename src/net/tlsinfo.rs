//! TLS depth: what a handshake negotiated (version, cipher suite, key-exchange group, full /
//! resumed / HelloRetryRequest) and the server's certificate chain.
//!
//! The microscope transport owns its rustls connection and reads these from it directly.
//! OpenDAL's reqwest client does not expose its connections, so in that mode oprobe builds the
//! *same* crypto provider and verifier with thin delegating wrappers that report, through a
//! hook, which cipher suite's AEAD was keyed, which key-exchange groups were started and
//! whether the server certificate was verified (a resumed handshake skips verification). The
//! wrappers forward every call unchanged: the negotiated parameters and the CPU cost of the
//! handshake are those of the default client.

use std::sync::{Arc, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::cipher::{
    AeadKey, Iv, KeyBlockShape, MessageDecrypter, MessageEncrypter, Tls12AeadAlgorithm,
    Tls13AeadAlgorithm, UnsupportedOperationError,
};
use rustls::crypto::{ActiveKeyExchange, CryptoProvider, SupportedKxGroup};
use rustls::{
    CipherSuite, ConnectionTrafficSecrets, DigitallySignedStruct, NamedGroup, ProtocolVersion,
    SignatureScheme, SupportedCipherSuite, Tls12CipherSuite, Tls13CipherSuite,
};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::TlsRow;

/// What a handshake reveals to the observing wrappers.
#[derive(Clone, Copy, Debug)]
pub enum TlsEvent {
    /// Traffic keys of this suite were installed.
    Cipher(CipherSuite),
    /// A key share of this group was generated (a second one in TLS 1.3 = HelloRetryRequest).
    KxStart(NamedGroup),
    /// The server certificate was verified (full handshake; resumption skips it).
    CertVerified,
}

pub type TlsHook = fn(TlsEvent);

/// Accumulated observations of one handshake.
#[derive(Clone, Copy, Debug, Default)]
pub struct HandshakeMarks {
    pub cipher: Option<CipherSuite>,
    pub kx_starts: u8,
    pub last_group: Option<NamedGroup>,
    /// Group TLS 1.3 remembered for the server at the end of the handshake (`set_kx_hint`).
    pub final_group: Option<NamedGroup>,
    pub cert_verified: bool,
}

impl HandshakeMarks {
    pub fn apply(&mut self, e: TlsEvent) {
        match e {
            TlsEvent::Cipher(c) => {
                self.cipher.get_or_insert(c);
            }
            TlsEvent::KxStart(g) => {
                self.kx_starts = self.kx_starts.saturating_add(1);
                self.last_group = Some(g);
            }
            TlsEvent::CertVerified => self.cert_verified = true,
        }
    }

    /// The handshake as a request-row `tls` value (`version` from the key schedule).
    pub fn row(&self, version: &str) -> TlsRow {
        let tls13 = version.contains("1.3");
        TlsRow {
            resumed: !self.cert_verified,
            version: version.to_owned(),
            cipher: self.cipher.map(suite_name),
            // TLS 1.3 starts one key share for the ClientHello; a second one answers a
            // HelloRetryRequest. (TLS 1.2's ECDHE share comes on top of the unused 1.3 one.)
            hrr: tls13 && self.kx_starts >= 2,
            group: self.final_group.or(self.last_group).map(group_name),
        }
    }
}

pub fn suite_name(c: CipherSuite) -> String {
    c.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{c:?}"))
}

pub fn group_name(g: NamedGroup) -> String {
    g.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{g:?}"))
}

/// rustls' version label (`TLSv1_3`) as oprobe prints it (`TLS1.3`).
pub fn version_name(v: ProtocolVersion) -> String {
    format!("{v:?}")
        .replace("TLSv1_", "TLS1.")
        .replace("TLSv", "TLS")
}

// ---------------------------------------------------------------------------------------------
// Observing wrappers

struct Aead13 {
    inner: &'static dyn Tls13AeadAlgorithm,
    suite: CipherSuite,
    hook: TlsHook,
}

impl Tls13AeadAlgorithm for Aead13 {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        (self.hook)(TlsEvent::Cipher(self.suite));
        self.inner.encrypter(key, iv)
    }
    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        self.inner.decrypter(key, iv)
    }
    fn key_len(&self) -> usize {
        self.inner.key_len()
    }
    fn extract_keys(
        &self,
        key: AeadKey,
        iv: Iv,
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        self.inner.extract_keys(key, iv)
    }
    fn fips(&self) -> bool {
        self.inner.fips()
    }
}

struct Aead12 {
    inner: &'static dyn Tls12AeadAlgorithm,
    suite: CipherSuite,
    hook: TlsHook,
}

impl Tls12AeadAlgorithm for Aead12 {
    fn encrypter(&self, key: AeadKey, iv: &[u8], extra: &[u8]) -> Box<dyn MessageEncrypter> {
        (self.hook)(TlsEvent::Cipher(self.suite));
        self.inner.encrypter(key, iv, extra)
    }
    fn decrypter(&self, key: AeadKey, iv: &[u8]) -> Box<dyn MessageDecrypter> {
        self.inner.decrypter(key, iv)
    }
    fn key_block_shape(&self) -> KeyBlockShape {
        self.inner.key_block_shape()
    }
    fn extract_keys(
        &self,
        key: AeadKey,
        iv: &[u8],
        explicit: &[u8],
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        self.inner.extract_keys(key, iv, explicit)
    }
    fn fips(&self) -> bool {
        self.inner.fips()
    }
}

#[derive(Debug)]
struct Kx {
    inner: &'static dyn SupportedKxGroup,
    hook: TlsHook,
}

impl SupportedKxGroup for Kx {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, rustls::Error> {
        (self.hook)(TlsEvent::KxStart(self.inner.name()));
        self.inner.start()
    }
    fn start_and_complete(
        &self,
        peer_pub_key: &[u8],
    ) -> Result<rustls::crypto::CompletedKeyExchange, rustls::Error> {
        (self.hook)(TlsEvent::KxStart(self.inner.name()));
        self.inner.start_and_complete(peer_pub_key)
    }
    fn ffdhe_group(&self) -> Option<rustls::ffdhe_groups::FfdheGroup<'static>> {
        self.inner.ffdhe_group()
    }
    fn name(&self) -> NamedGroup {
        self.inner.name()
    }
    fn fips(&self) -> bool {
        self.inner.fips()
    }
    fn usable_for_version(&self, v: ProtocolVersion) -> bool {
        self.inner.usable_for_version(v)
    }
}

fn common(c: &rustls::CipherSuiteCommon) -> rustls::CipherSuiteCommon {
    rustls::CipherSuiteCommon {
        suite: c.suite,
        hash_provider: c.hash_provider,
        confidentiality_limit: c.confidentiality_limit,
    }
}

/// `base` with every cipher suite and key-exchange group wrapped to report to `hook`. Built
/// once per hook (the wrappers are `'static`, like rustls' own suites).
pub fn observed_provider(base: &CryptoProvider, hook: TlsHook) -> Arc<CryptoProvider> {
    static CACHE: OnceLock<std::sync::Mutex<Vec<(usize, Arc<CryptoProvider>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, p)) = c.iter().find(|(h, _)| *h == hook as usize) {
        return p.clone();
    }
    let mut p = base.clone();
    p.cipher_suites = base
        .cipher_suites
        .iter()
        .map(|s| match s {
            SupportedCipherSuite::Tls13(t) => {
                let aead: &'static dyn Tls13AeadAlgorithm = Box::leak(Box::new(Aead13 {
                    inner: t.aead_alg,
                    suite: t.common.suite,
                    hook,
                }));
                let wrapped: &'static Tls13CipherSuite = Box::leak(Box::new(Tls13CipherSuite {
                    common: common(&t.common),
                    hkdf_provider: t.hkdf_provider,
                    aead_alg: aead,
                    quic: t.quic,
                }));
                SupportedCipherSuite::Tls13(wrapped)
            }
            SupportedCipherSuite::Tls12(t) => {
                let aead: &'static dyn Tls12AeadAlgorithm = Box::leak(Box::new(Aead12 {
                    inner: t.aead_alg,
                    suite: t.common.suite,
                    hook,
                }));
                let wrapped: &'static Tls12CipherSuite = Box::leak(Box::new(Tls12CipherSuite {
                    common: common(&t.common),
                    prf_provider: t.prf_provider,
                    kx: t.kx,
                    sign: t.sign,
                    aead_alg: aead,
                }));
                SupportedCipherSuite::Tls12(wrapped)
            }
        })
        .collect();
    p.kx_groups = base
        .kx_groups
        .iter()
        .map(|g| {
            let w: &'static dyn SupportedKxGroup = Box::leak(Box::new(Kx { inner: *g, hook }));
            w
        })
        .collect();
    let p = Arc::new(p);
    c.push((hook as usize, p.clone()));
    p
}

/// A certificate verifier that reports full handshakes to `hook`.
#[derive(Debug)]
pub struct ObservedVerifier<V> {
    pub inner: V,
    pub hook: TlsHook,
}

impl<V: ServerCertVerifier> ServerCertVerifier for ObservedVerifier<V> {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        (self.hook)(TlsEvent::CertVerified);
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }
    fn root_hint_subjects(&self) -> Option<&[rustls::DistinguishedName]> {
        self.inner.root_hint_subjects()
    }
}

// ---------------------------------------------------------------------------------------------
// Certificate chain (doctor)

/// One certificate the server presented.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CertInfo {
    pub subject: String,
    pub issuer: String,
    /// `notAfter`, RFC 3339 (UTC).
    pub not_after: String,
    /// Days until `notAfter` (negative: expired).
    pub expires_in_days: i64,
    pub der_bytes: u64,
}

/// The chain as presented (leaf first).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CertChain {
    pub certs: Vec<CertInfo>,
    /// Total DER size the server sends in every full handshake.
    pub total_bytes: u64,
}

impl CertChain {
    pub fn from_der(chain: &[CertificateDer<'_>]) -> Self {
        let certs: Vec<CertInfo> = chain.iter().map(|c| parse_cert(c.as_ref())).collect();
        Self {
            total_bytes: certs.iter().map(|c| c.der_bytes).sum(),
            certs,
        }
    }

    /// Days until the first certificate of the chain expires.
    pub fn min_expiry_days(&self) -> Option<i64> {
        self.certs
            .iter()
            .filter(|c| !c.not_after.is_empty())
            .map(|c| c.expires_in_days)
            .min()
    }
}

/// A DER TLV: (tag, content, rest).
fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *b.first()?;
    let l0 = *b.get(1)? as usize;
    let (len, hdr) = if l0 < 0x80 {
        (l0, 2)
    } else {
        let n = l0 & 0x7f;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *b.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let content = b.get(hdr..hdr + len)?;
    Some((tag, content, &b[hdr + len..]))
}

/// The common name (else the first attribute value) of an X.501 Name.
fn name_cn(mut name: &[u8]) -> String {
    let mut first = None;
    while let Some((_, set, rest)) = tlv(name) {
        name = rest;
        if let Some((_, atv, _)) = tlv(set)
            && let Some((_, oid, val)) = tlv(atv)
            && let Some((_, v, _)) = tlv(val)
        {
            let s = String::from_utf8_lossy(v).into_owned();
            if oid == [0x55, 0x04, 0x03] {
                return s;
            }
            first.get_or_insert(s);
        }
    }
    first.unwrap_or_default()
}

/// UTCTime (`YYMMDDHHMMSSZ`) or GeneralizedTime (`YYYYMMDDHHMMSSZ`) → (unix seconds, RFC 3339).
fn parse_time(tag: u8, v: &[u8]) -> Option<(i64, String)> {
    let s = std::str::from_utf8(v).ok()?;
    let (y, rest) = match tag {
        0x17 => {
            let yy: i64 = s.get(0..2)?.parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, s.get(2..)?)
        }
        0x18 => (s.get(0..4)?.parse().ok()?, s.get(4..)?),
        _ => return None,
    };
    let n = |r: std::ops::Range<usize>| -> Option<i64> { rest.get(r)?.parse().ok() };
    let (mo, d, h, mi, se) = (
        n(0..2)?,
        n(2..4)?,
        n(4..6)?,
        n(6..8)?,
        n(8..10).unwrap_or(0),
    );
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3600 + mi * 60 + se;
    Some((
        secs,
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}Z"),
    ))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_cert(der: &[u8]) -> CertInfo {
    let mut info = CertInfo {
        der_bytes: der.len() as u64,
        ..Default::default()
    };
    let parsed = (|| {
        let (_, cert, _) = tlv(der)?;
        let (_, tbs, _) = tlv(cert)?;
        let mut rest = tbs;
        // [0] version (optional)
        let (tag, _, r) = tlv(rest)?;
        if tag == 0xa0 {
            rest = r;
        }
        let (_, _, r) = tlv(rest)?; // serial
        let (_, _, r) = tlv(r)?; // signature algorithm
        let (_, issuer, r) = tlv(r)?;
        let (_, validity, r) = tlv(r)?;
        let (_, subject, _) = tlv(r)?;
        let (_, _, vr) = tlv(validity)?; // notBefore
        let (ttag, tval, _) = tlv(vr)?; // notAfter
        Some((name_cn(issuer), name_cn(subject), parse_time(ttag, tval)))
    })();
    if let Some((issuer, subject, time)) = parsed {
        info.issuer = issuer;
        info.subject = subject;
        if let Some((secs, s)) = time {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            info.not_after = s;
            info.expires_in_days = (secs - now).div_euclid(86_400);
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_lab_certificate_chain() {
        use rustls_pki_types::pem::PemObject;
        let pem = crate::lab::lab_ca_pem();
        let ders: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem.as_bytes())
            .collect::<Result<_, _>>()
            .unwrap();
        let chain = CertChain::from_der(&ders);
        assert_eq!(chain.certs.len(), 1);
        let c = &chain.certs[0];
        assert_eq!(c.subject, "oprobe lab CA (generated)");
        assert_eq!(c.issuer, c.subject);
        assert!((28..=31).contains(&c.expires_in_days), "{c:?}");
        assert!(c.not_after.ends_with('Z'));
        assert_eq!(chain.total_bytes, c.der_bytes);
        assert_eq!(chain.min_expiry_days(), Some(c.expires_in_days));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        let d = days_from_civil(2026, 11, 20);
        assert_eq!(crate::util::civil_from_days(d), (2026, 11, 20));
    }

    #[test]
    fn marks_describe_the_handshake() {
        let mut m = HandshakeMarks::default();
        m.apply(TlsEvent::KxStart(NamedGroup::X25519));
        m.apply(TlsEvent::CertVerified);
        m.apply(TlsEvent::Cipher(CipherSuite::TLS13_AES_128_GCM_SHA256));
        m.apply(TlsEvent::Cipher(CipherSuite::TLS13_AES_256_GCM_SHA384));
        let r = m.row("TLS1.3");
        assert!(!r.resumed && !r.hrr);
        assert_eq!(r.cipher.as_deref(), Some("TLS13_AES_128_GCM_SHA256"));
        assert_eq!(r.group.as_deref(), Some("X25519"));
        m.apply(TlsEvent::KxStart(NamedGroup::secp256r1));
        assert!(m.row("TLS1.3").hrr);
        assert!(!m.row("TLS1.2").hrr);
        assert!(HandshakeMarks::default().row("TLS1.3").resumed);
    }
}

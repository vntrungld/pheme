//! rustls verifiers that accept exactly the certificates whose fingerprint is trusted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use tracing::debug;

use crate::identity::fingerprint;
use crate::trust::SharedTrust;

#[derive(Debug)]
pub struct PinnedVerifier {
    trust: SharedTrust,
    /// When set, any certificate is accepted (pairing mode). Callers must check ALPN and
    /// trust status after the handshake.
    accept_any: Arc<AtomicBool>,
    provider: Arc<CryptoProvider>,
}

impl PinnedVerifier {
    pub fn new(trust: SharedTrust, accept_any: Arc<AtomicBool>) -> Arc<PinnedVerifier> {
        Arc::new(PinnedVerifier {
            trust,
            accept_any,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        })
    }

    pub fn provider(&self) -> Arc<CryptoProvider> {
        self.provider.clone()
    }

    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if self.accept_any.load(Ordering::SeqCst) {
            return Ok(());
        }
        let fp = fingerprint(cert);
        if self.trust.read().unwrap().is_trusted(&fp) {
            Ok(())
        } else {
            debug!(%fp, "rejecting untrusted certificate");
            // `ApplicationVerificationFailure` is the variant rustls/quinn carry over the wire
            // as a TLS alert, letting the connecting side (transport::connect_raw) distinguish
            // "you are not in my trust store" from other handshake/transport failures.
            Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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

impl ClientCertVerifier for PinnedVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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

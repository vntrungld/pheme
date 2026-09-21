//! Self-signed Ed25519 identity stored on disk.

use std::fs;
use std::path::Path;

use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

use crate::fsutil::write_atomic;
use crate::{NetError, Result};

pub struct Identity {
    pub name: String,
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    pub fingerprint: String,
}

/// Lower-case hex SHA-256 of the DER certificate.
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    hex::encode(Sha256::digest(cert.as_ref()))
}

impl Identity {
    pub fn load_or_create(dir: &Path, name: &str) -> Result<Identity> {
        fs::create_dir_all(dir)?;
        let cert_path = dir.join("identity.crt");
        let key_path = dir.join("identity.key");
        let cert_exists = cert_path.exists();
        let key_exists = key_path.exists();
        let (cert_der, key_der) = if cert_exists && key_exists {
            (fs::read(&cert_path)?, fs::read(&key_path)?)
        } else if cert_exists || key_exists {
            return Err(NetError::Tls(format!(
                "incomplete identity in {}: expected both identity.crt and identity.key (delete both to regenerate)",
                dir.display()
            )));
        } else {
            let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)
                .map_err(|e| NetError::Tls(e.to_string()))?;
            let mut params = rcgen::CertificateParams::new(vec![name.to_string()])
                .map_err(|e| NetError::Tls(e.to_string()))?;
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, name);
            // rcgen 0.14: `CertificateParams::self_signed(&KeyPair) -> Result<Certificate>`;
            // check docs.rs if the builder API differs in the resolved version.
            let cert = params
                .self_signed(&key)
                .map_err(|e| NetError::Tls(e.to_string()))?;
            let cert_der = cert.der().to_vec();
            let key_der = key.serialize_der();
            write_atomic(&cert_path, &cert_der, 0o644)?;
            write_atomic(&key_path, &key_der, 0o600)?;
            (cert_der, key_der)
        };
        let cert = CertificateDer::from(cert_der);
        let fingerprint = fingerprint(&cert);
        Ok(Identity {
            name: name.to_string(),
            cert,
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
            fingerprint,
        })
    }

    pub fn clone_key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_then_reloads_the_same_identity() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(dir.path(), "desk").unwrap();
        assert_eq!(a.fingerprint.len(), 64);
        assert!(dir.path().join("identity.crt").exists());
        assert!(dir.path().join("identity.key").exists());
        let b = Identity::load_or_create(dir.path(), "desk").unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.cert, b.cert);
        assert_eq!(fingerprint(&a.cert), a.fingerprint);
    }

    #[test]
    fn different_dirs_get_different_identities() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(d1.path(), "x").unwrap();
        let b = Identity::load_or_create(d2.path(), "x").unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn incomplete_identity_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        Identity::load_or_create(dir.path(), "desk").unwrap();
        let cert_path = dir.path().join("identity.crt");
        let key_path = dir.path().join("identity.key");
        let cert_before = fs::read(&cert_path).unwrap();
        fs::remove_file(&key_path).unwrap();

        let result = Identity::load_or_create(dir.path(), "desk");
        assert!(matches!(result, Err(NetError::Tls(_))));

        let cert_after = fs::read(&cert_path).unwrap();
        assert_eq!(cert_before, cert_after);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        Identity::load_or_create(dir.path(), "desk").unwrap();
        let key_path = dir.path().join("identity.key");
        let mode = fs::metadata(&key_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

use anyhow::{Context, Result};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::Path};

#[derive(Clone)]
pub struct Identity {
    pub cert: CertificateDer<'static>,
    key: Vec<u8>,
    pub fingerprint: String,
    pub id: String,
}
impl Identity {
    pub fn load_or_create(config_dir: &Path) -> Result<Self> {
        let dir = config_dir.join("flockswitch/net");
        fs::create_dir_all(&dir)?;
        let cert_path = dir.join("identity.der");
        let key_path = dir.join("identity.key");
        let (cert, key) = match (fs::read(&cert_path), fs::read(&key_path)) {
            (Ok(cert), Ok(key)) => (cert, key),
            (Err(c), Err(k))
                if c.kind() == std::io::ErrorKind::NotFound
                    && k.kind() == std::io::ErrorKind::NotFound =>
            {
                let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
                let cert = rcgen::CertificateParams::new(vec!["flockswitch.local".into()])?
                    .self_signed(&key)?;
                let key = key.serialize_der();
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(&key_path)?;
                file.write_all(&key)?;
                file.sync_all()?;
                fs::write(&cert_path, cert.der())?;
                (cert.der().to_vec(), key)
            }
            _ => anyhow::bail!("incomplete identity in {}", dir.display()),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
        }
        let fp = fingerprint(&cert);
        let identity = Self {
            cert: cert.into(),
            key,
            id: fp[..16].into(),
            fingerprint: fp,
        };
        rustls::sign::CertifiedKey::from_der(
            vec![identity.cert.clone()],
            identity.private_key(),
            &rustls::crypto::ring::default_provider(),
        )
        .context("invalid stored identity")?;
        Ok(identity)
    }
    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivatePkcs8KeyDer::from(self.key.clone()).into()
    }
}
pub fn fingerprint(cert: &[u8]) -> String {
    Sha256::digest(cert)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistent_identity_and_private_key() {
        let root = std::env::temp_dir().join(format!("flock-net-identity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let a = Identity::load_or_create(&root).unwrap();
        let b = Identity::load_or_create(&root).unwrap();
        assert_eq!(a.cert, b.cert);
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.fingerprint.len(), 64);
        assert_eq!(a.id, &a.fingerprint[..16]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.join("flockswitch/net/identity.key"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_file(root.join("flockswitch/net/identity.der")).unwrap();
        assert!(Identity::load_or_create(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

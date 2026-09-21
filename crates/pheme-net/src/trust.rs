//! Persistent list of trusted peer certificate fingerprints (`trusted.toml`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::fsutil::write_atomic;
use crate::{NetError, Result};

pub type SharedTrust = Arc<RwLock<TrustStore>>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustedPeer {
    pub name: String,
    pub fingerprint: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    peers: Vec<TrustedPeer>,
}

#[derive(Debug)]
pub struct TrustStore {
    path: PathBuf,
    peers: Vec<TrustedPeer>,
}

impl TrustStore {
    pub fn load(dir: &Path) -> Result<TrustStore> {
        fs::create_dir_all(dir)?;
        let path = dir.join("trusted.toml");
        let peers = if path.exists() {
            let text = fs::read_to_string(&path)?;
            toml::from_str::<TrustFile>(&text)
                .map_err(|e| NetError::Tls(format!("bad trusted.toml: {e}")))?
                .peers
        } else {
            Vec::new()
        };
        Ok(TrustStore { path, peers })
    }

    pub fn shared(self) -> SharedTrust {
        Arc::new(RwLock::new(self))
    }

    pub fn add(&mut self, name: &str, fingerprint: &str) {
        self.peers.retain(|p| p.fingerprint != fingerprint);
        self.peers.push(TrustedPeer {
            name: name.to_string(),
            fingerprint: fingerprint.to_string(),
        });
    }

    pub fn is_trusted(&self, fingerprint: &str) -> bool {
        self.peers.iter().any(|p| p.fingerprint == fingerprint)
    }

    pub fn name_of(&self, fingerprint: &str) -> Option<String> {
        self.peers
            .iter()
            .find(|p| p.fingerprint == fingerprint)
            .map(|p| p.name.clone())
    }

    pub fn peers(&self) -> &[TrustedPeer] {
        &self.peers
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(&TrustFile {
            peers: self.peers.clone(),
        })
        .expect("serializable");
        write_atomic(&self.path, text.as_bytes(), 0o644)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_trusts_nothing_and_saves_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = TrustStore::load(dir.path()).unwrap();
        assert!(!t.is_trusted("ab"));
        t.add("lap", "ab");
        t.add("lap2", "cd");
        t.add("lap-renamed", "ab"); // same fingerprint replaces the entry
        t.save().unwrap();
        let t2 = TrustStore::load(dir.path()).unwrap();
        assert!(t2.is_trusted("ab"));
        assert!(t2.is_trusted("cd"));
        assert_eq!(t2.name_of("ab"), Some("lap-renamed".to_string()));
        assert_eq!(t2.peers().len(), 2);
        assert!(dir.path().join("trusted.toml").exists());
    }

    #[test]
    fn save_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = TrustStore::load(dir.path()).unwrap();
        t.add("lap", "ab");
        t.save().unwrap();

        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(entries.contains(&"trusted.toml".to_string()));
        assert!(!entries.iter().any(|n| n.ends_with(".tmp")));
    }
}

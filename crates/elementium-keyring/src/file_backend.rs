use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, KeyInit};
use argon2::Argon2;
use tracing::debug;

use crate::error::{Result, SecretStoreError};
use crate::{SENSITIVE_KEYS, SecretStore};

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

pub struct FileBackend {
    path: PathBuf,
    key: [u8; KEY_LEN],
}

impl FileBackend {
    /// Create a new file backend with the given password.
    /// The file is created at `~/.config/Elementium/secrets.enc` (or platform equivalent).
    pub fn new(password: &str) -> Result<Self> {
        let path = secrets_file_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // If file exists, derive key from existing salt. Otherwise generate new salt.
        let key = if path.exists() {
            let data = fs::read(&path)?;
            if data.len() < SALT_LEN + NONCE_LEN {
                return Err(SecretStoreError::Decryption(
                    "secrets file too short".into(),
                ));
            }
            let salt = &data[..SALT_LEN];
            derive_key(password, salt)?
        } else {
            // New file — generate salt + write empty store
            let salt = random_bytes::<SALT_LEN>();
            let key = derive_key(password, &salt)?;
            let backend = Self { path: path.clone(), key };
            backend.write_map(&HashMap::new())?;
            return Ok(Self { path, key });
        };

        let backend = Self { path, key };

        // Verify we can decrypt
        backend.read_map()?;

        debug!("encrypted file backend ready at {:?}", backend.path);
        Ok(backend)
    }

    fn read_map(&self) -> Result<HashMap<String, String>> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }

        let data = fs::read(&self.path)?;
        if data.len() < SALT_LEN + NONCE_LEN {
            return Err(SecretStoreError::Decryption(
                "secrets file too short".into(),
            ));
        }

        let nonce_start = SALT_LEN;
        let ciphertext_start = SALT_LEN + NONCE_LEN;

        #[allow(deprecated)]
        let nonce = aes_gcm::Nonce::from_slice(&data[nonce_start..ciphertext_start]);
        let ciphertext = &data[ciphertext_start..];

        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|e| SecretStoreError::Decryption(e.to_string()))?;

        let plaintext = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| SecretStoreError::Decryption(e.to_string()))?;

        let map: HashMap<String, String> = serde_json::from_slice(&plaintext)?;
        Ok(map)
    }

    fn write_map(&self, map: &HashMap<String, String>) -> Result<()> {
        let plaintext = serde_json::to_vec(map)?;

        // Read existing salt or generate new one
        let salt = if self.path.exists() {
            let existing = fs::read(&self.path)?;
            if existing.len() >= SALT_LEN {
                let mut s = [0u8; SALT_LEN];
                s.copy_from_slice(&existing[..SALT_LEN]);
                s
            } else {
                random_bytes::<SALT_LEN>()
            }
        } else {
            random_bytes::<SALT_LEN>()
        };

        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);

        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|e| SecretStoreError::Encryption(e.to_string()))?;

        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_ref())
            .map_err(|e| SecretStoreError::Encryption(e.to_string()))?;

        // Build output: [salt][nonce][ciphertext]
        let mut output = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
        output.extend_from_slice(&salt);
        output.extend_from_slice(&nonce);
        output.extend_from_slice(&ciphertext);

        // Atomic write via tempfile + rename
        let parent = self.path.parent().ok_or_else(|| {
            SecretStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "no parent directory",
            ))
        })?;

        let tmp = tempfile::NamedTempFile::new_in(parent)?;
        fs::write(tmp.path(), &output)?;
        tmp.persist(&self.path)
            .map_err(|e| SecretStoreError::Io(e.error))?;

        Ok(())
    }
}

impl SecretStore for FileBackend {
    fn get(&self, key: &str) -> Result<Option<String>> {
        let map = self.read_map()?;
        Ok(map.get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut map = self.read_map()?;
        map.insert(key.to_string(), value.to_string());
        self.write_map(&map)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let mut map = self.read_map()?;
        map.remove(key);
        self.write_map(&map)
    }

    fn get_all(&self) -> Result<HashMap<String, String>> {
        let map = self.read_map()?;
        Ok(map
            .into_iter()
            .filter(|(k, _)| SENSITIVE_KEYS.contains(&k.as_str()))
            .collect())
    }
}

fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    let mut key = [0u8; KEY_LEN];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| SecretStoreError::KeyDerivation(e.to_string()))?;
    Ok(key)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use rand::RngCore;
    let mut buf = [0u8; N];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

fn secrets_file_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().ok_or_else(|| {
        SecretStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not determine config directory",
        ))
    })?;
    Ok(config_dir.join("Elementium").join("secrets.enc"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.enc");

        let backend = FileBackend {
            path: path.clone(),
            key: derive_key("test-password", &random_bytes::<SALT_LEN>()).unwrap(),
        };

        backend.set("mx_access_token", "syt_secret123").unwrap();
        backend.set("mx_pickle_key", "pickle!").unwrap();

        assert_eq!(
            backend.get("mx_access_token").unwrap(),
            Some("syt_secret123".to_string())
        );
        assert_eq!(
            backend.get("mx_pickle_key").unwrap(),
            Some("pickle!".to_string())
        );
        assert_eq!(backend.get("nonexistent").unwrap(), None);

        backend.delete("mx_access_token").unwrap();
        assert_eq!(backend.get("mx_access_token").unwrap(), None);
    }
}

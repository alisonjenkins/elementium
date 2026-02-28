use std::collections::HashMap;

use keyring::Entry;
use tracing::{debug, warn};

use crate::error::{Result, SecretStoreError};
use crate::{SENSITIVE_KEYS, SecretStore, SERVICE_NAME};

pub struct KeyringBackend;

impl KeyringBackend {
    /// Attempt to create a keyring backend by testing read/write/delete of a probe entry.
    /// Returns `Err` if the OS keyring is unavailable.
    pub fn try_new() -> Result<Self> {
        let test_key = "__elementium_test";
        let test_value = "probe";

        let entry =
            Entry::new(SERVICE_NAME, test_key).map_err(|e| SecretStoreError::Keyring(e.to_string()))?;

        entry
            .set_password(test_value)
            .map_err(|e| SecretStoreError::Keyring(e.to_string()))?;

        let readback = entry
            .get_password()
            .map_err(|e| SecretStoreError::Keyring(e.to_string()))?;

        if readback != test_value {
            return Err(SecretStoreError::Keyring(
                "keyring probe readback mismatch".into(),
            ));
        }

        entry
            .delete_credential()
            .map_err(|e| SecretStoreError::Keyring(e.to_string()))?;

        debug!("OS keyring backend available");
        Ok(Self)
    }
}

impl SecretStore for KeyringBackend {
    fn get(&self, key: &str) -> Result<Option<String>> {
        let entry =
            Entry::new(SERVICE_NAME, key).map_err(|e| SecretStoreError::Keyring(e.to_string()))?;

        match entry.get_password() {
            Ok(val) => Ok(Some(val)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => {
                warn!("keyring get({key}) failed: {e}");
                Err(SecretStoreError::Keyring(e.to_string()))
            }
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        let entry =
            Entry::new(SERVICE_NAME, key).map_err(|e| SecretStoreError::Keyring(e.to_string()))?;

        entry
            .set_password(value)
            .map_err(|e| SecretStoreError::Keyring(e.to_string()))
    }

    fn delete(&self, key: &str) -> Result<()> {
        let entry =
            Entry::new(SERVICE_NAME, key).map_err(|e| SecretStoreError::Keyring(e.to_string()))?;

        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()), // already gone
            Err(e) => Err(SecretStoreError::Keyring(e.to_string())),
        }
    }

    fn get_all(&self) -> Result<HashMap<String, String>> {
        let mut map = HashMap::new();
        for &key in SENSITIVE_KEYS {
            if let Some(val) = self.get(key)? {
                map.insert(key.to_string(), val);
            }
        }
        Ok(map)
    }
}

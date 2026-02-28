use thiserror::Error;

#[derive(Debug, Error)]
pub enum SecretStoreError {
    #[error("keyring error: {0}")]
    Keyring(String),

    #[error("encryption error: {0}")]
    Encryption(String),

    #[error("decryption error: {0}")]
    Decryption(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("key derivation error: {0}")]
    KeyDerivation(String),

    #[error("no backend available")]
    NoBackend,
}

pub type Result<T> = std::result::Result<T, SecretStoreError>;

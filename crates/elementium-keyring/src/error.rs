use thiserror::Error;

/// `NoBackend` was removed: it was never constructed anywhere in this crate (the OS-keyring
/// and encrypted-file backends are the only two, and each reports its own failures).
///
/// `Encryption`/`Decryption` used to fold three distinct causes into one `String`: an invalid
/// AES key length, a genuine AEAD encrypt/decrypt failure, and (`Decryption` only) a secrets
/// file too short to contain its own header. The first two are now split by their real
/// `#[source]` types (`crypto_common::InvalidLength` and `aead::Error` respectively -- two
/// different `RustCrypto` error types that used to be indistinguishable once stringified); the
/// third has no error object behind it at all (nothing failed -- the file is simply the wrong
/// shape), so it is `TruncatedFile`, a fact (a byte count), not a wrapped cause.
#[derive(Debug, Error)]
pub enum SecretStoreError {
    #[error("keyring error: {0}")]
    Keyring(#[source] keyring::Error),

    /// `KeyringBackend::try_new`'s probe wrote a value to the OS keyring and read back a
    /// different one. Not a `keyring::Error` -- the backend reported success on both calls;
    /// there is no crate error to preserve, only the fact that it lied about persisting the
    /// value we asked for.
    #[error("keyring probe readback mismatch: the OS keyring returned a different value than it was asked to store")]
    ProbeMismatch,

    /// AES-256-GCM rejected the derived key material's length. In practice unreachable: the
    /// key is always exactly `KEY_LEN` (32) bytes, `Aes256Gcm`'s required length, so nothing
    /// has ever hit this at runtime -- see this crate's audit report for why it is kept as a
    /// real, typed error (defensive against `KEY_LEN` and the cipher's key size drifting
    /// apart) rather than an `expect()`, which the workspace lints forbid outright anyway.
    #[error("invalid AES key length: {0}")]
    InvalidKeyLength(#[source] aes_gcm::aead::common::InvalidLength),

    #[error("encryption error: {0}")]
    Encryption(#[source] aes_gcm::Error),

    #[error("decryption error: {0}")]
    Decryption(#[source] aes_gcm::Error),

    /// The secrets file is shorter than the fixed `[salt][nonce]` header every version of it
    /// has, so it is truncated, corrupted, or not a secrets file at all. Not a decrypt
    /// failure: decryption was never attempted. `0` is the file's actual length in bytes.
    #[error("secrets file too short to contain its header ({0} bytes)")]
    TruncatedFile(usize),

    /// `SALT_LEN + NONCE_LEN + ciphertext.len()` would overflow `usize` when sizing the
    /// output buffer. Purely defensive: secrets are small strings, so this is not reachable
    /// with any input this crate could plausibly encrypt -- flagged for review rather than
    /// fixed, since the workspace's `arithmetic_side_effects` lint requires the checked
    /// arithmetic regardless of whether overflow is realistic.
    #[error("secrets file output size overflowed")]
    OutputTooLarge,

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("key derivation error: {0}")]
    KeyDerivation(#[source] argon2::Error),
}

pub type Result<T> = std::result::Result<T, SecretStoreError>;

#[cfg(test)]
mod tests {
    use super::SecretStoreError;

    /// Pins the split of the old `Keyring(String)`: the real `keyring::Error` must be
    /// walkable via `source()`, not just folded into a rendered message.
    #[test]
    fn keyring_failure_carries_the_keyring_error_as_its_source() {
        let err = SecretStoreError::Keyring(keyring::Error::NoEntry);
        assert!(matches!(err, SecretStoreError::Keyring(_)));
        assert!(std::error::Error::source(&err).is_some());
    }

    /// New variant: the OS keyring reporting success on both write and read but returning a
    /// different value than was written has no `keyring::Error` to preserve -- there is no
    /// cause, only the fact of the mismatch.
    #[test]
    fn probe_mismatch_has_no_underlying_source() {
        let err = SecretStoreError::ProbeMismatch;
        assert!(matches!(err, SecretStoreError::ProbeMismatch));
        assert!(std::error::Error::source(&err).is_none());
    }

    /// New variant: split out of the old `Encryption`/`Decryption` strings, which conflated
    /// an invalid key length with a genuine AEAD failure. Must carry its own source type.
    #[test]
    fn invalid_key_length_carries_its_source() {
        let err = SecretStoreError::InvalidKeyLength(aes_gcm::aead::common::InvalidLength);
        assert!(matches!(err, SecretStoreError::InvalidKeyLength(_)));
        assert!(std::error::Error::source(&err).is_some());
    }

    /// Pins the split of the old `Encryption(String)`: must carry the real `aead::Error`.
    #[test]
    fn encryption_failure_carries_the_aead_error_as_its_source() {
        let err = SecretStoreError::Encryption(aes_gcm::Error);
        assert!(matches!(err, SecretStoreError::Encryption(_)));
        assert!(std::error::Error::source(&err).is_some());
    }

    /// Pins the split of the old `Decryption(String)`: must carry the real `aead::Error`,
    /// distinct from the file-shape check now covered by `TruncatedFile`.
    #[test]
    fn decryption_failure_carries_the_aead_error_as_its_source() {
        let err = SecretStoreError::Decryption(aes_gcm::Error);
        assert!(matches!(err, SecretStoreError::Decryption(_)));
        assert!(std::error::Error::source(&err).is_some());
    }

    /// New variant: split out of the old `Decryption(String)`'s "secrets file too short"
    /// case. No decrypt was attempted, so there is no `aead::Error` to wrap -- the byte count
    /// is the whole fact.
    #[test]
    fn truncated_file_has_no_underlying_source_and_names_the_length() {
        let err = SecretStoreError::TruncatedFile(3);
        assert!(matches!(err, SecretStoreError::TruncatedFile(3)));
        assert!(std::error::Error::source(&err).is_none());
        assert!(err.to_string().contains('3'));
    }

    /// New variant: split out of the old `Encryption(String)`'s "output size overflow" case.
    /// A `checked_add` failure has no error object to preserve either.
    #[test]
    fn output_too_large_has_no_underlying_source() {
        let err = SecretStoreError::OutputTooLarge;
        assert!(matches!(err, SecretStoreError::OutputTooLarge));
        assert!(std::error::Error::source(&err).is_none());
    }

    /// Pins the split of the old `KeyDerivation(String)`: must carry the real `argon2::Error`.
    #[test]
    fn key_derivation_failure_carries_the_argon2_error_as_its_source() {
        let err = SecretStoreError::KeyDerivation(argon2::Error::MemoryTooLittle);
        assert!(matches!(err, SecretStoreError::KeyDerivation(_)));
        assert!(std::error::Error::source(&err).is_some());
    }
}

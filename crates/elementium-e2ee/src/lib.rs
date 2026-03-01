//! LiveKit-compatible E2EE frame encryption for MatrixRTC calls.
//!
//! Implements the same frame format as `livekit-client`'s E2EE Worker so that
//! Elementium can interoperate with other Element Call clients (browser, mobile).
//!
//! ## Frame format (after encryption)
//!
//! ```text
//! [unencrypted header (N bytes)] [encrypted payload] [IV (12 bytes)] [key_index (1 byte)]
//! ```
//!
//! - Audio: N=0, entire Opus packet is encrypted
//! - Video: N=1..3, VP8 header bytes left unencrypted so the SFU can identify keyframes
//! - IV: 12-byte nonce derived from a frame counter
//! - Encryption: AES-128-GCM with 16-byte auth tag appended to ciphertext
//! - Key derivation: HKDF-SHA256 with info `"LKFrameEncryptionKey"`

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

/// Maximum number of keys per participant in a key ring.
const MAX_KEYS: usize = 16;

/// Size of AES-GCM initialization vector.
const IV_SIZE: usize = 12;

/// Size of AES-GCM authentication tag.
const TAG_SIZE: usize = 16;

/// HKDF info string matching livekit-client's E2EE Worker.
const HKDF_INFO: &[u8] = b"LKFrameEncryptionKey";

/// Media kind for determining unencrypted header size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
}

/// Configuration options for E2EE, matching livekit-client's KeyProviderOptions.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct E2eeOptions {
    /// Number of ratchet steps to try when decryption fails with current key.
    #[serde(default = "default_ratchet_window")]
    pub ratchet_window_size: u32,
    /// Salt for HKDF key derivation (base64 or raw bytes).
    #[serde(default)]
    pub ratchet_salt: Option<String>,
    /// Whether to enable key ratcheting.
    #[serde(default = "default_true")]
    pub auto_ratchet: bool,
}

fn default_ratchet_window() -> u32 {
    0
}

fn default_true() -> bool {
    true
}

impl Default for E2eeOptions {
    fn default() -> Self {
        Self {
            ratchet_window_size: 0,
            ratchet_salt: None,
            auto_ratchet: true,
        }
    }
}

/// Errors from E2EE operations.
#[derive(Debug, thiserror::Error)]
pub enum E2eeError {
    #[error("no key available for participant {0:?} at index {1}")]
    NoKey(String, u8),
    #[error("frame too short to contain E2EE metadata ({0} bytes)")]
    FrameTooShort(usize),
    #[error("decryption failed: {0}")]
    DecryptionFailed(String),
    #[error("invalid key index {0}")]
    InvalidKeyIndex(u8),
}

/// A ring of encryption keys for a single participant.
struct KeyRing {
    /// Keys indexed by key_index (0..MAX_KEYS). `None` means slot is unused.
    keys: [Option<DerivedKey>; MAX_KEYS],
    /// The currently active key index for encryption.
    current_index: u8,
}

/// A derived encryption key ready for AES-GCM.
struct DerivedKey {
    /// The raw key material from the JS side.
    raw_material: Vec<u8>,
    /// The AES-128-GCM cipher derived from the raw material via HKDF.
    cipher: Aes128Gcm,
}

impl KeyRing {
    fn new() -> Self {
        Self {
            keys: std::array::from_fn(|_| None),
            current_index: 0,
        }
    }

    fn set_key(&mut self, index: u8, material: &[u8]) {
        let idx = (index as usize) % MAX_KEYS;
        let cipher = derive_cipher(material);
        self.keys[idx] = Some(DerivedKey {
            raw_material: material.to_vec(),
            cipher,
        });
        self.current_index = index;
    }

    fn get_cipher(&self, index: u8) -> Option<&Aes128Gcm> {
        let idx = (index as usize) % MAX_KEYS;
        self.keys[idx].as_ref().map(|k| &k.cipher)
    }

    fn current_cipher(&self) -> Option<(&Aes128Gcm, u8)> {
        self.get_cipher(self.current_index)
            .map(|c| (c, self.current_index))
    }

    /// Ratchet the key at `index`: derive a new key from the current material.
    fn ratchet(&mut self, index: u8) -> bool {
        let idx = (index as usize) % MAX_KEYS;
        if let Some(ref key) = self.keys[idx] {
            let new_material = ratchet_key(&key.raw_material);
            let cipher = derive_cipher(&new_material);
            self.keys[idx] = Some(DerivedKey {
                raw_material: new_material,
                cipher,
            });
            true
        } else {
            false
        }
    }
}

/// Manages encryption keys for all participants in a call.
struct KeyManager {
    /// Per-participant key rings. Key is participant identity string.
    participants: HashMap<String, KeyRing>,
    /// The local participant's identity (for encryption).
    local_identity: Option<String>,
}

impl KeyManager {
    fn new() -> Self {
        Self {
            participants: HashMap::new(),
            local_identity: None,
        }
    }

    fn set_key(&mut self, participant: &str, key_index: u8, material: &[u8]) {
        let ring = self
            .participants
            .entry(participant.to_string())
            .or_insert_with(KeyRing::new);
        ring.set_key(key_index, material);
        tracing::info!(
            participant = participant,
            key_index = key_index,
            "E2EE key set"
        );
    }
}

/// Thread-safe E2EE context shared across the media pipeline.
pub struct E2eeContext {
    inner: Arc<RwLock<E2eeContextInner>>,
}

struct E2eeContextInner {
    key_manager: KeyManager,
    options: E2eeOptions,
    /// Per-participant frame counters for IV generation (outbound).
    frame_counter: u64,
}

impl Clone for E2eeContext {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl E2eeContext {
    /// Create a new E2EE context with the given options.
    pub fn new(options: E2eeOptions) -> Self {
        Self {
            inner: Arc::new(RwLock::new(E2eeContextInner {
                key_manager: KeyManager::new(),
                options,
                frame_counter: 0,
            })),
        }
    }

    /// Set the local participant identity (used for choosing the encryption key).
    pub fn set_local_identity(&self, identity: &str) {
        let mut inner = self.inner.write().unwrap();
        inner.key_manager.local_identity = Some(identity.to_string());
    }

    /// Store an encryption key for a participant.
    pub fn set_key(&self, participant: &str, key_index: u8, key_material: &[u8]) {
        let mut inner = self.inner.write().unwrap();
        inner.key_manager.set_key(participant, key_index, key_material);
    }

    /// Check if E2EE has keys available for encryption.
    pub fn has_encryption_key(&self) -> bool {
        let inner = self.inner.read().unwrap();
        if let Some(ref identity) = inner.key_manager.local_identity {
            inner
                .key_manager
                .participants
                .get(identity)
                .and_then(|ring| ring.current_cipher())
                .is_some()
        } else {
            // If no local identity set, check if any key exists
            inner
                .key_manager
                .participants
                .values()
                .any(|ring| ring.current_cipher().is_some())
        }
    }

    /// Encrypt a media frame using the LiveKit E2EE frame format.
    ///
    /// Returns `None` if no encryption key is available (passthrough mode).
    pub fn encrypt_frame(&self, frame: &[u8], kind: MediaKind) -> Option<Vec<u8>> {
        let mut inner = self.inner.write().unwrap();

        // Get the local participant's current key index
        let identity = inner.key_manager.local_identity.clone()?;

        // Generate IV from frame counter (mutate before borrowing participants)
        let counter = inner.frame_counter;
        inner.frame_counter += 1;

        let ring = inner.key_manager.participants.get(&identity)?;
        let (cipher, key_index) = ring.current_cipher()?;

        let header_size = unencrypted_header_size(frame, kind);
        let header = &frame[..header_size];
        let payload = &frame[header_size..];

        let iv = build_iv(counter);
        let nonce = Nonce::from(iv);
        let ciphertext = match cipher.encrypt(&nonce, payload) {
            Ok(ct) => ct,
            Err(e) => {
                tracing::warn!("E2EE encrypt failed: {e}");
                return None;
            }
        };

        // Build output: [header] [ciphertext (includes GCM tag)] [IV] [key_index]
        let mut output = Vec::with_capacity(header_size + ciphertext.len() + IV_SIZE + 1);
        output.extend_from_slice(header);
        output.extend_from_slice(&ciphertext);
        output.extend_from_slice(&iv);
        output.push(key_index % MAX_KEYS as u8);

        Some(output)
    }

    /// Decrypt a media frame, trying all known participant keys.
    ///
    /// Used when the sender's identity is unknown (e.g., inbound RTP from the SFU
    /// where we only have the raw frame, not the participant identity).
    pub fn decrypt_frame_any(
        &self,
        frame: &[u8],
        kind: MediaKind,
    ) -> Result<Option<Vec<u8>>, E2eeError> {
        let participants: Vec<String> = {
            let inner = self.inner.read().unwrap();
            inner.key_manager.participants.keys().cloned().collect()
        };

        if participants.is_empty() {
            return Ok(None);
        }

        for participant in &participants {
            match self.decrypt_frame(frame, participant, kind) {
                Ok(Some(decrypted)) => return Ok(Some(decrypted)),
                Ok(None) => continue,
                Err(_) => continue,
            }
        }

        // All participants failed; return the last error for diagnostics
        Err(E2eeError::DecryptionFailed(format!(
            "tried {} participants, none could decrypt",
            participants.len()
        )))
    }

    /// Decrypt a media frame using the LiveKit E2EE frame format.
    ///
    /// Returns `Err` if the frame is malformed or decryption fails.
    /// Returns `Ok(None)` if no key is available for the participant.
    pub fn decrypt_frame(
        &self,
        frame: &[u8],
        participant: &str,
        kind: MediaKind,
    ) -> Result<Option<Vec<u8>>, E2eeError> {
        // Minimum frame size: at least 1 byte payload + IV + key_index
        let min_size = IV_SIZE + 1 + TAG_SIZE;
        if frame.len() < min_size {
            return Err(E2eeError::FrameTooShort(frame.len()));
        }

        // Extract key_index (last byte)
        let key_index = frame[frame.len() - 1];
        if key_index as usize >= MAX_KEYS {
            return Err(E2eeError::InvalidKeyIndex(key_index));
        }

        // Extract IV (12 bytes before key_index)
        let iv_start = frame.len() - 1 - IV_SIZE;
        let iv = &frame[iv_start..iv_start + IV_SIZE];

        // The rest is [header][ciphertext]
        let header_and_ciphertext = &frame[..iv_start];

        // Determine unencrypted header size from the frame beginning
        let header_size = unencrypted_header_size(header_and_ciphertext, kind);
        let header = &header_and_ciphertext[..header_size];
        let ciphertext = &header_and_ciphertext[header_size..];

        let inner = self.inner.read().unwrap();
        let options = inner.options.clone();

        let ring = match inner.key_manager.participants.get(participant) {
            Some(r) => r,
            None => return Ok(None),
        };

        // Copy IV into a fixed array for Nonce::from
        let iv_array: [u8; IV_SIZE] = iv.try_into().map_err(|_| E2eeError::FrameTooShort(frame.len()))?;

        // Try the indicated key index first
        if let Some(cipher) = ring.get_cipher(key_index) {
            let nonce = Nonce::from(iv_array);
            if let Ok(plaintext) = cipher.decrypt(&nonce, ciphertext) {
                let mut output = Vec::with_capacity(header_size + plaintext.len());
                output.extend_from_slice(header);
                output.extend_from_slice(&plaintext);
                return Ok(Some(output));
            }
        }

        // If ratcheting is enabled, try ratcheted keys
        if options.ratchet_window_size > 0 {
            drop(inner);
            let mut inner = self.inner.write().unwrap();
            if let Some(ring) = inner.key_manager.participants.get_mut(participant) {
                for _ in 0..options.ratchet_window_size {
                    ring.ratchet(key_index);
                    if let Some(cipher) = ring.get_cipher(key_index) {
                        let nonce = Nonce::from(iv_array);
                        if let Ok(plaintext) = cipher.decrypt(&nonce, ciphertext) {
                            let mut output = Vec::with_capacity(header_size + plaintext.len());
                            output.extend_from_slice(header);
                            output.extend_from_slice(&plaintext);
                            return Ok(Some(output));
                        }
                    }
                }
            }
        }

        Err(E2eeError::DecryptionFailed(format!(
            "participant={participant}, key_index={key_index}"
        )))
    }
}

/// Derive an AES-128-GCM cipher from raw key material using HKDF-SHA256.
fn derive_cipher(material: &[u8]) -> Aes128Gcm {
    let hk = Hkdf::<Sha256>::new(None, material);
    let mut okm = [0u8; 16]; // AES-128 = 16 bytes
    hk.expand(HKDF_INFO, &mut okm)
        .expect("HKDF expand should not fail for 16-byte output");
    Aes128Gcm::new_from_slice(&okm).expect("16 bytes is valid AES-128 key length")
}

/// Ratchet a key: derive new key material from existing material.
fn ratchet_key(material: &[u8]) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::new(None, material);
    let mut okm = vec![0u8; material.len().max(16)];
    hk.expand(b"LKFrameRatchet", &mut okm)
        .expect("HKDF expand should not fail");
    okm
}

/// Build a 12-byte IV from a frame counter.
fn build_iv(counter: u64) -> [u8; IV_SIZE] {
    let mut iv = [0u8; IV_SIZE];
    // Put the counter in the last 8 bytes (big-endian), matching livekit-client
    iv[4..12].copy_from_slice(&counter.to_be_bytes());
    iv
}

/// Determine how many bytes at the start of the frame should remain unencrypted.
///
/// - Audio (Opus): 0 bytes — entire frame is encrypted
/// - Video (VP8): 1 byte minimum (the first byte contains keyframe flag and version),
///   but we check the VP8 payload descriptor to preserve the right amount
fn unencrypted_header_size(frame: &[u8], kind: MediaKind) -> usize {
    match kind {
        MediaKind::Audio => 0,
        MediaKind::Video => {
            if frame.is_empty() {
                return 0;
            }
            // VP8 payload descriptor: first byte always present
            // Bit 7 (X): extension bit
            // If X=1, second byte is present with I, L, T, K flags
            // If I=1, PictureID is present (1 or 2 more bytes)
            let mut size = 1;
            if frame.len() > 1 && (frame[0] & 0x80) != 0 {
                // Extension byte present
                size = 2;
                if frame.len() > 2 && (frame[1] & 0x80) != 0 {
                    // PictureID present
                    size = 3;
                    if frame.len() > 3 && (frame[2] & 0x80) != 0 {
                        // 2-byte PictureID (M bit set)
                        size = 4;
                    }
                }
            }
            size.min(frame.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_audio_roundtrip() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"test-key-material-1234567890abc");

        let original = b"opus-frame-data-here";
        let encrypted = ctx
            .encrypt_frame(original, MediaKind::Audio)
            .expect("encryption should succeed");

        // Encrypted frame should be larger (IV + tag + key_index)
        assert!(encrypted.len() > original.len());
        // Encrypted payload should differ from original
        assert_ne!(&encrypted[..original.len()], &original[..]);

        let decrypted = ctx
            .decrypt_frame(&encrypted, "alice", MediaKind::Audio)
            .expect("decryption should not error")
            .expect("decryption should produce output");

        assert_eq!(&decrypted, original);
    }

    #[test]
    fn encrypt_decrypt_video_roundtrip() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"video-key-material-abcdefghijkl");

        // Simulated VP8 frame: first byte is payload descriptor
        let mut original = vec![0x90, 0x80, 0x42]; // VP8 header
        original.extend_from_slice(b"video-payload-data-here");

        let encrypted = ctx
            .encrypt_frame(&original, MediaKind::Video)
            .expect("encryption should succeed");

        // First few bytes (VP8 header) should be unencrypted
        assert_eq!(&encrypted[..3], &original[..3]);

        let decrypted = ctx
            .decrypt_frame(&encrypted, "alice", MediaKind::Video)
            .expect("decryption should not error")
            .expect("decryption should produce output");

        assert_eq!(decrypted, original);
    }

    #[test]
    fn decrypt_wrong_participant_returns_none() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"test-key-material-1234567890abc");

        let original = b"opus-frame";
        let encrypted = ctx
            .encrypt_frame(original, MediaKind::Audio)
            .expect("encryption should succeed");

        // Bob has no key set
        let result = ctx
            .decrypt_frame(&encrypted, "bob", MediaKind::Audio)
            .expect("should not error");
        assert!(result.is_none());
    }

    #[test]
    fn decrypt_wrong_key_fails() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"test-key-material-1234567890abc");

        let original = b"secret-audio-data";
        let encrypted = ctx
            .encrypt_frame(original, MediaKind::Audio)
            .expect("encryption should succeed");

        // Set a different key for alice
        ctx.set_key("alice", 0, b"wrong-key-material-xxxxxxxxx123");

        let result = ctx.decrypt_frame(&encrypted, "alice", MediaKind::Audio);
        assert!(result.is_err());
    }

    #[test]
    fn multiple_key_indices() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");

        // Set key at index 0, encrypt a frame
        ctx.set_key("alice", 0, b"key-zero-material-abcdefghijklm");
        let encrypted_0 = ctx
            .encrypt_frame(b"frame-0", MediaKind::Audio)
            .expect("encrypt should succeed");

        // Set key at index 1 (now current), encrypt another frame
        ctx.set_key("alice", 1, b"key-one-material-nopqrstuvwxyz1");
        let encrypted_1 = ctx
            .encrypt_frame(b"frame-1", MediaKind::Audio)
            .expect("encrypt should succeed");

        // Both should decrypt successfully (both keys still in ring)
        let dec_0 = ctx
            .decrypt_frame(&encrypted_0, "alice", MediaKind::Audio)
            .expect("should not error")
            .expect("should decrypt");
        assert_eq!(&dec_0, b"frame-0");

        let dec_1 = ctx
            .decrypt_frame(&encrypted_1, "alice", MediaKind::Audio)
            .expect("should not error")
            .expect("should decrypt");
        assert_eq!(&dec_1, b"frame-1");
    }

    #[test]
    fn frame_too_short() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_key("alice", 0, b"some-key");

        let result = ctx.decrypt_frame(&[0; 5], "alice", MediaKind::Audio);
        assert!(matches!(result, Err(E2eeError::FrameTooShort(_))));
    }

    #[test]
    fn no_key_returns_none_on_encrypt() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        // No local identity set
        let result = ctx.encrypt_frame(b"data", MediaKind::Audio);
        assert!(result.is_none());
    }

    #[test]
    fn key_ratcheting() {
        // Simulate: sender ratchets key and encrypts, receiver uses ratchet window to find it
        let sender_opts = E2eeOptions {
            ratchet_window_size: 5,
            auto_ratchet: true,
            ..Default::default()
        };
        let sender = E2eeContext::new(sender_opts);
        sender.set_local_identity("alice");
        sender.set_key("alice", 0, b"initial-key-material-1234567890");

        // Sender ratchets their key, then encrypts
        {
            let mut inner = sender.inner.write().unwrap();
            inner
                .key_manager
                .participants
                .get_mut("alice")
                .unwrap()
                .ratchet(0);
        }

        let encrypted = sender
            .encrypt_frame(b"test-data", MediaKind::Audio)
            .expect("encrypt should succeed");

        // Receiver has the original (pre-ratchet) key
        let receiver_opts = E2eeOptions {
            ratchet_window_size: 5,
            auto_ratchet: true,
            ..Default::default()
        };
        let receiver = E2eeContext::new(receiver_opts);
        receiver.set_key("alice", 0, b"initial-key-material-1234567890");

        // Decryption should succeed via ratchet window (receiver ratchets to find the key)
        let decrypted = receiver
            .decrypt_frame(&encrypted, "alice", MediaKind::Audio)
            .expect("should not error")
            .expect("ratchet window should find the key");
        assert_eq!(&decrypted, b"test-data");
    }

    #[test]
    fn unencrypted_header_audio() {
        assert_eq!(unencrypted_header_size(b"anything", MediaKind::Audio), 0);
    }

    #[test]
    fn unencrypted_header_video_simple() {
        // No extension bit
        assert_eq!(
            unencrypted_header_size(&[0x10, 0x00, 0x00], MediaKind::Video),
            1
        );
    }

    #[test]
    fn unencrypted_header_video_with_extension() {
        // Extension bit set, I bit set, short PictureID
        assert_eq!(
            unencrypted_header_size(&[0x90, 0x80, 0x42, 0x00], MediaKind::Video),
            3
        );
    }
}

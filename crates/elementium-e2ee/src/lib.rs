//! `LiveKit`-compatible E2EE frame encryption for `MatrixRTC` calls.
//!
//! Implements the same frame format as `livekit-client`'s E2EE Worker so that
//! Elementium can interoperate with other Element Call clients (browser, mobile).
//!
//! ## Frame format (after encryption)
//!
//! Transcribed from livekit-client's `e2ee.worker` (`FrameCryptor.encodeFunction`), which
//! documents it as:
//!
//! ```text
//! ---------+-------------------------+-+---------+----
//! payload  |IV...(length = IV_LENGTH)|R|IV_LENGTH|KID |
//! ---------+-------------------------+-+---------+----
//! ```
//!
//! i.e. the full frame is:
//!
//! ```text
//! [unencrypted header (N bytes)] [encrypted payload] [IV (12 bytes)] [IV_LENGTH (1 byte)] [key_index (1 byte)]
//! ```
//!
//! - The trailer is **two** bytes, not one: `IV_LENGTH` then `key_index`. Reading only the
//!   final byte still yields the right key index, which makes a one-byte trailer look
//!   plausible -- but it shifts the IV window by one byte and leaves a stray byte on the
//!   front of the ciphertext, so every frame then fails to authenticate.
//! - Audio: N=1. The Opus TOC byte is left in the clear (RFC 6716 §3.1) so the SFU can
//!   read the packet's framing; it is *not* encrypted.
//! - Video: N=10 for a keyframe, N=3 for a delta frame -- livekit's
//!   `UNENCRYPTED_BYTES` constants, covering the VP8 uncompressed data chunk.
//! - Those header bytes are additionally passed to AES-GCM as **associated data**, so they
//!   are authenticated even though they are not encrypted. Omitting the AAD fails
//!   authentication just as surely as using the wrong key.
//! - IV: 12-byte nonce, transmitted in the clear alongside the frame.
//! - Encryption: AES-128-GCM with 16-byte auth tag appended to ciphertext
//! - Key derivation: HKDF-SHA256, salt `"LKFrameEncryptionKey"`, info 128 zero bytes

pub mod h264;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use elementium_types::{PlaintextMedia, WireMedia};
use hkdf::Hkdf;
use sha2::Sha256;

/// Maximum number of keys per participant in a key ring.
const MAX_KEYS: usize = 256;

/// Maximum number of keys per participant in a key ring, as a `u8`.
///
/// Size of AES-GCM initialization vector.
const IV_SIZE: usize = 12;

/// [`IV_SIZE`] as it appears on the wire, in the frame trailer's `IV_LENGTH` byte.
///
/// A separate constant rather than a cast, matching the [`MAX_KEYS`]/[`MAX_KEYS_U8`] pair.
const IV_SIZE_U8: u8 = 12;

/// Size of the frame trailer: `[IV_LENGTH][key_index]`.
///
/// Two bytes, matching livekit-client's `frameTrailer`. See the module docs for why a
/// one-byte trailer silently "works" for the key index while breaking everything else.
const TRAILER_SIZE: usize = 2;

/// Size of AES-GCM authentication tag.
const TAG_SIZE: usize = 16;

/// HKDF info string matching livekit-client's E2EE Worker.
/// Default HKDF salt, matching livekit-client's `SALT` constant (`e2ee/constants.ts`).
/// Overridable via `E2eeOptions::ratchet_salt`, which is the same knob livekit exposes as
/// `KeyProviderOptions.ratchetSalt`.
const DEFAULT_RATCHET_SALT: &[u8] = b"LKFrameEncryptionKey";

/// HKDF `info` parameter: 128 zero bytes.
///
/// Matches livekit-client's `getAlgoOptions` (`e2ee/utils.ts`), which passes
/// `info: new ArrayBuffer(128)` -- a zero-filled buffer, *not* a label string. Getting
/// this wrong (or swapping it with the salt) still derives a perfectly valid-looking AES
/// key, it just is not the *same* key the peer derived, so every frame fails to
/// authenticate and all inbound media is silently dropped.
const HKDF_INFO: [u8; 128] = [0u8; 128];

/// The shared secret a key ring entry was built from (the key provider's raw key bytes).
///
/// Paired with [`HkdfSalt`] purely so the two byte-string inputs to HKDF cannot be passed
/// in the wrong order. They were: this crate derived keys with the salt and info arguments
/// swapped relative to livekit-client, which still produces a perfectly valid-looking
/// AES key -- just not the same one the peer derived, so every inbound frame failed to
/// authenticate and all remote media was dropped. Two distinct types make that class of
/// mistake a compile error rather than silence on the call.
#[derive(Clone, Copy)]
struct KeyMaterial<'a>(&'a [u8]);

/// The HKDF salt (livekit's `KeyProviderOptions.ratchetSalt`). See [`KeyMaterial`].
#[derive(Clone, Copy)]
struct HkdfSalt<'a>(&'a [u8]);

/// A key ring slot index. Every `u8` is one, which is the whole design.
///
/// The frame trailer carries a single byte, so a sender's index is `0..=255`. With 256
/// slots that byte *is* the slot and nothing has to be reduced -- which matters, because
/// both of the previous designs were wrong in ways that silenced calls.
///
/// Rejecting wire bytes >= the ring size came first. That made every peer past its 16th
/// rotation permanently inaudible, and bought no security: the index only chooses which
/// key to try, and AES-GCM authentication is what rejects a wrong one, so an attacker who
/// steers the choice still cannot forge a tag.
///
/// Reducing modulo 16 came next, and aliases. Element Call configures livekit with
/// `keyringSize: 256`, so a peer can legitimately be at index 19 -- which landed in the
/// same slot as index 3 and overwrote a key still in use. Two distinct keys sharing a slot
/// means whichever arrived last wins and the other participant goes silent, intermittently
/// and only on long calls.
///
/// 256 slots cost a few kilobytes per participant and make both faults unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyIndex(u8);

impl KeyIndex {
    /// Take a rotation counter -- from a local caller or from a frame trailer -- as a slot.
    ///
    /// Total: every `u8` names a real slot, so this cannot fail and nothing is lost.
    #[must_use]
    pub const fn new(raw: u8) -> Self {
        Self(raw)
    }

    /// The validated slot index into `KeyRing::keys` (`0..MAX_KEYS`).
    fn slot(self) -> usize {
        usize::from(self.0)
    }

    /// The wire-format byte for this index (already `0..MAX_KEYS`, so this is exact).
    #[must_use]
    pub const fn as_wire_byte(self) -> u8 {
        self.0
    }
}

impl std::fmt::Display for KeyIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a frame is, for the purpose of framing it.
///
/// Video is split by codec rather than left as one variant, because how much of a frame
/// stays in the clear is a codec question with two entirely different answers, and getting
/// it wrong produces frames the peer cannot authenticate — silently, and only at their end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    /// VP8: a fixed 10 or 3 bytes, read from the frame tag.
    VideoVp8,
    /// H.264: two bytes into the first slice NAL unit, and the ciphertext is RBSP-escaped.
    /// See [`h264`].
    VideoH264,
}

/// Configuration options for E2EE, matching livekit-client's `KeyProviderOptions`.
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

const fn default_ratchet_window() -> u32 {
    0
}

const fn default_true() -> bool {
    true
}

impl E2eeOptions {
    /// The HKDF salt to derive with: the configured `ratchet_salt` if set, otherwise
    /// livekit's default. Encoded as UTF-8 bytes, matching livekit-client's
    /// `getAlgoOptions`, which does `new TextEncoder().encode(salt)`.
    fn salt(&self) -> HkdfSalt<'_> {
        HkdfSalt(
            self.ratchet_salt
                .as_ref()
                .map_or(DEFAULT_RATCHET_SALT, |s| s.as_bytes()),
        )
    }
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
    /// The frame trailer's `IV_LENGTH` byte was not 12.
    ///
    /// livekit only ever emits 12, and this byte is attacker-controlled, so anything else
    /// is rejected rather than trusted as a length.
    #[error("unsupported IV length {0} in frame trailer (expected {IV_SIZE_U8})")]
    UnsupportedIvLength(u8),
    #[error("internal E2EE lock was poisoned by a panic in another thread")]
    LockPoisoned,
}

/// A ring of encryption keys for a single participant.
struct KeyRing {
    /// Keys indexed by `key_index` (`0..MAX_KEYS`). `None` means slot is unused.
    keys: [Option<DerivedKey>; MAX_KEYS],
    /// The currently active key index for encryption.
    current_index: KeyIndex,
}

/// A derived encryption key ready for AES-GCM.
struct DerivedKey {
    /// The raw key material from the JS side. `Zeroizing` wipes it from memory when this
    /// key is replaced (rotation/ratcheting) or dropped, rather than leaving copies of
    /// past key material sitting in freed heap memory.
    raw_material: zeroize::Zeroizing<Vec<u8>>,
    /// The AES-128-GCM cipher derived from the raw material via HKDF.
    cipher: Aes128Gcm,
    /// Non-secret [`key_fingerprint`] of `raw_material`, for correlating logs.
    fingerprint: [u8; 4],
}

impl KeyRing {
    fn new() -> Self {
        Self {
            keys: std::array::from_fn(|_| None),
            current_index: KeyIndex::new(0),
        }
    }

    fn set_key(&mut self, index: KeyIndex, material: KeyMaterial<'_>, salt: HkdfSalt<'_>) {
        self.replace_key(index, material, salt);
        self.current_index = index;
    }

    /// Overwrite the key at `index` without changing which key is used for *encryption*.
    ///
    /// Used when committing a successful ratchet found while decrypting an inbound frame:
    /// that says nothing about which key this ring should encrypt with.
    fn replace_key(&mut self, index: KeyIndex, material: KeyMaterial<'_>, salt: HkdfSalt<'_>) {
        let cipher = derive_cipher(material, salt);
        if let Some(slot) = self.keys.get_mut(index.slot()) {
            *slot = Some(DerivedKey {
                raw_material: zeroize::Zeroizing::new(material.0.to_vec()),
                cipher,
                fingerprint: key_fingerprint(material),
            });
        }
    }

    /// `(index, fingerprint_hex)` for every occupied slot.
    fn inventory(&self) -> Vec<(usize, String)> {
        self.keys
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|k| (i, fingerprint_hex(k.fingerprint))))
            .collect()
    }

    /// The raw key material behind the key at `index`, for deriving a ratchet chain.
    fn raw_material(&self, index: KeyIndex) -> Option<&[u8]> {
        self.keys
            .get(index.slot())
            .and_then(Option::as_ref)
            .map(|k| k.raw_material.as_slice())
    }

    fn get_cipher(&self, index: KeyIndex) -> Option<&Aes128Gcm> {
        self.keys
            .get(index.slot())
            .and_then(Option::as_ref)
            .map(|k| &k.cipher)
    }

    fn current_cipher(&self) -> Option<(&Aes128Gcm, KeyIndex)> {
        self.get_cipher(self.current_index)
            .map(|c| (c, self.current_index))
    }
}

/// A participant identity, distinct from any other `String`/`&str` floating around the call
/// stack (room ids, track ids, etc.).
///
/// Used as the `HashMap` key for per-participant key rings, so a caller can't accidentally
/// hand this API a different kind of id string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParticipantId(String);

impl ParticipantId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Manages encryption keys for all participants in a call.
struct KeyManager {
    /// Per-participant key rings.
    participants: HashMap<ParticipantId, KeyRing>,
    /// The local participant's identity (for encryption).
    local_identity: Option<ParticipantId>,
}

impl KeyManager {
    fn new() -> Self {
        Self {
            participants: HashMap::new(),
            local_identity: None,
        }
    }

    fn set_key(
        &mut self,
        participant: &ParticipantId,
        key_index: KeyIndex,
        material: &[u8],
        salt: HkdfSalt<'_>,
    ) {
        let ring = self
            .participants
            .entry(participant.clone())
            .or_insert_with(KeyRing::new);
        ring.set_key(key_index, KeyMaterial(material), salt);
        // Only the key length (a size, not secret material) and identifiers are
        // logged here. The key bytes/derived cipher material must never appear
        // in a log event.
        tracing::info!(
            participant = %participant,
            key_index = %key_index,
            key_len = material.len(),
            // Non-secret hash prefix. Lets a log reader tell "we never received their
            // key" apart from "we received a different/stale one than they are using",
            // which are indistinguishable from the decrypt failure alone.
            fingerprint = %fingerprint_hex(key_fingerprint(KeyMaterial(material))),
            "E2EE key set for participant"
        );
    }
}

/// A short, non-secret identifier for a key, for correlating logs across a call.
///
/// The first four bytes of `SHA-256(material)`. Four bytes of a hash of a 16-byte secret
/// is not a practical route back to the key, but it is enough to see at a glance whether
/// the key we hold for a participant is the same one we held a moment ago -- which is the
/// difference between "we never got their key" and "we got a stale one".
fn key_fingerprint(material: KeyMaterial<'_>) -> [u8; 4] {
    use sha2::Digest as _;
    let digest = Sha256::digest(material.0);
    let mut out = [0u8; 4];
    for (slot, byte) in out.iter_mut().zip(digest.iter()) {
        *slot = *byte;
    }
    out
}

/// The key index a wire frame claims, without validating anything else about the frame.
///
/// Diagnostics only: the sender's index is what makes a decrypt failure interpretable.
const fn peek_key_index(frame: &[u8]) -> Option<u8> {
    if frame.len() < TRAILER_SIZE {
        return None;
    }
    match frame.last() {
        Some(b) => Some(*b),
        None => None,
    }
}

/// Whether a frame's trailer looks like one livekit wrote.
///
/// The second-to-last byte is `IV_LENGTH`, and livekit always writes 12. Anything else
/// means the bytes being read are not a livekit frame tail, and therefore that the "key
/// index" read beside it is not a key index either -- it is whatever happened to be at
/// that offset.
///
/// This distinction is worth a whole diagnostic on its own. "No key decrypts this frame"
/// has two completely different causes that look identical: a key we were never given, or
/// a frame we are misreading. The first is someone else's problem to fix and the second is
/// ours, and without this there is nothing to tell them apart -- a real session reported
/// key indices 3 and 6 that no participant had ever announced, which is exactly what a
/// misread produces.
///
/// Cases that produce a wrong tail here: an Opus payload still wrapped in RFC 2198 RED, a
/// VP8 payload with its descriptor still attached, or a partially reassembled frame.
const fn trailer_looks_like_livekit(frame: &[u8]) -> Option<bool> {
    match frame.len().checked_sub(2) {
        Some(at) => match frame.split_at(at).1.first() {
            Some(iv_length) => Some(*iv_length == IV_SIZE_U8),
            None => None,
        },
        None => None,
    }
}

/// Render a [`key_fingerprint`] as hex for logging.
fn fingerprint_hex(fp: [u8; 4]) -> String {
    use std::fmt::Write as _;
    fp.iter().fold(String::with_capacity(8), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Thread-safe E2EE context shared across the media pipeline.
pub struct E2eeContext {
    inner: Arc<RwLock<E2eeContextInner>>,
    /// Count of frames no key could decrypt, for throttling the failure log.
    ///
    /// Outside the `RwLock` so the failure path never has to take a lock just to decide
    /// whether to log.
    undecryptable_frames: Arc<std::sync::atomic::AtomicU64>,
    /// Keys installed that have not yet decrypted anything, and when they were installed.
    ///
    /// The gap between the two is the quantity behind "it takes ages before I can hear
    /// anyone": a key travels as a to-device message, and until it arrives every frame the
    /// sender has already encrypted with it is undecryptable. Nothing else in the system
    /// reports that interval -- the key arriving and the audio starting are separate events
    /// in separate subsystems, and only their difference says whether distribution or
    /// something later is at fault.
    ///
    /// Entries are removed on first use, so this holds only keys that have never worked.
    first_use: Arc<Mutex<HashMap<(ParticipantId, KeyIndex), Instant>>>,
    /// How many entries `first_use` holds, so the decrypt path can skip the lock entirely
    /// once every key has been accounted for. Without it this would take a mutex on every
    /// frame of every call for a measurement that is made once per key.
    awaiting_first_use: Arc<std::sync::atomic::AtomicUsize>,
}

struct E2eeContextInner {
    key_manager: KeyManager,
    options: E2eeOptions,
    /// Per-participant frame counters for IV generation (outbound).
    frame_counter: u64,
    /// The SFU's "server injected frame" marker, if it sent one.
    ///
    /// An SFU may insert its own frames into a stream -- silence or a black picture while a
    /// publisher is muted, for instance. Those cannot be encrypted, because the SFU has no
    /// key, so they arrive in the clear with this trailer appended to identify them.
    /// livekit's own cryptor checks for it and passes such frames through untouched
    /// (`isFrameServerInjected`, `FrameCryptor.ts`).
    ///
    /// Without it every server-injected frame is fed to AES-GCM, fails to authenticate, and
    /// is dropped -- indistinguishable in the log from a missing key, and arriving exactly
    /// when a participant mutes, which is the least convenient moment to be debugging.
    sif_trailer: Option<Vec<u8>>,
}

impl Clone for E2eeContext {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            undecryptable_frames: Arc::clone(&self.undecryptable_frames),
            first_use: Arc::clone(&self.first_use),
            awaiting_first_use: Arc::clone(&self.awaiting_first_use),
        }
    }
}

impl E2eeContext {
    /// Create a new E2EE context with the given options.
    #[must_use]
    pub fn new(options: E2eeOptions) -> Self {
        Self {
            inner: Arc::new(RwLock::new(E2eeContextInner {
                key_manager: KeyManager::new(),
                options,
                frame_counter: 0,
                sif_trailer: None,
            })),
            undecryptable_frames: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            first_use: Arc::new(Mutex::new(HashMap::new())),
            awaiting_first_use: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Replace the options without disturbing the keys.
    ///
    /// livekit's key provider does not guarantee that its `init` reaches us before its
    /// first key: in a real call a key arrived two seconds ahead of it, so the bridge had
    /// to synthesise an init with no options at all -- which leaves the ratchet window at
    /// zero and ratcheting therefore off. Rebuilding the context when the real options
    /// finally arrive would throw away every key installed in between, so the options are
    /// updated in place and the keyring is left alone.
    pub fn set_options(&self, options: E2eeOptions) {
        let Ok(mut inner) = self.inner.write() else {
            tracing::error!("E2EE lock poisoned; options not updated");
            return;
        };
        tracing::info!(
            ratchet_window_size = options.ratchet_window_size,
            auto_ratchet = options.auto_ratchet,
            "E2EE options updated, keys preserved"
        );
        inner.options = options;
    }

    /// Every key currently held, as `participant@index:fingerprint`, for diagnostics.
    ///
    /// Never includes key material -- only the [`key_fingerprint`] hash prefix.
    fn key_inventory(&self) -> String {
        let Ok(inner) = self.lock_read() else {
            return "<lock poisoned>".to_owned();
        };
        let mut entries: Vec<String> = inner
            .key_manager
            .participants
            .iter()
            .flat_map(|(participant, ring)| {
                ring.inventory()
                    .into_iter()
                    .map(move |(index, fp)| format!("{participant}@{index}:{fp}"))
            })
            .collect();
        entries.sort();
        entries.join(" ")
    }

    /// Acquire the read lock, translating poisoning into a proper error
    /// instead of panicking.
    fn lock_read(&self) -> Result<RwLockReadGuard<'_, E2eeContextInner>, E2eeError> {
        self.inner.read().map_err(|_| {
            tracing::error!(reason = "lock_poisoned", "E2EE internal read lock poisoned");
            E2eeError::LockPoisoned
        })
    }

    /// Acquire the write lock, translating poisoning into a proper error
    /// instead of panicking.
    fn lock_write(&self) -> Result<RwLockWriteGuard<'_, E2eeContextInner>, E2eeError> {
        self.inner.write().map_err(|_| {
            tracing::error!(
                reason = "lock_poisoned",
                "E2EE internal write lock poisoned"
            );
            E2eeError::LockPoisoned
        })
    }

    /// Set the local participant identity (used for choosing the encryption key).
    ///
    /// If the internal lock has been poisoned by a panic in another thread,
    /// this call is a documented no-op (the identity is not updated).
    pub fn set_local_identity(&self, identity: &str) {
        let Ok(mut inner) = self.inner.write() else {
            tracing::error!(
                reason = "lock_poisoned",
                "E2EE lock poisoned; failed to set local identity"
            );
            return;
        };
        inner.key_manager.local_identity = Some(ParticipantId::new(identity));
        tracing::info!(participant = identity, "E2EE local identity set");
    }

    /// Store an encryption key for a participant.
    ///
    /// If the internal lock has been poisoned by a panic in another thread,
    /// this call is a documented no-op (the key is not stored).
    pub fn set_key(&self, participant: &str, key_index: u8, key_material: &[u8]) {
        let Ok(mut inner) = self.inner.write() else {
            tracing::error!(
                reason = "lock_poisoned",
                participant = participant,
                "E2EE lock poisoned; failed to set key"
            );
            return;
        };
        let participant = ParticipantId::new(participant);
        let index = KeyIndex::new(key_index);
        // Split the borrow: the salt comes from `options` while `key_manager` is mutated.
        let E2eeContextInner {
            options,
            key_manager,
            ..
        } = &mut *inner;
        key_manager.set_key(&participant, index, key_material, options.salt());
        drop(inner);
        self.note_key_installed(participant, index);
    }

    /// Start the clock on a newly installed key.
    ///
    /// Overwrites any previous entry for the same slot: a re-sent key restarts the
    /// measurement, which is what should be measured -- the interval that matters is from
    /// the material we are currently holding, not from one that turned out to be stale.
    fn note_key_installed(&self, participant: ParticipantId, index: KeyIndex) {
        let Ok(mut pending) = self.first_use.lock() else {
            tracing::error!(reason = "lock_poisoned", "E2EE key-timing lock poisoned");
            return;
        };
        if pending
            .insert((participant, index), Instant::now())
            .is_none()
        {
            self.awaiting_first_use
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Report how long a key waited before it first decrypted anything, once.
    ///
    /// `via` distinguishes the key we were given from one reached by ratcheting: a long
    /// wait that ends in a ratcheted key means the sender had moved on before their key
    /// reached us, which is a different fault from the key simply being slow.
    fn note_key_first_used(&self, participant: &ParticipantId, index: KeyIndex, via: &'static str) {
        // The common case by an enormous margin -- every frame after the first for each
        // key -- and it costs an atomic load rather than a mutex.
        if self
            .awaiting_first_use
            .load(std::sync::atomic::Ordering::Relaxed)
            == 0
        {
            return;
        }
        let Ok(mut pending) = self.first_use.lock() else {
            return;
        };
        let Some(installed) = pending.remove(&(participant.clone(), index)) else {
            return;
        };
        self.awaiting_first_use
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        drop(pending);
        tracing::info!(
            participant = %participant,
            key_index = %index,
            via,
            waited_ms = installed.elapsed().as_millis(),
            "E2EE key decrypted its first frame"
        );
    }

    /// Check if E2EE has keys available for encryption.
    ///
    /// Returns `false` (fail closed) if the internal lock has been poisoned
    /// by a panic in another thread.
    #[must_use]
    pub fn has_encryption_key(&self) -> bool {
        let Ok(inner) = self.inner.read() else {
            tracing::error!("E2EE lock poisoned; treating as no encryption key available");
            return false;
        };
        inner.key_manager.local_identity.as_ref().map_or_else(
            || {
                // If no local identity set, check if any key exists
                inner
                    .key_manager
                    .participants
                    .values()
                    .any(|ring| ring.current_cipher().is_some())
            },
            |identity| {
                inner
                    .key_manager
                    .participants
                    .get(identity)
                    .and_then(|ring| ring.current_cipher())
                    .is_some()
            },
        )
    }

    /// Encrypt a media frame using the `LiveKit` E2EE frame format.
    ///
    /// Returns `None` if no encryption key is available (passthrough mode),
    /// if the internal lock is poisoned, or if the outbound frame counter
    /// has been exhausted (which would otherwise force IV/nonce reuse).
    pub fn encrypt_frame(&self, frame: &PlaintextMedia, kind: MediaKind) -> Option<WireMedia> {
        let frame = frame.as_bytes();
        let Ok(mut inner) = self.inner.write() else {
            tracing::error!(
                reason = "lock_poisoned",
                "E2EE dropping outbound frame: internal lock poisoned"
            );
            return None;
        };

        // Get the local participant's current key index
        let Some(identity) = inner.key_manager.local_identity.clone() else {
            tracing::warn!(
                reason = "no_local_identity",
                "E2EE dropping outbound frame: local identity not set"
            );
            return None;
        };

        // Generate IV from frame counter (mutate before borrowing participants).
        // Use checked_add so we fail closed instead of wrapping the counter and
        // reusing an IV/nonce pair, which would be a catastrophic AES-GCM break.
        let counter = inner.frame_counter;
        let Some(next_counter) = inner.frame_counter.checked_add(1) else {
            tracing::error!(
                reason = "frame_counter_exhausted",
                participant = %identity,
                "E2EE dropping outbound frame: frame counter exhausted"
            );
            return None;
        };
        inner.frame_counter = next_counter;

        let Some(ring) = inner.key_manager.participants.get(&identity) else {
            tracing::warn!(
                reason = "no_key_for_participant",
                participant = %identity,
                "E2EE dropping outbound frame: no key ring for local participant"
            );
            return None;
        };
        let Some((cipher, key_index)) = ring.current_cipher() else {
            tracing::warn!(
                reason = "no_current_key",
                participant = %identity,
                "E2EE dropping outbound frame: no current key set"
            );
            return None;
        };

        let Framing { header_size, rbsp } = framing(frame, kind);
        let Some(header) = frame.get(..header_size) else {
            tracing::error!(
                reason = "malformed_header",
                participant = %identity,
                "E2EE dropping outbound frame: frame shorter than expected header"
            );
            return None;
        };
        let Some(payload) = frame.get(header_size..) else {
            tracing::error!(
                reason = "malformed_payload",
                participant = %identity,
                "E2EE dropping outbound frame: frame shorter than expected payload"
            );
            return None;
        };

        let iv = build_iv(counter);
        let nonce = Nonce::from(iv);
        // The unencrypted header travels in the clear but is authenticated as associated
        // data, exactly as livekit does (`additionalData: frameHeader`). A receiver that
        // omits the AAD gets an authentication failure indistinguishable from a wrong key.
        let aead_payload = aes_gcm::aead::Payload {
            msg: payload,
            aad: header,
        };
        let ciphertext = match cipher.encrypt(&nonce, aead_payload) {
            Ok(ct) => ct,
            Err(e) => {
                tracing::warn!(
                    reason = "cipher_error",
                    participant = %identity,
                    error = %e,
                    "E2EE dropping outbound frame: AES-GCM encryption failed"
                );
                return None;
            }
        };
        // Nothing further needs the lock; release it before allocating the output.
        drop(inner);

        // Build output: [header] [ciphertext (includes GCM tag)] [IV] [IV_LENGTH] [key_index]
        let capacity = ciphertext
            .len()
            .saturating_add(IV_SIZE)
            .saturating_add(TRAILER_SIZE);
        let mut body = Vec::with_capacity(capacity);
        body.extend_from_slice(&ciphertext);
        body.extend_from_slice(&iv);
        body.push(IV_SIZE_U8);
        body.push(key_index.as_wire_byte());

        // Everything after the clear header is escaped for H.264, exactly as livekit does.
        // Ciphertext is uniformly random, so it produces start-code sequences by chance;
        // without this a small fraction of frames are misparsed downstream.
        let body = if rbsp { h264::write_rbsp(&body) } else { body };

        let mut output = Vec::with_capacity(header_size.saturating_add(body.len()));
        output.extend_from_slice(header);
        output.extend_from_slice(&body);

        Some(WireMedia::from_encrypted(output))
    }

    /// Decrypt a media frame, trying all known participant keys.
    ///
    /// Used when the sender's identity is unknown (e.g., inbound RTP from the SFU
    /// where we only have the raw frame, not the participant identity).
    ///
    /// # Errors
    ///
    /// Returns [`E2eeError::LockPoisoned`] if the internal lock was poisoned, or
    /// [`E2eeError::DecryptionFailed`] if no known participant's key(s) could
    /// decrypt the frame.
    pub fn decrypt_frame_any(
        &self,
        frame: &WireMedia,
        kind: MediaKind,
    ) -> Result<Option<PlaintextMedia>, E2eeError> {
        // The SFU's own frames carry no encryption, because the SFU holds no key. Checked
        // before anything else: feeding one to AES-GCM cannot succeed, and the failure is
        // indistinguishable from a missing key.
        if self.is_server_injected(frame.as_bytes()) {
            return Ok(Some(PlaintextMedia::assume_peer_sends_unencrypted(
                WireMedia::from_network(frame.as_bytes().to_vec()),
            )));
        }

        let (participants, local_identity): (Vec<ParticipantId>, Option<ParticipantId>) = {
            let inner = self.lock_read()?;
            (
                inner.key_manager.participants.keys().cloned().collect(),
                inner.key_manager.local_identity.clone(),
            )
        };

        if participants.is_empty() {
            tracing::warn!(
                reason = "no_participants_with_keys",
                "E2EE dropping inbound frame: no participant keys known"
            );
            return Ok(None);
        }

        // Exclude our own identity from the candidates actually tried: an inbound frame
        // that "decrypts" under our own key is not one we should accept, and matching it
        // here would be the same mistake as the reverse -- a remote key landing under
        // `local_identity` and silently becoming the key we encrypt outbound media with.
        // The emptiness check above still covers "we know no keys at all"; this only
        // narrows which of the known keys are candidates for an *inbound* frame.
        for participant in participants.iter().filter(|id| Some(*id) != local_identity.as_ref()) {
            if let Ok(Some(decrypted)) = self.decrypt_frame(frame, participant.as_str(), kind) {
                return Ok(Some(decrypted));
            }
        }

        // This frame is genuinely lost: no key we hold authenticates it. Report the key
        // index the *sender* used alongside every key we actually have, because the
        // interesting cases are only distinguishable by comparing the two:
        //
        // - the sender's index is absent from the inventory -> their key never reached us
        // - the index is present but still fails -> we hold the wrong/stale key for it
        //
        // Throttled: at ~50 frames/sec per stream a lost key would otherwise bury the log.
        let lost = self
            .undecryptable_frames
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        if lost == 1 || lost.is_multiple_of(200) {
            tracing::warn!(
                reason = "no_key_decrypts_frame",
                frame_key_index = peek_key_index(frame.as_bytes())
                    .map_or_else(|| "?".to_owned(), |i| i.to_string()),
                // False here means the frame is not shaped like one livekit wrote, so the
                // index beside it is not a key index and no key could ever have worked.
                // See `trailer_looks_like_livekit`.
                trailer_is_livekit_shaped = trailer_looks_like_livekit(frame.as_bytes())
                    .map_or_else(|| "?".to_owned(), |ok| ok.to_string()),
                frame_len = frame.as_bytes().len(),
                media_kind = ?kind,
                undecryptable_frames = lost,
                keys_held = %self.key_inventory(),
                "E2EE dropping inbound frame: no key we hold decrypts it"
            );
        }

        Err(E2eeError::DecryptionFailed(format!(
            "tried {} participants, none could decrypt",
            participants.len()
        )))
    }

    /// Record the SFU's server-injected-frame marker.
    ///
    /// Sent by livekit as `setSifTrailer` when the room is joined. Frames ending with it
    /// are the SFU's own and are not encrypted.
    ///
    /// # Errors
    ///
    /// Returns [`E2eeError`] if the context lock is poisoned.
    pub fn set_sif_trailer(&self, trailer: Vec<u8>) -> Result<(), E2eeError> {
        let mut inner = self.lock_write()?;
        tracing::info!(
            len = trailer.len(),
            "E2EE server-injected-frame trailer recorded; such frames pass through in the clear"
        );
        inner.sif_trailer = if trailer.is_empty() {
            None
        } else {
            Some(trailer)
        };
        Ok(())
    }

    /// Whether this frame is one the SFU injected, and so is not encrypted at all.
    fn is_server_injected(&self, frame: &[u8]) -> bool {
        let Ok(inner) = self.lock_read() else {
            return false;
        };
        inner
            .sif_trailer
            .as_ref()
            .is_some_and(|trailer| frame.ends_with(trailer))
    }

    /// Decrypt a media frame using the `LiveKit` E2EE frame format.
    ///
    /// Returns `Err` if the frame is malformed or decryption fails.
    /// Returns `Ok(None)` if no key is available for the participant.
    ///
    /// # Errors
    ///
    /// Returns [`E2eeError::FrameTooShort`] if the frame is too small to contain
    /// the E2EE metadata, [`E2eeError::UnsupportedIvLength`] if the trailer's
    /// `IV_LENGTH` byte is not 12, [`E2eeError::LockPoisoned`] if the internal lock
    /// was poisoned, or [`E2eeError::DecryptionFailed`] if AES-GCM decryption
    /// fails for every key tried (including the ratchet window, if enabled).
    pub fn decrypt_frame(
        &self,
        frame: &WireMedia,
        participant: &str,
        kind: MediaKind,
    ) -> Result<Option<PlaintextMedia>, E2eeError> {
        // Undo the sender's RBSP escaping before anything reads offsets from the frame: the
        // IV and trailer sit at the end of the escaped region, so their positions are wrong
        // until this has run. Borrows `unescaped`, which must outlive the parts below.
        let mut unescaped = Vec::new();
        let bytes = rbsp_decoded(frame.as_bytes(), kind, &mut unescaped);
        let FrameParts {
            header,
            ciphertext,
            iv,
            key_index,
        } = split_frame(bytes, kind)?;
        let frame = bytes;

        let inner = self.lock_read()?;
        let options = inner.options.clone();
        let participant = ParticipantId::new(participant);

        let Some(ring) = inner.key_manager.participants.get(&participant) else {
            tracing::warn!(
                reason = "no_key_for_participant",
                participant = %participant,
                "E2EE dropping inbound frame: no key ring for participant"
            );
            return Ok(None);
        };

        // Copy IV into a fixed array for Nonce::from
        let iv_array: [u8; IV_SIZE] = iv
            .try_into()
            .map_err(|_| E2eeError::FrameTooShort(frame.len()))?;

        // The header travels in the clear but is authenticated: it must be supplied as
        // associated data or GCM authentication fails, looking exactly like a wrong key.
        let nonce = Nonce::from(iv_array);
        let aead_payload = || aes_gcm::aead::Payload {
            msg: ciphertext,
            aad: header,
        };

        // Try the indicated key index first
        if let Some(cipher) = ring.get_cipher(key_index)
            && let Ok(plaintext) = cipher.decrypt(&nonce, aead_payload())
        {
            let out = PlaintextMedia::from_decrypted(prepend_header(header, &plaintext));
            drop(inner);
            self.note_key_first_used(&participant, key_index, "installed_key");
            return Ok(Some(out));
        }

        // If ratcheting is enabled, walk the ratchet chain forward looking for a key that
        // authenticates this frame.
        //
        // Deliberately computed on a *copy* of the material rather than by mutating the
        // ring in place: a failed search must leave the participant's key exactly as it
        // was. Advancing the stored key on every failure -- as this once did -- burns
        // through the whole window on the first undecryptable frame and permanently
        // destroys the real key, so the participant can never be decrypted again even
        // once a good frame arrives.
        let ratchet_material = if options.ratchet_window_size > 0 {
            ring.raw_material(key_index)
                .map(<[u8]>::to_vec)
                .map(zeroize::Zeroizing::new)
        } else {
            None
        };
        drop(inner);

        if let Some(mut material) = ratchet_material {
            let salt = options.salt();
            for _ in 0..options.ratchet_window_size {
                material = zeroize::Zeroizing::new(ratchet_key(KeyMaterial(&material), salt));
                let cipher = derive_cipher(KeyMaterial(&material), salt);
                if let Ok(plaintext) = cipher.decrypt(&nonce, aead_payload()) {
                    // Commit the ratchet only now that it is known good, so subsequent
                    // frames skip the search.
                    let mut write = self.lock_write()?;
                    if let Some(ring) = write.key_manager.participants.get_mut(&participant) {
                        ring.replace_key(key_index, KeyMaterial(&material), salt);
                    }
                    drop(write);
                    self.note_key_first_used(&participant, key_index, "ratchet");
                    return Ok(Some(PlaintextMedia::from_decrypted(prepend_header(
                        header, &plaintext,
                    ))));
                }
            }
        }

        // Debug, not warn, and deliberately so.
        //
        // `decrypt_frame_any` calls this once per known participant until one succeeds, so
        // on a healthy call every successfully decrypted frame still produces a failure
        // here for each participant tried before the right one. Logging that at `warn`
        // made a working call look catastrophic: one real problem showed up as ~11,000
        // warnings, roughly four per frame, most of them describing normal operation.
        // The single authoritative "this frame is lost" event is emitted by
        // `decrypt_frame_any` instead.
        tracing::debug!(
            reason = "decryption_failed",
            participant = %participant,
            key_index = %key_index,
            ratchet_window_size = options.ratchet_window_size,
            "E2EE key did not decrypt this frame"
        );
        Err(E2eeError::DecryptionFailed(format!(
            "participant={participant}, key_index={key_index}"
        )))
    }
}

/// An inbound frame taken apart into the four pieces decryption needs.
struct FrameParts<'a> {
    /// Travels in the clear, but is authenticated as associated data.
    header: &'a [u8],
    ciphertext: &'a [u8],
    iv: &'a [u8],
    key_index: KeyIndex,
}

/// Undo an H.264 sender's RBSP escaping, leaving anything else untouched.
///
/// Only the region after the clear header is unescaped. The header is real H.264, which the
/// encoder has already escaped for its own reasons, and unescaping that would corrupt it.
///
/// Returns a borrow of `frame` when there is nothing to do, and of `scratch` otherwise, so
/// the common case costs nothing.
fn rbsp_decoded<'a>(frame: &'a [u8], kind: MediaKind, scratch: &'a mut Vec<u8>) -> &'a [u8] {
    let Framing {
        header_size,
        rbsp: true,
    } = framing(frame, kind)
    else {
        return frame;
    };
    let (Some(header), Some(body)) = (frame.get(..header_size), frame.get(header_size..)) else {
        return frame;
    };
    // livekit checks the frame rather than remembering what it did, so a sender that did not
    // escape is left alone. Matching that matters: the check is what makes this safe to run
    // on frames from clients older than the escaping.
    if !h264::needs_rbsp_unescaping(body) {
        return frame;
    }
    scratch.clear();
    scratch.extend_from_slice(header);
    scratch.extend_from_slice(&h264::parse_rbsp(body));
    scratch
}

/// Split an inbound frame into `[header][ciphertext][IV][IV_LENGTH][key_index]`.
///
/// Every offset is re-derived with checked arithmetic rather than relying on the length
/// check alone, so a later change to `min_size` cannot silently reintroduce a panic.
fn split_frame(frame: &[u8], kind: MediaKind) -> Result<FrameParts<'_>, E2eeError> {
    let too_short = || E2eeError::FrameTooShort(frame.len());

    // Minimum frame size: header + at least a bare GCM tag + IV + 2-byte trailer.
    let min_size = TAG_SIZE
        .saturating_add(IV_SIZE)
        .saturating_add(TRAILER_SIZE);
    if frame.len() < min_size {
        return Err(too_short());
    }

    // Trailer is `[IV_LENGTH][key_index]`. The key index is the sender's unreduced rotation
    // counter, so it wraps into the ring exactly as a local index does; see [`KeyIndex`].
    // `IV_LENGTH` is validated strictly, because it is a *length* and trusting an
    // attacker-supplied one would move the IV window.
    let trailer_start = frame
        .len()
        .checked_sub(TRAILER_SIZE)
        .ok_or_else(too_short)?;
    let trailer = frame.get(trailer_start..).ok_or_else(too_short)?;
    let raw_iv_len = *trailer.first().ok_or_else(too_short)?;
    if raw_iv_len != IV_SIZE_U8 {
        return Err(E2eeError::UnsupportedIvLength(raw_iv_len));
    }
    let key_index = KeyIndex::new(*trailer.get(1).ok_or_else(too_short)?);

    let iv_start = trailer_start.checked_sub(IV_SIZE).ok_or_else(too_short)?;
    let iv = frame.get(iv_start..trailer_start).ok_or_else(too_short)?;

    let header_and_ciphertext = frame.get(..iv_start).ok_or_else(too_short)?;
    let header_size = unencrypted_header_size(header_and_ciphertext, kind);
    let header = header_and_ciphertext
        .get(..header_size)
        .ok_or_else(too_short)?;
    let ciphertext = header_and_ciphertext
        .get(header_size..)
        .ok_or_else(too_short)?;

    Ok(FrameParts {
        header,
        ciphertext,
        iv,
        key_index,
    })
}

/// Build a decrypted frame's output buffer: the unencrypted `header` followed by `plaintext`.
fn prepend_header(header: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(header.len().saturating_add(plaintext.len()));
    output.extend_from_slice(header);
    output.extend_from_slice(plaintext);
    output
}

/// Derive an AES-128-GCM cipher from raw key material using HKDF-SHA256.
fn derive_cipher(material: KeyMaterial<'_>, salt: HkdfSalt<'_>) -> Aes128Gcm {
    let hk = Hkdf::<Sha256>::new(Some(salt.0), material.0);
    // `Zeroizing` wipes the derived key output from the stack once the cipher is built,
    // rather than leaving a copy of the AES key sitting in memory after this fn returns.
    let mut okm = zeroize::Zeroizing::new([0u8; 16]); // AES-128 = 16 bytes
    // HKDF-Expand only fails when the requested output length exceeds
    // 255 * hash_len (RFC 5869 section 2.3); a fixed 16-byte request is always valid.
    #[allow(clippy::expect_used)]
    hk.expand(&HKDF_INFO, okm.as_mut_slice())
        .expect("HKDF expand output length (16 bytes) is within RFC 5869 bounds");
    // `okm` is exactly 16 bytes, the required AES-128 key length, so this
    // conversion cannot fail.
    #[allow(clippy::expect_used)]
    Aes128Gcm::new_from_slice(okm.as_slice()).expect("16 bytes is valid AES-128 key length")
}

/// Ratchet a key: derive new key material from existing material.
fn ratchet_key(material: KeyMaterial<'_>, salt: HkdfSalt<'_>) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::new(Some(salt.0), material.0);
    // 256 bits, matching livekit-client's `ratchet()` which calls
    // `crypto.subtle.deriveBits(algorithmOptions, material, 256)`.
    let mut okm = vec![0u8; 32];
    // HKDF-Expand only fails when the requested output length exceeds
    // 255 * hash_len (RFC 5869 section 2.3); `okm` is always well within that bound.
    #[allow(clippy::expect_used)]
    hk.expand(&HKDF_INFO, &mut okm)
        .expect("HKDF expand output length is within RFC 5869 bounds");
    okm
}

/// Build a 12-byte IV from a frame counter.
fn build_iv(counter: u64) -> [u8; IV_SIZE] {
    let mut iv = [0u8; IV_SIZE];
    // Put the counter in the last 8 bytes (big-endian), matching livekit-client
    iv[4..12].copy_from_slice(&counter.to_be_bytes());
    iv
}

/// How a frame is to be framed: how much stays in the clear, and whether the rest is
/// RBSP-escaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Framing {
    header_size: usize,
    /// True only for H.264 frames in which a slice NAL unit was found. livekit escapes the
    /// ciphertext exactly then, and falls back to the VP8 rules when it does not.
    rbsp: bool,
}

/// Determine how many bytes at the start of the frame should remain unencrypted.
///
/// - Audio (Opus): the TOC byte
/// - VP8: 10 bytes on a key frame, 3 on a delta frame
/// - H.264: two bytes into the first slice NAL unit, falling back to the VP8 rules when
///   there is no slice to find — which is what livekit does when its own NALU scan throws
fn framing(frame: &[u8], kind: MediaKind) -> Framing {
    let vp8 = || {
        // livekit's `UNENCRYPTED_BYTES`: 10 bytes for a key frame, 3 for a delta frame --
        // the VP8 uncompressed data chunk (RFC 6386 §9.1), which is 3 bytes of frame tag
        // plus, on a key frame, the 3-byte start code and 4 bytes of dimensions.
        //
        // Bit 0 of the frame tag is the frame type: 0 = key frame, 1 = interframe.
        if frame.first().is_some_and(|b| b & 0x01 == 0) {
            10
        } else {
            3
        }
    };
    let (wanted, rbsp) = match kind {
        // RFC 6716 §3.1: the Opus TOC byte carries the packet's own framing, so livekit
        // leaves it in the clear. It is still authenticated, as associated data.
        MediaKind::Audio => (1, false),
        MediaKind::VideoVp8 => (vp8(), false),
        MediaKind::VideoH264 => {
            h264::unencrypted_bytes(frame).map_or_else(|| (vp8(), false), |n| (n, true))
        }
    };
    Framing {
        header_size: wanted.min(frame.len()),
        rbsp,
    }
}

/// Determine how many bytes at the start of the frame should remain unencrypted.
fn unencrypted_header_size(frame: &[u8], kind: MediaKind) -> usize {
    framing(frame, kind).header_size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Test assertions are meant to panic on failure; expect()/unwrap() with a
    // descriptive message is the idiomatic way to do that in test code.
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn encrypt_decrypt_audio_roundtrip() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"test-key-material-1234567890abc");

        let original = b"opus-frame-data-here";
        let encrypted = ctx
            .encrypt_frame(
                &PlaintextMedia::from_encoder(original.to_vec()),
                MediaKind::Audio,
            )
            .expect("encryption should succeed");

        // Encrypted frame should be larger (IV + tag + key_index)
        assert!(encrypted.len() > original.len());
        // Encrypted payload should differ from original
        let prefix = encrypted
            .as_bytes()
            .get(..original.len())
            .expect("frame has prefix");
        assert_ne!(prefix, &original[..]);

        let decrypted = ctx
            .decrypt_frame(&encrypted, "alice", MediaKind::Audio)
            .expect("decryption should not error")
            .expect("decryption should produce output");

        assert_eq!(decrypted.as_bytes(), original);
    }

    /// A key is timed from installation until it first works, and then stopped.
    ///
    /// The "and then stopped" half is the one worth a test: the measurement runs on the
    /// decrypt path, and one left armed would take a mutex on every frame of every call
    /// and re-report the same interval forever.
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn a_key_is_timed_until_its_first_success_and_no_longer() {
        use std::sync::atomic::Ordering;

        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"test-key-material-1234567890abc");
        assert_eq!(
            ctx.awaiting_first_use.load(Ordering::Relaxed),
            1,
            "a freshly installed key has not decrypted anything yet"
        );

        let frame = ctx
            .encrypt_frame(
                &PlaintextMedia::from_encoder(b"opus".to_vec()),
                MediaKind::Audio,
            )
            .expect("encryption should succeed");
        ctx.decrypt_frame(&frame, "alice", MediaKind::Audio)
            .expect("decryption should not error")
            .expect("decryption should produce output");
        assert_eq!(
            ctx.awaiting_first_use.load(Ordering::Relaxed),
            0,
            "the key has now been seen to work; nothing is left to measure"
        );

        ctx.decrypt_frame(&frame, "alice", MediaKind::Audio)
            .expect("decryption should not error")
            .expect("decryption should produce output");
        assert_eq!(
            ctx.awaiting_first_use.load(Ordering::Relaxed),
            0,
            "later frames must not re-arm the measurement"
        );
    }

    /// Re-installing a key restarts its clock, because the interval that matters is from
    /// the material actually in use -- not from an earlier copy that never worked.
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn reinstalling_a_key_does_not_double_count_it() {
        use std::sync::atomic::Ordering;

        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_key("bob", 3, b"test-key-material-1234567890abc");
        ctx.set_key("bob", 3, b"test-key-material-0987654321zyx");
        assert_eq!(
            ctx.awaiting_first_use.load(Ordering::Relaxed),
            1,
            "one slot, one pending measurement, however many times it is written"
        );
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn encrypt_decrypt_video_roundtrip() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"video-key-material-abcdefghijkl");

        // Simulated VP8 frame: first byte is payload descriptor
        let mut original = vec![0x90, 0x80, 0x42]; // VP8 header
        original.extend_from_slice(b"video-payload-data-here");

        let encrypted = ctx
            .encrypt_frame(
                &PlaintextMedia::from_encoder(original.clone()),
                MediaKind::VideoVp8,
            )
            .expect("encryption should succeed");

        // First few bytes (VP8 header) should be unencrypted
        assert_eq!(
            encrypted.as_bytes().get(..3).expect("frame has header"),
            original.get(..3).expect("original has header")
        );

        let decrypted = ctx
            .decrypt_frame(&encrypted, "alice", MediaKind::VideoVp8)
            .expect("decryption should not error")
            .expect("decryption should produce output");

        assert_eq!(decrypted.as_bytes(), original.as_slice());
    }

    /// An Annex B H.264 frame: SPS, PPS, then an IDR slice with a payload.
    fn h264_keyframe() -> Vec<u8> {
        let mut f = vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e];
        f.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
        f.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        f.extend_from_slice(b"slice-payload-that-is-long-enough-to-be-worth-encrypting");
        f
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn encrypt_decrypt_h264_roundtrip() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"video-key-material-abcdefghijkl");

        let original = h264_keyframe();
        let encrypted = ctx
            .encrypt_frame(
                &PlaintextMedia::from_encoder(original.clone()),
                MediaKind::VideoH264,
            )
            .expect("encryption should succeed");

        // The parameter sets and the first two bytes of the slice travel in the clear.
        let clear = h264::unencrypted_bytes(&original).expect("the frame has a slice");
        assert_eq!(
            encrypted.as_bytes().get(..clear),
            original.get(..clear),
            "the clear header must survive unchanged"
        );

        let decrypted = ctx
            .decrypt_frame(&encrypted, "alice", MediaKind::VideoH264)
            .expect("decryption should not error")
            .expect("decryption should produce output");
        assert_eq!(decrypted.as_bytes(), original.as_slice());
    }

    /// The reason the escaping exists, asserted rather than assumed.
    ///
    /// Ciphertext is random, so a start-code sequence turns up in it by chance. Anything
    /// walking the stream would read that as the beginning of a new NAL unit.
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn an_encrypted_h264_frame_contains_no_accidental_start_codes() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"video-key-material-abcdefghijkl");

        let original = h264_keyframe();
        let clear = h264::unencrypted_bytes(&original).expect("the frame has a slice");

        // One frame is not enough: the sequence appears by chance, so this needs enough
        // frames -- each with a different IV, hence different ciphertext -- to be evidence.
        for _ in 0..500 {
            let encrypted = ctx
                .encrypt_frame(
                    &PlaintextMedia::from_encoder(original.clone()),
                    MediaKind::VideoH264,
                )
                .expect("encryption should succeed");
            let body = encrypted
                .as_bytes()
                .get(clear..)
                .expect("frame is longer than its header");
            assert!(
                !body.windows(3).any(|w| w == [0, 0, 0] || w == [0, 0, 1]),
                "a start code survived into the encrypted body"
            );
        }
    }

    /// A frame encrypted as VP8 and read back as VP8 is unaffected by any of this.
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn h264_framing_does_not_disturb_vp8() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"video-key-material-abcdefghijkl");

        // A VP8 delta frame whose payload happens to contain a start-code sequence.
        let mut original = vec![0x91, 0x80, 0x42];
        original.extend_from_slice(&[0xaa, 0, 0, 1, 0xbb, 0, 0, 3, 0xcc]);

        let encrypted = ctx
            .encrypt_frame(
                &PlaintextMedia::from_encoder(original.clone()),
                MediaKind::VideoVp8,
            )
            .expect("encryption should succeed");
        let decrypted = ctx
            .decrypt_frame(&encrypted, "alice", MediaKind::VideoVp8)
            .expect("decryption should not error")
            .expect("decryption should produce output");
        assert_eq!(decrypted.as_bytes(), original.as_slice());
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn decrypt_wrong_participant_returns_none() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"test-key-material-1234567890abc");

        let original = b"opus-frame";
        let encrypted = ctx
            .encrypt_frame(
                &PlaintextMedia::from_encoder(original.to_vec()),
                MediaKind::Audio,
            )
            .expect("encryption should succeed");

        // Bob has no key set
        let result = ctx
            .decrypt_frame(&encrypted, "bob", MediaKind::Audio)
            .expect("should not error");
        assert!(result.is_none());
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn decrypt_wrong_key_fails() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"test-key-material-1234567890abc");

        let original = b"secret-audio-data";
        let encrypted = ctx
            .encrypt_frame(
                &PlaintextMedia::from_encoder(original.to_vec()),
                MediaKind::Audio,
            )
            .expect("encryption should succeed");

        // Set a different key for alice
        ctx.set_key("alice", 0, b"wrong-key-material-xxxxxxxxx123");

        let result = ctx.decrypt_frame(&encrypted, "alice", MediaKind::Audio);
        assert!(result.is_err());
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn multiple_key_indices() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");

        // Set key at index 0, encrypt a frame
        ctx.set_key("alice", 0, b"key-zero-material-abcdefghijklm");
        let encrypted_0 = ctx
            .encrypt_frame(
                &PlaintextMedia::from_encoder(b"frame-0".to_vec()),
                MediaKind::Audio,
            )
            .expect("encrypt should succeed");

        // Set key at index 1 (now current), encrypt another frame
        ctx.set_key("alice", 1, b"key-one-material-nopqrstuvwxyz1");
        let encrypted_1 = ctx
            .encrypt_frame(
                &PlaintextMedia::from_encoder(b"frame-1".to_vec()),
                MediaKind::Audio,
            )
            .expect("encrypt should succeed");

        // Both should decrypt successfully (both keys still in ring)
        let dec_0 = ctx
            .decrypt_frame(&encrypted_0, "alice", MediaKind::Audio)
            .expect("should not error")
            .expect("should decrypt");
        assert_eq!(dec_0.as_bytes(), b"frame-0");

        let dec_1 = ctx
            .decrypt_frame(&encrypted_1, "alice", MediaKind::Audio)
            .expect("should not error")
            .expect("should decrypt");
        assert_eq!(dec_1.as_bytes(), b"frame-1");
    }

    #[test]
    fn frame_too_short() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_key("alice", 0, b"some-key");

        let result = ctx.decrypt_frame(
            &WireMedia::from_network(vec![0; 5]),
            "alice",
            MediaKind::Audio,
        );
        assert!(matches!(result, Err(E2eeError::FrameTooShort(_))));
    }

    #[test]
    fn no_key_returns_none_on_encrypt() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        // No local identity set
        let result = ctx.encrypt_frame(
            &PlaintextMedia::from_encoder(b"data".to_vec()),
            MediaKind::Audio,
        );
        assert!(result.is_none());
    }

    #[test]
    fn options_can_be_updated_without_losing_keys() {
        // The ordering that caused this: a key arrives before livekit's `init`, the bridge
        // synthesises one with no options, and the real options turn up afterwards. If
        // that rebuilt the context the keys would be gone, and ratcheting would stay off
        // until the sender happened to send fresh ones.
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, &[3_u8; 16]);
        assert!(ctx.has_encryption_key(), "the key must be installed");

        ctx.set_options(E2eeOptions {
            ratchet_window_size: 10,
            ratchet_salt: None,
            auto_ratchet: true,
        });

        assert!(
            ctx.has_encryption_key(),
            "updating the options must not discard the keyring"
        );
        let frame = PlaintextMedia::from_encoder(vec![0x10, 0x20, 0x30, 0x40]);
        assert!(
            ctx.encrypt_frame(&frame, MediaKind::Audio).is_some(),
            "and the key must still encrypt"
        );
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
        let ratcheted = {
            let Ok(mut inner) = sender.inner.write() else {
                panic!("test lock is not poisoned");
            };
            inner
                .key_manager
                .participants
                .get_mut(&ParticipantId::new("alice"))
                .map(|ring| {
                    let salt = HkdfSalt(DEFAULT_RATCHET_SALT);
                    let current = ring
                        .raw_material(KeyIndex::new(0))
                        .expect("key was just set")
                        .to_vec();
                    let next = ratchet_key(KeyMaterial(&current), salt);
                    ring.replace_key(KeyIndex::new(0), KeyMaterial(&next), salt);
                })
        };
        assert_eq!(ratcheted, Some(()));

        let encrypted = sender
            .encrypt_frame(
                &PlaintextMedia::from_encoder(b"test-data".to_vec()),
                MediaKind::Audio,
            )
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
        assert_eq!(decrypted.as_bytes(), b"test-data");
    }

    #[test]
    fn unencrypted_header_audio_is_the_opus_toc_byte() {
        // livekit's `UNENCRYPTED_BYTES.audio`: the Opus TOC byte stays in the clear.
        assert_eq!(unencrypted_header_size(b"anything", MediaKind::Audio), 1);
    }

    #[test]
    fn unencrypted_header_never_exceeds_the_frame() {
        // A frame shorter than the nominal header must clamp rather than slice out of
        // bounds -- inbound frame lengths are attacker-controlled.
        assert_eq!(unencrypted_header_size(b"", MediaKind::Audio), 0);
        assert_eq!(
            unencrypted_header_size(&[0x10, 0x00], MediaKind::VideoVp8),
            2
        );
    }

    #[test]
    fn unencrypted_header_video_distinguishes_key_and_delta_frames() {
        // VP8 frame tag bit 0: 0 = key frame (10 clear bytes), 1 = interframe (3).
        let long = [0u8; 32];
        let mut keyframe = long;
        if let Some(b) = keyframe.first_mut() {
            *b = 0x10;
        }
        assert_eq!(unencrypted_header_size(&keyframe, MediaKind::VideoVp8), 10);

        let mut delta = long;
        if let Some(b) = delta.first_mut() {
            *b = 0x11;
        }
        assert_eq!(unencrypted_header_size(&delta, MediaKind::VideoVp8), 3);
    }

    /// A livekit frame's trailer says how long its IV is, and livekit always says 12.
    ///
    /// This is the cheapest way to tell "we were never given the key" from "we are reading
    /// the wrong bytes", which otherwise look identical: both drop every frame.
    /// The SFU's own frames are not encrypted and must pass through.
    ///
    /// It holds no key, so it cannot encrypt: the silence or black picture it injects
    /// while a publisher is muted arrives in the clear with a marker appended. Feeding one
    /// to AES-GCM cannot succeed, and the failure looks exactly like a missing key --
    /// arriving, inconveniently, the moment someone mutes.
    #[test]
    #[allow(clippy::expect_used)]
    fn server_injected_frames_pass_through_undecrypted() {
        const SIF: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"test-key-material-1234567890abc");
        ctx.set_sif_trailer(SIF.to_vec()).expect("trailer recorded");

        let mut injected = b"the SFU's own silence".to_vec();
        injected.extend_from_slice(SIF);
        let out = ctx
            .decrypt_frame_any(&WireMedia::from_network(injected.clone()), MediaKind::Audio)
            .expect("a server-injected frame is not an error")
            .expect("it passes through");
        assert_eq!(
            out.as_bytes(),
            injected.as_slice(),
            "passed through untouched"
        );

        // A frame that merely fails to decrypt is still a failure: the marker is what
        // distinguishes them, not the fact that decryption did not work.
        let ordinary = WireMedia::from_network(vec![0x11; 40]);
        assert!(ctx.decrypt_frame_any(&ordinary, MediaKind::Audio).is_err());
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn decrypt_frame_any_never_tries_the_local_identity() {
        // A frame encrypted with our own key is not a frame anyone should have sent us.
        // `decrypt_frame_any` iterates every participant with a key ring; before this
        // fix that included `local_identity`, so a frame we ourselves encrypted would
        // "decrypt" trivially against our own key and be handed back as if a peer sent
        // it. With the local identity excluded, only `bob`'s (different) key is tried,
        // and it does not match, so the frame is correctly reported as undecryptable.
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 0, b"alices-own-key-material-1234567");
        ctx.set_key("bob", 0, b"bobs-completely-different-key12");

        let own_frame = ctx
            .encrypt_frame(
                &PlaintextMedia::from_encoder(b"outbound-audio".to_vec()),
                MediaKind::Audio,
            )
            .expect("alice's own key encrypts outbound frames");

        let result = ctx.decrypt_frame_any(&own_frame, MediaKind::Audio);
        assert!(
            result.is_err(),
            "a frame encrypted under our own key must never be accepted as inbound"
        );
    }

    #[test]
    fn a_trailer_is_recognised_by_its_iv_length() {
        // A tail shaped the way livekit writes one: [..][IV_LENGTH=12][key_index].
        let mut good = vec![0x11_u8; 40];
        good.push(IV_SIZE_U8);
        good.push(3);
        assert_eq!(trailer_looks_like_livekit(&good), Some(true));

        // A payload whose tail is something else entirely -- RED framing, a VP8
        // descriptor, a partial frame. The last byte still reads as a plausible key index,
        // which is exactly why the byte before it has to be checked.
        let not_a_frame = vec![0x11_u8; 40];
        assert_eq!(trailer_looks_like_livekit(&not_a_frame), Some(false));

        // Too short to have a trailer at all.
        assert_eq!(trailer_looks_like_livekit(&[0x01]), None);
        assert_eq!(trailer_looks_like_livekit(&[]), None);
    }

    #[test]
    fn every_index_has_its_own_slot() {
        // The ring holds 256 keys because the trailer carries one byte, so the byte is the
        // slot and nothing is reduced. This used to reduce modulo 16, which aliased: with
        // Element Call's `keyringSize: 256` a peer can legitimately be at index 19, and
        // that overwrote the key at index 3 while it was still in use -- one participant
        // silencing another, intermittently and only on long calls.
        assert_ne!(KeyIndex::new(16), KeyIndex::new(0));
        assert_ne!(KeyIndex::new(19), KeyIndex::new(3));
        assert_ne!(KeyIndex::new(255), KeyIndex::new(15));

        // And the byte survives the round trip, which is what the far end reads back.
        for raw in [0_u8, 1, 15, 16, 19, 200, 255] {
            assert_eq!(KeyIndex::new(raw).as_wire_byte(), raw);
        }
    }
}

#[cfg(test)]
mod hkdf_interop_tests {
    use super::*;

    /// Pins our HKDF parameters against livekit-client's, using vectors computed
    /// independently (a plain HMAC-SHA256 HKDF implementation, outside this crate).
    ///
    /// This is the check that would have caught a real interop break: we derived with
    /// `salt = None` and `info = b"LKFrameEncryptionKey"`, whereas livekit-client's
    /// `getAlgoOptions` uses `salt = ratchetSalt` (`"LKFrameEncryptionKey"`) and
    /// `info = new ArrayBuffer(128)` (128 zero bytes). Swapping them still yields a
    /// valid-looking AES-128 key, so nothing errors -- every remote frame simply fails
    /// authentication and is dropped, which is silent and near-impossible to attribute.
    /// Round-tripping against ourselves cannot catch it; only a fixed vector can.
    #[test]
    #[allow(clippy::expect_used)]
    fn derived_key_matches_livekit_hkdf_parameters() {
        let material: Vec<u8> = (0..32u8).collect();
        let opts = E2eeOptions::default();
        let salt = opts.salt();
        assert_eq!(salt.0, b"LKFrameEncryptionKey");

        // HKDF-SHA256(salt = "LKFrameEncryptionKey", ikm = 0x00..0x1f, info = [0u8; 128])
        // truncated to the AES-128 key length.
        let expected_aes_key = hex_to_bytes("6d0dd7b3d6f4efde47ab9a9e15abde76");

        let hk = Hkdf::<Sha256>::new(Some(salt.0), &material);
        let mut okm = [0u8; 16];
        hk.expand(&HKDF_INFO, &mut okm).expect("expand");
        assert_eq!(
            okm.as_slice(),
            expected_aes_key.as_slice(),
            "AES key derivation must match livekit-client exactly, or no peer can decrypt us"
        );
    }

    /// The ratchet step must also match: livekit's `ratchet()` derives 256 bits with the
    /// same algorithm options.
    #[test]
    fn ratchet_matches_livekit_derive_bits() {
        let material: Vec<u8> = (0..32u8).collect();
        let opts = E2eeOptions::default();
        let salt = opts.salt();
        let expected =
            hex_to_bytes("6d0dd7b3d6f4efde47ab9a9e15abde7635c45a13bddf5da15eed01f40178d328");
        let got = ratchet_key(KeyMaterial(&material), salt);
        assert_eq!(got.len(), 32, "livekit ratchets to 256 bits");
        assert_eq!(
            got, expected,
            "ratchet output must match livekit-client exactly"
        );
    }

    /// A configured salt must actually be used, not silently ignored (it was: the salt
    /// option existed but never reached the derivation).
    #[test]
    fn configured_salt_changes_the_derived_key() {
        let material: Vec<u8> = (0..32u8).collect();
        let custom = E2eeOptions {
            ratchet_salt: Some("a-different-salt".to_string()),
            ..E2eeOptions::default()
        };
        let defaults = E2eeOptions::default();
        assert_ne!(
            ratchet_key(KeyMaterial(&material), custom.salt()),
            ratchet_key(KeyMaterial(&material), defaults.salt()),
        );
    }

    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .filter_map(|i| hex.get(i..i.saturating_add(2)))
            .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
            .collect()
    }
}

/// Interop tests for the *frame layout*, built independently of this crate's encryptor.
///
/// A round-trip test (encrypt with our code, decrypt with our code) cannot catch a layout
/// mistake -- it agrees with itself. These tests instead assemble a frame byte-for-byte
/// the way livekit-client's `e2ee.worker` does and require our decryptor to read it, which
/// is what a browser or mobile Element Call peer actually sends.
///
/// The layout is transcribed from `FrameCryptor.encodeFunction` in
/// `livekit-client/dist/livekit-client.e2ee.worker.mjs`:
///
/// ```text
/// [header][ciphertext][IV (12)][IV_LENGTH][key_index]
/// ```
///
/// with `additionalData` set to the header bytes.
#[cfg(test)]
mod wire_format_interop_tests {
    use super::*;

    const MATERIAL: &[u8] = b"interop-key-material-0123456789";

    /// Build a frame exactly as livekit-client would, without using [`E2eeContext`].
    #[allow(clippy::expect_used)]
    fn livekit_style_frame(plaintext: &[u8], header_len: usize, key_index: u8) -> Vec<u8> {
        let cipher = derive_cipher(KeyMaterial(MATERIAL), HkdfSalt(DEFAULT_RATCHET_SALT));
        let iv = [7u8; IV_SIZE];
        let header = plaintext
            .get(..header_len)
            .expect("header within plaintext");
        let body = plaintext.get(header_len..).expect("body within plaintext");

        let ciphertext = cipher
            .encrypt(
                &Nonce::from(iv),
                aes_gcm::aead::Payload {
                    msg: body,
                    aad: header,
                },
            )
            .expect("encryption succeeds");

        let mut frame = Vec::new();
        frame.extend_from_slice(header);
        frame.extend_from_slice(&ciphertext);
        frame.extend_from_slice(&iv);
        frame.push(IV_SIZE_U8); // frameTrailer[0]
        frame.push(key_index); // frameTrailer[1]
        frame
    }

    /// The decisive test: a frame built to livekit's layout must decrypt.
    ///
    /// This failed before the trailer was corrected from one byte to two -- reading only
    /// the final byte still yields the right key index, so the bug looked harmless while
    /// shifting the IV window by one byte and prefixing the ciphertext with a stray byte.
    #[test]
    #[allow(clippy::expect_used)]
    fn decrypts_an_audio_frame_laid_out_the_way_livekit_lays_it_out() {
        // Byte 0 is the Opus TOC byte, which livekit leaves unencrypted.
        let plaintext = b"\x78opus-payload-bytes".to_vec();
        let frame = livekit_style_frame(&plaintext, 1, 3);

        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_key("bob", 3, MATERIAL);

        let decrypted = ctx
            .decrypt_frame(&WireMedia::from_network(frame), "bob", MediaKind::Audio)
            .expect("decryption should not error")
            .expect("a key is configured");
        assert_eq!(decrypted.as_bytes(), plaintext.as_slice());
    }

    /// A sender's key index is a rotation counter carried whole, and every value it can
    /// take names its own key.
    ///
    /// Regression, twice over. This first rejected trailer bytes at or above the ring
    /// size, which made every peer past its 16th rotation permanently inaudible while
    /// their packets arrived normally and no key was missing. The fix reduced modulo 16
    /// instead, which aliased index 19 onto index 3 -- so a peer rotating past 16
    /// overwrote a key another participant was still using. Element Call configures
    /// `keyringSize: 256`, so both are reachable in a normal call.
    #[test]
    #[allow(clippy::expect_used)]
    fn a_sender_past_the_sixteenth_rotation_decrypts_without_disturbing_others() {
        let plaintext = b"\x78opus-payload-bytes".to_vec();
        // Ratcheting off: it would eventually find a key by searching, and this is about
        // the right key being in its own slot rather than being hunted for.
        let options = E2eeOptions {
            ratchet_window_size: 0,
            ..E2eeOptions::default()
        };
        let ctx = E2eeContext::new(options);

        // A live key, then a rotation past the old ring size. Under modulo 16 the second
        // landed in the first's slot and destroyed it.
        ctx.set_key("bob", 3, MATERIAL);
        ctx.set_key("bob", 19, b"a-different-key-000000000000000");

        let at_3 = livekit_style_frame(&plaintext, 1, 3);
        let decrypted = ctx
            .decrypt_frame(&WireMedia::from_network(at_3), "bob", MediaKind::Audio)
            .expect("a key set at 19 must not disturb the one at 3")
            .expect("a key is still configured at 3");
        assert_eq!(decrypted.as_bytes(), plaintext.as_slice());
    }

    /// Video keyframes leave 10 bytes in the clear, delta frames 3.
    #[test]
    #[allow(clippy::expect_used)]
    fn decrypts_video_frames_with_livekits_header_sizes() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_key("bob", 0, MATERIAL);

        // Frame tag bit 0 == 0 marks a key frame -> 10 unencrypted bytes.
        let mut keyframe = vec![0x10u8];
        keyframe.extend_from_slice(b"0123456789abcdef");
        assert_eq!(unencrypted_header_size(&keyframe, MediaKind::VideoVp8), 10);
        let frame = livekit_style_frame(&keyframe, 10, 0);
        let decrypted = ctx
            .decrypt_frame(&WireMedia::from_network(frame), "bob", MediaKind::VideoVp8)
            .expect("decryption should not error")
            .expect("a key is configured");
        assert_eq!(decrypted.as_bytes(), keyframe.as_slice());

        // Frame tag bit 0 == 1 marks an interframe -> 3 unencrypted bytes.
        let mut delta = vec![0x11u8];
        delta.extend_from_slice(b"0123456789abcdef");
        assert_eq!(unencrypted_header_size(&delta, MediaKind::VideoVp8), 3);
        let frame = livekit_style_frame(&delta, 3, 0);
        let decrypted = ctx
            .decrypt_frame(&WireMedia::from_network(frame), "bob", MediaKind::VideoVp8)
            .expect("decryption should not error")
            .expect("a key is configured");
        assert_eq!(decrypted.as_bytes(), delta.as_slice());
    }

    /// Our encryptor must emit the same layout it can read, byte positions included.
    #[test]
    #[allow(clippy::expect_used)]
    fn our_encryptor_emits_livekits_two_byte_trailer() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_local_identity("alice");
        ctx.set_key("alice", 5, MATERIAL);

        let plaintext = b"\x78opus-payload-bytes".to_vec();
        let encrypted = ctx
            .encrypt_frame(
                &PlaintextMedia::from_encoder(plaintext.clone()),
                MediaKind::Audio,
            )
            .expect("encryption succeeds");
        let bytes = encrypted.as_bytes();

        let trailer_start = bytes.len().saturating_sub(TRAILER_SIZE);
        assert_eq!(
            bytes.get(trailer_start),
            Some(&IV_SIZE_U8),
            "trailer[0] is IV_LENGTH"
        );
        assert_eq!(
            bytes.get(trailer_start + 1),
            Some(&5u8),
            "trailer[1] is the key index"
        );

        // Header is in the clear, exactly one Opus TOC byte for audio.
        assert_eq!(bytes.first(), plaintext.first());

        // Total length: header + payload + GCM tag + IV + trailer.
        assert_eq!(
            bytes.len(),
            plaintext.len() + TAG_SIZE + IV_SIZE + TRAILER_SIZE
        );
    }

    /// The `IV_LENGTH` byte is attacker-controlled, so anything but 12 is rejected rather
    /// than used as a length.
    #[test]
    fn rejects_a_frame_claiming_an_unsupported_iv_length() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_key("bob", 0, MATERIAL);

        let mut frame = livekit_style_frame(b"\x78payload", 1, 0);
        let trailer_start = frame.len().saturating_sub(TRAILER_SIZE);
        if let Some(b) = frame.get_mut(trailer_start) {
            *b = 16;
        }
        let result = ctx.decrypt_frame(&WireMedia::from_network(frame), "bob", MediaKind::Audio);
        assert!(matches!(result, Err(E2eeError::UnsupportedIvLength(16))));
    }

    /// A failed ratchet search must leave the participant's key intact.
    ///
    /// Previously the search mutated the stored key on every step, so one undecryptable
    /// frame burned the whole ratchet window and destroyed the real key permanently --
    /// after which even a perfectly good frame could never be decrypted.
    #[test]
    #[allow(clippy::expect_used)]
    fn a_failed_ratchet_search_does_not_destroy_the_key() {
        let ctx = E2eeContext::new(E2eeOptions {
            ratchet_window_size: 10,
            ..Default::default()
        });
        ctx.set_key("bob", 0, MATERIAL);

        // Garbage that no key in the window can authenticate.
        let mut junk = livekit_style_frame(b"\x78payload", 1, 0);
        if let Some(b) = junk.get_mut(3) {
            *b ^= 0xff;
        }
        assert!(
            ctx.decrypt_frame(&WireMedia::from_network(junk), "bob", MediaKind::Audio)
                .is_err()
        );

        // A genuine frame under the original key must still decrypt.
        let plaintext = b"\x78opus-payload-bytes".to_vec();
        let good = livekit_style_frame(&plaintext, 1, 0);
        let decrypted = ctx
            .decrypt_frame(&WireMedia::from_network(good), "bob", MediaKind::Audio)
            .expect("decryption should not error")
            .expect("the key must survive a failed ratchet search");
        assert_eq!(decrypted.as_bytes(), plaintext.as_slice());
    }
}

/// Tests for the diagnostics that make a decrypt failure interpretable.
///
/// These matter because the failure itself is silent to the user -- lost media sounds like
/// a quiet participant, not an error -- so the log is the only way the cause is ever found.
#[cfg(test)]
mod key_diagnostics_tests {
    use super::*;

    #[test]
    fn fingerprints_distinguish_keys_without_revealing_them() {
        use std::fmt::Write as _;
        let a = b"key-material-aaaaaaaaaaaaaaaaaaa";
        let b = b"key-material-bbbbbbbbbbbbbbbbbbb";
        let fp_a = fingerprint_hex(key_fingerprint(KeyMaterial(a)));
        let fp_b = fingerprint_hex(key_fingerprint(KeyMaterial(b)));

        assert_ne!(fp_a, fp_b, "different keys must be distinguishable");
        assert_eq!(
            fp_a,
            fingerprint_hex(key_fingerprint(KeyMaterial(a))),
            "stable"
        );
        assert_eq!(fp_a.len(), 8, "4 bytes rendered as hex");

        // The fingerprint must not be, or contain, the key.
        let material_hex = a.iter().fold(String::new(), |mut acc, byte| {
            let _ = write!(acc, "{byte:02x}");
            acc
        });
        assert!(
            !material_hex.contains(&fp_a),
            "fingerprint must not leak key bytes"
        );
    }

    /// The inventory is what tells "their key never arrived" apart from "we have the
    /// wrong key at that index" -- the two causes need opposite fixes.
    #[test]
    fn key_inventory_reports_every_held_key_by_participant_and_index() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_key("alice", 3, b"alice-key-material-00000000000");
        ctx.set_key("bob", 4, b"bob-key-material-000000000000");
        ctx.set_key("bob", 11, b"bob-second-key-material-00000");

        let inventory = ctx.key_inventory();
        assert!(inventory.contains("alice@3:"), "got {inventory}");
        assert!(inventory.contains("bob@4:"), "got {inventory}");
        assert!(inventory.contains("bob@11:"), "got {inventory}");
        // An index nobody holds must be absent, which is exactly the diagnostic signal.
        assert!(!inventory.contains("@5:"), "got {inventory}");
    }

    /// Replacing a participant's key must change its fingerprint, so a stale key is
    /// visible in the log rather than looking identical to the correct one.
    #[test]
    fn rotating_a_key_changes_its_fingerprint() {
        let ctx = E2eeContext::new(E2eeOptions::default());
        ctx.set_key("alice", 0, b"first-key-material-0000000000");
        let before = ctx.key_inventory();
        ctx.set_key("alice", 0, b"second-key-material-000000000");
        let after = ctx.key_inventory();

        assert_ne!(before, after, "a rotated key must be visibly different");
    }

    /// The frame's own key index is read for diagnostics even when nothing can decrypt it.
    #[test]
    fn peek_key_index_reads_the_senders_index_from_the_trailer() {
        let mut frame = vec![0u8; 40];
        let trailer_start = frame.len().saturating_sub(TRAILER_SIZE);
        if let Some(b) = frame.get_mut(trailer_start) {
            *b = IV_SIZE_U8;
        }
        if let Some(b) = frame.last_mut() {
            *b = 5;
        }
        assert_eq!(peek_key_index(&frame), Some(5));
        assert_eq!(
            peek_key_index(&[]),
            None,
            "a runt frame has no index to report"
        );
    }
}

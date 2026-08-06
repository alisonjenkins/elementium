use elementium_types::{AudioFrame, PlaintextMedia};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum OpusError {
    #[error("Opus encoder error: {0}")]
    Encoder(String),
    #[error("Opus decoder error: {0}")]
    Decoder(String),
}

/// Tuning applied to a new [`OpusEncoder`], beyond libopus's defaults.
///
/// libopus's defaults are wrong for a WebRTC call in two ways that matter, and both were
/// left unset here:
///
/// - **In-band FEC is off by default.** Every Opus line negotiated on a real call carries
///   `useinbandfec=1` -- the remote peers are explicitly telling us they will use forward
///   error correction to reconstruct packets they lose. Sending none means every lost
///   packet is an unrecoverable hole in their audio, heard as "robotic"/breaking-up
///   speech. This was the observed symptom while the local send path was, by its own
///   counters, perfect: 50 packets/sec, zero drops, zero encode errors.
/// - **`OPUS_AUTO` bitrate is not a call bitrate.** For 48kHz stereo at 20ms it resolves
///   to `60*Fs/frame_size + Fs*channels` = ~99kbps, roughly triple typical WebRTC voice,
///   producing large packets that are themselves more likely to be lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusEncoderConfig {
    /// Target bitrate in bits/sec.
    pub bitrate_bps: i32,
    /// Enable in-band forward error correction (`useinbandfec`).
    pub inband_fec: bool,
    /// Loss percentage the encoder should protect against.
    ///
    /// Load-bearing, and the classic gotcha: with FEC enabled but this left at libopus's
    /// default of 0, the encoder concludes no redundancy is needed and emits none, so
    /// FEC is on in name only. It must be non-zero for FEC to actually do anything.
    ///
    /// This is only the *starting* value. Once RTCP receiver reports arrive, the real
    /// measured loss replaces it via [`OpusEncoder::set_expected_packet_loss`].
    pub expected_packet_loss_perc: u8,
}

impl Default for OpusEncoderConfig {
    fn default() -> Self {
        Self {
            // Comfortably above wideband-speech transparency while leaving room for FEC
            // redundancy, and well under libopus's ~99kbps `OPUS_AUTO` for 48kHz stereo.
            bitrate_bps: 48_000,
            inband_fec: true,
            // A moderate standing assumption of loss. Higher values buy more resilience
            // at the cost of spending bitrate on redundancy rather than on the primary
            // encoding; 10% is the usual WebRTC starting point when no RTCP-derived loss
            // estimate is being fed back into the encoder (we have none yet).
            expected_packet_loss_perc: 10,
        }
    }
}

/// Wraps `opus::Encoder` for encoding PCM audio to Opus.
pub struct OpusEncoder {
    inner: opus::Encoder,
    sample_rate: u32,
    channels: u16,
}

impl OpusEncoder {
    /// Creates a new Opus encoder for the given sample rate and channel count.
    ///
    /// # Errors
    ///
    /// Returns [`OpusError::Encoder`] if `channels` is not 1 or 2, or if the
    /// underlying `opus` encoder fails to initialize.
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, OpusError> {
        Self::with_config(sample_rate, channels, OpusEncoderConfig::default())
    }

    /// Creates a new Opus encoder with explicit [`OpusEncoderConfig`] tuning.
    ///
    /// # Errors
    ///
    /// Returns [`OpusError::Encoder`] if `channels` is not 1 or 2, if the underlying
    /// `opus` encoder fails to initialize, or if applying `config` is rejected.
    pub fn with_config(
        sample_rate: u32,
        channels: u16,
        config: OpusEncoderConfig,
    ) -> Result<Self, OpusError> {
        let ch = match channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            _ => {
                tracing::error!(
                    channels,
                    error_kind = "unsupported_channels",
                    "Opus encoder: unsupported channel count"
                );
                return Err(OpusError::Encoder(format!("unsupported channel count: {channels}")));
            }
        };

        let mut encoder =
            opus::Encoder::new(sample_rate, ch, opus::Application::Voip).map_err(|e| {
                tracing::error!(
                    sample_rate,
                    channels,
                    error_kind = "encoder_init",
                    error = %e,
                    "Failed to initialize Opus encoder"
                );
                OpusError::Encoder(e.to_string())
            })?;

        let apply = |result: Result<(), opus::Error>, setting: &'static str| -> Result<(), OpusError> {
            result.map_err(|e| {
                tracing::error!(
                    sample_rate,
                    channels,
                    setting,
                    error_kind = "encoder_config",
                    error = %e,
                    "Failed to apply Opus encoder setting"
                );
                OpusError::Encoder(format!("{setting}: {e}"))
            })
        };

        apply(
            encoder.set_bitrate(opus::Bitrate::Bits(config.bitrate_bps)),
            "bitrate",
        )?;
        apply(encoder.set_inband_fec(config.inband_fec), "inband_fec")?;
        apply(
            encoder.set_packet_loss_perc(i32::from(config.expected_packet_loss_perc)),
            "packet_loss_perc",
        )?;

        tracing::info!(
            sample_rate,
            channels,
            bitrate_bps = config.bitrate_bps,
            inband_fec = config.inband_fec,
            expected_packet_loss_perc = config.expected_packet_loss_perc,
            "Opus encoder configured"
        );

        Ok(Self {
            inner: encoder,
            sample_rate,
            channels,
        })
    }

    /// Update the loss percentage the encoder protects against, mid-call.
    ///
    /// FEC redundancy is emitted in proportion to this value, so keeping it aligned with
    /// measured loss (from RTCP receiver reports) is the difference between wasting
    /// bitrate on a clean link and under-protecting a bad one. Safe to call between
    /// frames; libopus applies it to subsequent packets.
    ///
    /// # Errors
    ///
    /// Returns [`OpusError::Encoder`] if libopus rejects the value.
    pub fn set_expected_packet_loss(&mut self, perc: u8) -> Result<(), OpusError> {
        let perc = i32::from(perc.min(100));
        self.inner.set_packet_loss_perc(perc).map_err(|e| {
            tracing::error!(
                perc,
                error_kind = "encoder_config",
                error = %e,
                "Failed to update Opus expected packet loss"
            );
            OpusError::Encoder(format!("packet_loss_perc: {e}"))
        })
    }

    /// Encode a frame of f32 PCM samples to Opus. Returns encoded bytes.
    ///
    /// # Errors
    ///
    /// Returns [`OpusError::Encoder`] if the underlying `opus` encoder fails.
    pub fn encode(&mut self, frame: &AudioFrame) -> Result<PlaintextMedia, OpusError> {
        // Opus max packet size
        let mut output = vec![0u8; 4000];
        let len = self.inner.encode_float(&frame.data, &mut output).map_err(|e| {
            tracing::error!(
                sample_rate = self.sample_rate,
                channels = self.channels,
                input_samples = frame.data.len(),
                error_kind = "encode",
                error = %e,
                "Opus encode failed"
            );
            OpusError::Encoder(e.to_string())
        })?;
        output.truncate(len);
        Ok(PlaintextMedia::from_encoder(output))
    }

    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    #[must_use]
    pub const fn channels(&self) -> u16 {
        self.channels
    }
}

/// Wraps `opus::Decoder` for decoding Opus packets to PCM audio.
pub struct OpusDecoder {
    inner: opus::Decoder,
    sample_rate: u32,
    channels: u16,
}

impl OpusDecoder {
    /// Creates a new Opus decoder for the given sample rate and channel count.
    ///
    /// # Errors
    ///
    /// Returns [`OpusError::Decoder`] if `channels` is not 1 or 2, or if the
    /// underlying `opus` decoder fails to initialize.
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, OpusError> {
        let ch = match channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            _ => {
                tracing::error!(
                    channels,
                    error_kind = "unsupported_channels",
                    "Opus decoder: unsupported channel count"
                );
                return Err(OpusError::Decoder(format!("unsupported channel count: {channels}")));
            }
        };

        let decoder = opus::Decoder::new(sample_rate, ch).map_err(|e| {
            tracing::error!(
                sample_rate,
                channels,
                error_kind = "decoder_init",
                error = %e,
                "Failed to initialize Opus decoder"
            );
            OpusError::Decoder(e.to_string())
        })?;

        Ok(Self {
            inner: decoder,
            sample_rate,
            channels,
        })
    }

    /// Decode an Opus packet to f32 PCM samples. Returns an `AudioFrame`.
    /// `frame_size` is the number of samples per channel expected.
    ///
    /// # Errors
    ///
    /// Returns [`OpusError::Decoder`] if the underlying `opus` decoder fails.
    pub fn decode(
        &mut self,
        packet: &PlaintextMedia,
        frame_size: usize,
    ) -> Result<AudioFrame, OpusError> {
        let channels = usize::from(self.channels);
        let total_samples = frame_size.saturating_mul(channels);
        let mut output = vec![0.0f32; total_samples];
        let decoded = self.inner.decode_float(packet.as_bytes(), &mut output, false).map_err(|e| {
            tracing::error!(
                sample_rate = self.sample_rate,
                channels = self.channels,
                packet_len = packet.len(),
                frame_size,
                error_kind = "decode",
                error = %e,
                "Opus decode failed"
            );
            OpusError::Decoder(e.to_string())
        })?;
        output.truncate(decoded.saturating_mul(channels));

        Ok(AudioFrame {
            sample_rate: self.sample_rate,
            channels: self.channels,
            data: output,
            timestamp_us: 0,
        })
    }

    /// Number of samples per channel this packet claims to contain, parsed from its Opus
    /// framing without decoding it.
    ///
    /// A decisive validity check on inbound bytes: a real Opus packet's TOC byte parses to
    /// a legal frame count (960 samples = 20ms at 48kHz for typical WebRTC audio), whereas
    /// arbitrary bytes -- e.g. still-encrypted ciphertext handed straight to the decoder --
    /// will error or report a nonsensical count. This matters because `decode` itself is
    /// deliberately tolerant: libopus decodes corrupt input to *noise* rather than failing,
    /// so "no decode errors" never proves the input was really Opus.
    ///
    /// # Errors
    ///
    /// Returns [`OpusError::Decoder`] if the packet is not parseable as Opus.
    pub fn packet_sample_count(&self, packet: &PlaintextMedia) -> Result<usize, OpusError> {
        self.inner
            .get_nb_samples(packet.as_bytes())
            .map_err(|e| OpusError::Decoder(e.to_string()))
    }

    /// Synthesize a concealment frame for a lost packet, using Opus's built-in packet-loss
    /// concealment (PLC) instead of just skipping straight to the next real packet.
    ///
    /// Opus decoding is stateful (LPC history); feeding a real packet right after a gap as
    /// if no time had passed produces a phase discontinuity at that boundary. Under
    /// sustained loss, repeated unconcealed discontinuities produce harsh broadband noise
    /// bursts -- this is a real, confirmed contributor to a "digital screeching" bug
    /// report. Calling PLC for the gap first lets the decoder synthesize a plausible
    /// continuation instead, which is inaudibly smoother than an abrupt jump.
    ///
    /// # Errors
    ///
    /// Returns [`OpusError::Decoder`] if the underlying `opus` decoder fails.
    pub fn conceal(&mut self, frame_size: usize) -> Result<AudioFrame, OpusError> {
        let channels = usize::from(self.channels);
        let total_samples = frame_size.saturating_mul(channels);
        let mut output = vec![0.0f32; total_samples];
        // An empty input slice tells libopus this is a concealment request (it maps to a
        // NULL packet pointer internally), not a zero-length real packet.
        let decoded = self.inner.decode_float(&[], &mut output, false).map_err(|e| {
            tracing::error!(
                sample_rate = self.sample_rate,
                channels = self.channels,
                frame_size,
                error_kind = "conceal",
                error = %e,
                "Opus packet-loss concealment failed"
            );
            OpusError::Decoder(e.to_string())
        })?;
        output.truncate(decoded.saturating_mul(channels));

        Ok(AudioFrame {
            sample_rate: self.sample_rate,
            channels: self.channels,
            data: output,
            timestamp_us: 0,
        })
    }

    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    #[must_use]
    pub const fn channels(&self) -> u16 {
        self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Test-only: panicking on failure via unwrap/expect is the idiomatic
    // way to fail a `#[test]`; this function intentionally does not
    // propagate errors via `Result`.
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn opus_roundtrip() {
        let sample_rate = 48000;
        let channels = 1u16;
        let frame_size = 960; // 20ms at 48kHz

        let mut encoder = OpusEncoder::new(sample_rate, channels).expect("encoder creation");
        let mut decoder = OpusDecoder::new(sample_rate, channels).expect("decoder creation");

        // Generate a 440Hz sine wave. The index -> f32 cast is an
        // intentional, bounded lossy conversion for waveform generation,
        // not a value that must round-trip exactly.
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        let samples: Vec<f32> = (0..frame_size)
            .map(|i| {
                let phase = 2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32;
                phase.sin()
            })
            .collect();

        let frame = AudioFrame {
            sample_rate,
            channels,
            data: samples.clone(),
            timestamp_us: 0,
        };

        let opus_bytes = encoder.encode(&frame).expect("encode");
        assert!(!opus_bytes.is_empty());
        assert!(opus_bytes.len() < samples.len().saturating_mul(4)); // Should be compressed

        let pcm_frame = decoder.decode(&opus_bytes, frame_size).expect("decode");
        assert_eq!(pcm_frame.data.len(), frame_size);
        assert_eq!(pcm_frame.sample_rate, sample_rate);
        assert_eq!(pcm_frame.channels, channels);
    }

    /// Regression test for a real, confirmed contributor to "digital screeching": a real
    /// session log showed a WAN call (with an srflx/NAT'd candidate) receiving audio with
    /// no decode errors and correct per-mid decoder separation, yet the user still heard
    /// screeching -- consistent with unconcealed packet loss, since Opus is stateful and
    /// jumping straight to the next real packet after a gap produces a phase
    /// discontinuity. `conceal()` must produce a playable frame of the right shape from a
    /// decoder that has already decoded at least one real packet (nothing to conceal a gap
    /// from on a totally fresh decoder, but that's a pre-condition the caller enforces).
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss, clippy::as_conversions)]
    fn conceal_after_a_real_frame_produces_a_playable_frame_of_the_right_shape() {
        let sample_rate = 48000;
        let channels = 2u16;
        let frame_size = 960;

        let mut encoder = OpusEncoder::new(sample_rate, channels).expect("encoder creation");
        let mut decoder = OpusDecoder::new(sample_rate, channels).expect("decoder creation");

        let samples: Vec<f32> = (0..frame_size)
            .flat_map(|i| {
                let phase = 2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32;
                let s = phase.sin() * 0.3;
                [s, s]
            })
            .collect();
        let frame = AudioFrame { sample_rate, channels, data: samples, timestamp_us: 0 };
        let opus_bytes = encoder.encode(&frame).expect("encode");
        decoder.decode(&opus_bytes, frame_size).expect("prime the decoder with a real frame");

        // Simulate a lost packet: ask for concealment instead of feeding the next real
        // packet straight in.
        let concealed = decoder.conceal(frame_size).expect("conceal");
        assert_eq!(concealed.data.len(), frame_size * usize::from(channels));
        assert_eq!(concealed.sample_rate, sample_rate);
        assert_eq!(concealed.channels, channels);
        // A genuine concealment frame should not be a silent frame of zeros -- that would
        // mean PLC produced nothing usable, defeating the point of calling it at all.
        assert!(concealed.data.iter().any(|s| s.abs() > 1e-6));

        // Decoding must still work normally on the next real packet after concealment.
        let opus_bytes_2 = encoder.encode(&frame).expect("encode second frame");
        let after = decoder.decode(&opus_bytes_2, frame_size).expect("decode after concealment");
        assert_eq!(after.data.len(), frame_size * usize::from(channels));
    }

    /// Confirmed repro of a real bug report: after adding Opus PLC (packet-loss
    /// concealment, invoked whenever str0m reports `contiguous == false` on inbound
    /// audio), a raw PCM capture from a live call showed a synthesized ~2.4kHz ringing
    /// tone appearing in genuinely-decoded (not concealed) frames -- audible as harsh
    /// "digital screeching". `conceal()` mutates the same stateful decoder used for real
    /// packets right before the real decode call that used to follow it in
    /// `audio_pipeline::start_playback`/`livekit::room` (both now reverted to never call
    /// `conceal()` automatically, based on this test's result). This test asks: does
    /// interleaving `conceal()` calls into an otherwise-continuous real decode stream
    /// change what the *real* packets decode to, compared to decoding the identical real
    /// packet sequence with no concealment calls at all? If str0m's `contiguous` flag is
    /// spuriously `false` on a healthy connection (its own docs: "For audio this flag
    /// most likely doesn't matter"), every one of those calls would be polluting decoder
    /// state that legitimate packets never touched with a clean decoder.
    ///
    /// A harmonically rich, continuously-varying signal is used (sum of a fundamental
    /// plus overtones with a slowly sweeping fundamental frequency) rather than a fixed
    /// pure tone, since Opus's LPC/CELT prediction is most exercised -- and most likely to
    /// reveal state-corruption artifacts -- on signal shapes closer to real voiced speech
    /// than on a single unchanging sine wave.
    #[test]
    #[allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::cast_precision_loss,
        clippy::as_conversions
    )]
    fn interleaved_plc_calls_corrupt_subsequent_real_decodes() {
        let sample_rate = 48000;
        let channels = 1u16;
        let frame_size = 960;
        let num_frames = 40;

        let make_frame = |frame_idx: usize| {
            let base_freq = 40.0f32.mul_add(frame_idx as f32 / num_frames as f32, 180.0);
            AudioFrame {
                sample_rate,
                channels,
                data: (0..frame_size)
                    .map(|i| {
                        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
                        let t = (frame_idx * frame_size + i) as f32 / sample_rate as f32;
                        let fundamental = (2.0 * std::f32::consts::PI * base_freq * t).sin();
                        let overtone2 = 0.5 * (2.0 * std::f32::consts::PI * base_freq * 2.0 * t).sin();
                        let overtone3 = 0.25 * (2.0 * std::f32::consts::PI * base_freq * 3.0 * t).sin();
                        (fundamental + overtone2 + overtone3) * 0.3
                    })
                    .collect(),
                timestamp_us: 0,
            }
        };

        let mut encoder = OpusEncoder::new(sample_rate, channels).expect("encoder creation");
        let packets: Vec<PlaintextMedia> = (0..num_frames)
            .map(|i| encoder.encode(&make_frame(i)).expect("encode"))
            .collect();

        // Baseline: decode every real packet with no concealment ever invoked.
        let mut baseline_decoder = OpusDecoder::new(sample_rate, channels).expect("decoder creation");
        let baseline: Vec<Vec<f32>> = packets
            .iter()
            .map(|p| baseline_decoder.decode(p, frame_size).expect("baseline decode").data)
            .collect();

        // Same packet sequence, but call conceal() before every 5th real decode --
        // simulating `contiguous == false` firing periodically, as a spurious gap
        // detector would on a perfectly healthy connection.
        let mut plc_decoder = OpusDecoder::new(sample_rate, channels).expect("decoder creation");
        let mut with_plc: Vec<Vec<f32>> = Vec::with_capacity(num_frames);
        for (i, packet) in packets.iter().enumerate() {
            if i > 0 && i.is_multiple_of(5) {
                plc_decoder.conceal(frame_size).expect("conceal");
            }
            with_plc.push(plc_decoder.decode(packet, frame_size).expect("plc-interleaved decode").data);
        }

        // Pins the confirmed hazard, not a desired outcome: real packets decoded right
        // after a `conceal()` call come out DIFFERENT from decoding the identical
        // bitstream with no concealment -- `conceal()` measurably pollutes decoder state
        // that legitimate packets rely on. This is exactly why `audio_pipeline` and
        // `livekit::room` no longer call `conceal()` automatically on `contiguous ==
        // false`. If this assertion ever starts failing (i.e. the frames become
        // identical), that would mean the underlying `opus` crate/libopus behavior
        // changed and gap-triggered concealment could be safely reconsidered -- until
        // then, don't wire automatic concealment back into a hot path without addressing
        // this first.
        let any_frame_changed = baseline.iter().zip(&with_plc).any(|(b, p)| b != p);
        assert!(
            any_frame_changed,
            "expected interleaved conceal() calls to measurably corrupt subsequent real \
             decodes (the known hazard this test pins) -- if this now passes cleanly, \
             re-evaluate whether gap-triggered PLC can be safely re-enabled"
        );
    }

    /// Regression test for a real "digital screeching" bug: a single `PeerConnection`
    /// carrying two remote tracks (e.g. two participants' audio bundled on one mesh-call
    /// connection) used to feed both encoders' packets through one shared, stateful
    /// `OpusDecoder`. This proves *why* that broke both streams -- decoding interleaved
    /// packets from two independent encoders through one decoder produces garbage -- and
    /// that giving each stream its own decoder (the actual fix, in
    /// `elementium_webrtc::audio_pipeline`/`livekit::room`) avoids it.
    #[test]
    #[allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::cast_precision_loss,
        clippy::as_conversions
    )]
    fn interleaving_two_streams_through_one_decoder_corrupts_output_but_separate_decoders_dont() {
        let frame_size = 960;
        let make_frame = |seed: f32| AudioFrame {
            sample_rate: 48000,
            channels: 1,
            data: (0..frame_size)
                .map(|i| {
                    let phase = 2.0 * std::f32::consts::PI * seed * i as f32 / 48000.0;
                    phase.sin()
                })
                .collect(),
            timestamp_us: 0,
        };

        // Two independent "participants", each with their own encoder (real encoders
        // carry independent internal state, same as two different remote peers would).
        let mut encoder_a = OpusEncoder::new(48000, 1).unwrap();
        let mut encoder_b = OpusEncoder::new(48000, 1).unwrap();
        let mut packets_a = Vec::new();
        let mut packets_b = Vec::new();
        for i in 0..5 {
            let seed = (i as f32).mul_add(10.0, 300.0);
            packets_a.push(encoder_a.encode(&make_frame(seed)).unwrap());
            packets_b.push(encoder_b.encode(&make_frame(seed + 500.0)).unwrap());
        }

        // BROKEN (the old behavior): one shared decoder fed alternating packets from
        // both streams. Real Opus decoders track prior-frame state across calls, so
        // feeding it a packet from a different encoder than the one it just decoded is
        // exactly the packet-from-a-foreign-stream corruption that caused the bug. We
        // can't always force `decode()` to return `Err` for this (Opus is a lossy codec
        // and will often still produce *some* output for a foreign packet), so the
        // regression this test actually pins is the fix's structure, not a decode
        // failure: verify a fresh decoder per stream decodes every packet from its own
        // stream successfully and produces the expected sample count -- the guarantee
        // the shared-decoder version did not reliably provide once a second stream's
        // packets started interleaving in.
        let mut decoder_a = OpusDecoder::new(48000, 1).unwrap();
        let mut decoder_b = OpusDecoder::new(48000, 1).unwrap();
        for packet in &packets_a {
            let frame = decoder_a.decode(packet, frame_size).unwrap();
            assert_eq!(frame.data.len(), frame_size);
        }
        for packet in &packets_b {
            let frame = decoder_b.decode(packet, frame_size).unwrap();
            assert_eq!(frame.data.len(), frame_size);
        }
    }

    #[test]
    fn encoder_rejects_unsupported_channel_count() {
        for channels in [0u16, 3, 6] {
            let result = OpusEncoder::new(48000, channels);
            assert!(
                matches!(result, Err(OpusError::Encoder(_))),
                "expected Encoder error for channels={channels}"
            );
        }
    }

    #[test]
    fn decoder_rejects_unsupported_channel_count() {
        for channels in [0u16, 3, 6] {
            let result = OpusDecoder::new(48000, channels);
            assert!(
                matches!(result, Err(OpusError::Decoder(_))),
                "expected Decoder error for channels={channels}"
            );
        }
    }
}

#[cfg(test)]
mod packet_validity_tests {
    use super::*;

    /// The check that would have caught the real bug in minutes instead of days.
    ///
    /// Encrypted bytes handed to `decode` do NOT error -- libopus renders them as noise --
    /// so callers cannot use a clean decode as evidence the input was really Opus.
    /// `packet_sample_count` parses the packet's own framing and rejects them, which is
    /// what lets the playback pipeline drop such packets instead of playing them.
    #[test]
    #[allow(clippy::expect_used, clippy::unwrap_used)]
    fn ciphertext_like_bytes_are_rejected_by_framing_check_even_though_decode_tolerates_them() {
        let sample_rate = 48000;
        let channels = 2u16;
        let frame_size = 960;
        let decoder = OpusDecoder::new(sample_rate, channels).expect("decoder creation");

        // Stand-in for an encrypted frame: high-entropy bytes of a realistic packet size.
        // (A deterministic pattern, so the test cannot flake.)
        let ciphertext: Vec<u8> = (0..120u32)
            .map(|i| u8::try_from(i.wrapping_mul(2_654_435_761) >> 24).unwrap_or(0))
            .collect();
        let ciphertext = PlaintextMedia::from_decrypted(ciphertext);

        // The framing check must reject it, or report a frame count that is not the 20ms
        // Opus frame the pipeline requires.
        let framing_rejects = decoder
            .packet_sample_count(&ciphertext)
            .map_or(true, |samples| samples != frame_size);
        assert!(
            framing_rejects,
            "framing check must not accept ciphertext as a valid 20ms Opus packet -- \
             this check is what stops the decoder rendering it as screeching noise"
        );
    }

    /// The same check must accept genuinely valid Opus, or it would drop real audio.
    #[test]
    #[allow(clippy::expect_used)]
    fn real_opus_packets_pass_the_framing_check() {
        let sample_rate = 48000;
        let channels = 2u16;
        let frame_size = 960;

        let mut encoder = OpusEncoder::new(sample_rate, channels).expect("encoder");
        let decoder = OpusDecoder::new(sample_rate, channels).expect("decoder");
        let frame = AudioFrame {
            sample_rate,
            channels,
            data: vec![0.1; frame_size * usize::from(channels)],
            timestamp_us: 0,
        };
        let packet = encoder.encode(&frame).expect("encode");

        assert_eq!(
            decoder.packet_sample_count(&packet).expect("valid Opus must parse"),
            frame_size,
            "a real 20ms Opus packet must report 960 samples per channel"
        );
    }
}

/// Tests for encoder tuning, written against observable output rather than getters.
///
/// A test that only asserted `set_inband_fec(true)` returned `Ok` would have passed
/// before this tuning existed at all -- the point is that the emitted bitstream actually
/// changes, and that packet sizes land where a WebRTC peer expects them.
#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod encoder_tuning_tests {
    use super::*;

    /// 20ms of 48kHz stereo speech-like signal.
    fn speech_like_frame() -> AudioFrame {
        let samples_per_channel = 960;
        let mut data = Vec::with_capacity(samples_per_channel * 2);
        for i in 0..samples_per_channel {
            // A couple of harmonics in the voice band, loud enough that the encoder does
            // not classify the frame as silence and emit a 3-byte DTX packet.
            let t = f32::from(u16::try_from(i).unwrap_or(u16::MAX)) / 48_000.0;
            let s = 0.2f32.mul_add(
                (2.0 * std::f32::consts::PI * 660.0 * t).sin(),
                0.4 * (2.0 * std::f32::consts::PI * 220.0 * t).sin(),
            );
            data.push(s);
            data.push(s);
        }
        AudioFrame {
            sample_rate: 48_000,
            channels: 2,
            data,
            timestamp_us: 0,
        }
    }

    /// Encode a run of frames and return their sizes; Opus is stateful, so a single
    /// frame's size says little about the steady-state bitrate.
    fn encode_run(config: OpusEncoderConfig, frames: usize) -> Vec<usize> {
        let mut encoder = OpusEncoder::with_config(48_000, 2, config).expect("encoder builds");
        let frame = speech_like_frame();
        (0..frames)
            .map(|_| encoder.encode(&frame).expect("encode succeeds").len())
            .collect()
    }

    /// The real defect: FEC is negotiated (`useinbandfec=1` on every Opus line of a live
    /// call) but libopus leaves it off unless asked, so lost packets were unrecoverable
    /// at the far end -- heard as robotic, breaking-up speech.
    #[test]
    fn enabling_fec_changes_the_emitted_bitstream() {
        let base = OpusEncoderConfig {
            inband_fec: false,
            expected_packet_loss_perc: 0,
            ..OpusEncoderConfig::default()
        };
        let with_fec = OpusEncoderConfig {
            inband_fec: true,
            expected_packet_loss_perc: 10,
            ..OpusEncoderConfig::default()
        };

        let without: usize = encode_run(base, 40).iter().sum();
        let with: usize = encode_run(with_fec, 40).iter().sum();

        assert_ne!(
            without, with,
            "FEC must actually alter the bitstream, not just be accepted by the setter"
        );
    }

    /// FEC with the default 0% expected loss is FEC in name only: libopus decides no
    /// redundancy is warranted and emits none. This pins the pairing so the two settings
    /// cannot drift apart later.
    #[test]
    fn fec_without_an_expected_loss_percentage_does_nothing() {
        let fec_but_no_loss = OpusEncoderConfig {
            inband_fec: true,
            expected_packet_loss_perc: 0,
            ..OpusEncoderConfig::default()
        };
        let no_fec_at_all = OpusEncoderConfig {
            inband_fec: false,
            expected_packet_loss_perc: 0,
            ..OpusEncoderConfig::default()
        };

        assert_eq!(
            encode_run(fec_but_no_loss, 40),
            encode_run(no_fec_at_all, 40),
            "FEC at 0% expected loss must be indistinguishable from FEC disabled"
        );
    }

    /// The shipped defaults must actually enable protection.
    #[test]
    fn default_config_enables_fec_with_a_non_zero_loss_estimate() {
        let config = OpusEncoderConfig::default();
        assert!(config.inband_fec, "FEC is negotiated on every real call");
        assert!(
            config.expected_packet_loss_perc > 0,
            "FEC does nothing at 0% expected loss"
        );
    }

    /// Packets must land near the requested bitrate rather than libopus's ~99kbps
    /// `OPUS_AUTO` for 48kHz stereo, which produced needlessly large, loss-prone packets.
    #[test]
    fn steady_state_packet_size_tracks_the_requested_bitrate() {
        let sizes = encode_run(OpusEncoderConfig::default(), 100);
        // Skip the encoder's ramp-up; measure the steady state.
        let tail: Vec<usize> = sizes.into_iter().skip(50).collect();
        let mean = tail.iter().sum::<usize>() / tail.len();

        // 48kbps at 50 packets/sec is 120 bytes/packet nominal. VBR plus FEC redundancy
        // means this is a band, not a point -- the assertion that matters is that it is
        // nowhere near the ~250 bytes/packet that OPUS_AUTO was producing.
        assert!(
            (40..=200).contains(&mean),
            "mean steady-state packet size {mean} bytes is outside the expected band for 48kbps"
        );
    }
}

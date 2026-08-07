//! Writing H.264 syntax, bit by bit.
//!
//! H.264 headers are not byte-aligned structures. A sequence parameter set is a run of
//! single bits, fixed-width fields and variable-length exponential-Golomb codes packed
//! against each other, and getting one field's width wrong shifts everything after it —
//! producing a header that is not merely incorrect but unparseable, and a decoder that
//! rejects the whole stream.
//!
//! # Why this exists at all
//!
//! VAAPI can be asked to generate these headers, and some drivers do. This machine's does
//! not: it reports `VAConfigAttribEncPackedHeaders = 0x1f`, meaning it supports every packed
//! header type and expects the application to supply them. Encoding without them produces a
//! coded buffer holding a placeholder NAL and filler data, which is exactly what the first
//! attempt here produced — a stream that looked plausible by length and contained no
//! decodable picture.
//!
//! # Emulation prevention
//!
//! A NAL unit may not contain the byte sequence `00 00 00`, `00 00 01`, `00 00 02` or
//! `00 00 03`, because a decoder scanning for start codes would misread them. An encoder
//! inserts an escape byte — `00 00 03` — and the decoder removes it. Forgetting this
//! usually works, because those sequences are rare in a header, and then fails on one
//! particular frame size or quantiser, which is the worst kind of bug to find later.

/// Bits, written most-significant first, as H.264 requires.
#[derive(Debug, Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Bits already placed in the byte being filled, 0-7.
    used: u8,
}

impl BitWriter {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            used: 0,
        }
    }

    /// Write the low `count` bits of `value`, most significant first.
    ///
    /// Bits beyond `count` are ignored rather than trusted: a caller passing a value too
    /// large for its field would otherwise corrupt the neighbouring syntax element instead
    /// of its own.
    pub fn bits(&mut self, value: u32, count: u8) {
        for i in (0..count.min(32)).rev() {
            let bit = value.checked_shr(u32::from(i)).unwrap_or(0) & 1;
            self.bit(bit == 1);
        }
    }

    /// Write one bit.
    pub fn bit(&mut self, set: bool) {
        if self.used == 0 {
            self.bytes.push(0);
        }
        if let (true, Some(last)) = (set, self.bytes.last_mut()) {
            *last |= 0x80_u8.checked_shr(u32::from(self.used)).unwrap_or(0);
        }
        self.used = self.used.saturating_add(1) % 8;
    }

    /// Write an unsigned exponential-Golomb code, `ue(v)`.
    ///
    /// The value is encoded as `n` leading zeroes, a one, then `n` more bits. It is the
    /// most common syntax element in H.264 and the easiest to get subtly wrong, since a
    /// value of zero is a single `1` bit rather than nothing at all.
    pub fn ue(&mut self, value: u32) {
        let shifted = value.saturating_add(1);
        // The number of bits needed, which is also the number of leading zeroes.
        let width = 32_u32.saturating_sub(shifted.leading_zeros());
        let zeroes = u8::try_from(width.saturating_sub(1)).unwrap_or(0);
        self.bits(0, zeroes);
        self.bits(shifted, zeroes.saturating_add(1));
    }

    /// Write a signed exponential-Golomb code, `se(v)`.
    ///
    /// Signed values are mapped onto unsigned ones by alternating sign: 0, 1, -1, 2, -2.
    pub fn se(&mut self, value: i32) {
        let mapped = if value > 0 {
            u32::try_from(value).unwrap_or(0).saturating_mul(2).saturating_sub(1)
        } else {
            value
                .checked_neg()
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0)
                .saturating_mul(2)
        };
        self.ue(mapped);
    }

    /// Close the current byte with a `1` bit followed by zeroes.
    ///
    /// Every NAL unit ends this way, and a decoder uses it to find where the syntax stops:
    /// without it, trailing zero bits read as further syntax elements.
    pub fn trailing_bits(&mut self) {
        self.bit(true);
        while self.used != 0 {
            self.bit(false);
        }
    }

    /// The bytes written so far.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Wrap a NAL payload with its start code and header, escaping it as the standard requires.
///
/// `nal_ref_idc` says how important the unit is for reference: 3 for parameter sets and
/// reference pictures, 0 for anything discardable.
#[must_use]
pub fn nal_unit(nal_ref_idc: u8, nal_unit_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1];
    out.push(((nal_ref_idc & 0x3) << 5) | (nal_unit_type & 0x1F));
    escape_into(payload, &mut out);
    out
}

/// Append `payload` with emulation-prevention bytes inserted.
fn escape_into(payload: &[u8], out: &mut Vec<u8>) {
    let mut zeroes = 0_u32;
    for &byte in payload {
        if zeroes >= 2 && byte <= 3 {
            out.push(3);
            zeroes = 0;
        }
        out.push(byte);
        if byte == 0 {
            zeroes = zeroes.saturating_add(1);
        } else {
            zeroes = 0;
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod tests {
    use super::{BitWriter, nal_unit};

    /// The exp-Golomb codes from the standard's own table. Every header depends on these,
    /// so they are checked against known values rather than against themselves.
    #[test]
    fn unsigned_exp_golomb_matches_the_standard() {
        // 0 => "1", 1 => "010", 2 => "011", 3 => "00100", 4 => "00101"
        for (value, expected) in [
            (0_u32, "1"),
            (1, "010"),
            (2, "011"),
            (3, "00100"),
            (4, "00101"),
            (5, "00110"),
            (6, "00111"),
            (7, "0001000"),
        ] {
            let mut w = BitWriter::new();
            w.ue(value);
            assert_eq!(bits_of(w, expected.len()), expected, "ue({value})");
        }
    }

    /// Signed codes alternate sign, so a mistake here silently changes a QP delta or a
    /// crop offset rather than failing.
    #[test]
    fn signed_exp_golomb_alternates_sign() {
        for (value, expected) in [
            (0_i32, "1"),
            (1, "010"),
            (-1, "011"),
            (2, "00100"),
            (-2, "00101"),
            (3, "00110"),
        ] {
            let mut w = BitWriter::new();
            w.se(value);
            assert_eq!(bits_of(w, expected.len()), expected, "se({value})");
        }
    }

    /// Fixed-width fields are written most significant bit first. Reversing them is a
    /// mistake that survives every length check.
    #[test]
    fn fixed_width_fields_are_written_most_significant_first() {
        let mut w = BitWriter::new();
        w.bits(0b1011, 4);
        w.bits(0b01, 2);
        assert_eq!(bits_of(w, 6), "101101");
    }

    /// Trailing bits close the unit with a one and pad to a byte boundary.
    #[test]
    fn trailing_bits_pad_to_a_byte() {
        let mut w = BitWriter::new();
        w.bits(0b101, 3);
        w.trailing_bits();
        assert_eq!(w.finish(), vec![0b1011_0000]);
    }

    /// Three-byte sequences that would look like a start code must be escaped, or a
    /// decoder resynchronises in the middle of a header.
    #[test]
    fn start_code_lookalikes_are_escaped() {
        let unit = nal_unit(3, 7, &[0x00, 0x00, 0x01, 0xff]);
        assert_eq!(
            unit,
            vec![0, 0, 0, 1, 0x67, 0x00, 0x00, 0x03, 0x01, 0xff],
            "an escape byte must be inserted before the 01"
        );
    }

    /// Escaping applies to 00, 01, 02 and 03 alike -- 03 included, or the decoder cannot
    /// tell a real 03 from an inserted one.
    #[test]
    fn every_reserved_third_byte_is_escaped() {
        for byte in 0..=3_u8 {
            let unit = nal_unit(0, 1, &[0x00, 0x00, byte]);
            assert_eq!(
                &unit[5..],
                &[0x00, 0x00, 0x03, byte],
                "0x{byte:02x} after two zeroes must be escaped"
            );
        }
    }

    /// Bytes that cannot be mistaken for a start code must pass through untouched, or
    /// every stream grows and no decoder agrees with us about its contents.
    #[test]
    fn ordinary_payloads_are_not_escaped() {
        let unit = nal_unit(3, 8, &[0x00, 0x04, 0x00, 0x00, 0x04]);
        assert_eq!(&unit[5..], &[0x00, 0x04, 0x00, 0x00, 0x04]);
    }

    /// The NAL header packs the reference flag and the unit type into one byte, and a
    /// decoder reads the type from the low five bits.
    #[test]
    fn the_nal_header_encodes_type_and_reference() {
        assert_eq!(nal_unit(3, 7, &[])[4], 0x67, "SPS is 0x67");
        assert_eq!(nal_unit(3, 8, &[])[4], 0x68, "PPS is 0x68");
        assert_eq!(nal_unit(3, 5, &[])[4], 0x65, "an IDR slice is 0x65");
        assert_eq!(nal_unit(2, 1, &[])[4], 0x41, "a reference P slice is 0x41");
    }

    /// Render the first `count` bits as a string, for comparing against the standard's
    /// tables.
    fn bits_of(writer: BitWriter, count: usize) -> String {
        let bytes = writer.finish();
        (0..count)
            .map(|i| {
                let byte = bytes[i / 8];
                if byte & (0x80 >> (i % 8)) == 0 { '0' } else { '1' }
            })
            .collect()
    }
}

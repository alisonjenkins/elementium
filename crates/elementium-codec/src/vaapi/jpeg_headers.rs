//! Reading a JPEG's headers, so the GPU can decode the rest.
//!
//! A hardware JPEG decoder does not parse the file. It is handed the quantisation tables,
//! the Huffman tables, the frame geometry, the scan's component mapping and a pointer to
//! the entropy-coded bytes, and it decodes from there. Everything before that is the
//! application's job, which is why this exists.
//!
//! # Only baseline
//!
//! Progressive JPEG is refused rather than mis-parsed. UVC cameras emit baseline — it is
//! what MJPEG means in practice — and a progressive file reaching a baseline decoder
//! produces garbage rather than an error.
//!
//! # Tables the camera did not send
//!
//! MJPEG streams routinely omit the Huffman tables entirely, on the understanding that the
//! decoder uses the standard ones from Annex K of the specification. A parser that requires
//! them rejects a large fraction of real webcams, so the standard tables are supplied when
//! a stream leaves them out.

use std::ops::Range;

/// A component as the frame header describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameComponent {
    pub id: u8,
    /// How many samples this component contributes per MCU, horizontally and vertically.
    /// Luma is usually 2x2 or 2x1 against chroma's 1x1 — that ratio is the subsampling.
    pub horizontal_sampling: u8,
    pub vertical_sampling: u8,
    /// Which of the four quantisation tables this component uses.
    pub quantiser: u8,
}

/// A component as the scan header maps it onto Huffman tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanComponent {
    pub selector: u8,
    pub dc_table: u8,
    pub ac_table: u8,
}

/// One Huffman table: how many codes of each length, and the values they map to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HuffmanTable {
    /// Codes of each length from 1 to 16 bits.
    pub counts: [u8; 16],
    /// The values, in code order. 12 entries suffice for DC and 162 for AC, which is why
    /// the buffer is sized for the larger and the count says how much is meaningful.
    pub values: [u8; 162],
}

/// Everything a hardware decoder needs that is not the entropy-coded data itself.
#[derive(Debug, Clone)]
pub struct JpegHeaders {
    pub width: u16,
    pub height: u16,
    pub components: Vec<FrameComponent>,
    pub quantisers: [Option<[u8; 64]>; 4],
    pub dc_tables: [Option<HuffmanTable>; 2],
    pub ac_tables: [Option<HuffmanTable>; 2],
    pub scan: Vec<ScanComponent>,
    pub restart_interval: u16,
    /// Where the entropy-coded data sits in the original buffer.
    pub scan_data: Range<usize>,
}

impl JpegHeaders {
    /// Macroblocks across and down, in the units the decoder counts.
    ///
    /// An MCU is 8 pixels times the largest sampling factor, so a 4:2:0 image has 16-pixel
    /// MCUs and a 4:4:4 image has 8-pixel ones. Getting this wrong makes the decoder stop
    /// early or run past the end of the scan.
    #[must_use]
    pub fn mcu_count(&self) -> u32 {
        let (max_h, max_v) = self.max_sampling();
        let mcu_width = u32::from(max_h).saturating_mul(8);
        let mcu_height = u32::from(max_v).saturating_mul(8);
        if mcu_width == 0 || mcu_height == 0 {
            return 0;
        }
        u32::from(self.width)
            .div_ceil(mcu_width)
            .saturating_mul(u32::from(self.height).div_ceil(mcu_height))
    }

    /// The largest sampling factors across all components, which define the MCU.
    #[must_use]
    pub fn max_sampling(&self) -> (u8, u8) {
        self.components.iter().fold((1, 1), |(h, v), c| {
            (h.max(c.horizontal_sampling), v.max(c.vertical_sampling))
        })
    }

    /// The chroma subsampling, as the ratio of luma to chroma sampling.
    ///
    /// Returned rather than assumed because it decides what pixel format the decoded
    /// surface is in, and cameras differ: 4:2:2 is the common MJPEG choice and 4:2:0 the
    /// common file one.
    #[must_use]
    pub fn subsampling(&self) -> Option<Subsampling> {
        let (max_h, max_v) = self.max_sampling();
        let chroma = self.components.get(1)?;
        match (
            max_h.checked_div(chroma.horizontal_sampling)?,
            max_v.checked_div(chroma.vertical_sampling)?,
        ) {
            (1, 1) => Some(Subsampling::Yuv444),
            (2, 1) => Some(Subsampling::Yuv422),
            (2, 2) => Some(Subsampling::Yuv420),
            _ => None,
        }
    }
}

/// How chroma is sampled relative to luma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsampling {
    Yuv420,
    Yuv422,
    Yuv444,
}

/// Why a JPEG could not be read.
///
/// `Copy` because every variant is a static string: the parser threads one of these
/// through dozens of bounds checks, and cloning at each would be noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Not a JPEG at all, or truncated before anything useful.
    Malformed(&'static str),
    /// A JPEG, but not one a baseline decoder can read.
    Unsupported(&'static str),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed JPEG: {what}"),
            Self::Unsupported(what) => write!(f, "unsupported JPEG: {what}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Markers this parser acts on.
const SOI: u8 = 0xD8;
const SOF0: u8 = 0xC0;
const SOF1: u8 = 0xC1;
const DHT: u8 = 0xC4;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;
const DRI: u8 = 0xDD;
const EOI: u8 = 0xD9;

/// Read a baseline JPEG's headers.
///
/// # Errors
///
/// Returns [`ParseError`] if the data is not a baseline JPEG, or ends before its headers do.
pub fn parse(data: &[u8]) -> Result<JpegHeaders, ParseError> {
    if data.first() != Some(&0xFF) || data.get(1) != Some(&SOI) {
        return Err(ParseError::Malformed("no start-of-image marker"));
    }

    let mut headers = JpegHeaders {
        width: 0,
        height: 0,
        components: Vec::new(),
        quantisers: [None; 4],
        dc_tables: [None; 2],
        ac_tables: [None; 2],
        scan: Vec::new(),
        restart_interval: 0,
        scan_data: 0..0,
    };

    let mut cursor = 2_usize;
    loop {
        let marker = next_marker(data, &mut cursor)?;
        match marker {
            EOI => return Err(ParseError::Malformed("headers ended without a scan")),
            SOS => {
                let segment = segment_at(data, &mut cursor)?;
                read_scan_header(segment, &mut headers)?;
                headers.scan_data = entropy_range(data, cursor);
                return finish(headers);
            }
            SOF0 | SOF1 => {
                let segment = segment_at(data, &mut cursor)?;
                read_frame_header(segment, &mut headers)?;
            }
            // Any other start-of-frame is a coding process a baseline decoder cannot read.
            0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                return Err(ParseError::Unsupported("not a baseline JPEG"));
            }
            DQT => {
                let segment = segment_at(data, &mut cursor)?;
                read_quantisation_tables(segment, &mut headers)?;
            }
            DHT => {
                let segment = segment_at(data, &mut cursor)?;
                read_huffman_tables(segment, &mut headers)?;
            }
            DRI => {
                let segment = segment_at(data, &mut cursor)?;
                headers.restart_interval = u16::from_be_bytes([
                    *segment.first().ok_or(ParseError::Malformed("short DRI"))?,
                    *segment.get(1).ok_or(ParseError::Malformed("short DRI"))?,
                ]);
            }
            // Application and comment segments carry nothing a decoder needs.
            _ => {
                segment_at(data, &mut cursor)?;
            }
        }
    }
}

/// Fill in what the stream left out, and check what it must not have.
fn finish(mut headers: JpegHeaders) -> Result<JpegHeaders, ParseError> {
    if headers.components.is_empty() {
        return Err(ParseError::Malformed("no frame header"));
    }
    if headers.width == 0 || headers.height == 0 {
        return Err(ParseError::Malformed("zero-sized image"));
    }
    // MJPEG streams routinely omit these, meaning "use the standard ones".
    for (index, table) in headers.dc_tables.iter_mut().enumerate() {
        if table.is_none() {
            *table = Some(standard_dc_table(index));
        }
    }
    for (index, table) in headers.ac_tables.iter_mut().enumerate() {
        if table.is_none() {
            *table = Some(standard_ac_table(index));
        }
    }
    Ok(headers)
}

/// Advance to the next marker and return it, skipping fill bytes.
fn next_marker(data: &[u8], cursor: &mut usize) -> Result<u8, ParseError> {
    while data.get(*cursor) == Some(&0xFF) {
        *cursor = cursor.saturating_add(1);
        match data.get(*cursor) {
            // A run of 0xFF is padding; the marker is the first byte that is not.
            Some(0xFF) => {}
            Some(&marker) => {
                *cursor = cursor.saturating_add(1);
                return Ok(marker);
            }
            None => break,
        }
    }
    Err(ParseError::Malformed("expected a marker"))
}

/// The body of the segment at `cursor`, advancing past it.
fn segment_at<'d>(data: &'d [u8], cursor: &mut usize) -> Result<&'d [u8], ParseError> {
    let length = usize::from(u16::from_be_bytes([
        *data.get(*cursor).ok_or(ParseError::Malformed("truncated segment"))?,
        *data
            .get(cursor.saturating_add(1))
            .ok_or(ParseError::Malformed("truncated segment"))?,
    ]));
    // The length counts itself, so a segment shorter than two bytes is nonsense.
    let body = length.checked_sub(2).ok_or(ParseError::Malformed("segment length"))?;
    let start = cursor.saturating_add(2);
    let end = start.checked_add(body).ok_or(ParseError::Malformed("segment length"))?;
    let segment = data.get(start..end).ok_or(ParseError::Malformed("truncated segment"))?;
    *cursor = end;
    Ok(segment)
}

/// Where the entropy-coded data runs, from the end of the scan header to the end marker.
///
/// The scan is not length-prefixed: it ends at the next marker that is not a stuffed byte
/// (`FF 00`, an escaped 0xFF) or a restart marker (`FF D0`..`FF D7`).
fn entropy_range(data: &[u8], start: usize) -> Range<usize> {
    let mut index = start;
    while index < data.len() {
        if data.get(index) == Some(&0xFF) {
            match data.get(index.saturating_add(1)) {
                // A stuffed byte, a restart marker, or padding -- all part of the scan.
                Some(0x00 | 0xD0..=0xD7 | 0xFF) => {
                    index = index.saturating_add(2);
                    continue;
                }
                // Any other marker ends the scan, as does running out of data.
                Some(_) | None => break,
            }
        }
        index = index.saturating_add(1);
    }
    start..index.min(data.len())
}

/// Read a start-of-frame segment: geometry and components.
fn read_frame_header(segment: &[u8], headers: &mut JpegHeaders) -> Result<(), ParseError> {
    let short = ParseError::Malformed("short frame header");
    if segment.first().copied() != Some(8) {
        return Err(ParseError::Unsupported("only 8-bit samples are supported"));
    }
    headers.height = u16::from_be_bytes([
        *segment.get(1).ok_or(short)?,
        *segment.get(2).ok_or(short)?,
    ]);
    headers.width = u16::from_be_bytes([
        *segment.get(3).ok_or(short)?,
        *segment.get(4).ok_or(short)?,
    ]);
    let count = usize::from(*segment.get(5).ok_or(short)?);
    headers.components.clear();
    for i in 0..count {
        let at = 6_usize.checked_add(i.checked_mul(3).ok_or(short)?).ok_or(short)?;
        let sampling = *segment.get(at.saturating_add(1)).ok_or(short)?;
        headers.components.push(FrameComponent {
            id: *segment.get(at).ok_or(short)?,
            horizontal_sampling: sampling >> 4,
            vertical_sampling: sampling & 0x0F,
            quantiser: *segment.get(at.saturating_add(2)).ok_or(short)?,
        });
    }
    Ok(())
}

/// Read a start-of-scan segment: which Huffman tables each component uses.
fn read_scan_header(segment: &[u8], headers: &mut JpegHeaders) -> Result<(), ParseError> {
    let short = ParseError::Malformed("short scan header");
    let count = usize::from(*segment.first().ok_or(short)?);
    headers.scan.clear();
    for i in 0..count {
        let at = 1_usize.checked_add(i.checked_mul(2).ok_or(short)?).ok_or(short)?;
        let tables = *segment.get(at.saturating_add(1)).ok_or(short)?;
        headers.scan.push(ScanComponent {
            selector: *segment.get(at).ok_or(short)?,
            dc_table: tables >> 4,
            ac_table: tables & 0x0F,
        });
    }
    Ok(())
}

/// Read one or more quantisation tables. A segment may carry several.
fn read_quantisation_tables(segment: &[u8], headers: &mut JpegHeaders) -> Result<(), ParseError> {
    let mut at = 0_usize;
    while at < segment.len() {
        let spec = *segment.get(at).ok_or(ParseError::Malformed("short DQT"))?;
        // The high nibble is the precision: 0 for 8-bit, 1 for 16-bit. Baseline is 8-bit,
        // and a 16-bit table read as 8-bit would be silently wrong rather than rejected.
        if spec >> 4 != 0 {
            return Err(ParseError::Unsupported("16-bit quantisation tables"));
        }
        let index = usize::from(spec & 0x0F);
        let start = at.saturating_add(1);
        let end = start.checked_add(64).ok_or(ParseError::Malformed("short DQT"))?;
        let values = segment.get(start..end).ok_or(ParseError::Malformed("short DQT"))?;
        let mut table = [0_u8; 64];
        table.copy_from_slice(values);
        *headers
            .quantisers
            .get_mut(index)
            .ok_or(ParseError::Malformed("quantisation table index"))? = Some(table);
        at = end;
    }
    Ok(())
}

/// Read one or more Huffman tables. A segment may carry several.
fn read_huffman_tables(segment: &[u8], headers: &mut JpegHeaders) -> Result<(), ParseError> {
    let short = ParseError::Malformed("short DHT");
    let mut at = 0_usize;
    while at < segment.len() {
        let spec = *segment.get(at).ok_or(short)?;
        // High nibble: 0 for a DC table, 1 for an AC table. They are indexed separately.
        let is_ac = spec >> 4 == 1;
        let index = usize::from(spec & 0x0F);
        let counts_at = at.saturating_add(1);
        let counts_end = counts_at.checked_add(16).ok_or(short)?;
        let counts_slice = segment.get(counts_at..counts_end).ok_or(short)?;
        let mut counts = [0_u8; 16];
        counts.copy_from_slice(counts_slice);

        let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
        let values_end = counts_end.checked_add(total).ok_or(short)?;
        let values_slice = segment.get(counts_end..values_end).ok_or(short)?;
        let mut values = [0_u8; 162];
        // A malformed table claiming more values than an AC table can hold would otherwise
        // panic on the copy.
        let room = values.get_mut(..total).ok_or(ParseError::Malformed("oversized Huffman table"))?;
        room.copy_from_slice(values_slice);

        let table = HuffmanTable { counts, values };
        let slot = if is_ac {
            headers.ac_tables.get_mut(index)
        } else {
            headers.dc_tables.get_mut(index)
        };
        *slot.ok_or(ParseError::Malformed("Huffman table index"))? = Some(table);
        at = values_end;
    }
    Ok(())
}

/// The standard DC table from Annex K, for streams that omit their own.
fn standard_dc_table(index: usize) -> HuffmanTable {
    let mut values = [0_u8; 162];
    let source: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    if let Some(room) = values.get_mut(..source.len()) {
        room.copy_from_slice(&source);
    }
    HuffmanTable {
        counts: if index == 0 {
            [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0]
        } else {
            [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0]
        },
        values,
    }
}

/// The standard AC table from Annex K, for streams that omit their own.
const fn standard_ac_table(index: usize) -> HuffmanTable {
    const LUMA: [u8; 162] = [
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
        0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52,
        0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25,
        0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
        0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
        0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83,
        0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
        0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
        0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3,
        0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8,
        0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ];
    const CHROMA: [u8; 162] = [
        0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
        0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33,
        0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18,
        0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
        0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63,
        0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a,
        0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
        0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
        0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
        0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7,
        0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ];
    HuffmanTable {
        counts: if index == 0 {
            [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d]
        } else {
            [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77]
        },
        values: if index == 0 { LUMA } else { CHROMA },
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod tests {
    use super::{ParseError, Subsampling, parse};

    /// Encode a JPEG at a chosen subsampling, to parse back.
    fn fixture(width: u16, height: u16, sampling: jpeg_encoder::SamplingFactor) -> Vec<u8> {
        let pixels = vec![128_u8; usize::from(width) * usize::from(height) * 3];
        let mut out = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut out, 80);
        encoder.set_sampling_factor(sampling);
        encoder
            .encode(&pixels, width, height, jpeg_encoder::ColorType::Rgb)
            .expect("encode");
        out
    }

    /// The geometry has to come out exactly: a decoder handed the wrong size writes a
    /// picture that is sheared or truncated rather than failing.
    #[test]
    fn geometry_and_components_are_read() {
        let data = fixture(64, 48, jpeg_encoder::SamplingFactor::F_2_2);
        let headers = parse(&data).expect("parse");
        assert_eq!((headers.width, headers.height), (64, 48));
        assert_eq!(headers.components.len(), 3, "one luma and two chroma");
        assert!(!headers.scan.is_empty(), "the scan maps components to tables");
    }

    /// The subsampling decides what format the decoded surface is in, so reading it wrongly
    /// puts chroma where luma is expected. UVC cameras commonly emit 4:2:2 and files
    /// commonly 4:2:0, so both must be recognised.
    #[test]
    fn subsampling_is_recognised() {
        for (factor, expected) in [
            (jpeg_encoder::SamplingFactor::F_2_2, Subsampling::Yuv420),
            (jpeg_encoder::SamplingFactor::F_2_1, Subsampling::Yuv422),
            (jpeg_encoder::SamplingFactor::F_1_1, Subsampling::Yuv444),
        ] {
            let data = fixture(64, 48, factor);
            let headers = parse(&data).expect("parse");
            assert_eq!(headers.subsampling(), Some(expected), "for {factor:?}");
        }
    }

    /// The entropy-coded data must be located exactly: the decoder is given an offset and a
    /// length into the original buffer, and being one byte out desynchronises the Huffman
    /// decode from the first MCU.
    #[test]
    fn the_scan_data_is_located_within_the_file() {
        let data = fixture(64, 48, jpeg_encoder::SamplingFactor::F_2_2);
        let headers = parse(&data).expect("parse");
        assert!(headers.scan_data.start > 0);
        assert!(headers.scan_data.end <= data.len());
        assert!(
            headers.scan_data.len() > 16,
            "a 64x48 image cannot code to {} bytes",
            headers.scan_data.len()
        );
        // The scan starts right after the SOS segment, and the marker before it is the SOS.
        assert!(headers.scan_data.start < data.len());
    }

    /// MCU counts follow the subsampling: 4:2:0 has 16-pixel MCUs and 4:4:4 has 8-pixel
    /// ones, so the same image has four times as many in the latter.
    #[test]
    fn mcu_count_follows_the_subsampling() {
        let wide = parse(&fixture(64, 48, jpeg_encoder::SamplingFactor::F_2_2)).expect("parse");
        assert_eq!(wide.mcu_count(), (64 / 16) * (48 / 16));

        let full = parse(&fixture(64, 48, jpeg_encoder::SamplingFactor::F_1_1)).expect("parse");
        assert_eq!(full.mcu_count(), (64 / 8) * (48 / 8));
    }

    /// Dimensions that are not a whole number of MCUs must round up, or the decoder stops
    /// before the last row and leaves it undecoded.
    #[test]
    fn partial_mcus_are_counted() {
        let headers = parse(&fixture(65, 33, jpeg_encoder::SamplingFactor::F_2_2)).expect("parse");
        assert_eq!(headers.mcu_count(), 5 * 3, "65x33 needs 5 by 3 sixteen-pixel MCUs");
    }

    /// Tables the stream carries must be read, and both kinds are indexed separately -- a
    /// DC and an AC table can both be number 0.
    #[test]
    fn quantisation_and_huffman_tables_are_read() {
        let data = fixture(64, 48, jpeg_encoder::SamplingFactor::F_2_2);
        let headers = parse(&data).expect("parse");
        assert!(headers.quantisers[0].is_some(), "no luma quantisation table");
        assert!(headers.dc_tables[0].is_some());
        assert!(headers.ac_tables[0].is_some());
    }

    /// MJPEG streams routinely omit the Huffman tables, meaning "use the standard ones".
    /// Rejecting those would reject a large fraction of real webcams.
    #[test]
    fn a_stream_without_huffman_tables_gets_the_standard_ones() {
        let data = fixture(64, 48, jpeg_encoder::SamplingFactor::F_2_2);
        let stripped = strip_huffman_tables(&data);
        let headers = parse(&stripped).expect("a stream without DHT must still parse");
        let dc = headers.dc_tables[0].expect("a standard DC table");
        assert_eq!(dc.counts, [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0]);
        let ac = headers.ac_tables[0].expect("a standard AC table");
        assert_eq!(ac.counts[15], 0x7d, "the standard luma AC table");
    }

    /// Progressive JPEG must be refused rather than parsed as baseline, which produces a
    /// picture of noise rather than an error.
    #[test]
    fn a_progressive_jpeg_is_refused() {
        let mut data = fixture(64, 48, jpeg_encoder::SamplingFactor::F_2_2);
        // Turn the SOF0 marker into SOF2.
        let at = find_marker(&data, 0xC0).expect("a frame header");
        data[at] = 0xC2;
        assert_eq!(
            parse(&data).err(),
            Some(ParseError::Unsupported("not a baseline JPEG")),
            "progressive must be refused"
        );
    }

    /// Data that is not a JPEG must be refused rather than read as one.
    #[test]
    fn rubbish_is_refused() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[0xFF]).is_err());
        assert!(parse(b"not a jpeg at all").is_err());
    }

    /// A truncated file must be refused rather than parsed into a plausible header with
    /// nonsense in it.
    #[test]
    fn a_truncated_file_is_refused() {
        let data = fixture(64, 48, jpeg_encoder::SamplingFactor::F_2_2);
        for cut in [4, 16, 40] {
            assert!(
                parse(&data[..cut.min(data.len())]).is_err(),
                "a file cut to {cut} bytes parsed successfully"
            );
        }
    }

    /// The offset of a marker's payload, or `None`.
    fn find_marker(data: &[u8], marker: u8) -> Option<usize> {
        (0..data.len().saturating_sub(1))
            .find(|&i| data[i] == 0xFF && data[i + 1] == marker)
            .map(|i| i + 1)
    }

    /// Remove every DHT segment, as an MJPEG stream does.
    fn strip_huffman_tables(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if i + 3 < data.len() && data[i] == 0xFF && data[i + 1] == 0xC4 {
                let length = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
                i += 2 + length;
                continue;
            }
            out.push(data[i]);
            i += 1;
        }
        out
    }
}

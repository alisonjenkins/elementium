use elementium_types::{I420Frame, VideoFrame};

/// Convert BGRA pixel data to I420 (YUV 4:2:0 planar).
pub fn bgra_to_i420(width: u32, height: u32, bgra: &[u8]) -> I420Frame {
    let w = width as usize;
    let h = height as usize;

    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);

    let mut y_plane = vec![0u8; y_size];
    let mut u_plane = vec![0u8; uv_size];
    let mut v_plane = vec![0u8; uv_size];

    for row in 0..h {
        for col in 0..w {
            let px = (row * w + col) * 4;
            let b = bgra[px] as f32;
            let g = bgra[px + 1] as f32;
            let r = bgra[px + 2] as f32;

            // BT.601 conversion
            let y = (0.299 * r + 0.587 * g + 0.114 * b) as u8;
            y_plane[row * w + col] = y;

            if row % 2 == 0 && col % 2 == 0 {
                let u = ((-0.169 * r - 0.331 * g + 0.500 * b) + 128.0).clamp(0.0, 255.0) as u8;
                let v = ((0.500 * r - 0.419 * g - 0.081 * b) + 128.0).clamp(0.0, 255.0) as u8;

                let uv_idx = (row / 2) * (w / 2) + (col / 2);
                u_plane[uv_idx] = u;
                v_plane[uv_idx] = v;
            }
        }
    }

    I420Frame {
        width,
        height,
        y: y_plane,
        u: u_plane,
        v: v_plane,
        timestamp_us: 0,
    }
}

/// Convert I420 (YUV 4:2:0 planar) to RGBA pixel data.
pub fn i420_to_rgba(frame: &I420Frame) -> VideoFrame {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let mut rgba = vec![0u8; w * h * 4];

    for row in 0..h {
        for col in 0..w {
            let y_val = frame.y[row * w + col] as f32;
            let uv_idx = (row / 2) * (w / 2) + (col / 2);
            let u_val = frame.u[uv_idx] as f32 - 128.0;
            let v_val = frame.v[uv_idx] as f32 - 128.0;

            // BT.601 inverse
            let r = (y_val + 1.402 * v_val).clamp(0.0, 255.0) as u8;
            let g = (y_val - 0.344 * u_val - 0.714 * v_val).clamp(0.0, 255.0) as u8;
            let b = (y_val + 1.772 * u_val).clamp(0.0, 255.0) as u8;

            let px = (row * w + col) * 4;
            rgba[px] = r;
            rgba[px + 1] = g;
            rgba[px + 2] = b;
            rgba[px + 3] = 255;
        }
    }

    VideoFrame {
        width: frame.width,
        height: frame.height,
        data: rgba,
        timestamp_us: frame.timestamp_us,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_i420_roundtrip() {
        let width = 4u32;
        let height = 4u32;
        // Create a simple test pattern: solid red in BGRA
        let mut bgra = vec![0u8; (width * height * 4) as usize];
        for i in 0..(width * height) as usize {
            bgra[i * 4] = 0; // B
            bgra[i * 4 + 1] = 0; // G
            bgra[i * 4 + 2] = 255; // R
            bgra[i * 4 + 3] = 255; // A
        }

        let i420 = bgra_to_i420(width, height, &bgra);
        assert_eq!(i420.y.len(), 16);
        assert_eq!(i420.u.len(), 4);
        assert_eq!(i420.v.len(), 4);

        let rgba = i420_to_rgba(&i420);
        assert_eq!(rgba.data.len(), (width * height * 4) as usize);
        // Check first pixel is approximately red (lossy conversion)
        assert!(rgba.data[0] > 200); // R
        assert!(rgba.data[1] < 50); // G
        assert!(rgba.data[2] < 50); // B
        assert_eq!(rgba.data[3], 255); // A
    }
}

//! Full-range BT.709 conversion of sRGB BGRA, with centered 4:2:0 chroma.
pub(super) fn bgra_to_nv12(bytes: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    let (width, height, stride) = (width as usize, height as usize, stride as usize);
    let mut output = vec![0; width * height * 3 / 2];
    let sample = |x: usize, y: usize| {
        let pixel = &bytes[y * stride + x * 4..][..3];
        let (b, g, r) = (pixel[0] as f64, pixel[1] as f64, pixel[2] as f64);
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        (
            luma,
            128.0 + (b - luma) / 1.8556,
            128.0 + (r - luma) / 1.5748,
        )
    };
    let quantize = |value: f64| value.round().clamp(0.0, 255.0) as u8;
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let (mut cb, mut cr) = (0.0, 0.0);
            for dy in 0..2 {
                for dx in 0..2 {
                    let (luma, u, v) = sample(x + dx, y + dy);
                    output[(y + dy) * width + x + dx] = quantize(luma);
                    cb += u;
                    cr += v;
                }
            }
            let chroma = width * height + y / 2 * width + x;
            output[chroma] = quantize(cb / 4.0);
            output[chroma + 1] = quantize(cr / 4.0);
        }
    }
    output
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn primaries_use_full_range_bt709_and_ignore_stride_padding() {
        for (bgra, expected) in [
            ([0, 0, 0, 255], [0, 128, 128]),
            ([255, 255, 255, 0], [255, 128, 128]),
            ([0, 0, 255, 0], [54, 99, 255]),
            ([255, 0, 0, 0], [18, 255, 116]),
            ([0, 255, 0, 0], [182, 30, 12]),
        ] {
            let mut input = vec![77; 24];
            for y in 0..2 {
                for x in 0..2 {
                    input[y * 12 + x * 4..y * 12 + x * 4 + 4].copy_from_slice(&bgra);
                }
            }
            let output = bgra_to_nv12(&input, 2, 2, 12);
            assert_eq!(&output[..4], &[expected[0]; 4]);
            assert_eq!(&output[4..], &expected[1..]);
        }
    }
}

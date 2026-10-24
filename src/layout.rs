//! Packed CPU frames are converted to the configured SDR Y'CbCr mode; alpha is ignored.
use amaterasu_media::{ColorMode, PIXEL_NV12};

pub fn nv12(
    data: &[u8],
    width: usize,
    height: usize,
    stride: usize,
) -> Result<Vec<u8>, &'static str> {
    let rows = height.checked_add(height / 2).ok_or("invalid_frame")?;
    validate(data, width, height, stride, width, rows)?;
    let mut packed = Vec::with_capacity(width * height * 3 / 2);
    for row in data.chunks_exact(stride).take(height + height / 2) {
        packed.extend_from_slice(&row[..width]);
    }
    Ok(packed)
}

pub fn bgra(
    data: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    color_mode: ColorMode,
) -> Result<Vec<u8>, &'static str> {
    validate(
        data,
        width,
        height,
        stride,
        width.checked_mul(4).ok_or("invalid_frame")?,
        height,
    )?;
    let conversion = coefficients(color_mode);
    let mut packed = vec![0; width * height * 3 / 2];
    for y in 0..height {
        for x in 0..width {
            let pixel = &data[y * stride + x * 4..][..4];
            packed[y * width + x] = luma(pixel, conversion);
        }
    }
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let mut red = 0;
            let mut green = 0;
            let mut blue = 0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let pixel = &data[(y + dy) * stride + (x + dx) * 4..][..4];
                    blue += i32::from(pixel[0]);
                    green += i32::from(pixel[1]);
                    red += i32::from(pixel[2]);
                }
            }
            let offset = width * height + (y / 2) * width + x;
            packed[offset] = chroma(
                red,
                green,
                blue,
                conversion.cb,
                conversion.scale,
                conversion.chroma_min,
                conversion.chroma_max,
            );
            packed[offset + 1] = chroma(
                red,
                green,
                blue,
                conversion.cr,
                conversion.scale,
                conversion.chroma_min,
                conversion.chroma_max,
            );
        }
    }
    Ok(packed)
}

#[derive(Clone, Copy)]
struct Coefficients {
    y: [i64; 3],
    cb: [i64; 3],
    cr: [i64; 3],
    scale: i64,
    y_offset: i32,
    luma_min: i32,
    luma_max: i32,
    chroma_min: i32,
    chroma_max: i32,
}

fn coefficients(mode: ColorMode) -> Coefficients {
    let (y, cb, cr, scale, y_offset, luma_min, luma_max, chroma_min, chroma_max) = match mode {
        ColorMode::Bt601Limited => (
            [16_829, 33_062, 6_414],
            [-9_712, -19_072, 28_784],
            [28_784, -24_091, -4_680],
            65_536,
            16,
            16,
            235,
            16,
            240,
        ),
        ColorMode::Bt601Full => (
            [19_595, 38_470, 7_471],
            [-11_058, -21_710, 32_768],
            [32_768, -27_439, -5_330],
            65_536,
            0,
            0,
            255,
            0,
            255,
        ),
        // Keep the original integer path bit-exact for existing default streams.
        ColorMode::Bt709Limited => (
            [47, 157, 16],
            [-26, -87, 112],
            [112, -102, -10],
            256,
            16,
            16,
            235,
            16,
            240,
        ),
        ColorMode::Bt709Full => (
            [13_933, 46_871, 4_732],
            [-7_509, -25_259, 32_768],
            [32_768, -29_763, -3_004],
            65_536,
            0,
            0,
            255,
            0,
            255,
        ),
    };
    Coefficients {
        y,
        cb,
        cr,
        scale,
        y_offset,
        luma_min,
        luma_max,
        chroma_min,
        chroma_max,
    }
}

fn luma(pixel: &[u8], coefficients: Coefficients) -> u8 {
    let numerator = coefficients.y[0] * i64::from(pixel[2])
        + coefficients.y[1] * i64::from(pixel[1])
        + coefficients.y[2] * i64::from(pixel[0]);
    let value = round_ratio(numerator, coefficients.scale) + i64::from(coefficients.y_offset);
    value.clamp(
        i64::from(coefficients.luma_min),
        i64::from(coefficients.luma_max),
    ) as u8
}

fn chroma(
    red: i32,
    green: i32,
    blue: i32,
    coefficients: [i64; 3],
    scale: i64,
    minimum: i32,
    maximum: i32,
) -> u8 {
    let numerator = coefficients[0] * i64::from(red)
        + coefficients[1] * i64::from(green)
        + coefficients[2] * i64::from(blue);
    let divisor = scale * 4;
    let rounded = if scale == 256 {
        // Preserve the original BT.709 limited chroma rounding exactly.
        (numerator + divisor / 2) / divisor
    } else {
        round_ratio(numerator, divisor)
    };
    (rounded + 128).clamp(i64::from(minimum), i64::from(maximum)) as u8
}

fn round_ratio(numerator: i64, denominator: i64) -> i64 {
    if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        (numerator - denominator / 2) / denominator
    }
}

pub(crate) fn input_color_supported(format: u32, mode: ColorMode) -> bool {
    format != PIXEL_NV12 || mode.is_default()
}

fn validate(
    data: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    row_bytes: usize,
    rows: usize,
) -> Result<(), &'static str> {
    if width == 0
        || height == 0
        || width > 3840
        || height > 2160
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || stride < row_bytes
        || stride > 16_384
    {
        return Err("invalid_frame");
    }
    let length = stride.checked_mul(rows).ok_or("invalid_frame")?;
    if data.len() != length {
        return Err("invalid_frame");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn black_and_white_with_padding() {
        assert_eq!(
            bgra(&[0; 24], 2, 2, 12, ColorMode::default()).unwrap(),
            [16, 16, 16, 16, 128, 128]
        );
        assert_eq!(
            bgra(&[255; 24], 2, 2, 12, ColorMode::default()).unwrap(),
            [235, 235, 235, 235, 128, 128]
        );
        assert_eq!(
            nv12(&[1, 2, 9, 9, 3, 4, 9, 9, 5, 6, 9, 9], 2, 2, 4).unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
    }
    #[test]
    fn malformed_layouts_are_rejected() {
        assert!(bgra(&[], 2, 2, 8, ColorMode::default()).is_err());
        assert!(bgra(&[0; 16], 1, 2, 8, ColorMode::default()).is_err());
        assert!(nv12(&[0; 6], 2, 2, usize::MAX).is_err());
        assert!(nv12(&[0; 5], 2, 2, 2).is_err());
    }

    #[test]
    fn red_pixel_conversion_uses_requested_matrix_and_range() {
        let red = [0, 0, 255, 255].repeat(4);
        let expected = [
            (ColorMode::Bt601Limited, [81, 81, 81, 81, 90, 240]),
            (ColorMode::Bt601Full, [76, 76, 76, 76, 85, 255]),
            (ColorMode::Bt709Limited, [63, 63, 63, 63, 103, 240]),
            (ColorMode::Bt709Full, [54, 54, 54, 54, 99, 255]),
        ];
        for (mode, expected) in expected {
            assert_eq!(bgra(&red, 2, 2, 8, mode).unwrap(), expected);
        }
    }

    #[test]
    fn nondefault_mode_is_rejected_for_raw_nv12() {
        assert!(input_color_supported(PIXEL_NV12, ColorMode::default()));
        for mode in [
            ColorMode::Bt601Limited,
            ColorMode::Bt601Full,
            ColorMode::Bt709Full,
        ] {
            assert!(!input_color_supported(PIXEL_NV12, mode));
        }
        assert!(input_color_supported(
            amaterasu_media::PIXEL_BGRA8,
            ColorMode::Bt709Full
        ));
    }
}

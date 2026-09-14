//! Packed CPU frames use limited-range BT.709 YUV; alpha is ignored.

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
) -> Result<Vec<u8>, &'static str> {
    validate(
        data,
        width,
        height,
        stride,
        width.checked_mul(4).ok_or("invalid_frame")?,
        height,
    )?;
    let mut packed = vec![0; width * height * 3 / 2];
    for y in 0..height {
        for x in 0..width {
            let pixel = &data[y * stride + x * 4..][..4];
            packed[y * width + x] = luma(pixel);
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
            packed[offset] =
                ((-26 * red - 87 * green + 112 * blue + 512) / 1024 + 128).clamp(16, 240) as u8;
            packed[offset + 1] =
                ((112 * red - 102 * green - 10 * blue + 512) / 1024 + 128).clamp(16, 240) as u8;
        }
    }
    Ok(packed)
}

fn luma(pixel: &[u8]) -> u8 {
    ((47 * i32::from(pixel[2]) + 157 * i32::from(pixel[1]) + 16 * i32::from(pixel[0]) + 128) / 256
        + 16)
        .clamp(16, 235) as u8
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
            bgra(&[0; 24], 2, 2, 12).unwrap(),
            [16, 16, 16, 16, 128, 128]
        );
        assert_eq!(
            bgra(&[255; 24], 2, 2, 12).unwrap(),
            [235, 235, 235, 235, 128, 128]
        );
        assert_eq!(
            nv12(&[1, 2, 9, 9, 3, 4, 9, 9, 5, 6, 9, 9], 2, 2, 4).unwrap(),
            [1, 2, 3, 4, 5, 6]
        );
    }
    #[test]
    fn malformed_layouts_are_rejected() {
        assert!(bgra(&[], 2, 2, 8).is_err());
        assert!(bgra(&[0; 16], 1, 2, 8).is_err());
        assert!(nv12(&[0; 6], 2, 2, usize::MAX).is_err());
        assert!(nv12(&[0; 5], 2, 2, 2).is_err());
    }
}

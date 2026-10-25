//! Fit a complete portal image into the stream without changing desktop layout.
use super::OwnedFrame;
use amaterasu_media::{CpuFrame, PIXEL_BGRA8};
use amaterasu_module_sdk::abi::{STATUS_INVALID_ARGUMENT, Status};

pub(super) fn fit_bgra(
    source: &CpuFrame,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<OwnedFrame, Status> {
    if source.validate().is_err()
        || source.format != PIXEL_BGRA8
        || width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || pixels.len() != source.len
    {
        return Err(STATUS_INVALID_ARGUMENT);
    }
    let (fit_width, fit_height) = if u64::from(width) * u64::from(source.height)
        <= u64::from(height) * u64::from(source.width)
    {
        (
            width,
            (u64::from(width) * u64::from(source.height) / u64::from(source.width)).max(1) as u32,
        )
    } else {
        (
            (u64::from(height) * u64::from(source.width) / u64::from(source.height)).max(1) as u32,
            height,
        )
    };
    let left = (width - fit_width) / 2;
    let top = (height - fit_height) / 2;
    let mut bytes = Vec::new();
    let length = width as usize * height as usize * 4;
    bytes
        .try_reserve_exact(length)
        .map_err(|_| STATUS_INVALID_ARGUMENT)?;
    bytes.resize(length, 0);
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }
    let horizontal: Vec<_> = (0..fit_width)
        .map(|x| sample(x, fit_width, source.width))
        .collect();
    for y in 0..fit_height {
        let (first_y, second_y, y_weight) = sample(y, fit_height, source.height);
        for (x, &(first_x, second_x, x_weight)) in horizontal.iter().enumerate() {
            let first = first_y as usize * source.stride as usize;
            let second = second_y as usize * source.stride as usize;
            let destination = ((top + y) as usize * width as usize + left as usize + x) * 4;
            for channel in 0..3 {
                let upper = blend(
                    pixels[first + first_x as usize * 4 + channel],
                    pixels[first + second_x as usize * 4 + channel],
                    x_weight,
                );
                let lower = blend(
                    pixels[second + first_x as usize * 4 + channel],
                    pixels[second + second_x as usize * 4 + channel],
                    x_weight,
                );
                bytes[destination + channel] = blend(upper, lower, y_weight);
            }
        }
    }
    Ok(OwnedFrame {
        width,
        height,
        format: PIXEL_BGRA8,
        stride: width * 4,
        bytes,
    })
}

fn sample(position: u32, destination: u32, source: u32) -> (u32, u32, u32) {
    let fixed = if destination == 1 {
        u64::from(source - 1) * 128
    } else {
        u64::from(position) * u64::from(source - 1) * 256 / u64::from(destination - 1)
    };
    let first = (fixed / 256) as u32;
    (first, (first + 1).min(source - 1), (fixed % 256) as u32)
}

fn blend(first: u8, second: u8, weight: u32) -> u8 {
    ((u32::from(first) * (256 - weight) + u32::from(second) * weight + 128) / 256) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_all_four_corners_and_ignores_source_row_padding() {
        let mut pixels = vec![99; 6 * 4 * 4];
        for (x, y, color) in [(0, 0, 10), (3, 0, 20), (0, 3, 30), (3, 3, 40)] {
            pixels[y * 24 + x * 4..y * 24 + x * 4 + 4].copy_from_slice(&[color, color, color, 255]);
        }
        let frame = CpuFrame {
            width: 4,
            height: 4,
            format: PIXEL_BGRA8,
            stride: 24,
            data: pixels.as_ptr(),
            len: pixels.len(),
            timestamp_us: 0,
            sequence: 0,
        };
        let resized = fit_bgra(&frame, &pixels, 2, 2).unwrap();
        assert_eq!(
            resized.bytes,
            [
                10, 10, 10, 255, 20, 20, 20, 255, 30, 30, 30, 255, 40, 40, 40, 255
            ]
        );
    }

    #[test]
    fn centers_the_complete_wide_desktop_with_black_bars() {
        let pixels = vec![255; 4 * 2 * 4];
        let frame = CpuFrame {
            width: 4,
            height: 2,
            format: PIXEL_BGRA8,
            stride: 16,
            data: pixels.as_ptr(),
            len: pixels.len(),
            timestamp_us: 0,
            sequence: 0,
        };
        let resized = fit_bgra(&frame, &pixels, 4, 4).unwrap();
        for y in 0..4 {
            for pixel in resized.bytes[y * 16..(y + 1) * 16].as_chunks::<4>().0 {
                assert_eq!(
                    pixel,
                    if y == 0 || y == 3 {
                        &[0, 0, 0, 255]
                    } else {
                        &[255; 4]
                    }
                );
            }
        }
        assert!(fit_bgra(&frame, &pixels[..4], 4, 4).is_err());
        assert!(fit_bgra(&frame, &pixels, 0, 4).is_err());
    }
}

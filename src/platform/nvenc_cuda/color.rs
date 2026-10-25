//! Colour signalling and the GPU RGB -> Y'CbCr matrix for each SDR mode.
//!
//! The GL shader evaluates `dot(row.rgb, rgb) + row.a` on normalized,
//! gamma-encoded RGB and stores the result as an 8-bit unorm, which rounds to
//! nearest. These rows are therefore the whole conversion; the tests below
//! replay that arithmetic in f32 and compare it with the CPU reference
//! converter (`layout::bgra`) that every other backend uses.
use super::codec::Codec;
use amaterasu_media::ColorMode;

/// H.273 code points written to H.264/HEVC VUI or the AV1 colour config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorSignal {
    pub primaries: u32,
    pub transfer: u32,
    pub matrix: u32,
    pub full_range: bool,
}

const BT709: u32 = 1;
const SMPTE170M: u32 = 6;

pub fn signal(mode: ColorMode) -> ColorSignal {
    let (code, full_range) = match mode {
        ColorMode::Bt601Limited => (SMPTE170M, false),
        ColorMode::Bt601Full => (SMPTE170M, true),
        ColorMode::Bt709Limited => (BT709, false),
        ColorMode::Bt709Full => (BT709, true),
    };
    ColorSignal {
        primaries: code,
        transfer: code,
        matrix: code,
        full_range,
    }
}

/// The shader averages each 2x2 block, so chroma is centre sited. H.264 and
/// HEVC signal that as chroma_sample_loc_type 1. AV1 cannot express centre
/// siting and signals CSP_UNKNOWN (0) instead of a wrong position.
pub fn chroma_sample_location(codec: Codec) -> u32 {
    match codec {
        Codec::H264 | Codec::Hevc => 1,
        Codec::Av1 => 0,
    }
}

/// `{r, g, b, offset}` rows producing code value / 255.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConversionRows {
    pub luma: [f32; 4],
    pub cb: [f32; 4],
    pub cr: [f32; 4],
}

pub fn conversion_rows(mode: ColorMode) -> ConversionRows {
    let (red_weight, blue_weight) = match mode {
        ColorMode::Bt601Limited | ColorMode::Bt601Full => (0.299, 0.114),
        ColorMode::Bt709Limited | ColorMode::Bt709Full => (0.2126, 0.0722),
    };
    let green_weight = 1.0 - red_weight - blue_weight;
    // Limited range maps Y' to 16..235 and Cb/Cr to 16..240 of 255; full
    // range uses the whole code range with chroma centred on 128.
    let (luma_scale, luma_offset, chroma_scale) = match mode {
        ColorMode::Bt601Limited | ColorMode::Bt709Limited => {
            (219.0 / 255.0, 16.0 / 255.0, 224.0 / 255.0)
        }
        ColorMode::Bt601Full | ColorMode::Bt709Full => (1.0, 0.0, 1.0),
    };
    let chroma_offset = 128.0 / 255.0;
    let blue_difference = 2.0 * (1.0 - blue_weight);
    let red_difference = 2.0 * (1.0 - red_weight);
    let row = |weights: [f64; 3], scale: f64, offset: f64| {
        [
            (weights[0] * scale) as f32,
            (weights[1] * scale) as f32,
            (weights[2] * scale) as f32,
            offset as f32,
        ]
    };
    ConversionRows {
        luma: row(
            [red_weight, green_weight, blue_weight],
            luma_scale,
            luma_offset,
        ),
        cb: row(
            [
                -red_weight / blue_difference,
                -green_weight / blue_difference,
                0.5,
            ],
            chroma_scale,
            chroma_offset,
        ),
        cr: row(
            [
                0.5,
                -green_weight / red_difference,
                -blue_weight / red_difference,
            ],
            chroma_scale,
            chroma_offset,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout;

    const MODES: [ColorMode; 4] = [
        ColorMode::Bt601Limited,
        ColorMode::Bt601Full,
        ColorMode::Bt709Limited,
        ColorMode::Bt709Full,
    ];

    /// The shader's arithmetic: f32 dot product, clamp, round to unorm8.
    fn shade(row: [f32; 4], rgb: [f32; 3]) -> u8 {
        let value = row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2] + row[3];
        (value.clamp(0.0, 1.0) * 255.0).round() as u8
    }

    fn normalized(pixel: [u8; 3]) -> [f32; 3] {
        pixel.map(|channel| f32::from(channel) / 255.0)
    }

    /// GPU model of one 2x2 block given as BGR pixels: (four Y, Cb, Cr).
    fn gpu_block(rows: &ConversionRows, block: [[u8; 3]; 4]) -> ([u8; 4], u8, u8) {
        let rgb = block.map(|[blue, green, red]| normalized([red, green, blue]));
        let luma = rgb.map(|pixel| shade(rows.luma, pixel));
        let mut average = [0.0f32; 3];
        for pixel in rgb {
            for channel in 0..3 {
                average[channel] += pixel[channel];
            }
        }
        let average = average.map(|sum| sum * 0.25);
        (luma, shade(rows.cb, average), shade(rows.cr, average))
    }

    fn cpu_block(mode: ColorMode, block: [[u8; 3]; 4]) -> ([u8; 4], u8, u8) {
        let mut bgra = Vec::with_capacity(16);
        for [blue, green, red] in block {
            bgra.extend_from_slice(&[blue, green, red, 255]);
        }
        let nv12 = layout::bgra(&bgra, 2, 2, 8, mode).unwrap();
        ([nv12[0], nv12[1], nv12[2], nv12[3]], nv12[4], nv12[5])
    }

    /// Exact real-valued conversion of one block, rounded to code values.
    fn exact_block(mode: ColorMode, block: [[u8; 3]; 4]) -> ([u8; 4], u8, u8) {
        let (red_weight, blue_weight) = match mode {
            ColorMode::Bt601Limited | ColorMode::Bt601Full => (0.299, 0.114),
            ColorMode::Bt709Limited | ColorMode::Bt709Full => (0.2126, 0.0722),
        };
        let green_weight = 1.0 - red_weight - blue_weight;
        let limited = matches!(mode, ColorMode::Bt601Limited | ColorMode::Bt709Limited);
        let (luma_scale, luma_offset, chroma_scale) = if limited {
            (219.0, 16.0, 224.0)
        } else {
            (255.0, 0.0, 255.0)
        };
        let luma_of = |[blue, green, red]: [u8; 3]| {
            let [red, green, blue] = [red, green, blue].map(|value| f64::from(value) / 255.0);
            red_weight * red + green_weight * green + blue_weight * blue
        };
        let luma = block.map(|pixel| (luma_of(pixel) * luma_scale + luma_offset).round() as u8);
        let mean = |channel: usize| {
            block
                .iter()
                .map(|pixel| f64::from(pixel[channel]))
                .sum::<f64>()
                / 1020.0
        };
        let (blue, green, red) = (mean(0), mean(1), mean(2));
        let average_luma = red_weight * red + green_weight * green + blue_weight * blue;
        let cb = (blue - average_luma) / (2.0 * (1.0 - blue_weight));
        let cr = (red - average_luma) / (2.0 * (1.0 - red_weight));
        let chroma = |difference: f64| {
            (128.0 + chroma_scale * difference)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        (luma, chroma(cb), chroma(cr))
    }

    fn assert_within(
        label: &str,
        mode: ColorMode,
        block: [[u8; 3]; 4],
        gpu: ([u8; 4], u8, u8),
        reference: ([u8; 4], u8, u8),
        tolerance: u8,
    ) {
        for (gpu, reference) in gpu.0.iter().zip(reference.0) {
            assert!(
                gpu.abs_diff(reference) <= tolerance,
                "{label} {mode:?} {block:?} luma {gpu} vs {reference}"
            );
        }
        assert!(
            gpu.1.abs_diff(reference.1) <= tolerance,
            "{label} {mode:?} {block:?} cb {} vs {}",
            gpu.1,
            reference.1
        );
        assert!(
            gpu.2.abs_diff(reference.2) <= tolerance,
            "{label} {mode:?} {block:?} cr {} vs {}",
            gpu.2,
            reference.2
        );
    }

    /// The f32 shader matches exact arithmetic to one code value. The CPU
    /// converter's 16-bit matrices agree to one code value too; its legacy
    /// 8-bit BT.709-limited matrix truncates coefficients and rounds
    /// negative chroma upward, which costs up to two code values.
    fn assert_close(mode: ColorMode, block: [[u8; 3]; 4]) {
        let gpu = gpu_block(&conversion_rows(mode), block);
        assert_within("exact", mode, block, gpu, exact_block(mode, block), 1);
        let cpu_tolerance = if mode == ColorMode::Bt709Limited {
            2
        } else {
            1
        };
        assert_within(
            "cpu",
            mode,
            block,
            gpu,
            cpu_block(mode, block),
            cpu_tolerance,
        );
    }

    #[test]
    fn gpu_rows_match_the_cpu_reference_for_solid_colours() {
        let levels = [0u8, 1, 16, 64, 127, 128, 200, 235, 240, 254, 255];
        for mode in MODES {
            for red in levels {
                for green in levels {
                    for blue in levels {
                        assert_close(mode, [[blue, green, red]; 4]);
                    }
                }
            }
        }
    }

    #[test]
    fn gpu_chroma_box_average_matches_the_cpu_reference_for_mixed_blocks() {
        let mut state = 0x2545_f491_u32;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state & 0xff) as u8
        };
        for mode in MODES {
            for _ in 0..4096 {
                let block = std::array::from_fn(|_| [next(), next(), next()]);
                assert_close(mode, block);
            }
        }
    }

    #[test]
    fn reference_colours_land_on_their_nominal_code_values() {
        let limited = conversion_rows(ColorMode::Bt709Limited);
        assert_eq!(shade(limited.luma, [0.0; 3]), 16);
        assert_eq!(shade(limited.luma, [1.0; 3]), 235);
        assert_eq!(shade(limited.cb, [1.0; 3]), 128);
        assert_eq!(shade(limited.cb, [0.0, 0.0, 1.0]), 240);
        assert_eq!(shade(limited.cr, [1.0, 0.0, 0.0]), 240);
        let full = conversion_rows(ColorMode::Bt601Full);
        assert_eq!(shade(full.luma, [0.0; 3]), 0);
        assert_eq!(shade(full.luma, [1.0; 3]), 255);
        assert_eq!(shade(full.cr, [0.5; 3]), 128);
    }

    #[test]
    fn signalling_matches_the_selected_matrix_and_range() {
        assert_eq!(
            signal(ColorMode::Bt709Limited),
            ColorSignal {
                primaries: 1,
                transfer: 1,
                matrix: 1,
                full_range: false
            }
        );
        assert_eq!(
            signal(ColorMode::Bt601Full),
            ColorSignal {
                primaries: 6,
                transfer: 6,
                matrix: 6,
                full_range: true
            }
        );
        assert!(!signal(ColorMode::Bt601Limited).full_range);
        assert!(signal(ColorMode::Bt709Full).full_range);
        assert_eq!(chroma_sample_location(Codec::H264), 1);
        assert_eq!(chroma_sample_location(Codec::Hevc), 1);
        assert_eq!(chroma_sample_location(Codec::Av1), 0);
    }
}

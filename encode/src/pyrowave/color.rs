use amaterasu_media::ColorMode;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

#[derive(Clone, Copy)]
pub(super) struct OutputColor {
    matrix: Matrix,
    full_range: bool,
}

impl OutputColor {
    pub fn new(hdr: bool, mode: Option<ColorMode>) -> Self {
        let mode = mode.unwrap_or(ColorMode::Bt709Full);
        let matrix = match (hdr, mode) {
            (true, _) => Matrix::Bt2020,
            (false, ColorMode::Bt601Full | ColorMode::Bt601Limited) => Matrix::Bt601,
            (false, _) => Matrix::Bt709,
        };
        Self {
            matrix,
            full_range: matches!(mode, ColorMode::Bt601Full | ColorMode::Bt709Full),
        }
    }

    pub fn accepts_full_range_bt709_nv12(self) -> bool {
        self.matrix == Matrix::Bt709 && self.full_range
    }

    /// Affine RGB to Y'CbCr rows in the scaler's Cr, Y, Cb order.
    pub fn coefficients(self, ten_bit: bool) -> [[f32; 4]; 3] {
        let (kr, kb) = match self.matrix {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1.0 - kr - kb;
        let (code_scale, maximum) = if ten_bit { (4.0, 1023.0) } else { (1.0, 255.0) };
        let (luma_gain, luma_offset, chroma_gain) = if self.full_range {
            (1.0, 0.0, 1.0)
        } else {
            (
                219.0 * code_scale / maximum,
                16.0 * code_scale / maximum,
                224.0 * code_scale / maximum,
            )
        };
        let midpoint = 128.0 * code_scale / maximum;
        let cb_scale = chroma_gain / (2.0 * (1.0 - kb));
        let cr_scale = chroma_gain / (2.0 * (1.0 - kr));
        [
            [0.5 * chroma_gain, -kg * cr_scale, -kb * cr_scale, midpoint],
            [kr * luma_gain, kg * luma_gain, kb * luma_gain, luma_offset],
            [-kr * cb_scale, -kg * cb_scale, 0.5 * chroma_gain, midpoint],
        ]
    }

    /// PyroWave defines BT.709 and BT.2020 fields. The GameStream CSC
    /// negotiation remains authoritative for BT.601, which has no field value.
    pub fn sequence_bits(self) -> u32 {
        let hdr = u32::from(self.matrix == Matrix::Bt2020);
        (hdr * (7 << 27)) | (u32::from(!self.full_range) << 30)
    }
}

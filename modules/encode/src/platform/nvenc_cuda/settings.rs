//! Configuration validation for the native NVENC backend and translation to
//! the adapter's settings record.
use super::{
    codec::Codec,
    color::{chroma_sample_location, conversion_rows, signal},
};
use crate::model::Configuration;

/// Mirrors `struct am_nvcuda_settings` in native/adapter.h.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeSettings {
    pub codec: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub reference_frames: u32,
    pub full_range: u32,
    pub color_primaries: u32,
    pub transfer_characteristics: u32,
    pub matrix_coefficients: u32,
    pub chroma_sample_location: u32,
    pub luma_row: [f32; 4],
    pub cb_row: [f32; 4],
    pub cr_row: [f32; 4],
}

/// The generic bounds and unsupported-option checks apply first. The codec
/// must be the one this published identity encodes; colour modes are all
/// supported because conversion happens in this backend's shader.
pub fn validate(configuration: &Configuration, codec: Codec) -> Result<(), &'static str> {
    configuration.validate()?;
    if codec == Codec::H264 && configuration.max_reference_frames > 1 {
        return Err("nvenc_reference_frames_unsupported");
    }
    if Codec::from_video(configuration.codec()) != codec {
        return Err("nvenc_cuda_codec_mismatch");
    }
    Ok(())
}

/// Moonlight's constrained H.264 clients accept one reference picture, as
/// the FFmpeg NVENC backend also enforces, so H.264 always bounds its DPB to
/// one frame. HEVC and AV1 honour the request; zero leaves the driver default.
pub fn reference_frames(configuration: &Configuration, codec: Codec) -> u32 {
    match codec {
        Codec::H264 => 1,
        Codec::Hevc | Codec::Av1 => configuration.max_reference_frames,
    }
}

pub fn native_settings(configuration: &Configuration, codec: Codec) -> NativeSettings {
    let color = signal(configuration.color_mode);
    let rows = conversion_rows(configuration.color_mode);
    NativeSettings {
        codec: codec.native_id(),
        width: configuration.width,
        height: configuration.height,
        fps: configuration.fps,
        bitrate: configuration.bitrate,
        reference_frames: reference_frames(configuration, codec),
        full_range: u32::from(color.full_range),
        color_primaries: color.primaries,
        transfer_characteristics: color.transfer,
        matrix_coefficients: color.matrix,
        chroma_sample_location: chroma_sample_location(codec),
        luma_row: rows.luma,
        cb_row: rows.cb,
        cr_row: rows.cr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_media::{ColorMode, VideoCodec};

    fn configuration(codec: VideoCodec) -> Configuration {
        serde_json::from_value(serde_json::json!({
            "width": 1920, "height": 1080, "fps": 60, "bitrate": 20_000_000, "codec": codec
        }))
        .unwrap()
    }

    #[test]
    fn each_identity_accepts_only_its_own_codec() {
        for codec in Codec::ALL {
            for requested in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
                let result = validate(&configuration(requested), codec);
                if Codec::from_video(requested) == codec {
                    assert_eq!(result, Ok(()), "{codec:?}");
                } else {
                    assert_eq!(result, Err("nvenc_cuda_codec_mismatch"), "{codec:?}");
                }
            }
        }
    }

    #[test]
    fn generic_limits_and_unsupported_options_still_apply() {
        let mut too_wide = configuration(VideoCodec::Av1);
        too_wide.width = 4096;
        assert_eq!(
            validate(&too_wide, Codec::Av1),
            Err("invalid_configuration")
        );
        let mut references = configuration(VideoCodec::Hevc);
        references.max_reference_frames = 17;
        assert_eq!(
            validate(&references, Codec::Hevc),
            Err("invalid_configuration")
        );
        let mut preset = configuration(VideoCodec::H264);
        preset.nvenc_preset = Some(crate::model::NvencPreset::P4);
        assert_eq!(
            validate(&preset, Codec::H264),
            Err("nvenc_preset_unsupported")
        );
        let mut aq = configuration(VideoCodec::H264);
        aq.nvenc_spatial_aq = true;
        assert_eq!(
            validate(&aq, Codec::H264),
            Err("nvenc_spatial_aq_unsupported")
        );
    }

    #[test]
    fn every_colour_mode_is_accepted() {
        for mode in [
            ColorMode::Bt601Limited,
            ColorMode::Bt601Full,
            ColorMode::Bt709Limited,
            ColorMode::Bt709Full,
        ] {
            let mut config = configuration(VideoCodec::Hevc);
            config.color_mode = mode;
            assert_eq!(validate(&config, Codec::Hevc), Ok(()));
        }
    }

    #[test]
    fn reference_limits_follow_the_moonlight_constraints() {
        for requested in [0, 1] {
            let mut config = configuration(VideoCodec::H264);
            config.max_reference_frames = requested;
            assert_eq!(reference_frames(&config, Codec::H264), 1);
            assert_eq!(reference_frames(&config, Codec::Hevc), requested);
            assert_eq!(reference_frames(&config, Codec::Av1), requested);
        }
    }

    #[test]
    fn native_settings_carry_signalling_and_matching_matrix() {
        let mut config = configuration(VideoCodec::Av1);
        config.color_mode = ColorMode::Bt601Full;
        config.max_reference_frames = 1;
        let settings = native_settings(&config, Codec::Av1);
        assert_eq!(settings.codec, amaterasu_media::CODEC_AV1);
        assert_eq!((settings.width, settings.height), (1920, 1080));
        assert_eq!((settings.fps, settings.bitrate), (60, 20_000_000));
        assert_eq!(settings.reference_frames, 1);
        assert_eq!(settings.full_range, 1);
        assert_eq!(
            (
                settings.color_primaries,
                settings.transfer_characteristics,
                settings.matrix_coefficients
            ),
            (6, 6, 6)
        );
        assert_eq!(settings.chroma_sample_location, 0);
        let rows = conversion_rows(ColorMode::Bt601Full);
        assert_eq!(settings.luma_row, rows.luma);
        assert_eq!(settings.cb_row, rows.cb);
        assert_eq!(settings.cr_row, rows.cr);
    }
}

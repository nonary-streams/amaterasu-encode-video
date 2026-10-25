use amaterasu_media::{ColorMode, VideoCodec};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AmdAv1LatencyMode {
    #[default]
    Auto,
    None,
    PowerSaving,
    Realtime,
    Lowest,
}

impl AmdAv1LatencyMode {
    fn is_auto(mode: &Self) -> bool {
        *mode == Self::Auto
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    /// Codec and range accepted by the authenticated RTSP ANNOUNCE.
    #[serde(
        default = "default_video_stream_format",
        skip_serializing_if = "is_default_video_stream_format"
    )]
    pub video_stream_format: amaterasu_module_sdk::VideoStreamFormat,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// Client-requested H.264 slices per frame from Moonlight's ANNOUNCE.
    #[serde(
        default = "default_slices_per_frame",
        skip_serializing_if = "is_default_slices_per_frame"
    )]
    pub slices_per_frame: i32,
    /// Minimum software slice count requested by host settings.
    #[serde(
        default = "default_min_threads",
        skip_serializing_if = "is_default_min_threads"
    )]
    pub min_threads: i32,
    /// QP is retained for future CQP backends; x264 ABR does not consume it.
    #[serde(default = "default_qp", skip_serializing_if = "is_default_qp")]
    pub qp: i32,
    /// Software x264 preset/tune; hardware backends leave these unapplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sw_preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sw_tune: Option<String>,
    /// Zero leaves H.264 reference-frame selection to the encoder; a positive
    /// value requests the exact maximum in the stream's H.264 SPS.
    /// Bitstream family. Old callers omit this and continue to receive H.264.
    #[serde(default, skip_serializing_if = "is_h264")]
    pub codec: VideoCodec,
    /// SDR Y'CbCr interpretation used by the input pixels and encoded stream.
    #[serde(default, skip_serializing_if = "ColorMode::is_default")]
    pub color_mode: ColorMode,
    /// Zero leaves reference-frame selection to the encoder; one requires a
    /// single H.264 reference frame.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_reference_frames: u32,
    /// Frozen host hardware-encoder controls resolved before stream activation.
    #[serde(default, skip_serializing_if = "is_default_nvenc_settings")]
    pub nvenc_settings: amaterasu_module_sdk::NvencSettings,
    /// NVENC quality preset. This MFT-only backend does not support NVENC presets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvenc_preset: Option<NvencPreset>,
    /// NVENC H.264 CAVLC selection. The MFT-only backend does not support NVENC.
    #[serde(default, skip_serializing_if = "is_false")]
    pub nvenc_h264_cavlc: bool,
    /// NVENC spatial adaptive quantization. The MFT-only backend does not support NVENC.
    #[serde(default, skip_serializing_if = "is_false")]
    pub nvenc_spatial_aq: bool,
    /// AMD AV1 latency policy. The current H.264 MFT backend cannot apply it.
    #[serde(default, skip_serializing_if = "AmdAv1LatencyMode::is_auto")]
    pub amd_av1_latency_mode: AmdAv1LatencyMode,
}

fn default_video_stream_format() -> amaterasu_module_sdk::VideoStreamFormat {
    amaterasu_module_sdk::VideoStreamFormat::H264_SDR
}

fn is_default_video_stream_format(format: &amaterasu_module_sdk::VideoStreamFormat) -> bool {
    *format == amaterasu_module_sdk::VideoStreamFormat::H264_SDR
}

fn is_default_nvenc_settings(settings: &amaterasu_module_sdk::NvencSettings) -> bool {
    settings == &amaterasu_module_sdk::NvencSettings::default()
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[repr(u8)]
#[serde(try_from = "u8", into = "u8")]
pub enum NvencPreset {
    P1 = 1,
    P2 = 2,
    P3 = 3,
    P4 = 4,
    P5 = 5,
    P6 = 6,
    P7 = 7,
}

impl TryFrom<u8> for NvencPreset {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::P1),
            2 => Ok(Self::P2),
            3 => Ok(Self::P3),
            4 => Ok(Self::P4),
            5 => Ok(Self::P5),
            6 => Ok(Self::P6),
            7 => Ok(Self::P7),
            _ => Err("nvenc_preset_out_of_range"),
        }
    }
}

impl From<NvencPreset> for u8 {
    fn from(value: NvencPreset) -> Self {
        value as u8
    }
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn default_slices_per_frame() -> i32 {
    1
}

fn is_default_slices_per_frame(value: &i32) -> bool {
    *value == default_slices_per_frame()
}

fn default_min_threads() -> i32 {
    2
}

fn is_default_min_threads(value: &i32) -> bool {
    *value == default_min_threads()
}

fn default_qp() -> i32 {
    28
}

fn is_default_qp(value: &i32) -> bool {
    *value == default_qp()
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_h264(codec: &VideoCodec) -> bool {
    *codec == VideoCodec::H264
}

impl Configuration {
    /// A negotiated format overrides the legacy codec field when it is present.
    pub fn codec(&self) -> VideoCodec {
        if self.video_stream_format == amaterasu_module_sdk::VideoStreamFormat::H264_SDR {
            return self.codec;
        }
        match self.video_stream_format.codec {
            amaterasu_module_sdk::VideoCodec::H264 => VideoCodec::H264,
            amaterasu_module_sdk::VideoCodec::Hevc => VideoCodec::Hevc,
            amaterasu_module_sdk::VideoCodec::Av1 => VideoCodec::Av1,
        }
    }

    pub fn software_preset(&self) -> &str {
        self.sw_preset.as_deref().unwrap_or("superfast")
    }
    pub fn software_tune(&self) -> &str {
        self.sw_tune.as_deref().unwrap_or("zerolatency")
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        self.video_stream_format.validate()?;
        if self.video_stream_format != amaterasu_module_sdk::VideoStreamFormat::H264_SDR
            && self.codec != VideoCodec::H264
            && self.codec != self.codec()
        {
            return Err("conflicting_video_codec");
        }
        if self.video_stream_format.bit_depth != 8
            || self.video_stream_format.dynamic_range != amaterasu_module_sdk::DynamicRange::Sdr
        {
            return Err("video_format_unsupported_by_encoder");
        }
        self.nvenc_settings.validate()?;
        // The merged backends still use fixed native policy. Preserve scoped
        // configuration, but never silently ignore an explicit override.
        if self.nvenc_settings != amaterasu_module_sdk::NvencSettings::default() {
            return Err("nvenc_settings_unsupported");
        }
        for (value, maximum) in [(self.software_preset(), 128), (self.software_tune(), 256)] {
            if value.len() > maximum || value.chars().any(char::is_control) {
                return Err("invalid_software_configuration");
            }
        }
        // Only Linux backends encode HEVC or AV1. Each Linux backend still
        // rejects any codec it does not implement when it opens.
        if self.codec() != VideoCodec::H264 && !cfg!(target_os = "linux") {
            return Err("codec_unsupported");
        }
        if cfg!(windows) && !self.color_mode.is_default() {
            return Err("color_mode_unsupported");
        }
        if self.nvenc_h264_cavlc {
            return Err("nvenc_h264_cavlc_unsupported");
        }
        if self.nvenc_spatial_aq {
            return Err("nvenc_spatial_aq_unsupported");
        }
        if self.amd_av1_latency_mode != AmdAv1LatencyMode::Auto {
            return Err("amd_av1_latency_mode_unsupported");
        }
        if self.nvenc_preset.is_some() {
            return Err("nvenc_preset_unsupported");
        }
        if !(16..=3840).contains(&self.width)
            || !(16..=2160).contains(&self.height)
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || !(1..=120).contains(&self.fps)
            || !(100_000..=100_000_000).contains(&self.bitrate)
            || self.slices_per_frame < 1
            || self.max_reference_frames > 16
        {
            return Err("invalid_configuration");
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Enumerate,
    Probe {
        encoder: EncoderIdentity,
        configuration: Configuration,
    },
    Open {
        #[serde(default)]
        packet_boundaries: bool,
        encoder: EncoderIdentity,
        configuration: Configuration,
        #[serde(default)]
        operation_id: Option<String>,
    },
    Lookup {
        operation_id: String,
    },
    Close {
        session_id: u64,
    },
    Status {
        session_id: u64,
    },
}

/// Stable identity for a registered hardware MFT.  Enumeration order is not
/// an authorization boundary: a caller must present this complete identity,
/// which is matched again immediately before activation.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EncoderIdentity {
    pub clsid: String,
    pub hardware_url: Option<String>,
    pub friendly_name: String,
}

impl EncoderIdentity {
    pub fn validate(&self) -> Result<(), &'static str> {
        let valid_text = |value: &str, maximum: usize| {
            !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
        };
        if !valid_text(&self.clsid, 128)
            || !valid_text(&self.friendly_name, 512)
            || self
                .hardware_url
                .as_deref()
                .is_some_and(|value| !valid_text(value, 2048))
        {
            return Err("invalid_encoder_identity");
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
pub struct EncoderDescription {
    pub identity: EncoderIdentity,
    pub name: String,
    pub codec: &'static str,
    pub hardware: bool,
    /// PCI vendor identifier published by the registered hardware MFT, when
    /// the MFT provides it. This is capability metadata, not a selected index.
    pub vendor_id: Option<u32>,
    /// Optional capability fields; omitted entirely when a backend has none,
    /// so earlier descriptions serialize unchanged.
    #[serde(flatten)]
    pub capabilities: DescriptionCapabilities,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DescriptionCapabilities {
    /// Accepted frame sources: `cpu` (CpuFrame) and `dmabuf` (DmaBufFrame).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
}

/// One complete coded picture: an H.264/HEVC Annex B access unit or an AV1
/// temporal unit. Backends must establish their output boundary before
/// handing this owned packet to the native session queue.
pub struct Packet {
    pub bytes: Vec<u8>,
    pub timestamp_100ns: i64,
    pub keyframe: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_options_preserve_defaults_raw_strings_and_legacy_shape() {
        let legacy = serde_json::json!({"width":16,"height":16,"fps":60,"bitrate":1000000});
        let configuration: Configuration = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(configuration.min_threads, 2);
        assert_eq!(configuration.software_preset(), "superfast");
        assert_eq!(configuration.software_tune(), "zerolatency");
        assert_eq!(serde_json::to_value(configuration).unwrap(), legacy);
        for (field, limit) in [("sw_preset", 128), ("sw_tune", 256)] {
            for value in ["".to_owned(), "film,zerolatency".into(), "x".repeat(limit)] {
                let mut request = legacy.clone();
                request[field] = serde_json::json!(value);
                let configuration: Configuration = serde_json::from_value(request.clone()).unwrap();
                assert!(configuration.validate().is_ok());
                assert_eq!(serde_json::to_value(configuration).unwrap(), request);
            }
            for value in [
                "bad\0option".into(),
                "bad\noption".into(),
                "x".repeat(limit + 1),
            ] {
                let mut request = legacy.clone();
                request[field] = serde_json::json!(value);
                assert_eq!(
                    serde_json::from_value::<Configuration>(request)
                        .unwrap()
                        .validate(),
                    Err("invalid_software_configuration")
                );
            }
            let mut request = legacy.clone();
            request[field] = serde_json::json!(1);
            assert!(serde_json::from_value::<Configuration>(request).is_err());
        }
    }

    #[test]
    fn description_capabilities_are_omitted_when_empty() {
        let mut description = EncoderDescription {
            identity: EncoderIdentity {
                clsid: "test".into(),
                hardware_url: None,
                friendly_name: "Test".into(),
            },
            name: "Test".into(),
            codec: "h264",
            hardware: false,
            vendor_id: None,
            capabilities: DescriptionCapabilities::default(),
        };
        let plain = serde_json::to_value(&description).unwrap();
        assert_eq!(
            plain.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["codec", "hardware", "identity", "name", "vendor_id"]
        );
        description.capabilities = DescriptionCapabilities {
            inputs: vec!["cpu", "dmabuf"],
            max_width: Some(4096),
            max_height: Some(2304),
        };
        let extended = serde_json::to_value(&description).unwrap();
        assert_eq!(extended["inputs"], serde_json::json!(["cpu", "dmabuf"]));
        assert_eq!(extended["max_width"], 4096);
        assert_eq!(extended["max_height"], 2304);
    }

    #[test]
    fn av1_and_hevc_are_validated_only_on_linux() {
        for codec in [VideoCodec::Hevc, VideoCodec::Av1] {
            let configuration: Configuration = serde_json::from_value(serde_json::json!({
                "width": 16, "height": 16, "fps": 60, "bitrate": 1_000_000, "codec": codec
            }))
            .unwrap();
            if cfg!(target_os = "linux") {
                assert!(configuration.validate().is_ok());
            } else {
                assert_eq!(configuration.validate(), Err("codec_unsupported"));
            }
        }
    }

    #[test]
    fn encoder_identity_is_bounded_and_nonempty() {
        let valid = EncoderIdentity {
            clsid: "{test}".into(),
            hardware_url: Some("mft://test".into()),
            friendly_name: "Test H.264".into(),
        };
        assert!(valid.validate().is_ok());
        let mut invalid = valid;
        invalid.friendly_name = "\n".into();
        assert_eq!(invalid.validate(), Err("invalid_encoder_identity"));
    }

    #[test]
    fn nvenc_preset_round_trips_numeric_values_and_rejects_out_of_range() {
        for value in 1..=7 {
            let preset: NvencPreset = serde_json::from_str(&value.to_string()).unwrap();
            assert_eq!(u8::from(preset), value);
            assert_eq!(serde_json::to_string(&preset).unwrap(), value.to_string());
        }

        for invalid in ["0", "8", "-1", "1.0", "\"1\""] {
            assert!(serde_json::from_str::<NvencPreset>(invalid).is_err());
        }
    }

    #[test]
    fn configuration_without_nvenc_preset_remains_compatible() {
        let configuration: Configuration =
            serde_json::from_str(r#"{"width":16,"height":16,"fps":60,"bitrate":1000000}"#).unwrap();

        assert!(configuration.nvenc_preset.is_none());
        assert_eq!(configuration.amd_av1_latency_mode, AmdAv1LatencyMode::Auto);
        assert!(!configuration.nvenc_h264_cavlc);
        assert!(!configuration.nvenc_spatial_aq);
        assert!(configuration.validate().is_ok());
        let serialized = serde_json::to_value(configuration).unwrap();
        assert!(serialized.get("nvenc_preset").is_none());
        assert!(serialized.get("nvenc_h264_cavlc").is_none());
        assert!(serialized.get("nvenc_spatial_aq").is_none());
        assert!(serialized.get("amd_av1_latency_mode").is_none());
        assert!(serialized.get("color_mode").is_none());
    }

    #[test]
    fn color_mode_defaults_to_709_limited_and_is_serialized_when_selected() {
        let configuration: Configuration =
            serde_json::from_str(r#"{"width":16,"height":16,"fps":60,"bitrate":1000000}"#).unwrap();
        assert_eq!(configuration.color_mode, ColorMode::Bt709Limited);
        assert!(
            serde_json::to_value(&configuration)
                .unwrap()
                .get("color_mode")
                .is_none()
        );

        let explicit: Configuration = serde_json::from_value(serde_json::json!({
            "width":16,
            "height":16,
            "fps":60,
            "bitrate":1_000_000,
            "color_mode":"bt601_limited"
        }))
        .unwrap();
        assert_eq!(explicit.color_mode, ColorMode::Bt601Limited);
        assert_eq!(
            serde_json::to_value(explicit).unwrap()["color_mode"],
            "bt601_limited"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_nondefault_color_mode() {
        let mut configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width":16,"height":16,"fps":60,"bitrate":1_000_000
        }))
        .unwrap();
        configuration.color_mode = ColorMode::Bt709Full;
        assert_eq!(configuration.validate(), Err("color_mode_unsupported"));
    }

    #[test]
    fn negotiated_video_format_defaults_to_h264_sdr_and_preserves_hevc() {
        let mut request = serde_json::json!({
            "width":16,"height":16,"fps":60,"bitrate":1000000
        });
        let legacy: Configuration = serde_json::from_value(request.clone()).unwrap();
        assert_eq!(
            legacy.video_stream_format,
            amaterasu_module_sdk::VideoStreamFormat::H264_SDR
        );
        assert!(
            serde_json::to_value(&legacy)
                .unwrap()
                .get("video_stream_format")
                .is_none()
        );
        assert!(legacy.validate().is_ok());

        request["video_stream_format"] = serde_json::json!({
            "codec":"hevc", "bit_depth":8, "dynamic_range":"sdr"
        });
        let hevc: Configuration = serde_json::from_value(request).unwrap();
        assert!(hevc.validate().is_ok());
    }

    #[test]
    fn encode_boundary_rejects_out_of_range_nvenc_settings() {
        for (nvenc_preset, nvenc_vbv_increase) in [(8, 0), (1, 401)] {
            let mut configuration: Configuration = serde_json::from_value(serde_json::json!({
                "width": 16,
                "height": 16,
                "fps": 60,
                "bitrate": 1_000_000
            }))
            .unwrap();
            configuration.nvenc_settings = amaterasu_module_sdk::NvencSettings {
                nvenc_preset,
                nvenc_vbv_increase,
                ..Default::default()
            };
            assert_eq!(configuration.validate(), Err("invalid_nvenc_settings"));
        }
    }

    #[test]
    fn nvenc_h264_cavlc_requires_a_json_boolean_and_rejects_enabled_mode() {
        let base = serde_json::json!({
            "width": 16,
            "height": 16,
            "fps": 60,
            "bitrate": 1_000_000
        });

        let omitted: Configuration = serde_json::from_value(base.clone()).unwrap();
        let mut disabled = base.clone();
        disabled["nvenc_h264_cavlc"] = serde_json::json!(false);
        let disabled: Configuration = serde_json::from_value(disabled).unwrap();
        assert!(!disabled.nvenc_h264_cavlc);
        assert!(disabled == omitted);
        assert!(disabled.validate().is_ok());
        assert!(
            serde_json::to_value(disabled)
                .unwrap()
                .get("nvenc_h264_cavlc")
                .is_none()
        );

        let mut enabled = base;
        enabled["nvenc_h264_cavlc"] = serde_json::json!(true);
        let enabled: Configuration = serde_json::from_value(enabled).unwrap();
        assert!(enabled.nvenc_h264_cavlc);
        assert_eq!(
            serde_json::to_value(&enabled).unwrap()["nvenc_h264_cavlc"],
            true
        );
        assert_eq!(enabled.validate(), Err("nvenc_h264_cavlc_unsupported"));

        for invalid in ["\"enabled\"", "\"disabled\"", "0", "1", "null"] {
            let value = format!(
                r#"{{"width":16,"height":16,"fps":60,"bitrate":1000000,"nvenc_h264_cavlc":{invalid}}}"#
            );
            assert!(
                serde_json::from_str::<Configuration>(&value).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn nvenc_spatial_aq_requires_a_json_boolean_and_rejects_enabled_mode() {
        let base = serde_json::json!({
            "width": 16,
            "height": 16,
            "fps": 60,
            "bitrate": 1_000_000
        });

        let omitted: Configuration = serde_json::from_value(base.clone()).unwrap();
        assert!(!omitted.nvenc_spatial_aq);
        assert!(omitted.validate().is_ok());
        assert!(
            serde_json::to_value(&omitted)
                .unwrap()
                .get("nvenc_spatial_aq")
                .is_none()
        );

        let mut disabled = base.clone();
        disabled["nvenc_spatial_aq"] = serde_json::json!(false);
        let disabled: Configuration = serde_json::from_value(disabled).unwrap();
        assert!(!disabled.nvenc_spatial_aq);
        assert!(disabled.validate().is_ok());
        assert!(
            serde_json::to_value(disabled)
                .unwrap()
                .get("nvenc_spatial_aq")
                .is_none()
        );

        let mut enabled = base;
        enabled["nvenc_spatial_aq"] = serde_json::json!(true);
        let enabled: Configuration = serde_json::from_value(enabled).unwrap();
        assert!(enabled.nvenc_spatial_aq);
        assert_eq!(
            serde_json::to_value(&enabled).unwrap()["nvenc_spatial_aq"],
            true
        );
        assert_eq!(enabled.validate(), Err("nvenc_spatial_aq_unsupported"));

        for invalid in ["\"enabled\"", "\"disabled\"", "0", "1", "null"] {
            let value = format!(
                r#"{{"width":16,"height":16,"fps":60,"bitrate":1000000,"nvenc_spatial_aq":{invalid}}}"#
            );
            assert!(
                serde_json::from_str::<Configuration>(&value).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn amd_av1_latency_mode_uses_only_pinned_tokens_and_omits_auto() {
        for token in ["auto", "none", "power_saving", "realtime", "lowest"] {
            let mode: AmdAv1LatencyMode = serde_json::from_str(&format!("\"{token}\"")).unwrap();
            assert_eq!(
                serde_json::to_string(&mode).unwrap(),
                format!("\"{token}\"")
            );

            let configuration: Configuration = serde_json::from_value(serde_json::json!({
                "width": 16,
                "height": 16,
                "fps": 60,
                "bitrate": 1_000_000,
                "amd_av1_latency_mode": token
            }))
            .unwrap();
            let serialized = serde_json::to_value(&configuration).unwrap();
            if token == "auto" {
                assert!(serialized.get("amd_av1_latency_mode").is_none());
                assert!(configuration.validate().is_ok());
            } else {
                assert_eq!(serialized["amd_av1_latency_mode"], token);
                assert_eq!(
                    configuration.validate(),
                    Err("amd_av1_latency_mode_unsupported")
                );
            }
        }
        for invalid in [
            "\"AUTO\"",
            "\"PowerSaving\"",
            "\"power-saving\"",
            "\"balanced\"",
        ] {
            assert!(serde_json::from_str::<AmdAv1LatencyMode>(invalid).is_err());
        }

        let defaulted: Configuration = serde_json::from_str(
            r#"{"width":16,"height":16,"fps":60,"bitrate":1000000,"amd_av1_latency_mode":"auto"}"#,
        )
        .unwrap();
        assert_eq!(defaulted.codec, VideoCodec::H264);
        assert!(defaulted.validate().is_ok());
        let encoded = serde_json::to_value(defaulted).unwrap();
        assert!(encoded.get("codec").is_none());
        assert!(encoded.get("amd_av1_latency_mode").is_none());
    }

    #[test]
    fn nvenc_preset_is_rejected_by_the_mft_only_configuration() {
        let configuration = Configuration {
            video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
            width: 16,
            height: 16,
            fps: 60,
            bitrate: 1_000_000,
            slices_per_frame: 1,
            min_threads: 2,
            qp: 28,
            codec: VideoCodec::H264,
            color_mode: ColorMode::default(),
            max_reference_frames: 0,
            nvenc_settings: amaterasu_module_sdk::NvencSettings::default(),
            nvenc_preset: Some(NvencPreset::P1),
            nvenc_h264_cavlc: false,
            nvenc_spatial_aq: false,
            amd_av1_latency_mode: AmdAv1LatencyMode::Auto,
            sw_preset: None,
            sw_tune: None,
        };

        assert_eq!(configuration.validate(), Err("nvenc_preset_unsupported"));
    }

    #[test]
    fn min_threads_preserves_the_reference_signed_c_int_domain() {
        let legacy = serde_json::json!({"width":16,"height":16,"fps":60,"bitrate":1000000});
        let defaulted: Configuration = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(defaulted.min_threads, 2);
        assert_eq!(serde_json::to_value(defaulted).unwrap(), legacy);

        for value in [i32::MIN, -1, 0, 1, 2, 256, i32::MAX] {
            let mut request = legacy.clone();
            request["min_threads"] = serde_json::json!(value);
            let configuration: Configuration = serde_json::from_value(request.clone()).unwrap();
            assert_eq!(configuration.min_threads, value);
            assert!(configuration.validate().is_ok());
            let serialized = serde_json::to_value(configuration).unwrap();
            if value == 2 {
                assert_eq!(serialized, legacy);
            } else {
                assert_eq!(serialized, request);
            }
        }

        for invalid in ["1.0", "\"2\"", "null"] {
            let request = format!(
                r#"{{"width":16,"height":16,"fps":60,"bitrate":1000000,"min_threads":{invalid}}}"#
            );
            assert!(
                serde_json::from_str::<Configuration>(&request).is_err(),
                "{invalid}"
            );
        }
        for value in [i64::from(i32::MIN) - 1, i64::from(i32::MAX) + 1] {
            let mut request = legacy.clone();
            request["min_threads"] = serde_json::json!(value);
            assert!(serde_json::from_value::<Configuration>(request).is_err());
        }
    }
}

#[cfg(test)]
mod merged_policy_tests {
    use super::*;

    #[test]
    fn explicit_native_policy_is_rejected_until_a_backend_can_apply_it() {
        let mut configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width":16,"height":16,"fps":60,"bitrate":1_000_000
        }))
        .unwrap();
        configuration.nvenc_settings.nvenc_preset = 7;
        assert_eq!(configuration.validate(), Err("nvenc_settings_unsupported"));
        configuration.nvenc_settings = Default::default();
        configuration.nvenc_settings.nvenc_twopass = amaterasu_module_sdk::NvencTwoPass::FullRes;
        assert_eq!(configuration.validate(), Err("nvenc_settings_unsupported"));
        configuration.nvenc_settings = Default::default();
        assert_eq!(configuration.validate(), Ok(()));
    }
}

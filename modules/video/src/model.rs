use amaterasu_media::{CAPTURE_API_V1_0_SIZE, ColorMode, VideoCodec};
#[cfg(test)]
use amaterasu_module_sdk::EncoderBackend;
pub use amaterasu_module_sdk::EncoderSelection;
use amaterasu_module_sdk::{CaptureBindPlan, CaptureSlot, EncoderBindPlan};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// The default is the platform's ordinary desktop capture provider.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CaptureProvider {
    #[cfg_attr(not(target_os = "linux"), default)]
    Dxgi,
    Wgc,
    #[cfg_attr(target_os = "linux", default)]
    Pipewire,
    /// DMA-BUF capture of owned KWin virtual outputs. Optional: when it is not
    /// bound or cannot open, owned outputs use PipeWire instead.
    Vkms,
    /// An explicitly selected XRandR output captured from the X11 root.
    X11,
    /// An explicitly selected physical KMS primary-plane framebuffer.
    Kms,
}

impl CaptureProvider {
    pub fn service(self) -> &'static str {
        match self {
            Self::Dxgi => "amaterasu.capture-dxgi/1",
            Self::Wgc => "amaterasu.capture-wgc/1",
            Self::Pipewire => "amaterasu.capture-pipewire/1",
            Self::Vkms => "amaterasu.capture-vkms/1",
            Self::X11 => "amaterasu.capture-x11/1",
            Self::Kms => "amaterasu.capture-kms/1",
        }
    }

    /// A provider whose native binding may be absent at startup. Streams that
    /// would use it fall back to PipeWire and record why.
    pub fn binding_is_optional(self) -> bool {
        self == Self::Vkms
    }

    /// Linux providers that read an owned or portal output at a fixed period
    /// and retain repeat deadlines independent of capture polling.
    pub fn uses_periodic_capture(self) -> bool {
        matches!(self, Self::Pipewire | Self::Vkms)
    }
}

/// Whether `output` names a KWin virtual output owned by the virtual-display
/// module: `kwin:Virtual-N`, where N is the DRM connector type id (positive,
/// without leading zeros). The id is not bounded to the first eight connectors
/// because reloading the DRM driver does not reset type ids.
pub fn is_owned_kwin_output(output: &str) -> bool {
    let Some(index) = output.strip_prefix("kwin:Virtual-") else {
        return false;
    };
    !index.is_empty()
        && index.len() <= 10
        && !index.starts_with('0')
        && index.bytes().all(|byte| byte.is_ascii_digit())
        && index.parse::<i32>().is_ok_and(|value| value > 0)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PortalSourceType {
    Monitor,
    Window,
}

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub enabled: bool,
    /// Metadata-only capture event source. The signed runtime grant must also
    /// allow publishing the exact video observation topic; default is off.
    #[serde(default)]
    pub capture_observation: bool,
    /// Separate opt-in metadata stream for accepted frames and encoded output.
    #[serde(default)]
    pub frame_observation: bool,
    /// Exact trusted native CPU processing service. None leaves the hot path
    /// unchanged; binding also requires an exact signed native-use grant.
    #[serde(default)]
    pub cpu_processor_service: Option<String>,
    /// Exact host-selected encoder provider. The broker must grant both its
    /// control calls and native interface; no fallback to the built-in encoder.
    #[serde(default)]
    pub encoder_service: Option<String>,
    /// Package-verifier-derived approved encoder replacement. The native
    /// broker still checks the selected table and effective grant at bind.
    #[serde(default)]
    pub encoder_plan: Option<EncoderBindPlan>,
    /// Elapsed budget checked after each synchronous native call. It cannot
    /// preempt a provider that does not return.
    #[serde(default = "default_cpu_processor_budget_us")]
    pub cpu_processor_budget_us: u32,
    #[serde(default = "default_providers")]
    pub capture_providers: BTreeSet<CaptureProvider>,
    /// Exact X11-slot replacement from trusted host-owned module settings.
    /// Native-use/call grants remain bounded by the signed component request.
    #[serde(default)]
    pub x11_capture_service: Option<String>,
    /// Package-verifier-derived approved replacement. NativeBind checks the
    /// live provider table and effective native grants before use.
    #[serde(default)]
    pub x11_capture_plan: Option<CaptureBindPlan>,
    #[serde(default = "default_drain_timeout")]
    pub drain_timeout_ms: u64,
}
fn default_providers() -> BTreeSet<CaptureProvider> {
    #[cfg(target_os = "linux")]
    {
        [CaptureProvider::Pipewire, CaptureProvider::Vkms].into()
    }
    #[cfg(not(target_os = "linux"))]
    {
        [CaptureProvider::Dxgi].into()
    }
}
fn default_drain_timeout() -> u64 {
    5000
}
fn default_cpu_processor_budget_us() -> u32 {
    2_000
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            capture_observation: false,
            frame_observation: false,
            cpu_processor_service: None,
            encoder_service: None,
            encoder_plan: None,
            cpu_processor_budget_us: default_cpu_processor_budget_us(),
            capture_providers: default_providers(),
            x11_capture_service: None,
            x11_capture_plan: None,
            drain_timeout_ms: default_drain_timeout(),
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.capture_providers.is_empty() || !(100..=30000).contains(&self.drain_timeout_ms) {
            return Err("invalid_video_settings");
        }
        if !(100..=5_000).contains(&self.cpu_processor_budget_us)
            || self
                .cpu_processor_service
                .as_deref()
                .is_some_and(|service| !valid_external_service(service))
        {
            return Err("invalid_cpu_processor_service");
        }
        if self
            .encoder_service
            .as_deref()
            .is_some_and(|service| !valid_external_service(service))
            || self.encoder_plan.as_ref().is_some_and(|plan| {
                self.encoder_service.is_some()
                    || !valid_external_service(&plan.service)
                    || plan.interface_major != 1
                    || plan.minimum_table_size == 0
                    || plan.minimum_table_size > 1_048_576
            })
        {
            return Err("invalid_encoder_service");
        }
        if let Some(service) = &self.x11_capture_service
            && (!cfg!(target_os = "linux")
                || !self.capture_providers.contains(&CaptureProvider::X11)
                || !valid_external_service(service))
        {
            return Err("invalid_x11_capture_service");
        }
        if let Some(plan) = &self.x11_capture_plan
            && (!cfg!(target_os = "linux")
                || !self.capture_providers.contains(&CaptureProvider::X11)
                || self.x11_capture_service.is_some()
                || plan.slot != CaptureSlot::X11
                || !valid_external_service(&plan.service)
                || plan.interface_major != 1
                || plan.minimum_table_size < CAPTURE_API_V1_0_SIZE
                || plan.minimum_table_size > 1_048_576)
        {
            return Err("invalid_x11_capture_plan");
        }
        Ok(())
    }

    pub fn capture_service(&self, provider: CaptureProvider) -> &str {
        if provider == CaptureProvider::X11 {
            self.x11_capture_plan
                .as_ref()
                .map(|plan| plan.service.as_str())
                .or(self.x11_capture_service.as_deref())
                .unwrap_or_else(|| provider.service())
        } else {
            provider.service()
        }
    }

    pub fn capture_binding(&self, provider: CaptureProvider) -> (&str, u32, u32, u32) {
        if provider == CaptureProvider::X11
            && let Some(plan) = &self.x11_capture_plan
        {
            return (
                &plan.service,
                plan.interface_major,
                plan.minimum_interface_minor,
                plan.minimum_table_size,
            );
        }
        (self.capture_service(provider), 1, 0, CAPTURE_API_V1_0_SIZE)
    }

    pub fn encoder_service(&self) -> &str {
        self.encoder_plan
            .as_ref()
            .map(|plan| plan.service.as_str())
            .or(self.encoder_service.as_deref())
            .unwrap_or("amaterasu.encode/1")
    }

    pub fn encoder_binding(&self) -> (&str, u32, u32, u32) {
        if let Some(plan) = &self.encoder_plan {
            return (
                &plan.service,
                plan.interface_major,
                plan.minimum_interface_minor,
                plan.minimum_table_size,
            );
        }
        (
            self.encoder_service(),
            1,
            0,
            std::mem::size_of::<amaterasu_media::EncoderApi>() as u32,
        )
    }
}

fn valid_external_service(service: &str) -> bool {
    let Some(namespace) = service.strip_suffix("/1") else {
        return false;
    };
    service.len() <= 128
        && namespace.contains('.')
        && !namespace.starts_with("amaterasu.")
        && namespace.split('.').all(|label| {
            !label.is_empty()
                && label.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                })
        })
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
    /// Minimum software slice count requested by the host settings.
    #[serde(
        default = "default_min_threads",
        skip_serializing_if = "is_default_min_threads"
    )]
    pub min_threads: i32,
    /// QP remains metadata until a CQP encoder consumes it.
    #[serde(default = "default_qp", skip_serializing_if = "is_default_qp")]
    pub qp: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sw_preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sw_tune: Option<String>,
    /// Bitstream family; omitted legacy values retain the H.264 default.
    #[serde(default, skip_serializing_if = "is_h264")]
    pub codec: VideoCodec,
    /// SDR Y'CbCr interpretation used by the encoder input and output bitstream.
    #[serde(default, skip_serializing_if = "ColorMode::is_default")]
    pub color_mode: ColorMode,
    /// Zero leaves reference-frame selection to the encoder; one requires a
    /// single H.264 reference frame.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_reference_frames: u32,
    /// Frozen host hardware-encoder controls resolved before stream activation.
    #[serde(default, skip_serializing_if = "is_default_nvenc_settings")]
    pub nvenc_settings: amaterasu_module_sdk::NvencSettings,
    #[serde(default)]
    pub nvenc_spatial_aq: bool,
    /// AMD AV1 latency policy. Current capture and H.264 MFT paths cannot apply it.
    #[serde(default, skip_serializing_if = "AmdAv1LatencyMode::is_auto")]
    pub amd_av1_latency_mode: AmdAv1LatencyMode,
}

fn default_video_stream_format() -> amaterasu_module_sdk::VideoStreamFormat {
    amaterasu_module_sdk::VideoStreamFormat::H264_SDR
}

fn is_default_video_stream_format(format: &amaterasu_module_sdk::VideoStreamFormat) -> bool {
    *format == amaterasu_module_sdk::VideoStreamFormat::H264_SDR
}

/// Provider-reported source size and logical touch units. Desktop geometry does
/// not establish these dimensions, so video never derives them from that field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TouchSurface {
    pub width: u32,
    pub height: u32,
    pub logical_width: u32,
    pub logical_height: u32,
}

impl TouchSurface {
    pub fn validate(&self) -> bool {
        self.width != 0 && self.height != 0 && self.logical_width != 0 && self.logical_height != 0
    }
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn is_default_nvenc_settings(settings: &amaterasu_module_sdk::NvencSettings) -> bool {
    settings == &amaterasu_module_sdk::NvencSettings::default()
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

fn is_h264(codec: &VideoCodec) -> bool {
    *codec == VideoCodec::H264
}

impl Configuration {
    /// Accept either public codec representation, while refusing contradictory
    /// explicit non-default selections before allocating capture resources.
    pub fn normalize_codec(&mut self) -> Result<(), &'static str> {
        let negotiated = match self.video_stream_format.codec {
            amaterasu_module_sdk::VideoCodec::H264 => VideoCodec::H264,
            amaterasu_module_sdk::VideoCodec::Hevc => VideoCodec::Hevc,
            amaterasu_module_sdk::VideoCodec::Av1 => VideoCodec::Av1,
        };
        if self.video_stream_format != default_video_stream_format() {
            if self.codec != VideoCodec::H264 && self.codec != negotiated {
                return Err("conflicting_video_codec");
            }
            self.codec = negotiated;
        } else {
            self.video_stream_format.codec = match self.codec {
                VideoCodec::H264 => amaterasu_module_sdk::VideoCodec::H264,
                VideoCodec::Hevc => amaterasu_module_sdk::VideoCodec::Hevc,
                VideoCodec::Av1 => amaterasu_module_sdk::VideoCodec::Av1,
            };
        }
        Ok(())
    }

    pub fn validate(&self) -> bool {
        self.video_stream_format.validate().is_ok()
            && self.video_stream_format.bit_depth == 8
            && self.video_stream_format.dynamic_range == amaterasu_module_sdk::DynamicRange::Sdr
            && (16..=3840).contains(&self.width)
            && (16..=2160).contains(&self.height)
            && self.width.is_multiple_of(2)
            && self.height.is_multiple_of(2)
            && (1..=120).contains(&self.fps)
            && (100_000..=100_000_000).contains(&self.bitrate)
            && self.max_reference_frames <= 16
            && (cfg!(target_os = "linux") || self.codec == VideoCodec::H264)
            && (!cfg!(windows) || self.color_mode.is_default())
            && self.slices_per_frame >= 1
            && self.amd_av1_latency_mode == AmdAv1LatencyMode::Auto
            && self.nvenc_settings.validate().is_ok()
            && self.nvenc_settings == amaterasu_module_sdk::NvencSettings::default()
            && valid_software_option(&self.sw_preset, 128)
            && valid_software_option(&self.sw_tune, 256)
    }
}

fn valid_software_option(value: &Option<String>, maximum: usize) -> bool {
    value
        .as_deref()
        .is_none_or(|value| value.len() <= maximum && !value.chars().any(char::is_control))
}

#[derive(Clone, PartialEq)]
pub struct OpenSpec {
    pub packet_boundaries: bool,
    pub operation_id: Option<String>,
    pub provider: CaptureProvider,
    pub auto_capture: bool,
    pub constant_rate: bool,
    pub wgc_pacing_smoothing: bool,
    pub minimum_fps_target: f64,
    pub output: String,
    pub encoder: EncoderSelection,
    pub configuration: Configuration,
}

pub enum Operation {
    SelectPortalSource {
        source_type: PortalSourceType,
        operation_id: String,
    },
    PollPortalSelection {
        operation_id: String,
    },
    CancelPortalSelection {
        operation_id: String,
    },
    Open {
        packet_boundaries: bool,
        operation_id: Option<String>,
        capture_provider: CaptureProvider,
        auto_capture: bool,
        constant_rate: bool,
        wgc_pacing_smoothing: bool,
        minimum_fps_target: f64,
        output: String,
        encoder: EncoderSelection,
        configuration: Box<Configuration>,
    },
    Lookup {
        operation_id: String,
    },
    Close {
        stream: u64,
    },
    ForceIdr {
        stream: u64,
    },
    Drain {
        stream: u64,
    },
    Status {
        stream: Option<u64>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum OperationInput {
    SelectPortalSource {
        source_type: PortalSourceType,
        operation_id: String,
    },
    PollPortalSelection {
        operation_id: String,
    },
    CancelPortalSelection {
        operation_id: String,
    },
    Open {
        #[serde(default)]
        packet_boundaries: bool,
        #[serde(default)]
        operation_id: Option<String>,
        #[serde(default)]
        capture_provider: Present<CaptureProvider>,
        #[serde(default)]
        capture: Present<CaptureMode>,
        #[serde(default)]
        constant_rate: Present<bool>,
        #[serde(default = "default_wgc_pacing_smoothing")]
        wgc_pacing_smoothing: bool,
        #[serde(default = "default_minimum_fps_target")]
        minimum_fps_target: f64,
        output: String,
        encoder: EncoderSelection,
        configuration: Box<Configuration>,
    },
    Lookup {
        operation_id: String,
    },
    Close {
        stream: u64,
    },
    ForceIdr {
        stream: u64,
    },
    Drain {
        stream: u64,
    },
    Status {
        #[serde(default)]
        stream: Option<u64>,
    },
}

struct Present<T>(Option<T>);

impl<T> Default for Present<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<'de, T> Deserialize<'de> for Present<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        T::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}

#[derive(Clone, Copy, Deserialize)]
enum CaptureMode {
    #[serde(rename = "ddx")]
    Ddx,
    #[serde(rename = "wgc")]
    Wgc,
    #[serde(rename = "wgcc")]
    WgcConstant,
    #[serde(rename = "")]
    Empty,
    #[serde(rename = "auto")]
    Auto,
}

impl CaptureMode {
    fn selection(
        self,
        bootstrap_provider: Option<CaptureProvider>,
    ) -> (CaptureProvider, bool, bool) {
        match self {
            Self::Ddx => (CaptureProvider::Dxgi, false, false),
            Self::Wgc => (CaptureProvider::Wgc, false, false),
            Self::WgcConstant => (CaptureProvider::Wgc, false, true),
            // Auto preserves the provider selected during bootstrap, defaulting to
            // DXGI only when no bootstrap provider was supplied.
            Self::Empty | Self::Auto => (bootstrap_provider.unwrap_or_default(), true, false),
        }
    }
}

impl<'de> Deserialize<'de> for Operation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match OperationInput::deserialize(deserializer)? {
            OperationInput::SelectPortalSource {
                source_type,
                operation_id,
            } => Self::SelectPortalSource {
                source_type,
                operation_id,
            },
            OperationInput::PollPortalSelection { operation_id } => {
                Self::PollPortalSelection { operation_id }
            }
            OperationInput::CancelPortalSelection { operation_id } => {
                Self::CancelPortalSelection { operation_id }
            }
            OperationInput::Open {
                packet_boundaries,
                operation_id,
                capture_provider,
                capture,
                constant_rate,
                wgc_pacing_smoothing,
                minimum_fps_target,
                output,
                encoder,
                configuration,
            } => {
                let (provider, auto_capture, constant_rate) = if let Some(capture) = capture.0 {
                    // The canonical source mode owns its rate behavior, even when
                    // a caller also supplies the legacy constant_rate field. The
                    // legacy provider is retained as the Auto bootstrap selection.
                    capture.selection(capture_provider.0)
                } else {
                    (
                        capture_provider.0.unwrap_or_default(),
                        false,
                        constant_rate.0.unwrap_or(false),
                    )
                };
                Self::Open {
                    packet_boundaries,
                    operation_id,
                    capture_provider: provider,
                    auto_capture,
                    constant_rate,
                    wgc_pacing_smoothing,
                    minimum_fps_target,
                    output,
                    encoder,
                    configuration,
                }
            }
            OperationInput::Lookup { operation_id } => Self::Lookup { operation_id },
            OperationInput::Close { stream } => Self::Close { stream },
            OperationInput::ForceIdr { stream } => Self::ForceIdr { stream },
            OperationInput::Drain { stream } => Self::Drain { stream },
            OperationInput::Status { stream } => Self::Status { stream },
        })
    }
}

pub fn error(code: &str) -> Value {
    json!({"ok":false,"code":code})
}

fn default_minimum_fps_target() -> f64 {
    20.0
}

fn default_wgc_pacing_smoothing() -> bool {
    true
}

pub fn valid_minimum_fps_target(value: f64) -> bool {
    value.is_finite() && (0.0..=1000.0).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_encoder_configuration_preserves_options_and_rejects_invalid_bounds() {
        let mut request = open_request();
        request["configuration"]["sw_preset"] = json!("slower");
        request["configuration"]["sw_tune"] = json!("film,zerolatency");
        let Operation::Open { configuration, .. } =
            serde_json::from_value::<Operation>(request).unwrap()
        else {
            panic!("open expected")
        };
        assert!(configuration.validate());
        assert_eq!(configuration.sw_preset.as_deref(), Some("slower"));
        assert_eq!(configuration.sw_tune.as_deref(), Some("film,zerolatency"));
        let mut invalid = configuration.clone();
        invalid.sw_preset = Some("x".repeat(129));
        assert!(!invalid.validate());
        invalid.sw_preset = None;
        invalid.sw_tune = Some("bad\0option".into());
        assert!(!invalid.validate());
    }

    #[test]
    fn negotiated_video_format_defaults_to_h264_sdr_and_accepts_hevc_sdr() {
        let mut request = open_request();
        let Operation::Open { configuration, .. } =
            serde_json::from_value::<Operation>(request.clone()).unwrap()
        else {
            panic!("open expected")
        };
        assert_eq!(
            configuration.video_stream_format,
            amaterasu_module_sdk::VideoStreamFormat::H264_SDR
        );
        assert!(
            serde_json::to_value(&configuration)
                .unwrap()
                .get("video_stream_format")
                .is_none()
        );

        request["configuration"]["video_stream_format"] = json!({
            "codec":"hevc", "bit_depth":8, "dynamic_range":"sdr"
        });
        let Operation::Open { configuration, .. } = serde_json::from_value(request).unwrap() else {
            panic!("open expected")
        };
        assert!(configuration.validate());
    }

    #[test]
    fn codec_normalization_accepts_either_representation_and_rejects_conflicts() {
        for (media, negotiated) in [
            (VideoCodec::Hevc, amaterasu_module_sdk::VideoCodec::Hevc),
            (VideoCodec::Av1, amaterasu_module_sdk::VideoCodec::Av1),
        ] {
            let mut legacy: Configuration =
                serde_json::from_value(open_request()["configuration"].clone()).unwrap();
            legacy.codec = media;
            legacy.normalize_codec().unwrap();
            assert_eq!(legacy.video_stream_format.codec, negotiated);
            assert!(legacy.validate());
            let mut current: Configuration =
                serde_json::from_value(open_request()["configuration"].clone()).unwrap();
            current.video_stream_format.codec = negotiated;
            current.normalize_codec().unwrap();
            assert_eq!(current.codec, media);
            assert!(current.validate());
            current.video_stream_format.bit_depth = 10;
            assert!(!current.validate());
        }
        let mut conflicting: Configuration =
            serde_json::from_value(open_request()["configuration"].clone()).unwrap();
        conflicting.codec = VideoCodec::Hevc;
        conflicting.video_stream_format.codec = amaterasu_module_sdk::VideoCodec::Av1;
        assert_eq!(
            conflicting.normalize_codec(),
            Err("conflicting_video_codec")
        );
    }

    fn open_request() -> Value {
        json!({
            "action": "open",
            "output": "display-1",
            "encoder": {
                "clsid": "{00000000-0000-0000-0000-000000000001}",
                "friendly_name": "Test encoder"
            },
            "configuration": {
                "width": 16,
                "height": 16,
                "fps": 60,
                "bitrate": 100000
            }
        })
    }

    #[test]
    fn color_mode_defaults_to_709_limited_and_round_trips_explicit_modes() {
        let Operation::Open { configuration, .. } = serde_json::from_value(open_request()).unwrap()
        else {
            unreachable!();
        };
        assert_eq!(configuration.color_mode, ColorMode::Bt709Limited);
        assert!(
            serde_json::to_value(&configuration)
                .unwrap()
                .get("color_mode")
                .is_none()
        );

        let mut request = open_request();
        request["configuration"]["color_mode"] = json!("bt601_full");
        let Operation::Open { configuration, .. } = serde_json::from_value(request).unwrap() else {
            unreachable!();
        };
        assert_eq!(configuration.color_mode, ColorMode::Bt601Full);
        assert_eq!(
            serde_json::to_value(configuration).unwrap()["color_mode"],
            "bt601_full"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_nondefault_color_mode_before_open() {
        let mut request = open_request();
        request["configuration"]["color_mode"] = json!("bt709_full");
        let Operation::Open { configuration, .. } = serde_json::from_value(request).unwrap() else {
            unreachable!();
        };
        assert!(!configuration.validate());
    }

    #[test]
    fn provider_identifiers_are_finite_and_settings_have_bounded_deadline() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(
            settings.capture_providers,
            [CaptureProvider::Pipewire, CaptureProvider::Vkms].into()
        );
        #[cfg(not(target_os = "linux"))]
        assert_eq!(settings.capture_providers, [CaptureProvider::Dxgi].into());
        for input in [
            r#"{"capture_providers":["arbitrary/service"]}"#,
            r#"{"capture_providers":[]}"#,
            r#"{"drain_timeout_ms":0}"#,
            r#"{"drain_timeout_ms":30001}"#,
        ] {
            assert!(
                serde_json::from_str::<Settings>(input)
                    .map(|settings| settings.validate().is_err())
                    .unwrap_or(true)
            );
        }
        assert!(serde_json::from_value::<Operation>(json!({"action":"open","capture_provider":"other","output":"x","encoder":0,"configuration":{"width":16,"height":16,"fps":1,"bitrate":100000}})).is_err());
        for source_type in ["monitor", "window"] {
            assert!(
                serde_json::from_value::<Operation>(json!({
                    "action": "select_portal_source",
                    "source_type": source_type,
                    "operation_id": "choose-1"
                }))
                .is_ok()
            );
        }
        assert!(
            serde_json::from_value::<Operation>(json!({
                "action": "select_portal_source",
                "source_type": "virtual_display",
                "operation_id": "choose-1"
            }))
            .is_err()
        );
    }

    #[test]
    fn open_accepts_numeric_encoder_indices_and_explicit_identities() {
        let mut request = open_request();
        request["encoder"] = json!(0);
        let Operation::Open { encoder, .. } = serde_json::from_value::<Operation>(request).unwrap()
        else {
            panic!("open request expected");
        };
        assert!(matches!(encoder, EncoderSelection::Index(0)));

        let mut request = open_request();
        request["encoder"] = json!("auto");
        let Operation::Open { encoder, .. } = serde_json::from_value::<Operation>(request).unwrap()
        else {
            panic!("open request expected");
        };
        assert!(matches!(
            encoder,
            EncoderSelection::Backend(EncoderBackend::Auto)
        ));

        let Operation::Open { encoder, .. } =
            serde_json::from_value::<Operation>(open_request()).unwrap()
        else {
            panic!("open request expected");
        };
        assert!(matches!(encoder, EncoderSelection::Identity(_)));

        let mut invalid = open_request();
        invalid["encoder"] = json!(-1);
        assert!(serde_json::from_value::<Operation>(invalid).is_err());
    }

    #[test]
    fn reference_frame_limit_is_opt_in_and_bounded() {
        let legacy: Configuration = serde_json::from_value(json!({
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "bitrate": 5_000_000
        }))
        .unwrap();
        assert_eq!(legacy.max_reference_frames, 0);
        assert!(legacy.validate());
        assert!(serde_json::to_value(&legacy).unwrap()["max_reference_frames"].is_null());

        let mut required = legacy;
        required.max_reference_frames = 1;
        assert!(required.validate());
        assert_eq!(
            serde_json::to_value(&required).unwrap()["max_reference_frames"],
            1
        );
        required.max_reference_frames = 2;
        assert!(required.validate());
        required.max_reference_frames = 16;
        assert!(required.validate());
        required.max_reference_frames = 17;
        assert!(!required.validate());
    }

    #[test]
    fn amd_av1_latency_mode_uses_only_pinned_tokens_and_preserves_legacy_shape() {
        let legacy: Configuration = serde_json::from_value(json!({
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "bitrate": 5_000_000
        }))
        .unwrap();
        assert_eq!(legacy.amd_av1_latency_mode, AmdAv1LatencyMode::Auto);
        assert!(legacy.validate());
        assert!(
            serde_json::to_value(&legacy)
                .unwrap()
                .get("amd_av1_latency_mode")
                .is_none()
        );

        for token in ["auto", "none", "power_saving", "realtime", "lowest"] {
            let mut request = serde_json::to_value(&legacy).unwrap();
            request["amd_av1_latency_mode"] = json!(token);
            let configuration: Configuration = serde_json::from_value(request).unwrap();
            let serialized = serde_json::to_value(configuration).unwrap();
            if token == "auto" {
                assert!(serialized.get("amd_av1_latency_mode").is_none());
            } else {
                assert_eq!(serialized["amd_av1_latency_mode"], token);
            }
        }

        for invalid in ["AUTO", "PowerSaving", "power-saving", "balanced"] {
            let mut request = serde_json::to_value(&legacy).unwrap();
            request["amd_av1_latency_mode"] = json!(invalid);
            assert!(serde_json::from_value::<Configuration>(request).is_err());
        }

        let mut explicit = legacy;
        explicit.amd_av1_latency_mode = AmdAv1LatencyMode::Realtime;
        assert!(!explicit.validate());
    }

    #[test]
    fn constant_rate_mode_is_compatible_and_opt_in() {
        let mut request = open_request();
        let legacy: Operation = serde_json::from_value(request.clone()).unwrap();
        let Operation::Open {
            capture_provider,
            auto_capture,
            constant_rate,
            wgc_pacing_smoothing,
            minimum_fps_target,
            ..
        } = legacy
        else {
            unreachable!();
        };
        assert_eq!(capture_provider, CaptureProvider::default());
        assert!(!auto_capture);
        assert!(!constant_rate);
        assert!(wgc_pacing_smoothing);
        assert_eq!(minimum_fps_target, 20.0);

        request["capture_provider"] = json!("wgc");
        request["constant_rate"] = json!(true);
        request["wgc_pacing_smoothing"] = json!(false);
        request["minimum_fps_target"] = json!(23.5);
        assert!(matches!(
            serde_json::from_value::<Operation>(request).unwrap(),
            Operation::Open {
                capture_provider: CaptureProvider::Wgc,
                constant_rate: true,
                wgc_pacing_smoothing: false,
                minimum_fps_target: 23.5,
                ..
            }
        ));
    }

    #[test]
    fn canonical_capture_modes_map_to_existing_provider_and_rate_fields() {
        for (mode, bootstrap_provider, expected_provider, expected_rate, expected_auto) in [
            (
                "ddx",
                CaptureProvider::Wgc,
                CaptureProvider::Dxgi,
                false,
                false,
            ),
            (
                "wgc",
                CaptureProvider::Dxgi,
                CaptureProvider::Wgc,
                false,
                false,
            ),
            (
                "wgcc",
                CaptureProvider::Dxgi,
                CaptureProvider::Wgc,
                true,
                false,
            ),
            ("", CaptureProvider::Wgc, CaptureProvider::Wgc, false, true),
            (
                "auto",
                CaptureProvider::Pipewire,
                CaptureProvider::Pipewire,
                false,
                true,
            ),
            (
                "auto",
                CaptureProvider::Wgc,
                CaptureProvider::Wgc,
                false,
                true,
            ),
        ] {
            let mut request = open_request();
            request["capture"] = json!(mode);
            request["capture_provider"] = json!(bootstrap_provider);
            request["constant_rate"] = json!(!expected_rate);

            let Operation::Open {
                capture_provider,
                auto_capture,
                constant_rate,
                ..
            } = serde_json::from_value(request).unwrap()
            else {
                unreachable!();
            };
            assert_eq!(capture_provider, expected_provider, "mode {mode}");
            assert_eq!(auto_capture, expected_auto, "mode {mode}");
            assert_eq!(constant_rate, expected_rate, "mode {mode}");
        }
    }

    #[test]
    fn x11_provider_is_explicit_and_requires_a_signed_allowlist_entry() {
        assert_eq!(CaptureProvider::X11.service(), "amaterasu.capture-x11/1");
        assert_eq!(serde_json::to_value(CaptureProvider::X11).unwrap(), "x11");

        let settings = Settings {
            enabled: true,
            capture_observation: false,
            frame_observation: false,
            cpu_processor_service: None,
            encoder_service: None,
            encoder_plan: None,
            cpu_processor_budget_us: default_cpu_processor_budget_us(),
            capture_providers: [CaptureProvider::X11].into(),
            x11_capture_service: None,
            x11_capture_plan: None,
            drain_timeout_ms: 5000,
        };
        assert!(settings.validate().is_ok());
        let empty = Settings {
            capture_providers: BTreeSet::new(),
            ..settings
        };
        assert_eq!(empty.validate(), Err("invalid_video_settings"));
    }

    #[test]
    fn x11_replacement_requires_an_exact_external_service_in_signed_settings() {
        let mut settings: Settings = serde_json::from_value(json!({
            "enabled": true,
            "capture_providers": ["x11"]
        }))
        .unwrap();
        assert_eq!(
            settings.capture_service(CaptureProvider::X11),
            "amaterasu.capture-x11/1"
        );
        settings.x11_capture_service = Some("example.capture-test/1".into());
        let parsed: Settings = serde_json::from_value(json!({
            "enabled": true,
            "capture_providers": ["x11"],
            "x11_capture_service": "example.capture-test/1"
        }))
        .unwrap();
        assert_eq!(parsed.x11_capture_service, settings.x11_capture_service);
        if cfg!(target_os = "linux") {
            assert!(settings.validate().is_ok());
            assert_eq!(
                settings.capture_service(CaptureProvider::X11),
                "example.capture-test/1"
            );
            assert_eq!(
                settings.capture_service(CaptureProvider::Pipewire),
                CaptureProvider::Pipewire.service()
            );
        } else {
            assert_eq!(settings.validate(), Err("invalid_x11_capture_service"));
        }

        for invalid in [
            "amaterasu.capture-x11/1",
            "amaterasu.capture-kms/1",
            "example.capture-test/2",
            "example.capture-test/1/1",
            "example..capture-test/1",
            "Example.capture-test/1",
            "example.capture-test/1\n",
        ] {
            settings.x11_capture_service = Some(invalid.into());
            assert_eq!(settings.validate(), Err("invalid_x11_capture_service"));
        }

        settings.x11_capture_service = Some("example.capture-test/1".into());
        settings.capture_providers.clear();
        settings.capture_providers.insert(CaptureProvider::Pipewire);
        assert_eq!(settings.validate(), Err("invalid_x11_capture_service"));
    }

    #[test]
    fn cpu_processing_requires_an_explicit_external_service_and_bounded_budget() {
        let mut settings = Settings::default();
        assert!(settings.cpu_processor_service.is_none());
        assert!(settings.validate().is_ok());
        settings.cpu_processor_service = Some("example.video-effects/1".into());
        assert!(settings.validate().is_ok());
        for invalid in [
            "amaterasu.video/1",
            "example.video-effects/2",
            "Example.effects/1",
            "example..effects/1",
        ] {
            settings.cpu_processor_service = Some(invalid.into());
            assert_eq!(settings.validate(), Err("invalid_cpu_processor_service"));
        }
        settings.cpu_processor_service = Some("example.video-effects/1".into());
        settings.cpu_processor_budget_us = 5_001;
        assert_eq!(settings.validate(), Err("invalid_cpu_processor_service"));
        settings.cpu_processor_budget_us = 100;
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn encoder_replacement_requires_an_exact_external_service() {
        let mut settings = Settings::default();
        assert_eq!(settings.encoder_service(), "amaterasu.encode/1");
        settings.encoder_service = Some("example.encode-test/1".into());
        assert!(settings.validate().is_ok());
        assert_eq!(settings.encoder_service(), "example.encode-test/1");
        for invalid in [
            "amaterasu.encode/1",
            "example.encode-test/2",
            "Example.encode-test/1",
            "example..encode-test/1",
        ] {
            settings.encoder_service = Some(invalid.into());
            assert_eq!(settings.validate(), Err("invalid_encoder_service"));
        }
        settings.encoder_service = None;
        settings.encoder_plan = Some(EncoderBindPlan {
            provider_id: "example.encoder".into(),
            component_id: "example.encoder".into(),
            service: "example.encoder/1".into(),
            interface_major: 1,
            minimum_interface_minor: 2,
            minimum_table_size: 128,
        });
        assert!(settings.validate().is_ok());
        assert_eq!(settings.encoder_binding(), ("example.encoder/1", 1, 2, 128));
        settings.encoder_service = Some("example.other/1".into());
        assert_eq!(settings.validate(), Err("invalid_encoder_service"));
    }

    #[test]
    fn kms_provider_is_explicit_and_requires_a_signed_allowlist_entry() {
        assert_eq!(CaptureProvider::Kms.service(), "amaterasu.capture-kms/1");
        assert_eq!(serde_json::to_value(CaptureProvider::Kms).unwrap(), "kms");

        let settings = Settings {
            enabled: true,
            capture_observation: false,
            frame_observation: false,
            cpu_processor_service: None,
            encoder_service: None,
            encoder_plan: None,
            cpu_processor_budget_us: default_cpu_processor_budget_us(),
            capture_providers: [CaptureProvider::Kms].into(),
            x11_capture_service: None,
            x11_capture_plan: None,
            drain_timeout_ms: 5000,
        };
        assert!(settings.validate().is_ok());
        let empty = Settings {
            capture_providers: BTreeSet::new(),
            ..settings
        };
        assert_eq!(empty.validate(), Err("invalid_video_settings"));
    }

    #[test]
    fn legacy_provider_preserves_explicit_constant_rate() {
        let mut request = open_request();
        request["capture_provider"] = json!("wgc");
        request["constant_rate"] = json!(true);

        assert!(matches!(
            serde_json::from_value::<Operation>(request).unwrap(),
            Operation::Open {
                capture_provider: CaptureProvider::Wgc,
                constant_rate: true,
                ..
            }
        ));
    }

    #[test]
    fn canonical_auto_defaults_to_platform_provider_when_none_is_supplied() {
        let mut request = open_request();
        request["capture"] = json!("auto");

        assert!(matches!(
            serde_json::from_value::<Operation>(request).unwrap(),
            Operation::Open {
                capture_provider,
                auto_capture: true,
                constant_rate: false,
                ..
            } if capture_provider == CaptureProvider::default()
        ));
    }

    #[test]
    fn owned_kwin_outputs_accept_connector_ids_past_the_first_pool() {
        for output in [
            "kwin:Virtual-1",
            "kwin:Virtual-8",
            "kwin:Virtual-9",
            "kwin:Virtual-16",
        ] {
            assert!(is_owned_kwin_output(output), "{output}");
        }
        for output in [
            "kwin:Virtual-0",
            "kwin:Virtual-01",
            "kwin:Virtual-",
            "kwin:Virtual-2147483648",
            "kwin:Virtual-1a",
            "kwin:HDMI-A-1",
            "Virtual-1",
            "portal-selected-session-source",
        ] {
            assert!(!is_owned_kwin_output(output), "{output}");
        }
    }

    #[test]
    fn vkms_provider_is_an_optional_periodic_linux_provider() {
        assert_eq!(CaptureProvider::Vkms.service(), "amaterasu.capture-vkms/1");
        assert!(CaptureProvider::Vkms.binding_is_optional());
        assert!(!CaptureProvider::Pipewire.binding_is_optional());
        assert!(CaptureProvider::Vkms.uses_periodic_capture());
        assert!(!CaptureProvider::Wgc.uses_periodic_capture());
        let settings: Settings =
            serde_json::from_str(r#"{"capture_providers":["pipewire","vkms"]}"#).unwrap();
        assert!(settings.capture_providers.contains(&CaptureProvider::Vkms));
    }

    #[test]
    fn canonical_capture_rejects_unknown_modes() {
        let mut request = open_request();
        request["capture"] = json!("unknown");
        assert!(serde_json::from_value::<Operation>(request).is_err());
    }
}

use amaterasu_media::{ColorMode, VideoCodec};
#[cfg(test)]
use amaterasu_module_sdk::EncoderBackend;
pub use amaterasu_module_sdk::EncoderSelection;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The default is the platform's ordinary desktop capture provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
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
    External(ExternalProvider),
}

/// Bounded identity and trusted startup-resolved service. Requests never choose a service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExternalProvider {
    identity: [u8; 64],
    identity_len: u8,
    service: [u8; 128],
    service_len: u8,
    dmabuf: bool,
    generic_cpu: bool,
}

fn valid_provider_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_external_service(value: &str) -> bool {
    let Some((name, version)) = value.rsplit_once('/') else {
        return false;
    };
    !name.is_empty()
        && name.len() <= 126
        && version == "1"
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

impl ExternalProvider {
    fn new(identity: &str, service: &str, dmabuf: bool, generic_cpu: bool) -> Option<Self> {
        if !valid_provider_identity(identity)
            || (!service.is_empty() && !valid_external_service(service))
        {
            return None;
        }
        let mut result = Self {
            identity: [0; 64],
            identity_len: identity.len() as u8,
            service: [0; 128],
            service_len: service.len() as u8,
            dmabuf,
            generic_cpu,
        };
        result.identity[..identity.len()].copy_from_slice(identity.as_bytes());
        result.service[..service.len()].copy_from_slice(service.as_bytes());
        Some(result)
    }
    fn identity(&self) -> &str {
        std::str::from_utf8(&self.identity[..self.identity_len as usize]).expect("validated ASCII")
    }
    fn service(&self) -> &str {
        std::str::from_utf8(&self.service[..self.service_len as usize]).expect("validated ASCII")
    }
}

impl Serialize for CaptureProvider {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Dxgi => "dxgi",
            Self::Wgc => "wgc",
            Self::Pipewire => "pipewire",
            Self::Vkms => "vkms",
            Self::X11 => "x11",
            Self::Kms => "kms",
            Self::External(value) => value.identity(),
        })
    }
}
impl<'de> Deserialize<'de> for CaptureProvider {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "dxgi" => Self::Dxgi,
            "wgc" => Self::Wgc,
            "pipewire" => Self::Pipewire,
            "vkms" => Self::Vkms,
            "x11" => Self::X11,
            "kms" => Self::Kms,
            _ => Self::External(
                ExternalProvider::new(&value, "", false, false)
                    .ok_or_else(|| serde::de::Error::custom("invalid capture provider"))?,
            ),
        })
    }
}

impl CaptureProvider {
    pub fn uses_generic_cpu(&self) -> bool {
        matches!(self, Self::External(provider) if provider.generic_cpu)
    }

    pub fn uses_external_dmabuf(&self) -> bool {
        matches!(self, Self::External(provider) if provider.dmabuf)
    }

    pub fn identity(&self) -> &str {
        match self {
            Self::Dxgi => "dxgi",
            Self::Wgc => "wgc",
            Self::Pipewire => "pipewire",
            Self::Vkms => "vkms",
            Self::X11 => "x11",
            Self::Kms => "kms",
            Self::External(value) => value.identity(),
        }
    }

    pub fn service(&self) -> &str {
        match self {
            Self::Dxgi => "amaterasu.capture-dxgi/1",
            Self::Wgc => "amaterasu.capture-wgc/1",
            Self::Pipewire => "amaterasu.capture-pipewire/1",
            Self::Vkms => "amaterasu.capture-vkms/1",
            Self::X11 => "amaterasu.capture-x11/1",
            Self::Kms => "amaterasu.capture-kms/1",
            Self::External(value) => value.service(),
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
        matches!(self, Self::Pipewire | Self::Vkms) || self.uses_external_dmabuf()
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
#[serde(untagged)]
pub enum ExternalProviderRegistration {
    Cpu(String),
    Detailed(ExternalProviderOptions),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalProviderOptions {
    service: String,
    frame_transport: ExternalFrameTransport,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalFrameTransport {
    Cpu,
    Dmabuf,
    GenericCpu,
}

impl ExternalProviderRegistration {
    fn service(&self) -> &str {
        match self {
            Self::Cpu(service) => service,
            Self::Detailed(options) => &options.service,
        }
    }
    fn generic_cpu(&self) -> bool {
        matches!(
            self,
            Self::Detailed(ExternalProviderOptions {
                frame_transport: ExternalFrameTransport::GenericCpu,
                ..
            })
        )
    }
    fn dmabuf(&self) -> bool {
        matches!(
            self,
            Self::Detailed(ExternalProviderOptions {
                frame_transport: ExternalFrameTransport::Dmabuf,
                ..
            })
        )
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEncoderRegistration {
    pub control_service: String,
    pub native_service: String,
    pub source_ids: BTreeSet<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePreflightRoute {
    pub provider: CaptureProvider,
    #[serde(rename = "class")]
    pub source_class: amaterasu_module_sdk::capture::SourceClass,
}

impl Clone for SourcePreflightRoute {
    fn clone(&self) -> Self {
        *self
    }
}
impl Copy for SourcePreflightRoute {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_providers")]
    pub capture_providers: BTreeSet<CaptureProvider>,
    #[serde(default)]
    pub external_capture_providers: BTreeMap<String, ExternalProviderRegistration>,
    /// Trusted opaque route keys for nonmutating source queries. Requests
    /// cannot choose a native service or fall back to another provider.
    #[serde(default)]
    pub source_preflight_routes: BTreeMap<String, SourcePreflightRoute>,
    #[serde(default)]
    pub preflight_only_principals: BTreeSet<String>,
    #[serde(default)]
    pub external_encoder_providers: BTreeMap<String, String>,
    #[serde(default)]
    pub external_encoder_consumers: BTreeMap<String, String>,
    #[serde(default)]
    pub source_encoder_providers: BTreeMap<String, SourceEncoderRegistration>,
    #[serde(default)]
    pub cpu_processor_service: Option<String>,
    #[serde(default)]
    pub gpu_processor_service: Option<String>,
    /// Keep the selected processor inactive until a stream explicitly selects it.
    #[serde(default)]
    pub gpu_processor_opt_in: bool,
    /// Refuse borrowed/CPU fallbacks when this composition requires retained GPU frames.
    #[serde(default)]
    pub require_frame_lease: bool,
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
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            capture_providers: default_providers(),
            external_capture_providers: BTreeMap::new(),
            source_preflight_routes: BTreeMap::new(),
            preflight_only_principals: BTreeSet::new(),
            external_encoder_providers: BTreeMap::new(),
            external_encoder_consumers: BTreeMap::new(),
            source_encoder_providers: BTreeMap::new(),
            cpu_processor_service: None,
            gpu_processor_service: None,
            gpu_processor_opt_in: false,
            require_frame_lease: false,
            drain_timeout_ms: default_drain_timeout(),
        }
    }
}
impl Settings {
    fn source_services_distinct(&self) -> bool {
        let mut occupied: BTreeSet<&str> = BTreeSet::new();
        occupied.insert("amaterasu.encode/1");
        for provider in &self.capture_providers {
            if !occupied.insert(provider.service()) {
                return false;
            }
        }
        for registration in self.external_capture_providers.values() {
            if !occupied.insert(registration.service()) {
                return false;
            }
        }
        for service in self.external_encoder_providers.values() {
            if !occupied.insert(service) {
                return false;
            }
        }
        for service in [
            self.cpu_processor_service.as_deref(),
            self.gpu_processor_service.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !occupied.insert(service) {
                return false;
            }
        }
        for service in self.external_encoder_consumers.values() {
            if !occupied.insert(service) {
                return false;
            }
        }
        let mut source_controls = BTreeSet::new();
        self.source_encoder_providers.values().all(|registration| {
            source_controls.insert(registration.control_service.as_str())
                && (self
                    .external_encoder_providers
                    .values()
                    .any(|service| service == &registration.control_service)
                    || occupied.insert(&registration.control_service))
                && occupied.insert(&registration.native_service)
        })
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.source_preflight_routes.len() > 32
            || self.source_preflight_routes.iter().any(|(key, provider)| {
                key.is_empty()
                    || key.len() > 128
                    || key.chars().any(char::is_control)
                    || !self
                        .resolved_providers()
                        .iter()
                        .any(|selected| selected.identity() == provider.provider.identity())
            })
        {
            return Err("invalid_source_preflight_routes");
        }
        if self.preflight_only_principals.len() > 32
            || self.preflight_only_principals.iter().any(|principal| {
                principal.is_empty()
                    || principal.len() > 128
                    || principal.chars().any(char::is_control)
            })
        {
            return Err("invalid_preflight_only_principals");
        }
        if (self.capture_providers.is_empty()
            && self.external_capture_providers.is_empty()
            && self.source_encoder_providers.is_empty())
            || !self.source_services_distinct()
            || self
                .capture_providers
                .iter()
                .any(|p| matches!(p, CaptureProvider::External(_)))
            || self
                .cpu_processor_service
                .as_ref()
                .is_some_and(|service| !valid_external_service(service))
            || self
                .gpu_processor_service
                .as_ref()
                .is_some_and(|service| !valid_external_service(service))
            || (self.cpu_processor_service.is_some() && self.gpu_processor_service.is_some())
            || self.external_encoder_consumers.len() > 16
            || self
                .external_encoder_consumers
                .iter()
                .any(|(alias, service)| {
                    !self.external_encoder_providers.contains_key(alias)
                        || !valid_external_service(service)
                })
            || self.source_encoder_providers.len() > 16
            || self
                .source_encoder_providers
                .iter()
                .any(|(id, registration)| {
                    !valid_provider_identity(id)
                        || id == "builtin"
                        || self.external_encoder_providers.contains_key(id)
                        || !valid_external_service(&registration.control_service)
                        || !valid_external_service(&registration.native_service)
                        || registration.control_service == registration.native_service
                        || registration.control_service == "amaterasu.encode/1"
                        || registration.source_ids.is_empty()
                        || registration.source_ids.contains(&0)
                        || registration.source_ids.len() > 64
                })
            || self.external_encoder_providers.len() > 16
            || self.external_encoder_providers.iter().any(|(id, service)| {
                !valid_provider_identity(id)
                    || id == "builtin"
                    || !valid_external_service(service)
                    || service == "amaterasu.encode/1"
            })
            || self
                .external_encoder_providers
                .values()
                .collect::<BTreeSet<_>>()
                .len()
                != self.external_encoder_providers.len()
            || self.external_capture_providers.len() > 16
            || self.external_capture_providers.iter().any(|(id, service)| {
                !valid_provider_identity(id)
                    || !valid_external_service(service.service())
                    || matches!(
                        id.as_str(),
                        "dxgi" | "wgc" | "pipewire" | "vkms" | "x11" | "kms"
                    )
                    || self
                        .capture_providers
                        .iter()
                        .any(|provider| provider.service() == service.service())
            })
            || self
                .external_capture_providers
                .values()
                .map(ExternalProviderRegistration::service)
                .collect::<BTreeSet<_>>()
                .len()
                != self.external_capture_providers.len()
            || !(100..=30000).contains(&self.drain_timeout_ms)
        {
            return Err("invalid_video_settings");
        }
        Ok(())
    }

    pub fn resolved_providers(&self) -> BTreeSet<CaptureProvider> {
        self.capture_providers
            .iter()
            .copied()
            .chain(
                self.external_capture_providers
                    .iter()
                    .filter_map(|(id, service)| {
                        ExternalProvider::new(
                            id,
                            service.service(),
                            service.dmabuf(),
                            service.generic_cpu(),
                        )
                        .map(CaptureProvider::External)
                    }),
            )
            .collect()
    }

    pub fn resolved_source_preflight_routes(&self) -> BTreeMap<String, SourcePreflightRoute> {
        let providers = self.resolved_providers();
        self.source_preflight_routes
            .iter()
            .filter_map(|(key, route)| {
                providers
                    .iter()
                    .find(|provider| provider.identity() == route.provider.identity())
                    .map(|provider| {
                        (
                            key.clone(),
                            SourcePreflightRoute {
                                provider: *provider,
                                source_class: route.source_class,
                            },
                        )
                    })
            })
            .collect()
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    /// Lifecycle-authorized selection and opaque provider-specific settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_processing: Option<GpuProcessing>,
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

/// Frozen processor selection supplied by the trusted stream owner.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GpuProcessing {
    pub service: String,
    pub enabled: bool,
    /// Capture-owned source range required by the selected processing plan.
    pub source_hdr_required: bool,
    pub settings: serde_json::Map<String, serde_json::Value>,
    /// Trusted lifecycle authority for reading changes to this selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_settings: Option<LiveProcessingSettings>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LiveProcessingSettings {
    pub target: amaterasu_module_sdk::extension_api::LifecycleTarget,
    pub provider: String,
    pub revision: String,
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

impl GpuProcessing {
    fn validate(&self) -> bool {
        !self.service.is_empty()
            && self.service.len() <= 128
            && self.service.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-' | b'/')
            })
            && serde_json::to_vec(&self.settings).is_ok_and(|settings| settings.len() <= 16 * 1024)
            && self.live_settings.as_ref().is_none_or(|live| {
                matches!(
                    live.target,
                    amaterasu_module_sdk::extension_api::LifecycleTarget::Stream { .. }
                ) && !live.provider.is_empty()
                    && live.provider.len() <= 128
                    && !live.revision.is_empty()
                    && live.revision.len() <= 128
                    && serde_json::to_vec(&live.target).is_ok_and(|target| target.len() <= 1024)
            })
    }
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

    pub fn validate_source(&self) -> bool {
        // Source encoders bypass capture frame inspection, so they cannot be
        // used with an explicitly selected processing policy.
        self.gpu_processing.is_none()
            && self.video_stream_format.validate().is_ok()
            && (1..=16384).contains(&self.width)
            && (1..=16384).contains(&self.height)
            && self.width.is_multiple_of(2)
            && self.height.is_multiple_of(2)
            && (1..=1000).contains(&self.fps)
            && (1..=1_000_000_000).contains(&self.bitrate)
            && self.max_reference_frames <= 16
            && self.slices_per_frame >= 1
            && self.color_mode.is_default()
            && self.amd_av1_latency_mode == AmdAv1LatencyMode::Auto
            && self.nvenc_settings == amaterasu_module_sdk::NvencSettings::default()
            && self.sw_preset.is_none()
            && self.sw_tune.is_none()
    }

    pub fn validate(&self) -> bool {
        self.gpu_processing.as_ref().is_none_or(|processing| {
            processing.validate()
                && (processing.enabled
                    || processing.source_hdr_required
                        == (self.video_stream_format.dynamic_range
                            == amaterasu_module_sdk::DynamicRange::Hdr))
        }) && self.video_stream_format.validate().is_ok()
            && (16..=3840).contains(&self.width)
            && (16..=2160).contains(&self.height)
            && self.width.is_multiple_of(2)
            && self.height.is_multiple_of(2)
            && (1..=120).contains(&self.fps)
            && (100_000..=100_000_000).contains(&self.bitrate)
            && self.max_reference_frames <= 16
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
    pub encoder_provider: Option<String>,
    pub source_id: Option<u64>,
    pub encoder_options: Option<serde_json::Value>,
    pub configuration: Configuration,
}

// Provider identifiers are bounded inline values; control operations are short-lived.
#[allow(clippy::large_enum_variant)]
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
        encoder_provider: Option<String>,
        configuration: Box<Configuration>,
    },
    OpenWithOptions {
        specification: Box<OpenSpec>,
    },
    OpenSource {
        packet_boundaries: bool,
        operation_id: Option<String>,
        source_id: u64,
        encoder_options: Option<serde_json::Value>,
        encoder_provider: String,
        encoder: EncoderSelection,
        configuration: Box<Configuration>,
    },
    Lookup {
        operation_id: String,
    },
    ObserveCapture {
        stream: u64,
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
#[allow(clippy::large_enum_variant)]
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
        #[serde(default)]
        output: String,
        #[serde(default)]
        source_id: Option<u64>,
        #[serde(default)]
        encoder_options: Option<serde_json::Value>,
        encoder: EncoderSelection,
        #[serde(default)]
        encoder_provider: Option<String>,
        configuration: Box<Configuration>,
    },
    Lookup {
        operation_id: String,
    },
    ObserveCapture {
        stream: u64,
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
                source_id,
                encoder_options,
                encoder,
                encoder_provider,
                configuration,
            } => {
                if let Some(source_id) = source_id {
                    let Some(encoder_provider) = encoder_provider else {
                        return Err(serde::de::Error::custom("source encoder provider required"));
                    };
                    if capture_provider.0.is_some()
                        || capture.0.is_some()
                        || constant_rate.0.is_some()
                        || !output.is_empty()
                    {
                        return Err(serde::de::Error::custom(
                            "source mode cannot specify capture",
                        ));
                    }
                    return Ok(Self::OpenSource {
                        packet_boundaries,
                        operation_id,
                        source_id,
                        encoder_options,
                        encoder_provider,
                        encoder,
                        configuration,
                    });
                }
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
                if let Some(options) = encoder_options {
                    return Ok(Self::OpenWithOptions {
                        specification: Box::new(OpenSpec {
                            packet_boundaries,
                            operation_id,
                            provider,
                            auto_capture,
                            constant_rate,
                            wgc_pacing_smoothing,
                            minimum_fps_target,
                            output,
                            encoder,
                            encoder_provider,
                            source_id: None,
                            encoder_options: Some(options),
                            configuration: *configuration,
                        }),
                    });
                }
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
                    encoder_provider,
                    configuration,
                }
            }
            OperationInput::Lookup { operation_id } => Self::Lookup { operation_id },
            OperationInput::ObserveCapture { stream } => Self::ObserveCapture { stream },
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
            assert!(current.validate());
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

    #[test]
    fn selected_providers_can_accept_ten_bit_sdr_and_hdr_without_host_os_gates() {
        for codec in [
            amaterasu_module_sdk::VideoCodec::Hevc,
            amaterasu_module_sdk::VideoCodec::Av1,
        ] {
            for dynamic_range in [
                amaterasu_module_sdk::DynamicRange::Sdr,
                amaterasu_module_sdk::DynamicRange::Hdr,
            ] {
                let mut configuration: Configuration =
                    serde_json::from_value(open_request()["configuration"].clone()).unwrap();
                configuration.video_stream_format = amaterasu_module_sdk::VideoStreamFormat {
                    codec,
                    bit_depth: 10,
                    dynamic_range,
                };
                configuration.normalize_codec().unwrap();
                assert!(configuration.validate());
                assert!(configuration.validate_source());
            }
        }
        let mut configuration: Configuration =
            serde_json::from_value(open_request()["configuration"].clone()).unwrap();
        configuration.color_mode = ColorMode::Bt709Full;
        assert!(
            configuration.validate(),
            "the selected encoder decides color capability"
        );
        configuration.video_stream_format.bit_depth = 10;
        assert!(
            !configuration.validate(),
            "invalid H.264 formats remain invalid on every platform"
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
        assert!(serde_json::from_value::<Operation>(json!({"action":"open","capture_provider":"other/service","output":"x","encoder":0,"configuration":{"width":16,"height":16,"fps":1,"bitrate":100000}})).is_err());
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
    fn generic_cpu_resource_negotiation_requires_explicit_registration() {
        let settings: Settings = serde_json::from_value(json!({"capture_providers": [], "external_capture_providers": {"vendor": {"service": "vendor.capture/1", "frame_transport": "generic_cpu"}}})).unwrap();
        assert!(settings.validate().is_ok());
        let provider = *settings.resolved_providers().iter().next().unwrap();
        assert!(provider.uses_generic_cpu());
        assert!(!provider.uses_external_dmabuf());
        let legacy: Settings = serde_json::from_value(json!({"capture_providers": [], "external_capture_providers": {"vendor": "vendor.capture/1"}})).unwrap();
        assert!(
            !legacy
                .resolved_providers()
                .iter()
                .next()
                .unwrap()
                .uses_generic_cpu()
        );
    }

    #[test]
    fn external_provider_registration_validates_names_services_and_uniqueness() {
        let valid: Settings = serde_json::from_value(json!({"capture_providers": [], "external_capture_providers":{"vendor_1":"vendor.capture/1"}})).unwrap();
        assert!(valid.validate().is_ok());
        let provider = *valid.resolved_providers().iter().next().unwrap();
        assert_eq!(provider.service(), "vendor.capture/1");
        assert_eq!(serde_json::to_value(provider).unwrap(), "vendor_1");
        for mapping in [
            json!({"dxgi":"vendor.capture/1"}),
            json!({"Bad":"vendor.capture/1"}),
            json!({"vendor":"vendor.capture/2"}),
            json!({"one":"vendor.capture/1", "two":"vendor.capture/1"}),
            json!({"vendor":"vendor.capture/1/other"}),
        ] {
            let settings: Settings =
                serde_json::from_value(json!({"external_capture_providers":mapping})).unwrap();
            assert!(settings.validate().is_err());
        }
    }

    #[test]
    fn external_alias_can_bind_builtin_service_only_once() {
        for builtin in [
            CaptureProvider::Dxgi,
            CaptureProvider::Wgc,
            CaptureProvider::Pipewire,
            CaptureProvider::Vkms,
            CaptureProvider::X11,
            CaptureProvider::Kms,
        ] {
            let alias = json!({"service": builtin.service(), "frame_transport": "dmabuf"});
            let exclusive: Settings = serde_json::from_value(
                json!({"capture_providers": [], "external_capture_providers":{"custom":alias}}),
            )
            .unwrap();
            assert!(exclusive.validate().is_ok(), "{}", builtin.identity());
            assert_eq!(
                exclusive
                    .resolved_providers()
                    .iter()
                    .next()
                    .unwrap()
                    .service(),
                builtin.service()
            );
            let collision: Settings = serde_json::from_value(json!({"capture_providers": [builtin], "external_capture_providers":{"custom":alias}})).unwrap();
            assert_eq!(collision.validate(), Err("invalid_video_settings"));
        }
        let alias_collision: Settings = serde_json::from_value(json!({"capture_providers":[], "external_capture_providers":{"vkms":{"service":"vendor.capture/1", "frame_transport":"dmabuf"}}})).unwrap();
        assert_eq!(alias_collision.validate(), Err("invalid_video_settings"));
    }

    #[test]
    fn x11_provider_is_explicit_and_requires_a_signed_allowlist_entry() {
        assert_eq!(CaptureProvider::X11.service(), "amaterasu.capture-x11/1");
        assert_eq!(serde_json::to_value(CaptureProvider::X11).unwrap(), "x11");

        let settings = Settings {
            enabled: true,
            capture_providers: [CaptureProvider::X11].into(),
            external_capture_providers: BTreeMap::new(),
            source_preflight_routes: BTreeMap::new(),
            preflight_only_principals: BTreeSet::new(),
            external_encoder_providers: BTreeMap::new(),
            external_encoder_consumers: BTreeMap::new(),
            source_encoder_providers: BTreeMap::new(),
            cpu_processor_service: None,
            gpu_processor_service: None,
            gpu_processor_opt_in: false,
            require_frame_lease: false,
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
    fn kms_provider_is_explicit_and_requires_a_signed_allowlist_entry() {
        assert_eq!(CaptureProvider::Kms.service(), "amaterasu.capture-kms/1");
        assert_eq!(serde_json::to_value(CaptureProvider::Kms).unwrap(), "kms");

        let settings = Settings {
            enabled: true,
            capture_providers: [CaptureProvider::Kms].into(),
            external_capture_providers: BTreeMap::new(),
            source_preflight_routes: BTreeMap::new(),
            preflight_only_principals: BTreeSet::new(),
            external_encoder_providers: BTreeMap::new(),
            external_encoder_consumers: BTreeMap::new(),
            source_encoder_providers: BTreeMap::new(),
            cpu_processor_service: None,
            gpu_processor_service: None,
            gpu_processor_opt_in: false,
            require_frame_lease: false,
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

    #[test]
    fn processor_settings_validate_services_and_require_one_frame_domain() {
        let valid: Settings =
            serde_json::from_value(json!({"gpu_processor_service":"example.process/1"})).unwrap();
        assert!(valid.validate().is_ok());
        let both: Settings = serde_json::from_value(json!({"gpu_processor_service":"example.gpu/1", "cpu_processor_service":"example.cpu/1"})).unwrap();
        assert_eq!(both.validate(), Err("invalid_video_settings"));
        for service in ["", "no-version", "example /1", "../service/1"] {
            let settings: Settings =
                serde_json::from_value(json!({"gpu_processor_service":service})).unwrap();
            assert_eq!(settings.validate(), Err("invalid_video_settings"));
        }
    }
    #[test]
    fn external_encoder_settings_reject_untrusted_or_duplicate_services() {
        let valid: Settings = serde_json::from_value(
            json!({"external_encoder_providers":{"vendor":"example.encode/1"}}),
        )
        .unwrap();
        assert!(valid.validate().is_ok());
        for mapping in [
            json!({"builtin":"example.encode/1"}),
            json!({"vendor":"amaterasu.encode/1"}),
            json!({"vendor":"example.encode/2"}),
            json!({"a":"example.encode/1","b":"example.encode/1"}),
        ] {
            let settings: Settings =
                serde_json::from_value(json!({"external_encoder_providers":mapping})).unwrap();
            assert_eq!(settings.validate(), Err("invalid_video_settings"));
        }
        let mapping: BTreeMap<String, String> = (0..17)
            .map(|n| (format!("p{n}"), format!("example.encoder{n}/1")))
            .collect();
        let settings: Settings =
            serde_json::from_value(json!({"external_encoder_providers":mapping})).unwrap();
        assert_eq!(settings.validate(), Err("invalid_video_settings"));
    }
    #[test]
    fn source_and_resource_services_must_be_distinct_and_source_only_is_allowed() {
        let valid = json!({"capture_providers":[], "source_encoder_providers": {"s":{"control_service":"example.control/1", "native_service":"example.native/1", "source_ids":[42]}}});
        assert!(
            serde_json::from_value::<Settings>(valid.clone())
                .unwrap()
                .validate()
                .is_ok()
        );
        let mut collision = valid.clone();
        collision["source_encoder_providers"]["other"] = json!({"control_service":"example.other/1", "native_service":"example.native/1", "source_ids":[43]});
        assert_eq!(
            serde_json::from_value::<Settings>(collision)
                .unwrap()
                .validate(),
            Err("invalid_video_settings")
        );
        let mut collision = valid.clone();
        collision["gpu_processor_service"] = json!("example.native/1");
        assert_eq!(
            serde_json::from_value::<Settings>(collision)
                .unwrap()
                .validate(),
            Err("invalid_video_settings")
        );
        let mut bad_source = valid;
        bad_source["source_encoder_providers"]["s"]["source_ids"] = json!([0]);
        assert_eq!(
            serde_json::from_value::<Settings>(bad_source)
                .unwrap()
                .validate(),
            Err("invalid_video_settings")
        );
        let shared = json!({"external_encoder_providers":{"v":"example.control/1"}, "source_encoder_providers":{"s":{"control_service":"example.control/1","native_service":"example.source/1","source_ids":[42]}}, "external_encoder_consumers":{"v":"example.consumer/1"}});
        assert!(
            serde_json::from_value::<Settings>(shared)
                .unwrap()
                .validate()
                .is_ok()
        );
        let ambiguous = json!({"external_capture_providers":{"c":"example.shared/1"}, "external_encoder_providers":{"v":"example.shared/1"}});
        assert_eq!(
            serde_json::from_value::<Settings>(ambiguous)
                .unwrap()
                .validate(),
            Err("invalid_video_settings")
        );
        let collision = json!({"external_encoder_providers":{"v":"example.encoder/1"},"external_encoder_consumers":{"v":"example.encoder/1"}});
        assert_eq!(
            serde_json::from_value::<Settings>(collision)
                .unwrap()
                .validate(),
            Err("invalid_video_settings")
        );
        let orphan = json!({"external_encoder_consumers":{"v":"example.consumer/1"}});
        assert_eq!(
            serde_json::from_value::<Settings>(orphan)
                .unwrap()
                .validate(),
            Err("invalid_video_settings")
        );
    }
    #[test]
    fn source_validation_allows_standard_sdr_codecs_and_rejects_unhandled_depth() {
        for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
            let mut configuration: Configuration = serde_json::from_value(
                json!({"width":4096,"height":2160,"fps":144,"bitrate":1_000_000}),
            )
            .unwrap();
            configuration.codec = codec;
            configuration.normalize_codec().unwrap();
            assert!(configuration.validate_source());
        }
        let mut configuration: Configuration =
            serde_json::from_value(json!({"width":16,"height":16,"fps":60,"bitrate":1_000_000}))
                .unwrap();
        configuration.video_stream_format.bit_depth = 10;
        assert!(!configuration.validate_source());
    }
}

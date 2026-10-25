//! Platform encoder selection with explicitly opted-in Linux backends.
#[cfg_attr(not(any(amaterasu_x264, test)), allow(dead_code))]
mod x264;
pub use x264::availability as software_availability;

#[cfg(target_os = "linux")]
mod ffmpeg_nvenc;

#[cfg(target_os = "linux")]
mod ffmpeg_vaapi;

#[cfg(target_os = "linux")]
pub(crate) mod nvenc_cuda;

#[cfg(target_os = "linux")]
#[cfg_attr(not(amaterasu_x265_hevc), allow(dead_code))]
mod x265_hevc;

#[cfg(windows)]
mod windows;

use crate::input::{GpuInput, InputSupport};
use crate::model::{Configuration, EncoderDescription, EncoderIdentity, Packet};

#[derive(Clone, Copy)]
pub struct Diagnostic {
    pub stage: &'static str,
    pub hresult: u32,
}

#[derive(Clone, Copy)]
pub struct Metrics {
    pub need_input_events: u64,
    pub have_output_events: u64,
    pub other_events: u64,
    pub process_input_calls: u64,
    pub process_output_calls: u64,
}

pub fn last_failure() -> Option<Diagnostic> {
    #[cfg(windows)]
    {
        return windows::last_failure().map(|detail| Diagnostic {
            stage: detail.stage,
            hresult: detail.hresult,
        });
    }
    // The NVENC, CUDA, EGL or GL code is reported in the hresult field.
    #[cfg(amaterasu_nvenc_cuda)]
    return nvenc_cuda::last_open_failure().map(|status| Diagnostic {
        stage: nvenc_cuda::stage_name(status.stage),
        hresult: status.native_code as u32,
    });
    #[cfg(not(any(windows, amaterasu_nvenc_cuda)))]
    None
}

pub fn metrics() -> Metrics {
    #[cfg(windows)]
    {
        let metrics = windows::metrics();
        return Metrics {
            need_input_events: metrics.need_input_events,
            have_output_events: metrics.have_output_events,
            other_events: metrics.other_events,
            process_input_calls: metrics.process_input_calls,
            process_output_calls: metrics.process_output_calls,
        };
    }
    #[cfg(not(windows))]
    Metrics {
        need_input_events: 0,
        have_output_events: 0,
        other_events: 0,
        process_input_calls: 0,
        process_output_calls: 0,
    }
}

pub fn enumerate() -> Result<Vec<EncoderDescription>, &'static str> {
    let software = x264::description();
    #[cfg(windows)]
    let mut encoders = match windows::enumerate() {
        Ok(encoders) => encoders,
        Err(error) if software.is_none() => return Err(error),
        Err(_) => Vec::new(),
    };
    #[cfg(not(windows))]
    let mut encoders = Vec::new();

    // Native NVENC is listed first so automatic selection prefers it over
    // the FFmpeg NVENC path on the same GPU.
    #[cfg(target_os = "linux")]
    encoders.splice(0..0, nvenc_cuda::descriptions());

    #[cfg(target_os = "linux")]
    if let Some(hardware) = ffmpeg_nvenc::description() {
        encoders.push(hardware);
    }

    #[cfg(target_os = "linux")]
    if let Some(hardware) = ffmpeg_vaapi::description() {
        encoders.push(hardware);
    }

    // x265 stays out of this public registry until HEVC fragment transport has
    // passed integrated end-to-end validation.
    if let Some(software) = software {
        encoders.push(software);
    }
    Ok(encoders)
}

pub enum Encoder {
    #[cfg(windows)]
    Hardware(windows::Encoder),
    #[cfg(all(target_os = "linux", amaterasu_nvenc_cuda))]
    NvencCuda(nvenc_cuda::Encoder),
    #[cfg(all(target_os = "linux", amaterasu_ffmpeg_nvenc))]
    Nvenc(ffmpeg_nvenc::Encoder),
    #[cfg(all(target_os = "linux", amaterasu_ffmpeg_vaapi))]
    Vaapi(ffmpeg_vaapi::Encoder),
    #[cfg(all(target_os = "linux", amaterasu_x265_hevc))]
    Hevc(x265_hevc::Encoder),
    Software(x264::Encoder),
}

impl Encoder {
    pub fn open_with_log_emitter(
        identity: &EncoderIdentity,
        configuration: &Configuration,
        emitter: Option<amaterasu_module_sdk::LogEmitter>,
    ) -> Result<Self, &'static str> {
        if x264::selected(identity) {
            return x264::open_with_log_emitter(identity, configuration, emitter)
                .map(Self::Software);
        }
        Self::open(identity, configuration)
    }

    pub fn open(
        identity: &EncoderIdentity,
        configuration: &Configuration,
    ) -> Result<Self, &'static str> {
        #[cfg(target_os = "linux")]
        if nvenc_cuda::selected(identity) {
            #[cfg(amaterasu_nvenc_cuda)]
            return nvenc_cuda::Encoder::open(identity, configuration).map(Self::NvencCuda);
            #[cfg(not(amaterasu_nvenc_cuda))]
            return Err(nvenc_cuda::unavailable_reason());
        }

        #[cfg(all(target_os = "linux", amaterasu_ffmpeg_nvenc))]
        if ffmpeg_nvenc::selected(identity) {
            return ffmpeg_nvenc::open(identity, configuration).map(Self::Nvenc);
        }

        #[cfg(all(target_os = "linux", amaterasu_ffmpeg_vaapi))]
        if ffmpeg_vaapi::selected(identity) {
            return ffmpeg_vaapi::open(identity, configuration).map(Self::Vaapi);
        }

        #[cfg(target_os = "linux")]
        if x265_hevc::selected(identity) {
            #[cfg(amaterasu_x265_hevc)]
            return x265_hevc::Encoder::open(identity, configuration).map(Self::Hevc);
            #[cfg(not(amaterasu_x265_hevc))]
            return Err("hevc_feature_disabled");
        }

        if x264::selected(identity) {
            return x264::open(identity, configuration).map(Self::Software);
        }

        #[cfg(windows)]
        {
            windows::Encoder::open(identity, configuration).map(Self::Hardware)
        }
        #[cfg(not(windows))]
        {
            let _ = configuration;
            #[cfg(target_os = "linux")]
            {
                Err("hardware_encoder_unavailable")
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err("unsupported_platform")
            }
        }
    }

    pub fn submit(
        &mut self,
        bytes: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<(), &'static str> {
        match self {
            #[cfg(windows)]
            Self::Hardware(encoder) => encoder.submit(bytes, timestamp, force),
            #[cfg(all(target_os = "linux", amaterasu_nvenc_cuda))]
            Self::NvencCuda(encoder) => encoder.submit(bytes, timestamp, force),
            #[cfg(all(target_os = "linux", amaterasu_ffmpeg_nvenc))]
            Self::Nvenc(encoder) => encoder.submit(bytes, timestamp, force),
            #[cfg(all(target_os = "linux", amaterasu_ffmpeg_vaapi))]
            Self::Vaapi(encoder) => encoder.submit(bytes, timestamp, force),
            #[cfg(all(target_os = "linux", amaterasu_x265_hevc))]
            Self::Hevc(encoder) => encoder.submit(bytes, timestamp, force),
            Self::Software(encoder) => encoder.submit(bytes, timestamp, force),
        }
    }

    /// Linux encoders run on the dedicated worker without blocking the caller.
    /// Windows MFT keeps the existing synchronous behavior.
    pub fn uses_background_encode(&self) -> bool {
        match self {
            #[cfg(windows)]
            Self::Hardware(_) => false,
            #[cfg(all(target_os = "linux", amaterasu_nvenc_cuda))]
            Self::NvencCuda(_) => true,
            #[cfg(all(target_os = "linux", amaterasu_ffmpeg_nvenc))]
            Self::Nvenc(_) => true,
            #[cfg(all(target_os = "linux", amaterasu_ffmpeg_vaapi))]
            Self::Vaapi(_) => true,
            #[cfg(all(target_os = "linux", amaterasu_x265_hevc))]
            Self::Hevc(_) => true,
            Self::Software(_) => true,
        }
    }

    pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        match self {
            #[cfg(windows)]
            Self::Hardware(encoder) => encoder.receive(),
            #[cfg(all(target_os = "linux", amaterasu_nvenc_cuda))]
            Self::NvencCuda(encoder) => encoder.receive(),
            #[cfg(all(target_os = "linux", amaterasu_ffmpeg_nvenc))]
            Self::Nvenc(encoder) => encoder.receive(),
            #[cfg(all(target_os = "linux", amaterasu_ffmpeg_vaapi))]
            Self::Vaapi(encoder) => encoder.receive(),
            #[cfg(all(target_os = "linux", amaterasu_x265_hevc))]
            Self::Hevc(encoder) => encoder.receive(),
            Self::Software(encoder) => encoder.receive(),
        }
    }

    pub fn drain(&mut self) -> Result<(), &'static str> {
        match self {
            #[cfg(windows)]
            Self::Hardware(encoder) => encoder.drain(),
            #[cfg(all(target_os = "linux", amaterasu_nvenc_cuda))]
            Self::NvencCuda(encoder) => encoder.drain(),
            #[cfg(all(target_os = "linux", amaterasu_ffmpeg_nvenc))]
            Self::Nvenc(encoder) => encoder.drain(),
            #[cfg(all(target_os = "linux", amaterasu_ffmpeg_vaapi))]
            Self::Vaapi(encoder) => encoder.drain(),
            #[cfg(all(target_os = "linux", amaterasu_x265_hevc))]
            Self::Hevc(encoder) => encoder.drain(),
            Self::Software(encoder) => encoder.drain(),
        }
    }

    /// Only the native NVENC backend converts on the GPU or imports DMA-BUFs.
    pub fn input_support(&self) -> InputSupport {
        match self {
            #[cfg(all(target_os = "linux", amaterasu_nvenc_cuda))]
            Self::NvencCuda(encoder) => encoder.input_support(),
            _ => InputSupport::default(),
        }
    }

    pub fn submit_gpu(
        &mut self,
        input: GpuInput,
        timestamp: i64,
        force: bool,
    ) -> Result<(), &'static str> {
        match self {
            #[cfg(all(target_os = "linux", amaterasu_nvenc_cuda))]
            Self::NvencCuda(encoder) => encoder.submit_gpu(input, timestamp, force),
            _ => {
                drop(input);
                let _ = (timestamp, force);
                Err("gpu_input_unsupported")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_identity_is_explicit_and_does_not_alias_hardware() {
        let software = x264::identity();
        assert!(x264::selected(&software));
        let mut hardware = software.clone();
        hardware.clsid = "hardware-transform".into();
        assert!(!x264::selected(&hardware));
    }

    #[test]
    fn software_encoder_is_listed_only_when_runtime_is_available() {
        assert_eq!(x264::description().is_some(), x264::availability().is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_does_not_claim_or_open_an_unregistered_hardware_encoder() {
        let identity = EncoderIdentity {
            clsid: "amaterasu.hardware.vaapi.h264.unregistered".into(),
            hardware_url: Some("ffmpeg://unprobed-device".into()),
            friendly_name: "VA-API H.264".into(),
        };
        let configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "bitrate": 15_000_000
        }))
        .unwrap();

        assert!(!ffmpeg_nvenc::selected(&identity));
        assert!(!ffmpeg_vaapi::selected(&identity));
        assert!(!nvenc_cuda::selected(&identity));
        assert!(matches!(
            Encoder::open(&identity, &configuration),
            Err("hardware_encoder_unavailable")
        ));
    }

    #[cfg(not(amaterasu_x264))]
    #[test]
    fn unavailable_software_encoder_fails_closed() {
        let mut configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width": 16,
            "height": 16,
            "fps": 60,
            "bitrate": 1_000_000
        }))
        .unwrap();
        assert!(matches!(
            Encoder::open(&x264::identity(), &configuration),
            Err("x264_feature_disabled" | "x264_headers_unavailable")
        ));
        configuration.width = 0;
        assert!(matches!(
            Encoder::open(&x264::identity(), &configuration),
            Err("x264_feature_disabled" | "x264_headers_unavailable")
        ));
    }
}

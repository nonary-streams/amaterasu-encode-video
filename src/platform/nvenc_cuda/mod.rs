//! Native NVIDIA NVENC on a CUDA context: H.264, HEVC and AV1, 8-bit SDR
//! 4:2:0, with GPU RGB -> NV12 conversion and DMA-BUF import.
//!
//! The C adapter in `native/` is compiled only by the `linux-nvenc-cuda`
//! feature when its headers are found (cfg `amaterasu_nvenc_cuda`); every
//! GPU library is loaded with dlopen at run time. Without the adapter the
//! identities are still recognized and open reports why it is unavailable.
//!
//! Everything below the policy modules runs on the encoder worker thread
//! that opened the encoder; no native handle leaves that thread.
// Policy modules are plain Rust and unit-tested on every build; the native
// runtime is their only production user.
pub(crate) mod av1;
#[cfg_attr(not(amaterasu_nvenc_cuda), allow(dead_code))]
mod codec;
#[cfg(any(amaterasu_nvenc_cuda, test))]
mod color;
#[cfg_attr(not(amaterasu_nvenc_cuda), allow(dead_code))]
mod description;
#[cfg(any(amaterasu_nvenc_cuda, test))]
mod output;
#[cfg(any(amaterasu_nvenc_cuda, test))]
mod settings;
#[cfg(any(amaterasu_nvenc_cuda, test))]
mod versions;

#[cfg(amaterasu_nvenc_cuda)]
mod runtime;

#[cfg(all(test, amaterasu_nvenc_cuda))]
mod gpu_tests;

use crate::model::{EncoderDescription, EncoderIdentity};

#[cfg(amaterasu_nvenc_cuda)]
pub use runtime::{Encoder, last_open_failure};

/// Names for the adapter's `am_nvcuda_stage` values (native/internal.h), so
/// enumeration can say which native step refused the last open.
#[cfg_attr(not(amaterasu_nvenc_cuda), allow(dead_code))]
pub fn stage_name(stage: u32) -> &'static str {
    const NAMES: [&str; 21] = [
        "none",
        "nvenc_cuda_library",
        "nvenc_cuda_driver",
        "nvenc_cuda_api",
        "nvenc_cuda_device",
        "nvenc_cuda_session",
        "nvenc_cuda_capabilities",
        "nvenc_cuda_preset",
        "nvenc_cuda_initialize",
        "nvenc_cuda_input",
        "nvenc_cuda_bitstream",
        "nvenc_cuda_encode",
        "nvenc_cuda_drain",
        "nvenc_cuda_egl_display",
        "nvenc_cuda_gl_context",
        "nvenc_cuda_gl_resources",
        "nvenc_cuda_cuda_interop",
        "nvenc_cuda_upload",
        "nvenc_cuda_fence",
        "nvenc_cuda_import",
        "nvenc_cuda_convert",
    ];
    NAMES
        .get(stage as usize)
        .copied()
        .unwrap_or("nvenc_cuda_unknown")
}

/// True for a published NVENC/CUDA identity: a known codec clsid located by
/// PCI bus id. Open locates that device again before touching it.
pub fn selected(identity: &EncoderIdentity) -> bool {
    codec::Codec::from_clsid(&identity.clsid).is_some()
        && identity
            .hardware_url
            .as_deref()
            .and_then(description::pci_bus_id)
            .is_some()
}

/// Probes the GPUs. Without the adapter nothing is probed or published.
pub fn descriptions() -> Vec<EncoderDescription> {
    #[cfg(amaterasu_nvenc_cuda)]
    {
        runtime::descriptions()
    }
    #[cfg(not(amaterasu_nvenc_cuda))]
    Vec::new()
}

#[cfg(not(amaterasu_nvenc_cuda))]
pub fn unavailable_reason() -> &'static str {
    if cfg!(feature = "linux-nvenc-cuda") {
        "nvenc_cuda_headers_unavailable"
    } else {
        "nvenc_cuda_feature_disabled"
    }
}

/// Adapter result codes (`enum am_nvcuda_result`) as session error codes.
/// Device, library, API and configuration failures are confirmed open
/// failures a caller may answer by selecting another encoder.
#[cfg_attr(not(amaterasu_nvenc_cuda), allow(dead_code))]
fn result_code(result: i32) -> &'static str {
    match result {
        1 => "nvenc_cuda_library_unavailable",
        2 => "nvenc_cuda_driver_unavailable",
        3 => "nvenc_cuda_api_unsupported",
        4 => "nvenc_cuda_device_unavailable",
        5 => "nvenc_cuda_configuration_unsupported",
        6 => "nvenc_cuda_invalid_argument",
        7 => "nvenc_cuda_allocation_failed",
        9 => "nvenc_cuda_invalid_output",
        10 => "nvenc_cuda_converter_unavailable",
        11 => "nvenc_cuda_import_failed",
        12 => "nvenc_cuda_fence_timeout",
        _ => "nvenc_cuda_native_failure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_names_follow_the_native_stage_order() {
        assert_eq!(stage_name(0), "none");
        assert_eq!(stage_name(13), "nvenc_cuda_egl_display");
        assert_eq!(stage_name(14), "nvenc_cuda_gl_context");
        assert_eq!(stage_name(20), "nvenc_cuda_convert");
        assert_eq!(stage_name(21), "nvenc_cuda_unknown");
    }

    fn identity(clsid: &str, hardware_url: Option<&str>) -> EncoderIdentity {
        EncoderIdentity {
            clsid: clsid.into(),
            hardware_url: hardware_url.map(Into::into),
            friendly_name: "NVIDIA NVENC".into(),
        }
    }

    #[test]
    fn selection_requires_a_codec_clsid_and_a_pci_location() {
        for codec in codec::Codec::ALL {
            assert!(selected(&identity(
                &codec.clsid(),
                Some("pci:0000:01:00.0")
            )));
            assert!(!selected(&identity(&codec.clsid(), None)));
            assert!(!selected(&identity(
                &codec.clsid(),
                Some("ffmpeg://h264_nvenc")
            )));
        }
        assert!(!selected(&identity(
            "amaterasu.hardware.nvidia.nvenc.ffmpeg",
            Some("pci:0000:01:00.0")
        )));
        assert!(!selected(&identity("amaterasu.software.x264", None)));
    }

    #[test]
    fn adapter_results_have_distinct_stable_codes() {
        let codes: Vec<&str> = (1..=12).map(result_code).collect();
        let mut unique = codes.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len());
        assert_eq!(result_code(4), "nvenc_cuda_device_unavailable");
        assert_eq!(result_code(99), "nvenc_cuda_native_failure");
    }

    #[cfg(not(amaterasu_nvenc_cuda))]
    #[test]
    fn without_the_adapter_nothing_is_published_and_open_explains_why() {
        assert!(descriptions().is_empty());
        assert!(matches!(
            unavailable_reason(),
            "nvenc_cuda_headers_unavailable" | "nvenc_cuda_feature_disabled"
        ));
    }
}

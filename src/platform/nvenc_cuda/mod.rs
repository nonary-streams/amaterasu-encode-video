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
#[cfg(any(amaterasu_nvenc_cuda, test))]
mod av1;
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
pub use runtime::{Encoder, NativeStatus, last_open_failure};

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

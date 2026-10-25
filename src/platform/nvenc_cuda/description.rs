//! Encoder descriptions published for probed NVIDIA devices: one per codec
//! the device actually encodes, located by PCI bus id.
use super::codec::Codec;
use crate::model::{DescriptionCapabilities, EncoderDescription, EncoderIdentity};

const NVIDIA_VENDOR_ID: u32 = 0x10de;
/// The generic configuration bounds (`Configuration::validate`).
const CONFIGURATION_MAX_WIDTH: u32 = 3840;
const CONFIGURATION_MAX_HEIGHT: u32 = 2160;
const HARDWARE_URL_PREFIX: &str = "pci:";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodecCaps {
    pub supported: bool,
    pub max_width: u32,
    pub max_height: u32,
}

/// One probed CUDA device, decoded from the adapter's record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceRecord {
    pub pci_bus_id: String,
    pub name: String,
    /// Indexed by `Codec::capability_index`.
    pub codecs: [CodecCaps; 3],
    /// The GL converter can run on this GPU; required for every input path.
    pub gpu_conversion: bool,
    pub dmabuf_import: bool,
}

pub fn hardware_url(pci_bus_id: &str) -> String {
    format!("{HARDWARE_URL_PREFIX}{pci_bus_id}")
}

/// The PCI bus id from a published hardware URL, if well formed.
pub fn pci_bus_id(hardware_url: &str) -> Option<&str> {
    let bus_id = hardware_url.strip_prefix(HARDWARE_URL_PREFIX)?;
    let well_formed = !bus_id.is_empty()
        && bus_id.len() < 32
        && bus_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b':' || byte == b'.');
    well_formed.then_some(bus_id)
}

pub fn descriptions(devices: &[DeviceRecord]) -> Vec<EncoderDescription> {
    devices
        .iter()
        .filter(|device| {
            device.gpu_conversion && pci_bus_id(&hardware_url(&device.pci_bus_id)).is_some()
        })
        .flat_map(|device| {
            Codec::ALL
                .into_iter()
                .filter_map(move |codec| describe(device, codec))
        })
        .collect()
}

fn describe(device: &DeviceRecord, codec: Codec) -> Option<EncoderDescription> {
    let caps = device.codecs[codec.capability_index()];
    if !caps.supported || caps.max_width < 16 || caps.max_height < 16 {
        return None;
    }
    let name = format!("{} NVENC {} (CUDA)", device.name, codec.display_name());
    let mut inputs = vec!["cpu"];
    if device.dmabuf_import {
        inputs.push("dmabuf");
    }
    Some(EncoderDescription {
        identity: EncoderIdentity {
            clsid: codec.clsid(),
            hardware_url: Some(hardware_url(&device.pci_bus_id)),
            friendly_name: name.clone(),
        },
        name,
        codec: codec.token(),
        hardware: true,
        vendor_id: Some(NVIDIA_VENDOR_ID),
        capabilities: DescriptionCapabilities {
            inputs,
            max_width: Some(caps.max_width.min(CONFIGURATION_MAX_WIDTH)),
            max_height: Some(caps.max_height.min(CONFIGURATION_MAX_HEIGHT)),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(max_width: u32, max_height: u32) -> CodecCaps {
        CodecCaps {
            supported: true,
            max_width,
            max_height,
        }
    }

    fn ada() -> DeviceRecord {
        DeviceRecord {
            pci_bus_id: "0000:01:00.0".into(),
            name: "NVIDIA GeForce RTX 4080".into(),
            codecs: [caps(4096, 4096), caps(8192, 8192), caps(8192, 8192)],
            gpu_conversion: true,
            dmabuf_import: true,
        }
    }

    #[test]
    fn each_supported_codec_is_published_with_its_inputs_and_limits() {
        let published = descriptions(&[ada()]);
        let codecs: Vec<&str> = published.iter().map(|entry| entry.codec).collect();
        assert_eq!(codecs, ["h264", "hevc", "av1"]);
        let av1 = &published[2];
        assert_eq!(av1.identity.clsid, "amaterasu.hardware.nvenc-cuda.av1");
        assert_eq!(
            av1.identity.hardware_url.as_deref(),
            Some("pci:0000:01:00.0")
        );
        assert_eq!(av1.name, "NVIDIA GeForce RTX 4080 NVENC AV1 (CUDA)");
        assert!(av1.hardware);
        assert_eq!(av1.vendor_id, Some(0x10de));
        assert_eq!(av1.capabilities.inputs, ["cpu", "dmabuf"]);
        assert_eq!(av1.capabilities.max_width, Some(3840));
        assert_eq!(av1.capabilities.max_height, Some(2160));
        assert!(
            published
                .iter()
                .all(|entry| entry.identity.validate().is_ok())
        );
    }

    #[test]
    fn unsupported_codecs_and_devices_are_filtered() {
        let mut turing = ada();
        turing.pci_bus_id = "0000:02:00.0".into();
        turing.codecs[Codec::Av1.capability_index()] = CodecCaps::default();
        turing.dmabuf_import = false;
        let mut headless = ada();
        headless.pci_bus_id = "0000:03:00.0".into();
        headless.gpu_conversion = false;
        let mut malformed = ada();
        malformed.pci_bus_id = "../../dev".into();
        let mut tiny = ada();
        tiny.pci_bus_id = "0000:04:00.0".into();
        tiny.codecs = [caps(8, 8), CodecCaps::default(), CodecCaps::default()];

        let published = descriptions(&[turing, headless, malformed, tiny]);
        let entries: Vec<(&str, &str)> = published
            .iter()
            .map(|entry| (entry.codec, entry.identity.hardware_url.as_deref().unwrap()))
            .collect();
        assert_eq!(
            entries,
            [("h264", "pci:0000:02:00.0"), ("hevc", "pci:0000:02:00.0")]
        );
        assert!(
            published
                .iter()
                .all(|entry| entry.capabilities.inputs == ["cpu"])
        );
    }

    #[test]
    fn hardware_urls_accept_only_pci_bus_ids() {
        assert_eq!(pci_bus_id("pci:0000:01:00.0"), Some("0000:01:00.0"));
        assert_eq!(pci_bus_id("pci:"), None);
        assert_eq!(pci_bus_id("ffmpeg://h264_nvenc"), None);
        assert_eq!(pci_bus_id("pci:0000:01:00.0/x"), None);
        assert_eq!(pci_bus_id(&format!("pci:{}", "0".repeat(32))), None);
    }
}

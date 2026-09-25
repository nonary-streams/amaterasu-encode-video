use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// Zero leaves reference-frame selection to the encoder; one requires a
    /// single H.264 reference frame.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_reference_frames: u32,
    /// NVENC quality preset. This MFT-only backend does not support NVENC presets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nvenc_preset: Option<NvencPreset>,
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

impl Configuration {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.nvenc_preset.is_some() {
            return Err("nvenc_preset_unsupported");
        }
        if !(16..=3840).contains(&self.width)
            || !(16..=2160).contains(&self.height)
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || !(1..=120).contains(&self.fps)
            || !(100_000..=100_000_000).contains(&self.bitrate)
            || self.max_reference_frames > 1
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
}

/// One complete H.264 coded picture. Backends must establish their output
/// boundary before handing this owned packet to the native session queue.
pub struct Packet {
    pub bytes: Vec<u8>,
    pub timestamp_100ns: i64,
    pub keyframe: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(configuration.validate().is_ok());
        let serialized = serde_json::to_value(configuration).unwrap();
        assert!(serialized.get("nvenc_preset").is_none());
    }

    #[test]
    fn nvenc_preset_is_rejected_by_the_mft_only_configuration() {
        let configuration = Configuration {
            width: 16,
            height: 16,
            fps: 60,
            bitrate: 1_000_000,
            max_reference_frames: 0,
            nvenc_preset: Some(NvencPreset::P1),
        };

        assert_eq!(configuration.validate(), Err("nvenc_preset_unsupported"));
    }
}

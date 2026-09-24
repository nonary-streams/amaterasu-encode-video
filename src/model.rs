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
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl Configuration {
    pub fn validate(&self) -> Result<(), &'static str> {
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
}

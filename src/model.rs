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
        encoder: u32,
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

#[derive(Clone, Serialize)]
pub struct EncoderDescription {
    pub index: u32,
    pub name: String,
    pub codec: &'static str,
    pub hardware: bool,
}

/// One complete H.264 coded picture. Backends must establish their output
/// boundary before handing this owned packet to the native session queue.
pub struct Packet {
    pub bytes: Vec<u8>,
    pub timestamp_100ns: i64,
    pub keyframe: bool,
}

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CaptureProvider {
    #[default]
    Dxgi,
    Wgc,
}

impl CaptureProvider {
    pub fn service(self) -> &'static str {
        match self {
            Self::Dxgi => "amaterasu.capture-dxgi/1",
            Self::Wgc => "amaterasu.capture-wgc/1",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_providers")]
    pub capture_providers: BTreeSet<CaptureProvider>,
    #[serde(default = "default_drain_timeout")]
    pub drain_timeout_ms: u64,
}
fn default_providers() -> BTreeSet<CaptureProvider> {
    [CaptureProvider::Dxgi].into()
}
fn default_drain_timeout() -> u64 {
    5000
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            capture_providers: default_providers(),
            drain_timeout_ms: default_drain_timeout(),
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.capture_providers.is_empty() || !(100..=30000).contains(&self.drain_timeout_ms) {
            return Err("invalid_video_settings");
        }
        Ok(())
    }
}

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

/// Stable identity of the selected encoder MFT.  The video module passes this
/// opaque, validated value to the encoder service; it never converts it to an
/// enumeration ordinal.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EncoderIdentity {
    pub clsid: String,
    pub hardware_url: Option<String>,
    pub friendly_name: String,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl Configuration {
    pub fn validate(&self) -> bool {
        (16..=3840).contains(&self.width)
            && (16..=2160).contains(&self.height)
            && self.width.is_multiple_of(2)
            && self.height.is_multiple_of(2)
            && (1..=120).contains(&self.fps)
            && (100_000..=100_000_000).contains(&self.bitrate)
            && self.max_reference_frames <= 1
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct OpenSpec {
    pub packet_boundaries: bool,
    pub operation_id: Option<String>,
    pub provider: CaptureProvider,
    pub constant_rate: bool,
    pub output: String,
    pub encoder: EncoderIdentity,
    pub configuration: Configuration,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Open {
        #[serde(default)]
        packet_boundaries: bool,
        #[serde(default)]
        operation_id: Option<String>,
        #[serde(default)]
        capture_provider: CaptureProvider,
        #[serde(default)]
        constant_rate: bool,
        output: String,
        encoder: EncoderIdentity,
        configuration: Configuration,
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

pub fn error(code: &str) -> Value {
    json!({"ok":false,"code":code})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_identifiers_are_finite_and_settings_have_bounded_deadline() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
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
        assert!(!required.validate());
    }

    #[test]
    fn constant_rate_mode_is_compatible_and_opt_in() {
        let mut request = json!({
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
        });
        let legacy: Operation = serde_json::from_value(request.clone()).unwrap();
        assert!(matches!(
            legacy,
            Operation::Open {
                constant_rate: false,
                ..
            }
        ));

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
}

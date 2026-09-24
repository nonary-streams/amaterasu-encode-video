use serde::{Deserialize, Deserializer, Serialize};
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

#[derive(Clone, PartialEq)]
pub struct OpenSpec {
    pub packet_boundaries: bool,
    pub operation_id: Option<String>,
    pub provider: CaptureProvider,
    pub constant_rate: bool,
    pub wgc_pacing_smoothing: bool,
    pub minimum_fps_target: f64,
    pub output: String,
    pub encoder: EncoderIdentity,
    pub configuration: Configuration,
}

pub enum Operation {
    Open {
        packet_boundaries: bool,
        operation_id: Option<String>,
        capture_provider: CaptureProvider,
        constant_rate: bool,
        wgc_pacing_smoothing: bool,
        minimum_fps_target: f64,
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
        stream: Option<u64>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum OperationInput {
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
    fn selection(self, bootstrap_provider: Option<CaptureProvider>) -> (CaptureProvider, bool) {
        match self {
            Self::Ddx => (CaptureProvider::Dxgi, false),
            Self::Wgc => (CaptureProvider::Wgc, false),
            Self::WgcConstant => (CaptureProvider::Wgc, true),
            // Auto preserves the provider selected during bootstrap, defaulting to
            // DXGI only when no bootstrap provider was supplied.
            Self::Empty | Self::Auto => (bootstrap_provider.unwrap_or_default(), false),
        }
    }
}

impl<'de> Deserialize<'de> for Operation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match OperationInput::deserialize(deserializer)? {
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
                let (provider, constant_rate) = if let Some(capture) = capture.0 {
                    // The canonical source mode owns its rate behavior, even when
                    // a caller also supplies the legacy constant_rate field. The
                    // legacy provider is retained as the Auto bootstrap selection.
                    capture.selection(capture_provider.0)
                } else {
                    (
                        capture_provider.0.unwrap_or_default(),
                        constant_rate.0.unwrap_or(false),
                    )
                };
                Self::Open {
                    packet_boundaries,
                    operation_id,
                    capture_provider: provider,
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
        let mut request = open_request();
        let legacy: Operation = serde_json::from_value(request.clone()).unwrap();
        let Operation::Open {
            capture_provider,
            constant_rate,
            wgc_pacing_smoothing,
            minimum_fps_target,
            ..
        } = legacy
        else {
            unreachable!();
        };
        assert_eq!(capture_provider, CaptureProvider::Dxgi);
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
        for (mode, bootstrap_provider, expected_provider, expected_rate) in [
            ("ddx", CaptureProvider::Wgc, CaptureProvider::Dxgi, false),
            ("wgc", CaptureProvider::Dxgi, CaptureProvider::Wgc, false),
            ("wgcc", CaptureProvider::Dxgi, CaptureProvider::Wgc, true),
            ("", CaptureProvider::Wgc, CaptureProvider::Wgc, false),
            ("auto", CaptureProvider::Wgc, CaptureProvider::Wgc, false),
        ] {
            let mut request = open_request();
            request["capture"] = json!(mode);
            request["capture_provider"] = json!(bootstrap_provider);
            request["constant_rate"] = json!(!expected_rate);

            let Operation::Open {
                capture_provider,
                constant_rate,
                ..
            } = serde_json::from_value(request).unwrap()
            else {
                unreachable!();
            };
            assert_eq!(capture_provider, expected_provider, "mode {mode}");
            assert_eq!(constant_rate, expected_rate, "mode {mode}");
        }
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
    fn canonical_auto_defaults_to_dxgi_when_no_bootstrap_provider_is_supplied() {
        let mut request = open_request();
        request["capture"] = json!("auto");

        assert!(matches!(
            serde_json::from_value::<Operation>(request).unwrap(),
            Operation::Open {
                capture_provider: CaptureProvider::Dxgi,
                constant_rate: false,
                ..
            }
        ));
    }

    #[test]
    fn canonical_capture_rejects_unknown_modes() {
        let mut request = open_request();
        request["capture"] = json!("unknown");
        assert!(serde_json::from_value::<Operation>(request).is_err());
    }
}

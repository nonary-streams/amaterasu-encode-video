//! Version-matched x264 ABI adapter. The worker owns all native resources.
use crate::model::{Configuration, EncoderDescription, EncoderIdentity, Packet};
use amaterasu_module_sdk::LogEmitter;

use amaterasu_media::ColorMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VuiColor {
    color_primaries: u32,
    transfer_characteristics: u32,
    matrix_coefficients: u32,
    full_range: u32,
}

fn vui_color(mode: ColorMode) -> VuiColor {
    let (color, full_range) = match mode {
        ColorMode::Bt601Limited => (6, 0),
        ColorMode::Bt601Full => (6, 1),
        ColorMode::Bt709Limited => (1, 0),
        ColorMode::Bt709Full => (1, 1),
    };
    VuiColor {
        color_primaries: color,
        transfer_characteristics: color,
        matrix_coefficients: color,
        full_range,
    }
}

const SOFTWARE_ID: &str = "amaterasu.software.x264";
const SOFTWARE_NAME: &str = "x264 software H.264";
pub fn identity() -> EncoderIdentity {
    EncoderIdentity {
        clsid: SOFTWARE_ID.into(),
        hardware_url: None,
        friendly_name: SOFTWARE_NAME.into(),
    }
}

pub fn selected(identity: &EncoderIdentity) -> bool {
    identity.clsid == SOFTWARE_ID
}

#[cfg(test)]
mod color_tests {
    use super::*;

    #[test]
    fn vui_signaling_matches_sdr_matrix_and_range() {
        for (mode, color, full_range) in [
            (ColorMode::Bt601Limited, 6, 0),
            (ColorMode::Bt601Full, 6, 1),
            (ColorMode::Bt709Limited, 1, 0),
            (ColorMode::Bt709Full, 1, 1),
        ] {
            assert_eq!(
                vui_color(mode),
                VuiColor {
                    color_primaries: color,
                    transfer_characteristics: color,
                    matrix_coefficients: color,
                    full_range,
                }
            );
        }
    }
}

pub fn description() -> Option<EncoderDescription> {
    availability().is_ok().then(|| EncoderDescription {
        identity: identity(),
        name: SOFTWARE_NAME.into(),
        codec: "h264",
        hardware: false,
        vendor_id: None,
        capabilities: Default::default(),
    })
}

#[cfg(not(amaterasu_x264))]
pub fn availability() -> Result<(), &'static str> {
    if cfg!(feature = "software-x264") {
        Err("x264_headers_unavailable")
    } else {
        Err("x264_feature_disabled")
    }
}

#[cfg(not(amaterasu_x264))]
pub struct Encoder;

#[cfg(not(amaterasu_x264))]
impl Encoder {
    pub fn open(_: &EncoderIdentity, _: &Configuration) -> Result<Self, &'static str> {
        Err(availability().unwrap_err())
    }
    pub fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<(), &'static str> {
        Err("software_encoder_unavailable")
    }
    pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        Err("software_encoder_unavailable")
    }
    pub fn drain(&mut self) -> Result<(), &'static str> {
        Err("software_encoder_unavailable")
    }
}

#[cfg(amaterasu_x264)]
mod runtime;
#[cfg(amaterasu_x264)]
pub use runtime::{Encoder, availability};

#[cfg(amaterasu_x264)]
pub fn open(
    identity: &EncoderIdentity,
    configuration: &Configuration,
) -> Result<Encoder, &'static str> {
    Encoder::open(identity, configuration)
}

#[cfg(not(amaterasu_x264))]
pub fn open(
    identity: &EncoderIdentity,
    configuration: &Configuration,
) -> Result<Encoder, &'static str> {
    Encoder::open(identity, configuration)
}

#[cfg(all(test, not(amaterasu_x264)))]
mod tests {
    use super::*;

    #[test]
    fn missing_build_prerequisite_never_enumerates_or_opens_software() {
        let reason = if cfg!(feature = "software-x264") {
            "x264_headers_unavailable"
        } else {
            "x264_feature_disabled"
        };
        assert_eq!(availability(), Err(reason));
        assert!(description().is_none());
        let config: Configuration = serde_json::from_value(
            serde_json::json!({"width":16,"height":16,"fps":60,"bitrate":1000000}),
        )
        .unwrap();
        assert!(matches!(
            Encoder::open(&identity(), &config),
            Err(error) if error == reason
        ));
    }
}

#[cfg(amaterasu_x264)]
pub fn open_with_log_emitter(
    identity: &EncoderIdentity,
    configuration: &Configuration,
    emitter: Option<LogEmitter>,
) -> Result<Encoder, &'static str> {
    Encoder::open_with_log_emitter(identity, configuration, emitter)
}
#[cfg(not(amaterasu_x264))]
pub fn open_with_log_emitter(
    identity: &EncoderIdentity,
    configuration: &Configuration,
    _emitter: Option<LogEmitter>,
) -> Result<Encoder, &'static str> {
    Encoder::open(identity, configuration)
}

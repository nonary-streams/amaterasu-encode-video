//! Version-matched x264 ABI adapter. The worker owns all native resources.
use crate::model::{Configuration, EncoderDescription, EncoderIdentity, Packet};
#[cfg(not(amaterasu_x264))]
use amaterasu_module_sdk::LogEmitter;

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

pub fn description() -> Option<EncoderDescription> {
    availability().is_ok().then(|| EncoderDescription {
        identity: identity(),
        name: SOFTWARE_NAME.into(),
        codec: "h264",
        hardware: false,
        vendor_id: None,
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
    pub fn open_with_log_emitter(
        _: &EncoderIdentity,
        _: &Configuration,
        _: Option<LogEmitter>,
    ) -> Result<Self, &'static str> {
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
            Encoder::open_with_log_emitter(&identity(), &config, None),
            Err(error) if error == reason
        ));
    }
}

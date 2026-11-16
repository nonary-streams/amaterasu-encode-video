//! Correlate encoder probes before capture resources are allocated.
use crate::model::Configuration;
use amaterasu_module_sdk::EncoderIdentity;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeReply {
    ok: bool,
    codec: String,
    encoder: EncoderIdentity,
    configuration: Configuration,
    usable: bool,
    settled: bool,
    code: Option<String>,
    reason: Option<String>,
}

/// GPU processing is owned by video, not by the encoder provider.
pub(super) fn configuration_for_encoder(configuration: &Configuration) -> Configuration {
    let mut projected = configuration.clone();
    projected.gpu_processing = None;
    projected
}

/// A negative result authorizes fallback only after exact correlation and teardown.
pub(super) fn probe_result(
    identity: &EncoderIdentity,
    configuration: &Configuration,
    payload: &Value,
) -> Result<Option<String>, &'static str> {
    let reply: ProbeReply =
        serde_json::from_value(payload.clone()).map_err(|_| "invalid_encoder_probe_reply")?;
    if !reply.ok
        || !reply.settled
        || reply.codec != super::codec_name(configuration.codec)
        || &reply.encoder != identity
        || reply.configuration != configuration_for_encoder(configuration)
    {
        return Err("invalid_encoder_probe_reply");
    }
    if reply.usable {
        return (reply.code.is_none() && reply.reason.is_none())
            .then_some(None)
            .ok_or("invalid_encoder_probe_reply");
    }
    let Some(reason) = reply.reason else {
        return Err("invalid_encoder_probe_reply");
    };
    if reply.code.as_deref() != Some("encoder_unusable")
        || reason.is_empty()
        || reason.len() > 128
        || reason.chars().any(char::is_control)
    {
        return Err("invalid_encoder_probe_reply");
    }
    Ok(Some(reason))
}

#[cfg(test)]
mod chroma_tests {
    use super::*;

    #[test]
    fn provider_probe_must_confirm_the_requested_chroma() {
        let identity = EncoderIdentity {
            clsid: "external".into(),
            friendly_name: "External".into(),
            hardware_url: None,
        };
        let mut configuration: Configuration = serde_json::from_value(
            serde_json::json!({"width":1280,"height":720,"fps":60,"bitrate":10000000}),
        )
        .unwrap();
        configuration.video_stream_format.chroma_sampling =
            amaterasu_module_sdk::ChromaSampling::Yuv444;
        let mut payload = serde_json::json!({"ok":true,"codec":"h264","encoder":identity,"configuration":configuration,"usable":true,"settled":true,"code":null,"reason":null});
        assert_eq!(probe_result(&identity, &configuration, &payload), Ok(None));
        payload["configuration"]["video_stream_format"]["chroma_sampling"] =
            serde_json::json!("yuv420");
        assert_eq!(
            probe_result(&identity, &configuration, &payload),
            Err("invalid_encoder_probe_reply")
        );
    }
}

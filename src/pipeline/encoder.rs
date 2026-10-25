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
        || &reply.configuration != configuration
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

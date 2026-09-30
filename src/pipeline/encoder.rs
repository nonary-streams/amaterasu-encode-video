//! Resolve a selection before allocation, then keep its stable identity frozen.
use crate::model::Configuration;
use amaterasu_module_sdk::{
    EncoderBackend, EncoderIdentity, EncoderSelection, SOFTWARE_ENCODER_ID,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;

const MAX_ENCODERS: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Description {
    identity: EncoderIdentity,
    name: String,
    codec: String,
    hardware: bool,
    #[serde(default, rename = "vendor_id")]
    _vendor_id: Option<u32>,
}

fn descriptions(payload: &Value) -> Result<Vec<Description>, &'static str> {
    if payload.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("encoder_enumeration_failed");
    }
    if payload.get("enabled").and_then(Value::as_bool) != Some(true) {
        return Err("encoder_disabled");
    }
    let entries = payload
        .get("encoders")
        .and_then(Value::as_array)
        .ok_or("invalid_encoder_enumeration")?;
    if entries.len() > MAX_ENCODERS {
        return Err("invalid_encoder_enumeration");
    }
    let mut identities = BTreeSet::new();
    let mut encoders = Vec::with_capacity(entries.len());
    for entry in entries {
        let encoder: Description =
            serde_json::from_value(entry.clone()).map_err(|_| "invalid_encoder_enumeration")?;
        if encoder.identity.validate().is_err()
            || encoder.name.is_empty()
            || encoder.name.len() > 512
            || encoder.name.chars().any(char::is_control)
            || encoder.codec != "h264"
            || !identities.insert(encoder.identity.clone())
            || encoder.identity.clsid == SOFTWARE_ENCODER_ID && encoder.hardware
        {
            return Err("invalid_encoder_enumeration");
        }
        encoders.push(encoder);
    }
    Ok(encoders)
}

pub(super) fn auto_candidates(payload: &Value) -> Result<Vec<EncoderIdentity>, &'static str> {
    let encoders = descriptions(payload)?;
    let candidates: Vec<_> = encoders
        .iter()
        .filter(|entry| entry.hardware)
        .chain(
            encoders
                .iter()
                .filter(|entry| !entry.hardware && entry.identity.clsid == SOFTWARE_ENCODER_ID),
        )
        .map(|entry| entry.identity.clone())
        .collect();
    if candidates.is_empty() {
        Err("encoder_unavailable")
    } else {
        Ok(candidates)
    }
}

pub(super) fn resolve(
    selection: &EncoderSelection,
    payload: &Value,
) -> Result<EncoderIdentity, &'static str> {
    let encoders = descriptions(payload)?;
    let software = || {
        encoders
            .iter()
            .find(|encoder| !encoder.hardware && encoder.identity.clsid == SOFTWARE_ENCODER_ID)
    };
    let selected = match selection {
        EncoderSelection::Index(index) => encoders.get(*index as usize),
        EncoderSelection::Backend(EncoderBackend::Auto) => encoders
            .iter()
            .find(|encoder| encoder.hardware)
            .or_else(software),
        EncoderSelection::Backend(EncoderBackend::Software) => software(),
        EncoderSelection::Identity(identity) => encoders
            .iter()
            .find(|encoder| &encoder.identity == identity),
    };
    selected
        .map(|encoder| encoder.identity.clone())
        .ok_or_else(|| {
            if matches!(
                selection,
                EncoderSelection::Backend(EncoderBackend::Software)
            ) {
                match payload["software_encoder"]["reason"].as_str() {
                    Some("x264_feature_disabled") => "x264_feature_disabled",
                    Some("x264_headers_unavailable") => "x264_headers_unavailable",
                    Some("x264_runtime_unavailable") => "x264_runtime_unavailable",
                    _ => "software_encoder_unavailable",
                }
            } else {
                "encoder_unavailable"
            }
        })
}

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
        || reply.codec != "h264"
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn description(clsid: &str, hardware: bool) -> Value {
        json!({"identity":{"clsid":clsid,"hardware_url":null,"friendly_name":"H.264"},"name":"H.264","codec":"h264","hardware":hardware,"vendor_id":null})
    }

    #[test]
    fn auto_prefers_hardware_and_software_requires_the_actual_backend() {
        let payload = json!({"ok":true,"enabled":true,"encoders":[description(SOFTWARE_ENCODER_ID,false),description("hardware",true)]});
        assert_eq!(
            resolve(&EncoderSelection::Backend(EncoderBackend::Auto), &payload)
                .unwrap()
                .clsid,
            "hardware"
        );
        assert_eq!(
            resolve(
                &EncoderSelection::Backend(EncoderBackend::Software),
                &payload
            )
            .unwrap()
            .clsid,
            SOFTWARE_ENCODER_ID
        );
        assert_eq!(
            resolve(&EncoderSelection::Index(0), &payload)
                .unwrap()
                .clsid,
            SOFTWARE_ENCODER_ID
        );
        assert_eq!(
            resolve(&EncoderSelection::Index(2), &payload),
            Err("encoder_unavailable")
        );
    }

    #[test]
    fn malformed_disabled_duplicate_and_oversized_lists_fail_before_selection() {
        let selection = EncoderSelection::Index(0);
        for encoders in [
            json!([description("hardware", true), description("hardware", true)]),
            json!(vec![description("hardware", true); 65]),
            json!([{"identity":{},"hardware":true}]),
        ] {
            assert_eq!(
                resolve(
                    &selection,
                    &json!({"ok":true,"enabled":true,"encoders":encoders})
                ),
                Err("invalid_encoder_enumeration")
            );
        }
        assert_eq!(
            resolve(
                &selection,
                &json!({"ok":true,"enabled":false,"encoders":[]})
            ),
            Err("encoder_disabled")
        );
        assert_eq!(
            resolve(&selection, &json!({"ok":false})),
            Err("encoder_enumeration_failed")
        );
        assert_eq!(
            resolve(
                &EncoderSelection::Backend(EncoderBackend::Software),
                &json!({"ok":true,"enabled":true,"encoders":[],"software_encoder":{"available":false,"reason":"x264_feature_disabled"}})
            ),
            Err("x264_feature_disabled")
        );
    }
}

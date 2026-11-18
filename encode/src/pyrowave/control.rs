//! Adapter from the shared video control request to the codec's own configuration.
use super::session::{CODEC, CODEC_HDR10, Configuration, Profile, RECORDS};
use amaterasu_media::*;
use serde_json::{Value, json};

pub(super) fn project(payload: &Value) -> Result<Option<(Value, Value)>, &'static str> {
    if payload.get("encoder").is_none() {
        return Ok(None);
    }
    let action = payload["action"].as_str().ok_or("invalid_request")?;
    if !matches!(action, "open" | "probe_source") {
        return Err("invalid_request");
    }
    let identity = &payload["encoder"];
    if identity["clsid"] != "pyrowave" {
        return Err("encoder_identity_mismatch");
    }
    let original = &payload["configuration"];
    let profile = &original["extension_profile"];
    let codec = &profile["codec"];
    let ten_bit = codec["value"].as_u64() == Some(CODEC_HDR10.value);
    let hdr = ten_bit && original_range(payload);
    let selected = if ten_bit { CODEC_HDR10 } else { CODEC };
    if codec
        != &json!({"namespace_id":selected.namespace_id,"value":selected.value,"schema":selected.schema})
        || profile["framing"]
            != json!({"namespace_id":RECORDS.namespace_id,"value":RECORDS.value,"schema":RECORDS.schema})
    {
        return Err("encoder_format_unsupported");
    }
    if payload["source_id"].as_u64().is_none_or(|id| id == 0) {
        return Err("invalid_source_id");
    }
    if original["fps_milli"]
        .as_u64()
        .is_some_and(|rate| rate != original["fps"].as_u64().unwrap_or(0) * 1000)
    {
        return Err("fractional_cadence_unsupported");
    }
    if original["gpu_processing"].is_object() {
        return Err("processor_not_supported");
    }
    let clock = &payload["resource_output_clock"];
    if action == "open" && clock["domain"] != "stream_relative" {
        return Err("encoder_clock_unavailable");
    }
    let (epoch, origin) = if action == "open" {
        let text = clock["epoch_id"]
            .as_str()
            .ok_or("encoder_clock_unavailable")?;
        if text.len() != 32 {
            return Err("encoder_clock_unavailable");
        }
        let mut epoch = [0u8; 16];
        for (i, byte) in epoch.iter_mut().enumerate() {
            *byte = u8::from_str_radix(
                text.get(i * 2..i * 2 + 2)
                    .ok_or("encoder_clock_unavailable")?,
                16,
            )
            .map_err(|_| "encoder_clock_unavailable")?;
        }
        (
            epoch,
            clock["host_origin_us"]
                .as_u64()
                .ok_or("encoder_clock_unavailable")?,
        )
    } else {
        ([1; 16], 1)
    };
    let number = |key| original[key].as_u64().ok_or("invalid_configuration");
    let configuration = Configuration {
        profile: if hdr {
            Profile::Hdr10
        } else if ten_bit {
            Profile::Sdr10
        } else {
            Profile::Sdr8
        },
        color_mode: original
            .get("color_mode")
            .map(|mode| serde_json::from_value(mode.clone()).map_err(|_| "invalid_color_mode"))
            .transpose()?,
        width: u32::try_from(number("width")?).map_err(|_| "invalid_configuration")?,
        height: u32::try_from(number("height")?).map_err(|_| "invalid_configuration")?,
        fps: u32::try_from(number("fps")?).map_err(|_| "invalid_configuration")?,
        bitrate: number("bitrate")?,
        clock_epoch: epoch,
        clock_origin_us: origin,
    };
    configuration.validate()?;
    let normalized = if action == "open" {
        json!({"action":"open", "operation_id":payload["operation_id"], "configuration":configuration})
    } else {
        json!({"action":"probe", "configuration":configuration})
    };
    let echo = json!({"encoder":identity,"configuration":original,"extension_profile":profile,
        "source_id":payload["source_id"],"packet_boundaries":true});
    Ok(Some((normalized, echo)))
}

pub(super) fn common(
    native: &amaterasu_module_sdk::encode::SessionConfig,
) -> Result<Configuration, amaterasu_module_sdk::abi::Status> {
    use amaterasu_module_sdk::abi::STATUS_INVALID_ARGUMENT;
    if native.options.len() != std::mem::size_of::<EncoderCommonConfigV1>() {
        return Err(STATUS_INVALID_ARGUMENT);
    }
    // SAFETY: exact byte size is checked; this C descriptor contains integer fields only.
    let common = unsafe {
        std::ptr::read_unaligned(native.options.as_ptr().cast::<EncoderCommonConfigV1>())
    };
    if common.crop_x != 0
        || common.crop_y != 0
        || common.visible_width != common.coded_width
        || common.visible_height != common.coded_height
        || common.profile != 0
        || common.level != 0
        || common.output_clock.size as usize != std::mem::size_of::<FrameClockMetadata>()
    {
        return Err(MEDIA_UNSUPPORTED);
    }
    if common.version != 1
        || common.size as usize != std::mem::size_of::<EncoderCommonConfigV1>()
        || common.flags & !ENCODER_CONFIG_HAS_COLOR != 0
        || common.reserved != 0
        || common.fps_denominator != 1
        || common.output_clock.domain != FRAME_CLOCK_STREAM_RELATIVE
        || common.output_clock.version != 1
        || common.output_clock.reserved != 0
    {
        return Err(STATUS_INVALID_ARGUMENT);
    }
    let ten_bit = native.codec == CODEC_HDR10;
    let hdr = ten_bit
        && common.flags & ENCODER_CONFIG_HAS_COLOR != 0
        && common.color.transfer == FRAME_TRANSFER_PQ;
    if common.bit_depth != if ten_bit { 10 } else { 8 } || common.chroma != ENCODER_CHROMA_420 {
        return Err(MEDIA_UNSUPPORTED);
    }
    let configuration = Configuration {
        profile: if hdr {
            Profile::Hdr10
        } else if ten_bit {
            Profile::Sdr10
        } else {
            Profile::Sdr8
        },
        color_mode: if common.flags & ENCODER_CONFIG_HAS_COLOR != 0 {
            native_color_mode(common.color, hdr)?
        } else {
            None
        },
        width: common.coded_width,
        height: common.coded_height,
        fps: common.fps_numerator,
        bitrate: common.bitrate_bps,
        clock_epoch: common.output_clock.epoch_id,
        clock_origin_us: common.output_clock_origin_us,
    };
    configuration
        .validate()
        .map_err(|_| STATUS_INVALID_ARGUMENT)?;
    Ok(configuration)
}

fn native_color_mode(
    color: FrameColorMetadata,
    hdr: bool,
) -> Result<Option<ColorMode>, amaterasu_module_sdk::abi::Status> {
    if color.range == FRAME_COLOR_UNKNOWN && color.matrix == FRAME_COLOR_UNKNOWN {
        return Ok(None);
    }
    let full = match color.range {
        FRAME_RANGE_FULL => true,
        FRAME_RANGE_LIMITED => false,
        _ => return Err(MEDIA_UNSUPPORTED),
    };
    let bt601 = match color.matrix {
        FRAME_MATRIX_BT2020_NCL if hdr => false,
        FRAME_MATRIX_BT601 if !hdr => true,
        FRAME_MATRIX_BT709 if !hdr => false,
        _ => return Err(MEDIA_UNSUPPORTED),
    };
    Ok(Some(match (bt601, full) {
        (true, true) => ColorMode::Bt601Full,
        (true, false) => ColorMode::Bt601Limited,
        (false, true) => ColorMode::Bt709Full,
        (false, false) => ColorMode::Bt709Limited,
    }))
}

fn original_range(payload: &Value) -> bool {
    payload["configuration"]["video_stream_format"]["dynamic_range"] == "hdr"
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_video_requests_keep_exact_identity_and_clock() {
        let codec =
            json!({"namespace_id":CODEC.namespace_id,"value":CODEC.value,"schema":CODEC.schema});
        let framing = json!({"namespace_id":RECORDS.namespace_id,"value":RECORDS.value,"schema":RECORDS.schema});
        let mut request = json!({"action":"open","operation_id":"test","encoder":{"clsid":"pyrowave","friendly_name":"PyroWave"},"source_id":1,
            "resource_output_clock":{"domain":"stream_relative","epoch_id":"07070707070707070707070707070707","host_origin_us":123},
            "configuration":{"width":64,"height":48,"fps":60,"bitrate":1000000,"extension_profile":{"codec":codec,"framing":framing}}});
        let (projected, echo) = project(&request).unwrap().unwrap();
        assert_eq!(projected["configuration"]["clock_origin_us"], 123);
        assert_eq!(echo["configuration"], request["configuration"]);
        request["configuration"]["extension_profile"]["codec"]["schema"] = json!(1);
        assert!(project(&request).is_err());
    }
}

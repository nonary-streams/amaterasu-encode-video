//! Transient codec validation uses owned synthetic NV12, never a capture provider.
use crate::{
    bitstream::Normalizer,
    factory::Factory,
    model::{Configuration, EncoderIdentity},
    sessions::Backend,
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const PROBE_TIMESTAMP: i64 = 1_000_000;
const MAX_PROBE_PACKETS: usize = 16;
const PROBE_OUTPUT_BUDGET: Duration = Duration::from_secs(2);

pub fn run(
    factory: &dyn Factory,
    encoder: EncoderIdentity,
    configuration: Configuration,
) -> Result<Value, &'static str> {
    encoder.validate()?;
    configuration.validate()?;
    let result = match factory.open(encoder.clone(), configuration.clone()) {
        Ok(mut backend) => {
            let result = validate_output(&mut *backend, &configuration);
            // Close/join must complete before a negative result can authorize
            // another candidate. A failed close remains an ordinary error.
            backend
                .close()
                .map_err(|_| "encoder_probe_cleanup_failed")?;
            result
        }
        // Factory failure transfers no live backend. NativeFactory joins its
        // initialization worker before returning an activation error.
        Err("encoder_shutdown_failed") => return Err("encoder_probe_cleanup_failed"),
        Err(code) => Err(code),
    };
    let reason = result.err();
    Ok(json!({
        "ok":true,
        "codec":"h264",
        "encoder":encoder,
        "configuration":configuration,
        "usable":reason.is_none(),
        "settled":true,
        "code":reason.map(|_| "encoder_unusable"),
        "reason":reason
    }))
}

fn validate_output(
    backend: &mut dyn Backend,
    configuration: &Configuration,
) -> Result<(), &'static str> {
    let pixels = usize::try_from(configuration.width)
        .ok()
        .and_then(|width| width.checked_mul(configuration.height as usize))
        .ok_or("invalid_configuration")?;
    let mut frame = vec![128; pixels + pixels / 2];
    frame[..pixels].fill(16);
    backend.submit(frame, PROBE_TIMESTAMP, true)?;
    backend.drain()?;
    let deadline = Instant::now() + PROBE_OUTPUT_BUDGET;
    let mut normalizer = Normalizer::default();
    let mut found_idr = false;
    for _ in 0..MAX_PROBE_PACKETS {
        if Instant::now() >= deadline {
            return Err("encoder_probe_timeout");
        }
        let Some(packet) = backend.receive()? else {
            return found_idr.then_some(()).ok_or("encoder_probe_no_idr");
        };
        if packet.timestamp_100ns != PROBE_TIMESTAMP
            || !(packet.bytes.starts_with(&[0, 0, 0, 1]) || packet.bytes.starts_with(&[0, 0, 1]))
        {
            return Err("encoder_probe_invalid_output");
        }
        let (_, idr) = normalizer
            .sample(&packet.bytes)
            .map_err(|_| "encoder_probe_invalid_output")?;
        if packet.keyframe != idr {
            return Err("encoder_probe_invalid_output");
        }
        found_idr |= idr;
    }
    Err("encoder_probe_output_limit")
}

#[cfg(test)]
mod tests;

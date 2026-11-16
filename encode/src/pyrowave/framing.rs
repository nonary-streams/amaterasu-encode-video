//! PyroWave owns the adaptation from codec packets to its client wire framing.
use super::session::{CODEC, CODEC_HDR10, RECORDS};
use amaterasu_media::*;
use amaterasu_module_sdk::{
    abi::*,
    video_framing::{FramingProvider, OwnedFramePlan},
};

pub(super) struct Framer;
fn supported(codec: EncoderFormatId, framing: EncoderFormatId) -> bool {
    let same = |a: EncoderFormatId, b: amaterasu_module_sdk::encode::FormatId| {
        a.namespace_id == b.namespace_id && a.value == b.value && a.schema == b.schema
    };
    (same(codec, CODEC) || same(codec, CODEC_HDR10)) && same(framing, RECORDS)
}
impl FramingProvider for Framer {
    fn probe(
        &self,
        _: &str,
        codec: EncoderFormatId,
        framing: EncoderFormatId,
        limits: &VideoFramingLimits,
    ) -> Result<(), Status> {
        if !supported(codec, framing)
            || limits.version != VIDEO_FRAMING_LIMITS_VERSION
            || limits.packet_size <= 16
        {
            return Err(MEDIA_UNSUPPORTED);
        }
        Ok(())
    }
    fn plan(
        &self,
        principal: &str,
        unit: &NegotiatedVideoUnit,
        bytes: &[u8],
        limits: &VideoFramingLimits,
    ) -> Result<OwnedFramePlan, Status> {
        self.probe(principal, unit.codec, unit.framing, limits)?;
        // The encoder's bounded packet envelope is validated even for the
        // compatibility framing: malformed native output must never reach UDP.
        let word = |offset: usize| -> Result<u32, Status> {
            Ok(u32::from_le_bytes(
                bytes
                    .get(offset..offset + 4)
                    .ok_or(STATUS_INVALID_ARGUMENT)?
                    .try_into()
                    .map_err(|_| STATUS_INVALID_ARGUMENT)?,
            ))
        };
        let count = word(0)? as usize;
        if count == 0 || count > 65536 {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let mut offset = 4usize;
        for _ in 0..count {
            let length = word(offset)? as usize;
            offset = offset
                .checked_add(4)
                .and_then(|v| v.checked_add(length))
                .filter(|v| *v <= bytes.len())
                .ok_or(STATUS_INVALID_ARGUMENT)?;
            if length == 0 {
                return Err(STATUS_INVALID_ARGUMENT);
            }
        }
        if offset != bytes.len() {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let shards = u32::try_from(
            bytes
                .len()
                .checked_add(8)
                .ok_or(STATUS_INVALID_ARGUMENT)?
                .div_ceil((limits.packet_size - 16) as usize),
        )
        .map_err(|_| STATUS_INVALID_ARGUMENT)?;
        let percentage = limits.min_fec_percentage;
        let parity_for = |data: u32| {
            let rate_for_minimum = if limits.min_parity_shards == 0 {
                0
            } else {
                ((limits.min_parity_shards - 1) * 100 / data) + 1
            };
            let rate = percentage.max(rate_for_minimum);
            (rate, (data * rate).div_ceil(100))
        };
        let maximum_data = if percentage == 0 && limits.min_parity_shards == 0 {
            1023
        } else {
            (1..=255)
                .rev()
                .find(|data| {
                    let (rate, parity) = parity_for(*data);
                    rate <= limits.max_fec_percentage && *data + parity <= 255
                })
                .ok_or(MEDIA_UNSUPPORTED)?
        };
        let mut blocks = Vec::new();
        let mut first = 0;
        while first < shards {
            if blocks.len() >= limits.max_fec_blocks as usize {
                return Err(MEDIA_UNSUPPORTED);
            }
            let data = (shards - first).min(maximum_data);
            // Derive parity from the exact percentage the receiver sees.
            let (effective, parity) = parity_for(data);
            blocks.push(VideoFecBlock {
                first_data_shard: first,
                data_shards: data,
                parity_shards: parity,
                percentage: effective,
            });
            first += data;
        }
        Ok(OwnedFramePlan {
            framed_bytes: bytes.to_vec(),
            record_starts: Vec::new(),
            critical_prefix_shards: 0,
            blocks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_packet_envelope_has_exact_udp_plan_and_rejects_truncation() {
        let mut bytes = 1u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        let unit = NegotiatedVideoUnit {
            size: std::mem::size_of::<NegotiatedVideoUnit>() as u32,
            version: 1,
            stream: 1,
            generation: 1,
            offer_id: 3,
            codec: EncoderFormatId {
                namespace_id: CODEC.namespace_id,
                value: CODEC.value,
                schema: CODEC.schema,
            },
            framing: EncoderFormatId {
                namespace_id: RECORDS.namespace_id,
                value: RECORDS.value,
                schema: RECORDS.schema,
            },
            access_unit_id: 1,
            submission_id: 1,
            source_id: 1,
            config_generation: 1,
            timestamp_us: 0,
            clock: FrameClockMetadata {
                version: 1,
                size: std::mem::size_of::<FrameClockMetadata>() as u32,
                domain: FRAME_CLOCK_STREAM_RELATIVE,
                reserved: 0,
                epoch_id: [1; 16],
            },
            flags: VIDEO_UNIT_KEYFRAME,
            reserved: 0,
            data: BorrowedBytes::new(&bytes),
        };
        let limits = VideoFramingLimits {
            size: std::mem::size_of::<VideoFramingLimits>() as u32,
            version: VIDEO_FRAMING_LIMITS_VERSION,
            packet_size: 1024,
            max_datagrams: 64,
            max_queue_bytes: 65536,
            max_wire_bytes: 65536,
            max_fec_blocks: 4,
            max_fec_percentage: 255,
            ..Default::default()
        };
        let plan = Framer.plan("test", &unit, &bytes, &limits).unwrap();
        assert_eq!(plan.framed_bytes, bytes);
        assert_eq!(plan.blocks[0].data_shards, 1);
        let protected = VideoFramingLimits {
            min_fec_percentage: 20,
            min_parity_shards: 2,
            ..limits
        };
        let plan = Framer.plan("test", &unit, &bytes, &protected).unwrap();
        assert_eq!(plan.blocks[0].parity_shards, 2);
        assert_eq!(
            (plan.blocks[0].data_shards * plan.blocks[0].percentage).div_ceil(100),
            2
        );
        // The SDK applies the same admission validation used by the host.
        let mut blocks = [VideoFecBlock::default(); VIDEO_FRAME_MAX_BLOCKS];
        blocks[..plan.blocks.len()].copy_from_slice(&plan.blocks);
        let header = VideoFramePlan {
            size: std::mem::size_of::<VideoFramePlan>() as u32,
            version: 1,
            framed_bytes: BorrowedBytes::new(&plan.framed_bytes),
            record_starts: BorrowedBytes::new(&plan.record_starts),
            critical_prefix_shards: 0,
            block_count: plan.blocks.len() as u32,
            blocks,
        };
        // SAFETY: all plan storage stays live for synchronous validation.
        assert!(unsafe { header.validate(&protected) }.is_ok());
        assert!(
            Framer
                .plan("test", &unit, &bytes[..bytes.len() - 1], &limits)
                .is_err()
        );
    }
}

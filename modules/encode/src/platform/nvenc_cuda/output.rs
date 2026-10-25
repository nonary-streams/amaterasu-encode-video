//! Turns each locked NVENC bitstream into one media packet: an Annex B
//! access unit with parameter sets on IDR (H.264, HEVC), or a temporal unit
//! led by a temporal delimiter (AV1). The bitstream decides whether a packet
//! is a key frame; NVENC's picture type is only a cross-check.
use super::{av1::TemporalUnits, codec::Codec};
use crate::{bitstream, hevc};

/// `NV_ENC_PIC_TYPE_IDR`.
pub const PICTURE_TYPE_IDR: u32 = 3;

pub enum OutputFramer {
    H264(bitstream::Normalizer),
    Hevc(hevc::Normalizer),
    Av1(TemporalUnits),
}

impl OutputFramer {
    pub fn new(codec: Codec) -> Self {
        match codec {
            Codec::H264 => Self::H264(bitstream::Normalizer::default()),
            Codec::Hevc => Self::Hevc(hevc::Normalizer::default()),
            Codec::Av1 => Self::Av1(TemporalUnits::default()),
        }
    }

    /// Returns the packet bytes and whether they start a random access point.
    pub fn frame(
        &mut self,
        bitstream: &[u8],
        picture_type: u32,
    ) -> Result<(Vec<u8>, bool), &'static str> {
        let (bytes, keyframe) = match self {
            Self::H264(normalizer) => normalizer.sample(bitstream)?,
            Self::Hevc(normalizer) => normalizer.sample(bitstream)?,
            Self::Av1(units) => units.frame(bitstream)?,
        };
        if picture_type == PICTURE_TYPE_IDR && !keyframe {
            return Err("nvenc_cuda_keyframe_mismatch");
        }
        Ok((bytes, keyframe))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H264_IDR: [u8; 17] = [
        0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88,
    ];
    const H264_P: [u8; 6] = [0, 0, 0, 1, 0x41, 0x9a];

    #[test]
    fn h264_access_units_are_annex_b_with_idr_from_nal_type() {
        let mut framer = OutputFramer::new(Codec::H264);
        let (bytes, keyframe) = framer.frame(&H264_IDR, PICTURE_TYPE_IDR).unwrap();
        assert!(keyframe);
        assert_eq!(
            bytes,
            [
                0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88
            ]
        );
        let (bytes, keyframe) = framer.frame(&H264_P, 1).unwrap();
        assert!(!keyframe);
        assert_eq!(bytes, H264_P);
    }

    #[test]
    fn an_idr_picture_type_without_an_idr_bitstream_fails_closed() {
        let mut framer = OutputFramer::new(Codec::H264);
        assert_eq!(
            framer.frame(&H264_P, PICTURE_TYPE_IDR),
            Err("nvenc_cuda_keyframe_mismatch")
        );
    }

    #[test]
    fn hevc_idr_access_units_lead_with_their_parameter_sets() {
        let vps = [0x40, 0x01, 0x0c];
        let sps = [0x42, 0x01, 0x01];
        let pps = [0x44, 0x01, 0xc1];
        let idr = [0x26, 0x01, 0xaf];
        let mut access_unit = Vec::new();
        for unit in [&vps[..], &sps, &pps, &idr] {
            access_unit.extend_from_slice(&[0, 0, 0, 1]);
            access_unit.extend_from_slice(unit);
        }
        let mut framer = OutputFramer::new(Codec::Hevc);
        let (bytes, keyframe) = framer.frame(&access_unit, PICTURE_TYPE_IDR).unwrap();
        assert!(keyframe);
        assert_eq!(bytes, access_unit);
        let trailing = [0, 0, 0, 1, 0x02, 0x01, 0xd0];
        let (bytes, keyframe) = framer.frame(&trailing, 1).unwrap();
        assert!(!keyframe);
        assert_eq!(bytes, trailing);
    }

    #[test]
    fn av1_output_is_a_temporal_unit() {
        let mut framer = OutputFramer::new(Codec::Av1);
        let sequence = [0x0a, 0x01, 0x00];
        let frame = [0x32, 0x01, 0x00];
        let input = [&sequence[..], &frame].concat();
        let (bytes, keyframe) = framer.frame(&input, PICTURE_TYPE_IDR).unwrap();
        assert!(keyframe);
        assert_eq!(bytes, [&[0x12, 0x00][..], &sequence, &frame].concat());
    }
}

//! Codec identity for the native NVENC backend: one published encoder per
//! codec, distinguished by clsid.
use amaterasu_media::VideoCodec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

const CLSID_PREFIX: &str = "amaterasu.hardware.nvenc-cuda.";

impl Codec {
    pub const ALL: [Self; 3] = [Self::H264, Self::Hevc, Self::Av1];

    pub fn from_video(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::H264 => Self::H264,
            VideoCodec::Hevc => Self::Hevc,
            VideoCodec::Av1 => Self::Av1,
        }
    }

    /// The adapter's codec id; equal to the media ABI `CODEC_*` value.
    pub fn native_id(self) -> u32 {
        match self {
            Self::H264 => amaterasu_media::CODEC_H264,
            Self::Hevc => amaterasu_media::CODEC_HEVC,
            Self::Av1 => amaterasu_media::CODEC_AV1,
        }
    }

    /// Index into the adapter's per-device codec capability array.
    pub fn capability_index(self) -> usize {
        self.native_id() as usize - 1
    }

    /// The `codec` token in encoder descriptions and open replies.
    pub fn token(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
            Self::Av1 => "AV1",
        }
    }

    pub fn clsid(self) -> String {
        format!("{CLSID_PREFIX}{}", self.token())
    }

    pub fn from_clsid(clsid: &str) -> Option<Self> {
        let token = clsid.strip_prefix(CLSID_PREFIX)?;
        Self::ALL.into_iter().find(|codec| codec.token() == token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsids_round_trip_and_reject_other_backends() {
        for codec in Codec::ALL {
            assert_eq!(Codec::from_clsid(&codec.clsid()), Some(codec));
            assert_eq!(
                Codec::from_video(match codec {
                    Codec::H264 => VideoCodec::H264,
                    Codec::Hevc => VideoCodec::Hevc,
                    Codec::Av1 => VideoCodec::Av1,
                }),
                codec
            );
        }
        assert_eq!(Codec::H264.clsid(), "amaterasu.hardware.nvenc-cuda.h264");
        assert_eq!(
            Codec::from_clsid("amaterasu.hardware.nvidia.nvenc.ffmpeg"),
            None
        );
        assert_eq!(Codec::from_clsid("amaterasu.hardware.nvenc-cuda.vp9"), None);
        assert_eq!(Codec::from_clsid("amaterasu.hardware.nvenc-cuda."), None);
    }

    #[test]
    fn native_ids_match_the_media_abi_and_capability_slots() {
        assert_eq!(Codec::H264.native_id(), VideoCodec::H264.abi_id());
        assert_eq!(Codec::Hevc.native_id(), VideoCodec::Hevc.abi_id());
        assert_eq!(Codec::Av1.native_id(), VideoCodec::Av1.abi_id());
        let slots: Vec<usize> = Codec::ALL
            .iter()
            .map(|codec| codec.capability_index())
            .collect();
        assert_eq!(slots, [0, 1, 2]);
    }
}

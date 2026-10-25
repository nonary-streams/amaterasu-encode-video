//! AV1 temporal-unit framing for media packets.
//!
//! A packet is one low-overhead temporal unit: OBUs that each carry
//! `obu_has_size_field`, starting with a temporal delimiter. AV1 has no start
//! codes, so a missing size field cannot be recovered and fails closed. A
//! missing leading delimiter is inserted. Key frames carry a sequence header;
//! the last one seen is inserted when the encoder omitted it.
const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
const MAX_OBUS: usize = 1024;
const TEMPORAL_DELIMITER: [u8; 2] = [0x12, 0x00];

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_TILE_GROUP: u8 = 4;
const OBU_METADATA: u8 = 5;
const OBU_FRAME: u8 = 6;
const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;
const OBU_PADDING: u8 = 15;

struct Obu<'a> {
    kind: u8,
    /// The complete OBU: header, optional extension, size field and payload.
    bytes: &'a [u8],
    payload: &'a [u8],
}

#[derive(Default)]
pub struct TemporalUnits {
    sequence_header: Option<Vec<u8>>,
    reduced_still_picture_header: bool,
}

impl TemporalUnits {
    /// Returns the normalized temporal unit and whether it is a key frame.
    pub fn frame(&mut self, bytes: &[u8]) -> Result<(Vec<u8>, bool), &'static str> {
        let obus = parse(bytes)?;
        if obus
            .iter()
            .skip(1)
            .any(|obu| obu.kind == OBU_TEMPORAL_DELIMITER)
        {
            return Err("av1_multiple_temporal_units");
        }
        let sequence_header = obus.iter().find(|obu| obu.kind == OBU_SEQUENCE_HEADER);
        let reduced_still_picture_header = match sequence_header {
            Some(header) => reduced_still_picture_header(header.payload)?,
            None => self.reduced_still_picture_header,
        };
        if sequence_header.is_none() && self.sequence_header.is_none() {
            return Err("av1_sequence_header_missing");
        }
        let frame = obus
            .iter()
            .find(|obu| matches!(obu.kind, OBU_FRAME | OBU_FRAME_HEADER))
            .ok_or("av1_frame_missing")?;
        let keyframe = reduced_still_picture_header || is_key_frame(frame.payload)?;

        let mut output = Vec::with_capacity(bytes.len() + 2);
        output.extend_from_slice(&TEMPORAL_DELIMITER);
        if keyframe && sequence_header.is_none() {
            let cached = self
                .sequence_header
                .as_deref()
                .ok_or("av1_sequence_header_missing")?;
            output.extend_from_slice(cached);
        }
        for obu in obus.iter().filter(|obu| obu.kind != OBU_TEMPORAL_DELIMITER) {
            output.extend_from_slice(obu.bytes);
        }
        if output.len() > MAX_PACKET_BYTES {
            return Err("av1_packet_limit");
        }
        if let Some(header) = sequence_header {
            self.sequence_header = Some(header.bytes.to_vec());
            self.reduced_still_picture_header = reduced_still_picture_header;
        }
        Ok((output, keyframe))
    }
}

fn parse(bytes: &[u8]) -> Result<Vec<Obu<'_>>, &'static str> {
    if bytes.is_empty() || bytes.len() > MAX_PACKET_BYTES {
        return Err("invalid_av1_temporal_unit");
    }
    let mut obus = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if obus.len() >= MAX_OBUS {
            return Err("invalid_av1_temporal_unit");
        }
        let (obu, length) = parse_obu(&bytes[offset..])?;
        obus.push(obu);
        offset += length;
    }
    Ok(obus)
}

fn parse_obu(bytes: &[u8]) -> Result<(Obu<'_>, usize), &'static str> {
    let header = *bytes.first().ok_or("invalid_av1_obu")?;
    let forbidden = header & 0x80 != 0;
    let kind = (header >> 3) & 0x0f;
    let has_extension = header & 0x04 != 0;
    let has_size = header & 0x02 != 0;
    if forbidden || header & 0x01 != 0 {
        return Err("invalid_av1_obu");
    }
    if !has_size {
        return Err("av1_obu_size_missing");
    }
    if !matches!(
        kind,
        OBU_SEQUENCE_HEADER
            | OBU_TEMPORAL_DELIMITER
            | OBU_FRAME_HEADER
            | OBU_TILE_GROUP
            | OBU_METADATA
            | OBU_FRAME
            | OBU_REDUNDANT_FRAME_HEADER
            | OBU_PADDING
    ) {
        return Err("av1_obu_type_unsupported");
    }
    let mut offset = 1 + usize::from(has_extension);
    if offset > bytes.len() {
        return Err("invalid_av1_obu");
    }
    let (size, size_length) = leb128(&bytes[offset..])?;
    offset += size_length;
    let end = offset.checked_add(size).ok_or("invalid_av1_obu")?;
    let payload = bytes.get(offset..end).ok_or("invalid_av1_obu")?;
    Ok((
        Obu {
            kind,
            bytes: &bytes[..end],
            payload,
        },
        end,
    ))
}

/// AV1 leb128(): at most eight bytes, value below 2^32.
fn leb128(bytes: &[u8]) -> Result<(usize, usize), &'static str> {
    let mut value: u64 = 0;
    for (index, byte) in bytes.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            let value = u32::try_from(value).map_err(|_| "invalid_av1_obu")?;
            return Ok((value as usize, index + 1));
        }
    }
    Err("invalid_av1_obu")
}

/// sequence_header_obu(): seq_profile f(3), still_picture f(1),
/// reduced_still_picture_header f(1).
fn reduced_still_picture_header(payload: &[u8]) -> Result<bool, &'static str> {
    let first = *payload.first().ok_or("invalid_av1_sequence_header")?;
    Ok(first & 0x08 != 0)
}

/// uncompressed_header() without a reduced still-picture header begins with
/// show_existing_frame f(1) and frame_type f(2); KEY_FRAME is 0. A shown
/// existing frame is not a new random access point here.
fn is_key_frame(payload: &[u8]) -> Result<bool, &'static str> {
    let first = *payload.first().ok_or("invalid_av1_frame_header")?;
    let show_existing_frame = first & 0x80 != 0;
    let frame_type = (first >> 5) & 0x03;
    Ok(!show_existing_frame && frame_type == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 128);
        let mut bytes = vec![(kind << 3) | 0x02, payload.len() as u8];
        bytes.extend_from_slice(payload);
        bytes
    }

    fn unit(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    const KEY_FRAME: [u8; 3] = [0x00, 0xaa, 0xbb];
    const INTER_FRAME: [u8; 3] = [0x20, 0xcc, 0xdd];
    const SEQUENCE: [u8; 4] = [0x00, 0x00, 0x00, 0x42];

    #[test]
    fn key_frames_keep_their_delimiter_and_sequence_header() {
        let mut units = TemporalUnits::default();
        let input = unit(&[
            obu(OBU_TEMPORAL_DELIMITER, &[]),
            obu(OBU_SEQUENCE_HEADER, &SEQUENCE),
            obu(OBU_FRAME, &KEY_FRAME),
        ]);
        let (output, keyframe) = units.frame(&input).unwrap();
        assert!(keyframe);
        assert_eq!(output, input);
    }

    #[test]
    fn a_missing_delimiter_is_inserted_and_inter_frames_are_not_keys() {
        let mut units = TemporalUnits::default();
        units
            .frame(&unit(&[
                obu(OBU_SEQUENCE_HEADER, &SEQUENCE),
                obu(OBU_FRAME, &KEY_FRAME),
            ]))
            .unwrap();
        let (output, keyframe) = units.frame(&obu(OBU_FRAME, &INTER_FRAME)).unwrap();
        assert!(!keyframe);
        assert_eq!(
            output,
            unit(&[TEMPORAL_DELIMITER.to_vec(), obu(OBU_FRAME, &INTER_FRAME)])
        );
    }

    #[test]
    fn a_key_frame_without_a_sequence_header_gets_the_cached_one() {
        let mut units = TemporalUnits::default();
        units
            .frame(&unit(&[
                obu(OBU_SEQUENCE_HEADER, &SEQUENCE),
                obu(OBU_FRAME, &KEY_FRAME),
            ]))
            .unwrap();
        let (output, keyframe) = units
            .frame(&unit(&[
                obu(OBU_TEMPORAL_DELIMITER, &[]),
                obu(OBU_FRAME_HEADER, &KEY_FRAME),
                obu(OBU_TILE_GROUP, &[1, 2, 3]),
            ]))
            .unwrap();
        assert!(keyframe);
        assert_eq!(
            output,
            unit(&[
                obu(OBU_TEMPORAL_DELIMITER, &[]),
                obu(OBU_SEQUENCE_HEADER, &SEQUENCE),
                obu(OBU_FRAME_HEADER, &KEY_FRAME),
                obu(OBU_TILE_GROUP, &[1, 2, 3]),
            ])
        );
    }

    #[test]
    fn malformed_or_ambiguous_units_fail_closed() {
        let mut units = TemporalUnits::default();
        assert_eq!(
            units.frame(&obu(OBU_FRAME, &KEY_FRAME)),
            Err("av1_sequence_header_missing")
        );
        let unsized_obu = [OBU_FRAME << 3, 0x00, 0x00];
        assert_eq!(units.frame(&unsized_obu), Err("av1_obu_size_missing"));
        let truncated = [(OBU_FRAME << 3) | 0x02, 0x05, 0x00];
        assert_eq!(units.frame(&truncated), Err("invalid_av1_obu"));
        let two_units = unit(&[
            obu(OBU_TEMPORAL_DELIMITER, &[]),
            obu(OBU_SEQUENCE_HEADER, &SEQUENCE),
            obu(OBU_FRAME, &KEY_FRAME),
            obu(OBU_TEMPORAL_DELIMITER, &[]),
            obu(OBU_FRAME, &INTER_FRAME),
        ]);
        assert_eq!(units.frame(&two_units), Err("av1_multiple_temporal_units"));
        let header_only = unit(&[obu(OBU_SEQUENCE_HEADER, &SEQUENCE)]);
        assert_eq!(units.frame(&header_only), Err("av1_frame_missing"));
        let reserved_type = obu(9, &[0]);
        assert_eq!(units.frame(&reserved_type), Err("av1_obu_type_unsupported"));
        assert_eq!(units.frame(&[]), Err("invalid_av1_temporal_unit"));
    }

    #[test]
    fn multi_byte_sizes_and_extensions_are_parsed() {
        let payload = vec![0x20; 300];
        let mut extended = vec![(OBU_FRAME << 3) | 0x06, 0x00, 0xac, 0x02];
        extended.extend_from_slice(&payload);
        let (parsed, length) = parse_obu(&extended).unwrap();
        assert_eq!(length, extended.len());
        assert_eq!(parsed.payload.len(), 300);
        assert_eq!(
            leb128(&[0xff, 0xff, 0xff, 0xff, 0x0f]),
            Ok((u32::MAX as usize, 5))
        );
        assert!(leb128(&[0xff, 0xff, 0xff, 0xff, 0x1f]).is_err());
        assert!(leb128(&[0x80; 8]).is_err());
    }

    /// Two temporal units written by FFmpeg's `obu` muxer from libaom-av1
    /// (64x64 testsrc, realtime usage); the first is a key frame.
    #[test]
    fn libaom_reference_stream_frames_as_two_temporal_units() {
        let stream = include_bytes!("fixtures/libaom_two_frames.obu");
        let mut starts = Vec::new();
        let mut offset = 0;
        while offset < stream.len() {
            let (obu, length) = parse_obu(&stream[offset..]).unwrap();
            if obu.kind == OBU_TEMPORAL_DELIMITER {
                starts.push(offset);
            }
            offset += length;
        }
        assert_eq!(starts.len(), 2);
        let mut units = TemporalUnits::default();
        let first = &stream[starts[0]..starts[1]];
        let second = &stream[starts[1]..];
        let (output, keyframe) = units.frame(first).unwrap();
        assert!(keyframe);
        assert_eq!(output, first);
        let (output, keyframe) = units.frame(second).unwrap();
        assert!(!keyframe);
        assert_eq!(output, second);
    }
}

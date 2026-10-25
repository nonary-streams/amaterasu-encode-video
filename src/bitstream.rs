//! Normalize length-prefixed MF samples to Annex B and retain parameter sets.
const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub struct Normalizer {
    length_prefixed: bool,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl Normalizer {
    #[cfg(any(windows, test, amaterasu_ffmpeg_nvenc, amaterasu_ffmpeg_vaapi))]
    pub fn header(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        if bytes.is_empty() {
            return Ok(());
        }
        let length_prefixed = bytes[0] == 1;
        let units = if length_prefixed {
            configuration_units(bytes)?
        } else {
            units(bytes, false)?
        };
        self.length_prefixed = length_prefixed;
        self.remember(&units);
        Ok(())
    }
    pub fn sample(&mut self, bytes: &[u8]) -> Result<(Vec<u8>, bool), &'static str> {
        let units = units(bytes, self.length_prefixed)?;
        let idr = units.iter().any(|unit| unit[0] & 31 == 5);
        let mut output = Vec::new();
        if idr {
            for (kind, parameter) in [(7, &self.sps), (8, &self.pps)] {
                if !units.iter().any(|unit| unit[0] & 31 == kind) {
                    append(
                        &mut output,
                        parameter.as_deref().ok_or("missing_parameter_sets")?,
                    )?;
                }
            }
        }
        for unit in &units {
            append(&mut output, unit)?;
        }
        self.remember(&units);
        Ok((output, idr))
    }
    fn remember(&mut self, units: &[&[u8]]) {
        for unit in units {
            match unit[0] & 31 {
                7 => self.sps = Some(unit.to_vec()),
                8 => self.pps = Some(unit.to_vec()),
                _ => {}
            }
        }
    }
}

fn append(output: &mut Vec<u8>, unit: &[u8]) -> Result<(), &'static str> {
    if output.len() + 4 + unit.len() > MAX_PACKET_BYTES {
        return Err("packet_limit");
    }
    output.extend_from_slice(&[0, 0, 0, 1]);
    output.extend_from_slice(unit);
    Ok(())
}

fn start_code(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index..index + 4) == Some(&[0, 0, 0, 1]) {
        Some(4)
    } else if bytes.get(index..index + 3) == Some(&[0, 0, 1]) {
        Some(3)
    } else {
        None
    }
}

fn units(bytes: &[u8], length_prefixed: bool) -> Result<Vec<&[u8]>, &'static str> {
    if bytes.is_empty() || bytes.len() > MAX_PACKET_BYTES {
        return Err("invalid_h264");
    }
    let mut output = Vec::new();
    let mut offset = 0;
    if !length_prefixed && start_code(bytes, 0).is_some() {
        while offset < bytes.len() {
            if output.len() >= 1024 {
                return Err("invalid_h264");
            }
            let start = offset + start_code(bytes, offset).ok_or("invalid_h264")?;
            let mut end = start;
            while end < bytes.len() && start_code(bytes, end).is_none() {
                end += 1;
            }
            let mut payload_end = end;
            while payload_end > start && bytes[payload_end - 1] == 0 {
                payload_end -= 1;
            }
            output.push(&bytes[start..payload_end]);
            offset = end;
        }
    } else {
        while offset < bytes.len() {
            if output.len() >= 1024 {
                return Err("invalid_h264");
            }
            let length = u32::from_be_bytes(
                bytes
                    .get(offset..offset + 4)
                    .ok_or("invalid_h264")?
                    .try_into()
                    .map_err(|_| "invalid_h264")?,
            ) as usize;
            offset += 4;
            let end = offset.checked_add(length).ok_or("invalid_h264")?;
            output.push(bytes.get(offset..end).ok_or("invalid_h264")?);
            offset = end;
        }
    }
    validate_units(&output)?;
    Ok(output)
}

fn validate_units(units: &[&[u8]]) -> Result<(), &'static str> {
    if units.len() > 1024
        || units.iter().any(|unit| {
            unit.is_empty() || unit[0] & 128 != 0 || !(1..=23).contains(&(unit[0] & 31))
        })
    {
        return Err("invalid_h264");
    }
    Ok(())
}

#[cfg(any(windows, test, amaterasu_ffmpeg_nvenc, amaterasu_ffmpeg_vaapi))]
fn configuration_units(bytes: &[u8]) -> Result<Vec<&[u8]>, &'static str> {
    // AVCDecoderConfigurationRecord with four-byte NAL lengths. Other length
    // widths are deliberately rejected rather than guessed from sample bytes.
    if bytes.len() < 7 || bytes[4] & 3 != 3 {
        return Err("unsupported_avcc");
    }
    let mut offset = 6;
    let mut output = Vec::new();
    for group in 0..2 {
        let count = if group == 0 {
            bytes[5] & 31
        } else {
            let value = *bytes.get(offset).ok_or("invalid_h264")?;
            offset += 1;
            value
        };
        for _ in 0..count {
            let length = u16::from_be_bytes(
                bytes
                    .get(offset..offset + 2)
                    .ok_or("invalid_h264")?
                    .try_into()
                    .map_err(|_| "invalid_h264")?,
            ) as usize;
            offset += 2;
            output.push(bytes.get(offset..offset + length).ok_or("invalid_h264")?);
            offset += length;
        }
    }
    if offset != bytes.len() {
        return Err("unsupported_avcc");
    }
    validate_units(&output)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalizes_avcc_and_supplies_parameter_sets_for_idr() {
        let mut normalizer = Normalizer::default();
        normalizer
            .header(&[1, 66, 0, 30, 255, 225, 0, 2, 0x67, 1, 1, 0, 2, 0x68, 2])
            .unwrap();
        let (bytes, idr) = normalizer.sample(&[0, 0, 0, 2, 0x65, 3]).unwrap();
        assert!(idr);
        assert_eq!(
            bytes,
            [
                0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3
            ]
        );
        let mut annex_b = Normalizer::default();
        assert_eq!(annex_b.sample(&bytes).unwrap(), (bytes, true));
        let mut ambiguous = vec![0, 0, 1, 0];
        ambiguous.extend_from_slice(&[0x41; 256]);
        assert_eq!(normalizer.sample(&ambiguous).unwrap().0.len(), 260);
    }
    #[test]
    fn rejects_truncated_nals_and_missing_idr_parameters() {
        let mut normalizer = Normalizer::default();
        assert!(normalizer.sample(&[0, 0, 0, 8, 0x41]).is_err());
        assert!(normalizer.sample(&[0, 0, 1]).is_err());
        assert!(normalizer.sample(&[0, 0, 1, 0x65, 1]).is_err());
        assert!(normalizer.header(&[1, 66, 0, 30, 252, 0, 0]).is_err());
    }

    #[test]
    fn failed_update_preserves_cached_parameter_sets_and_format() {
        let mut normalizer = Normalizer::default();
        normalizer
            .header(&[0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2])
            .unwrap();

        let mut oversized_output = vec![
            0, 0, 1, 0x67, 9, // replacement SPS
            0, 0, 1, 0x68, 8, // replacement PPS
            0, 0, 1, 0x65, // IDR
        ];
        oversized_output.resize(MAX_PACKET_BYTES - 1, 0x41);
        assert_eq!(normalizer.sample(&oversized_output), Err("packet_limit"));

        assert_eq!(
            normalizer.header(&[1, 66, 0, 30, 252, 0, 0]),
            Err("unsupported_avcc")
        );
        let (packet, idr) = normalizer.sample(&[0, 0, 1, 0x65, 3]).unwrap();
        assert!(idr);
        assert_eq!(
            packet,
            [
                0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3
            ]
        );
    }
}

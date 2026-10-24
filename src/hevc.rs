//! Bounded Annex B handling for single-layer HEVC access units.
const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
const MAX_NALS: usize = 1024;

#[derive(Default)]
pub struct Normalizer {
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl Normalizer {
    pub fn header(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        let units = annex_b_units(bytes)?;
        let mut candidate = self.clone_sets();
        remember(&mut candidate, &units);
        if candidate.0.is_none() || candidate.1.is_none() || candidate.2.is_none() {
            return Err("hevc_parameter_sets_missing");
        }
        self.vps = candidate.0;
        self.sps = candidate.1;
        self.pps = candidate.2;
        Ok(())
    }

    pub fn sample(&mut self, bytes: &[u8]) -> Result<(Vec<u8>, bool), &'static str> {
        let units = annex_b_units(bytes)?;
        if units
            .iter()
            .any(|unit| matches!(nal_type(unit), 16..=18 | 21..=23))
        {
            return Err("hevc_random_access_unsupported");
        }
        let irap = units.iter().any(|unit| matches!(nal_type(unit), 19 | 20));
        let has_non_idr_picture = units.iter().any(|unit| (0..=7).contains(&nal_type(unit)));
        if !irap && !has_non_idr_picture || irap && has_non_idr_picture {
            return Err("invalid_hevc_access_unit");
        }
        if !irap && units.iter().any(|unit| matches!(nal_type(unit), 32..=34)) {
            return Err("invalid_hevc_access_unit");
        }
        let mut candidate = self.clone_sets();
        remember(&mut candidate, &units);
        let mut output = Vec::with_capacity(bytes.len().saturating_add(256));
        if irap {
            for cached in [&candidate.0, &candidate.1, &candidate.2] {
                append(
                    &mut output,
                    cached.as_deref().ok_or("hevc_parameter_sets_missing")?,
                )?;
            }
        }
        for unit in &units {
            if !irap || !matches!(nal_type(unit), 32..=34) {
                append(&mut output, unit)?;
            }
        }
        self.vps = candidate.0;
        self.sps = candidate.1;
        self.pps = candidate.2;
        Ok((output, irap))
    }

    fn clone_sets(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>) {
        (self.vps.clone(), self.sps.clone(), self.pps.clone())
    }
}

fn nal_type(unit: &[u8]) -> u8 {
    (unit[0] >> 1) & 0x3f
}

fn remember(sets: &mut (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>), units: &[&[u8]]) {
    for unit in units {
        match nal_type(unit) {
            32 => sets.0 = Some(unit.to_vec()),
            33 => sets.1 = Some(unit.to_vec()),
            34 => sets.2 = Some(unit.to_vec()),
            _ => {}
        }
    }
}

fn append(output: &mut Vec<u8>, unit: &[u8]) -> Result<(), &'static str> {
    let size = output
        .len()
        .checked_add(4)
        .and_then(|length| length.checked_add(unit.len()))
        .ok_or("hevc_packet_limit")?;
    if size > MAX_PACKET_BYTES {
        return Err("hevc_packet_limit");
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

fn annex_b_units(bytes: &[u8]) -> Result<Vec<&[u8]>, &'static str> {
    if bytes.is_empty() || bytes.len() > MAX_PACKET_BYTES || start_code(bytes, 0).is_none() {
        return Err("invalid_hevc_annex_b");
    }
    let mut units = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if units.len() >= MAX_NALS {
            return Err("invalid_hevc_annex_b");
        }
        let start = offset + start_code(bytes, offset).ok_or("invalid_hevc_annex_b")?;
        let mut end = start;
        while end < bytes.len() && start_code(bytes, end).is_none() {
            end += 1;
        }
        let mut payload_end = end;
        while payload_end > start && bytes[payload_end - 1] == 0 {
            payload_end -= 1;
        }
        let unit = &bytes[start..payload_end];
        if unit.len() < 3
            || unit[0] & 0x80 != 0
            || unit[1] & 7 == 0
            || !supported_nal_type((unit[0] >> 1) & 0x3f)
            || (((unit[0] & 1) << 5) | ((unit[1] >> 3) & 0x1f)) != 0
        {
            return Err("invalid_hevc_annex_b");
        }
        units.push(unit);
        offset = end;
    }
    if units.is_empty() {
        return Err("invalid_hevc_annex_b");
    }
    Ok(units)
}

fn supported_nal_type(kind: u8) -> bool {
    matches!(kind, 0..=7 | 19 | 20 | 32..=35 | 39 | 40)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(kind: u8, body: u8) -> Vec<u8> {
        vec![0, 0, 0, 1, kind << 1, 1, body]
    }

    #[test]
    fn caches_and_repeats_vps_sps_pps_before_irap() {
        let mut headers = nal(32, 1);
        headers.extend(nal(33, 2));
        headers.extend(nal(34, 3));
        let mut normalizer = Normalizer::default();
        normalizer.header(&headers).unwrap();

        let (packet, irap) = normalizer.sample(&nal(19, 9)).unwrap();
        assert!(irap);
        assert_eq!(&packet[..headers.len()], headers.as_slice());
        assert_eq!(&packet[headers.len()..], nal(19, 9).as_slice());
        assert_eq!(normalizer.sample(&nal(1, 8)).unwrap().1, false);
        assert_eq!(
            normalizer.sample(&nal(39, 7)),
            Err("invalid_hevc_access_unit")
        );
        let mut mixed_picture = nal(19, 7);
        mixed_picture.extend(nal(1, 8));
        assert_eq!(
            normalizer.sample(&mixed_picture),
            Err("invalid_hevc_access_unit")
        );
    }

    #[test]
    fn updates_parameter_sets_and_rejects_incomplete_or_malformed_input() {
        let mut normalizer = Normalizer::default();
        assert_eq!(
            normalizer.header(&nal(33, 1)),
            Err("hevc_parameter_sets_missing")
        );
        assert!(normalizer.sample(&nal(19, 4)).is_err());

        let mut all = nal(32, 1);
        all.extend(nal(33, 2));
        all.extend(nal(34, 3));
        normalizer.header(&all).unwrap();
        assert_eq!(
            normalizer.sample(&[0, 0, 1, 0x80, 1, 0]).unwrap_err(),
            "invalid_hevc_annex_b"
        );
        let too_many = (0..MAX_NALS + 1)
            .flat_map(|_| [0, 0, 1, 0x02, 0x01, 0])
            .collect::<Vec<_>>();
        assert!(normalizer.sample(&too_many).is_err());
    }

    #[test]
    fn rejects_cra_and_nonzero_layer_ids_instead_of_claiming_closed_gop_recovery() {
        let mut all = nal(32, 1);
        all.extend(nal(33, 2));
        all.extend(nal(34, 3));
        let mut normalizer = Normalizer::default();
        normalizer.header(&all).unwrap();
        for unsupported in [16, 18, 21, 23] {
            assert_eq!(
                normalizer.sample(&nal(unsupported, 9)),
                Err("hevc_random_access_unsupported")
            );
        }

        let multilayer = [0, 0, 1, 0x03, 0x09, 0x80];
        assert_eq!(normalizer.sample(&multilayer), Err("invalid_hevc_annex_b"));
    }
}

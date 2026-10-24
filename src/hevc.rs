//! Bounded Annex B handling for single-layer HEVC access units.
const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
const MAX_NALS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpsSignal {
    pub profile_idc: u8,
    pub chroma_format_idc: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub full_range: bool,
}

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

    pub fn validate_main_bt709_limited(&self) -> Result<(), &'static str> {
        let sps = self.sps.as_deref().ok_or("hevc_parameter_sets_missing")?;
        let signal = parse_sps_signal(sps)?;
        if signal.profile_idc != 1
            || signal.chroma_format_idc != 1
            || signal.bit_depth_luma_minus8 != 0
            || signal.bit_depth_chroma_minus8 != 0
            || signal.color_primaries != 1
            || signal.transfer_characteristics != 1
            || signal.matrix_coefficients != 1
            || signal.full_range
        {
            return Err("hevc_sps_configuration_unsupported");
        }
        Ok(())
    }

    fn clone_sets(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>) {
        (self.vps.clone(), self.sps.clone(), self.pps.clone())
    }
}

pub fn parse_sps_signal(nal: &[u8]) -> Result<SpsSignal, &'static str> {
    if nal.len() < 3 || nal_type(nal) != 33 {
        return Err("invalid_hevc_sps");
    }
    let rbsp = unescape_rbsp(&nal[2..])?;
    let mut bits = BitReader::new(&rbsp);
    bits.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = bits.read_bits(3)? as usize;
    if max_sub_layers_minus1 > 6 {
        return Err("invalid_hevc_sps");
    }
    bits.skip(1)?; // sps_temporal_id_nesting_flag

    let profile_space = bits.read_bits(2)?;
    bits.skip(1)?; // general_tier_flag
    let profile_idc = bits.read_bits(5)? as u8;
    if profile_space != 0 {
        return Err("hevc_sps_configuration_unsupported");
    }
    bits.skip(32 + 48 + 8)?; // compatibility, constraints, level
    let mut sub_layer_profile_present = [false; 7];
    let mut sub_layer_level_present = [false; 7];
    for index in 0..max_sub_layers_minus1 {
        sub_layer_profile_present[index] = bits.read_flag()?;
        sub_layer_level_present[index] = bits.read_flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            bits.skip(2)?; // reserved_zero_2bits
        }
    }
    for index in 0..max_sub_layers_minus1 {
        if sub_layer_profile_present[index] {
            bits.skip(2 + 1 + 5 + 32 + 48)?;
        }
        if sub_layer_level_present[index] {
            bits.skip(8)?;
        }
    }

    bits.read_ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = bits.read_ue()?;
    if chroma_format_idc > 3 {
        return Err("invalid_hevc_sps");
    }
    if chroma_format_idc == 3 {
        bits.skip(1)?; // separate_colour_plane_flag
    }
    let width = bits.read_ue()?;
    let height = bits.read_ue()?;
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err("invalid_hevc_sps");
    }
    if bits.read_flag()? {
        for _ in 0..4 {
            bits.read_ue()?; // conf_win_*_offset
        }
    }
    let bit_depth_luma_minus8 = bits.read_ue()?;
    let bit_depth_chroma_minus8 = bits.read_ue()?;
    if bit_depth_luma_minus8 > 8 || bit_depth_chroma_minus8 > 8 {
        return Err("invalid_hevc_sps");
    }
    let log2_max_pic_order_cnt_lsb_minus4 = bits.read_ue()?;
    if log2_max_pic_order_cnt_lsb_minus4 > 12 {
        return Err("invalid_hevc_sps");
    }
    let ordering_all_layers = bits.read_flag()?;
    let first_layer = if ordering_all_layers {
        0
    } else {
        max_sub_layers_minus1
    };
    for _ in first_layer..=max_sub_layers_minus1 {
        if bits.read_ue()? > 16 || bits.read_ue()? > 16 {
            return Err("invalid_hevc_sps");
        }
        bits.read_ue()?;
    }

    for _ in 0..6 {
        bits.read_ue()?; // coding/transform block and hierarchy sizes
    }
    if bits.read_flag()? && bits.read_flag()? {
        skip_scaling_list_data(&mut bits)?;
    }
    bits.skip(2)?; // amp_enabled_flag, sample_adaptive_offset_enabled_flag
    if bits.read_flag()? {
        bits.skip(8)?;
        bits.read_ue()?;
        bits.read_ue()?;
        bits.skip(1)?;
    }
    let short_term_sets = bits.read_ue()? as usize;
    if short_term_sets > 64 {
        return Err("invalid_hevc_sps");
    }
    let mut delta_poc_counts = Vec::with_capacity(short_term_sets);
    for index in 0..short_term_sets {
        delta_poc_counts.push(skip_short_term_ref_pic_set(
            &mut bits,
            index,
            &delta_poc_counts,
        )?);
    }
    if bits.read_flag()? {
        let long_term_count = bits.read_ue()? as usize;
        if long_term_count > 32 {
            return Err("invalid_hevc_sps");
        }
        let poc_bits = log2_max_pic_order_cnt_lsb_minus4 + 4;
        for _ in 0..long_term_count {
            bits.skip(poc_bits as usize + 1)?;
        }
    }
    bits.skip(2)?; // sps_temporal_mvp_enabled_flag, strong_intra_smoothing_enabled_flag
    if !bits.read_flag()? {
        return Err("hevc_sps_vui_missing");
    }
    skip_vui_prefix(&mut bits)?;
    if !bits.read_flag()? {
        return Err("hevc_sps_color_signal_missing");
    }
    bits.skip(3)?; // video_format
    let full_range = bits.read_flag()?;
    if !bits.read_flag()? {
        return Err("hevc_sps_color_signal_missing");
    }
    let color_primaries = bits.read_bits(8)? as u8;
    let transfer_characteristics = bits.read_bits(8)? as u8;
    let matrix_coefficients = bits.read_bits(8)? as u8;
    Ok(SpsSignal {
        profile_idc,
        chroma_format_idc,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        color_primaries,
        transfer_characteristics,
        matrix_coefficients,
        full_range,
    })
}

fn unescape_rbsp(ebsp: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut rbsp = Vec::with_capacity(ebsp.len());
    let mut zero_count = 0usize;
    let mut index = 0usize;
    while index < ebsp.len() {
        let byte = ebsp[index];
        if zero_count >= 2 && byte == 3 {
            let next = *ebsp.get(index + 1).ok_or("invalid_hevc_sps")?;
            if next > 3 {
                return Err("invalid_hevc_sps");
            }
            index += 1;
            continue;
        }
        rbsp.push(byte);
        zero_count = if byte == 0 { zero_count + 1 } else { 0 };
        index += 1;
    }
    Ok(rbsp)
}

struct BitReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn read_bits(&mut self, count: usize) -> Result<u32, &'static str> {
        if count > 32
            || self
                .position
                .checked_add(count)
                .is_none_or(|end| end > self.bytes.len() * 8)
        {
            return Err("invalid_hevc_sps");
        }
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1)
                | u32::from((self.bytes[self.position / 8] >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(value)
    }

    fn read_flag(&mut self) -> Result<bool, &'static str> {
        Ok(self.read_bits(1)? != 0)
    }

    fn skip(&mut self, count: usize) -> Result<(), &'static str> {
        if self
            .position
            .checked_add(count)
            .is_none_or(|end| end > self.bytes.len() * 8)
        {
            return Err("invalid_hevc_sps");
        }
        self.position += count;
        Ok(())
    }

    fn read_ue(&mut self) -> Result<u32, &'static str> {
        let mut leading_zeroes = 0;
        while !self.read_flag()? {
            leading_zeroes += 1;
            if leading_zeroes > 31 {
                return Err("invalid_hevc_sps");
            }
        }
        let suffix = self.read_bits(leading_zeroes)?;
        Ok(((1u64 << leading_zeroes) - 1 + u64::from(suffix)) as u32)
    }

    fn read_se(&mut self) -> Result<i32, &'static str> {
        let code = self.read_ue()?;
        let magnitude = (u64::from(code) + 1) / 2;
        if magnitude > i32::MAX as u64 {
            return Err("invalid_hevc_sps");
        }
        Ok(if code & 1 == 0 {
            -(magnitude as i32)
        } else {
            magnitude as i32
        })
    }
}

fn skip_scaling_list_data(bits: &mut BitReader<'_>) -> Result<(), &'static str> {
    for size_id in 0..4 {
        let step = if size_id == 3 { 3 } else { 1 };
        for _matrix_id in (0..6).step_by(step) {
            if !bits.read_flag()? {
                bits.read_ue()?;
                continue;
            }
            if size_id > 1 {
                bits.read_se()?;
            }
            let coefficient_count = 1usize << (4 + 2 * size_id);
            for _ in 0..coefficient_count.min(64) {
                bits.read_se()?;
            }
        }
    }
    Ok(())
}

fn skip_short_term_ref_pic_set(
    bits: &mut BitReader<'_>,
    index: usize,
    previous_delta_counts: &[usize],
) -> Result<usize, &'static str> {
    if index > 0 && bits.read_flag()? {
        bits.skip(1)?;
        bits.read_ue()?;
        let reference_index = index - 1;
        let reference_count = *previous_delta_counts
            .get(reference_index)
            .ok_or("invalid_hevc_sps")?;
        let mut count = 0;
        for _ in 0..=reference_count {
            let used = bits.read_flag()?;
            let use_delta = if used { true } else { bits.read_flag()? };
            count += usize::from(used || use_delta);
        }
        return Ok(count);
    }

    let negative_count = bits.read_ue()? as usize;
    let positive_count = bits.read_ue()? as usize;
    let total_count = negative_count
        .checked_add(positive_count)
        .filter(|count| *count <= 64)
        .ok_or("invalid_hevc_sps")?;
    for _ in 0..total_count {
        bits.read_ue()?;
        bits.skip(1)?;
    }
    Ok(total_count)
}

fn skip_vui_prefix(bits: &mut BitReader<'_>) -> Result<(), &'static str> {
    if bits.read_flag()? {
        let aspect_ratio_idc = bits.read_bits(8)?;
        if aspect_ratio_idc == 255 {
            bits.skip(32)?;
        }
    }
    if bits.read_flag()? {
        bits.skip(1)?;
    }
    if !bits.read_flag()? {
        return Err("hevc_sps_color_signal_missing");
    }
    Ok(())
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
    // Admit unsupported random-access types to the access-unit classifier so
    // it can report the specific closed-GOP limitation instead of a framing
    // error. Other unhandled types remain rejected here.
    matches!(kind, 0..=7 | 16..=20 | 21..=23 | 32..=35 | 39 | 40)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BitWriter {
        bytes: Vec<u8>,
        bit_count: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                bit_count: 0,
            }
        }

        fn bits(&mut self, value: u32, count: usize) {
            for shift in (0..count).rev() {
                if self.bit_count % 8 == 0 {
                    self.bytes.push(0);
                }
                let bit = (value >> shift) & 1;
                let offset = 7 - self.bit_count % 8;
                *self.bytes.last_mut().unwrap() |= (bit as u8) << offset;
                self.bit_count += 1;
            }
        }

        fn ue(&mut self, value: u32) {
            let code = value + 1;
            let width = 32 - code.leading_zeros() as usize;
            self.bits(0, width - 1);
            self.bits(code, width);
        }

        fn finish(mut self) -> Vec<u8> {
            self.bits(1, 1); // rbsp_stop_one_bit
            while self.bit_count % 8 != 0 {
                self.bits(0, 1);
            }
            self.bytes
        }
    }

    fn escaped_sps(
        profile_idc: u8,
        luma_depth_minus8: u32,
        full_range: bool,
        matrix: u8,
    ) -> Vec<u8> {
        let mut bits = BitWriter::new();
        bits.bits(0, 4); // sps_video_parameter_set_id
        bits.bits(0, 3); // sps_max_sub_layers_minus1
        bits.bits(1, 1); // sps_temporal_id_nesting_flag
        bits.bits(0, 2); // general_profile_space
        bits.bits(0, 1); // general_tier_flag
        bits.bits(u32::from(profile_idc), 5);
        bits.bits(0, 32); // general_profile_compatibility_flags
        bits.bits(0, 48); // general_constraint_indicator_flags
        bits.bits(120, 8); // general_level_idc
        bits.ue(0); // sps_seq_parameter_set_id
        bits.ue(1); // chroma_format_idc: 4:2:0
        bits.ue(64); // pic_width_in_luma_samples
        bits.ue(64); // pic_height_in_luma_samples
        bits.bits(0, 1); // conformance_window_flag
        bits.ue(luma_depth_minus8);
        bits.ue(0); // bit_depth_chroma_minus8
        bits.ue(0); // log2_max_pic_order_cnt_lsb_minus4
        bits.bits(0, 1); // sps_sub_layer_ordering_info_present_flag
        bits.ue(0); // max_dec_pic_buffering_minus1
        bits.ue(0); // max_num_reorder_pics
        bits.ue(0); // max_latency_increase_plus1
        for _ in 0..6 {
            bits.ue(0);
        }
        bits.bits(0, 1); // scaling_list_enabled_flag
        bits.bits(0, 1); // amp_enabled_flag
        bits.bits(0, 1); // sample_adaptive_offset_enabled_flag
        bits.bits(0, 1); // pcm_enabled_flag
        bits.ue(0); // num_short_term_ref_pic_sets
        bits.bits(0, 1); // long_term_ref_pics_present_flag
        bits.bits(0, 1); // sps_temporal_mvp_enabled_flag
        bits.bits(1, 1); // strong_intra_smoothing_enabled_flag
        bits.bits(1, 1); // vui_parameters_present_flag
        bits.bits(0, 1); // aspect_ratio_info_present_flag
        bits.bits(0, 1); // overscan_info_present_flag
        bits.bits(1, 1); // video_signal_type_present_flag
        bits.bits(5, 3); // video_format: unspecified
        bits.bits(u32::from(full_range), 1);
        bits.bits(1, 1); // colour_description_present_flag
        bits.bits(1, 8); // colour_primaries: BT.709
        bits.bits(1, 8); // transfer_characteristics: BT.709
        bits.bits(u32::from(matrix), 8);
        let rbsp = bits.finish();
        let mut nal = vec![33 << 1, 1];
        let mut zero_count = 0;
        for byte in rbsp {
            if zero_count >= 2 && byte <= 3 {
                nal.push(3);
                zero_count = 0;
            }
            nal.push(byte);
            zero_count = if byte == 0 { zero_count + 1 } else { 0 };
        }
        nal
    }

    fn header_with_sps(sps: &[u8]) -> Vec<u8> {
        let mut header = nal(32, 1);
        header.extend_from_slice(&[0, 0, 0, 1]);
        header.extend_from_slice(sps);
        header.extend(nal(34, 3));
        header
    }

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

    #[test]
    fn parses_and_enforces_emitted_main_8bit_420_bt709_limited_sps() {
        let supported = escaped_sps(1, 0, false, 1);
        let signal = parse_sps_signal(&supported).unwrap();
        assert_eq!(signal.profile_idc, 1);
        assert_eq!(signal.chroma_format_idc, 1);
        assert_eq!(signal.bit_depth_luma_minus8, 0);
        assert_eq!(signal.bit_depth_chroma_minus8, 0);
        assert_eq!(signal.color_primaries, 1);
        assert_eq!(signal.transfer_characteristics, 1);
        assert_eq!(signal.matrix_coefficients, 1);
        assert!(!signal.full_range);

        let mut normalizer = Normalizer::default();
        normalizer.header(&header_with_sps(&supported)).unwrap();
        assert_eq!(normalizer.validate_main_bt709_limited(), Ok(()));

        for unsupported in [
            escaped_sps(2, 0, false, 1),
            escaped_sps(1, 2, false, 1),
            escaped_sps(1, 0, true, 1),
            escaped_sps(1, 0, false, 9),
        ] {
            let mut normalizer = Normalizer::default();
            normalizer.header(&header_with_sps(&unsupported)).unwrap();
            assert_eq!(
                normalizer.validate_main_bt709_limited(),
                Err("hevc_sps_configuration_unsupported")
            );
        }
    }
}

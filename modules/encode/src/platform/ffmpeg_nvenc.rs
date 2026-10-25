//! Opt-in FFmpeg `h264_nvenc` backend. FFmpeg handles are owned by the
//! encoder worker; only borrowed packet bytes cross the C ABI and are copied
//! before the next receive call.
#[cfg(amaterasu_ffmpeg_nvenc)]
use crate::bitstream::Normalizer;
use crate::model::EncoderDescription;
#[cfg(amaterasu_ffmpeg_nvenc)]
use crate::model::Packet;
#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
use crate::model::{Configuration, EncoderIdentity};
#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
use amaterasu_media::ColorMode;
#[cfg(amaterasu_ffmpeg_nvenc)]
use std::{collections::VecDeque, ffi::c_void, ptr::NonNull};

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
const ENCODER_ID: &str = "amaterasu.hardware.nvidia.nvenc.ffmpeg";
#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
const ENCODER_URL: &str = "ffmpeg://h264_nvenc";
#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
const ENCODER_NAME: &str = "NVIDIA NVENC H.264 (FFmpeg)";
#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
#[cfg(amaterasu_ffmpeg_nvenc)]
const MAX_PENDING_PACKETS: usize = 16;
#[cfg(amaterasu_ffmpeg_nvenc)]
const RECEIVE_AGAIN: i32 = 0;
#[cfg(amaterasu_ffmpeg_nvenc)]
const RECEIVE_PACKET: i32 = 1;
#[cfg(amaterasu_ffmpeg_nvenc)]
const RECEIVE_EOF: i32 = 2;
#[cfg(amaterasu_ffmpeg_nvenc)]
const SEND_RETRY_AFTER_RECEIVE: i32 = 3;

#[cfg(amaterasu_ffmpeg_nvenc)]
unsafe extern "C" {
    fn amaterasu_ffmpeg_nvenc_open(
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        max_reference_frames: i32,
        full_range: i32,
        error_code: *mut i32,
    ) -> *mut c_void;
    fn amaterasu_ffmpeg_nvenc_close(encoder: *mut c_void);
    fn amaterasu_ffmpeg_nvenc_header(
        encoder: *mut c_void,
        data: *mut *const u8,
        length: *mut usize,
    ) -> i32;
    fn amaterasu_ffmpeg_nvenc_submit(
        encoder: *mut c_void,
        nv12: *const u8,
        length: usize,
        timestamp_100ns: i64,
        force_idr: i32,
    ) -> i32;
    fn amaterasu_ffmpeg_nvenc_drain(encoder: *mut c_void) -> i32;
    fn amaterasu_ffmpeg_nvenc_receive(
        encoder: *mut c_void,
        data: *mut *const u8,
        length: *mut usize,
        timestamp_100ns: *mut i64,
        keyframe: *mut i32,
    ) -> i32;
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
fn identity() -> EncoderIdentity {
    EncoderIdentity {
        clsid: ENCODER_ID.into(),
        hardware_url: Some(ENCODER_URL.into()),
        friendly_name: ENCODER_NAME.into(),
    }
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
fn description_with_probe(mut probe: impl FnMut(ColorMode) -> bool) -> Option<EncoderDescription> {
    let supports_limited = probe(ColorMode::Bt709Limited);
    let supports_full = supports_limited && probe(ColorMode::Bt709Full);
    (supports_limited && supports_full).then(|| EncoderDescription {
        identity: identity(),
        name: ENCODER_NAME.into(),
        codec: "h264",
        hardware: true,
        vendor_id: Some(0x10de),
        capabilities: Default::default(),
    })
}

pub fn description() -> Option<EncoderDescription> {
    #[cfg(amaterasu_ffmpeg_nvenc)]
    {
        description_with_probe(probe_hardware)
    }
    #[cfg(not(amaterasu_ffmpeg_nvenc))]
    None
}

#[cfg(amaterasu_ffmpeg_nvenc)]
fn probe_hardware(color_mode: ColorMode) -> bool {
    let mut error_code = 0;
    // SAFETY: probe opens a private throwaway context with fixed bounded
    // settings and no frame data; it retains no handles after returning.
    let full_range = i32::from(color_mode == ColorMode::Bt709Full);
    let raw = unsafe {
        amaterasu_ffmpeg_nvenc_open(640, 480, 30, 1_000_000, 0, full_range, &mut error_code)
    };
    let Some(native) = NonNull::new(raw) else {
        return false;
    };

    let mut data = std::ptr::null();
    let mut length = 0usize;
    // SAFETY: extradata remains valid until this probe context is closed.
    let header_status =
        unsafe { amaterasu_ffmpeg_nvenc_header(native.as_ptr(), &mut data, &mut length) };
    let mut normalizer = Normalizer::default();
    let header_valid = header_status == 0
        && !data.is_null()
        && length > 0
        && length <= 1024 * 1024
        && normalizer
            .header(unsafe { std::slice::from_raw_parts(data, length) })
            .is_ok()
        && validate_header_color(
            unsafe { std::slice::from_raw_parts(data, length) },
            color_mode,
        )
        .is_ok();
    // SAFETY: this is the unique probe handle and is closed before returning.
    unsafe { amaterasu_ffmpeg_nvenc_close(native.as_ptr()) };
    header_valid
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
pub fn selected(candidate: &EncoderIdentity) -> bool {
    candidate == &identity()
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
fn validate_configuration(configuration: &Configuration) -> Result<(), &'static str> {
    configuration.validate()?;
    if configuration.max_reference_frames > 1 {
        return Err("nvenc_reference_frames_unsupported");
    }
    if configuration.codec() != amaterasu_media::VideoCodec::H264 {
        return Err("nvenc_codec_unsupported");
    }
    if !matches!(
        configuration.color_mode,
        ColorMode::Bt709Limited | ColorMode::Bt709Full
    ) {
        return Err("nvenc_color_mode_unsupported");
    }
    Ok(())
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SpsColorSignal {
    full_range: bool,
    primaries: u8,
    transfer: u8,
    matrix: u8,
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
fn expected_color_signal(color_mode: ColorMode) -> Result<SpsColorSignal, &'static str> {
    match color_mode {
        ColorMode::Bt709Limited => Ok(SpsColorSignal {
            full_range: false,
            primaries: 1,
            transfer: 1,
            matrix: 1,
        }),
        ColorMode::Bt709Full => Ok(SpsColorSignal {
            full_range: true,
            primaries: 1,
            transfer: 1,
            matrix: 1,
        }),
        _ => Err("nvenc_color_mode_unsupported"),
    }
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
fn validate_header_color(data: &[u8], color_mode: ColorMode) -> Result<(), &'static str> {
    let expected = expected_color_signal(color_mode)?;
    let sps_units = sps_units(data, None)?;
    if sps_units.is_empty()
        || sps_units
            .iter()
            .any(|sps| parse_sps_color_signal(sps) != Ok(expected))
    {
        return Err("nvenc_vui_mismatch");
    }
    Ok(())
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
fn validate_packet_color(
    data: &[u8],
    color_mode: ColorMode,
    length_prefixed: bool,
) -> Result<(), &'static str> {
    let expected = expected_color_signal(color_mode)?;
    for sps in sps_units(data, Some(length_prefixed))? {
        if parse_sps_color_signal(sps)? != expected {
            return Err("nvenc_vui_mismatch");
        }
    }
    Ok(())
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
// None selects the extradata format; samples must use the format established
// by that extradata. A four-byte AVCC length can resemble an Annex B prefix.
fn sps_units(
    data: &[u8],
    sample_length_prefixed: Option<bool>,
) -> Result<Vec<&[u8]>, &'static str> {
    if data.is_empty() || data.len() > MAX_PACKET_BYTES {
        return Err("invalid_h264_header");
    }
    let mut units = Vec::new();
    if sample_length_prefixed != Some(true) && start_code(data, 0).is_some() {
        let mut offset = start_code(data, 0).ok_or("invalid_h264_header")?;
        let mut nal_count = 0;
        while offset < data.len() {
            nal_count += 1;
            if nal_count > 1024 {
                return Err("invalid_h264_header");
            }
            let nal_start = offset;
            let mut end = nal_start;
            while end < data.len() && start_code(data, end).is_none() {
                end += 1;
            }
            let mut payload_end = end;
            while payload_end > nal_start && data[payload_end - 1] == 0 {
                payload_end -= 1;
            }
            let nal = &data[nal_start..payload_end];
            if nal.first().is_some_and(|header| header & 31 == 7) {
                if units.len() >= 1024 {
                    return Err("invalid_h264_header");
                }
                units.push(nal);
            }
            offset = end;
            if offset < data.len() {
                offset += start_code(data, offset).ok_or("invalid_h264_header")?;
            }
        }
        return Ok(units);
    }

    if sample_length_prefixed.is_none() && data[0] == 1 {
        if data.len() < 7 || data[4] & 3 != 3 {
            return Err("invalid_h264_header");
        }
        let mut offset = 6usize;
        for _ in 0..(data[5] & 31) {
            let length = u16::from_be_bytes(
                data.get(offset..offset + 2)
                    .ok_or("invalid_h264_header")?
                    .try_into()
                    .map_err(|_| "invalid_h264_header")?,
            ) as usize;
            offset += 2;
            let end = offset.checked_add(length).ok_or("invalid_h264_header")?;
            let nal = data.get(offset..end).ok_or("invalid_h264_header")?;
            if nal.first().is_some_and(|header| header & 31 == 7) {
                if units.len() >= 1024 {
                    return Err("invalid_h264_header");
                }
                units.push(nal);
            }
            offset = end;
        }
        return if units.is_empty() {
            Err("missing_h264_sps")
        } else {
            Ok(units)
        };
    }

    if sample_length_prefixed.is_none() && data[0] & 31 == 7 {
        return Ok(vec![data]);
    }

    let mut offset = 0usize;
    let mut nal_count = 0;
    while offset < data.len() {
        nal_count += 1;
        if nal_count > 1024 {
            return Err("invalid_h264_header");
        }
        let length = u32::from_be_bytes(
            data.get(offset..offset + 4)
                .ok_or("invalid_h264_header")?
                .try_into()
                .map_err(|_| "invalid_h264_header")?,
        ) as usize;
        offset += 4;
        let end = offset.checked_add(length).ok_or("invalid_h264_header")?;
        let nal = data.get(offset..end).ok_or("invalid_h264_header")?;
        if nal.first().is_some_and(|header| header & 31 == 7) {
            if units.len() >= 1024 {
                return Err("invalid_h264_header");
            }
            units.push(nal);
        }
        offset = end;
    }
    Ok(units)
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
fn start_code(data: &[u8], offset: usize) -> Option<usize> {
    if data.get(offset..offset.checked_add(4)?) == Some(&[0, 0, 0, 1]) {
        Some(4)
    } else if data.get(offset..offset.checked_add(3)?) == Some(&[0, 0, 1]) {
        Some(3)
    } else {
        None
    }
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
fn parse_sps_color_signal(nal: &[u8]) -> Result<SpsColorSignal, &'static str> {
    if nal.len() < 5 || nal.len() > 64 * 1024 || nal[0] & 31 != 7 || nal[0] & 128 != 0 {
        return Err("invalid_h264_sps");
    }
    let mut rbsp = Vec::with_capacity(nal.len() - 1);
    let mut zeroes = 0;
    for (index, byte) in nal[1..].iter().enumerate() {
        if zeroes >= 2 && *byte == 3 {
            if nal.get(index + 2).is_none_or(|next| *next > 3) {
                return Err("invalid_h264_sps");
            }
            zeroes = 0;
            continue;
        }
        if zeroes >= 2 && *byte < 3 {
            return Err("invalid_h264_sps");
        }
        rbsp.push(*byte);
        zeroes = if *byte == 0 { zeroes + 1 } else { 0 };
    }
    let mut bits = BitReader::new(&rbsp);
    let profile = bits.read_bits(8)? as u8;
    if profile != 66 {
        return Err("nvenc_h264_profile_unsupported");
    }
    bits.skip_bits(8)?; // constraint flags and reserved bits
    bits.skip_bits(8)?; // level_idc
    bits.read_ue()?; // seq_parameter_set_id
    bits.read_ue()?; // log2_max_frame_num_minus4
    match bits.read_ue()? {
        0 => {
            bits.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            bits.read_bit()?; // delta_pic_order_always_zero_flag
            bits.read_se()?;
            bits.read_se()?;
            let cycle = bits.read_ue()?;
            if cycle > 256 {
                return Err("invalid_h264_sps");
            }
            for _ in 0..cycle {
                bits.read_se()?;
            }
        }
        2 => {}
        _ => return Err("invalid_h264_sps"),
    }
    if bits.read_ue()? > 1 {
        return Err("nvenc_reference_frames_unsupported");
    }
    bits.read_bit()?; // gaps_in_frame_num_value_allowed_flag
    bits.read_ue()?; // pic_width_in_mbs_minus1
    bits.read_ue()?; // pic_height_in_map_units_minus1
    if !bits.read_bit()? {
        bits.read_bit()?; // mb_adaptive_frame_field_flag
    }
    bits.read_bit()?; // direct_8x8_inference_flag
    if bits.read_bit()? {
        for _ in 0..4 {
            bits.read_ue()?; // frame crop offsets
        }
    }
    if !bits.read_bit()? {
        return Err("nvenc_vui_missing");
    }
    if bits.read_bit()? {
        let aspect_ratio_idc = bits.read_bits(8)?;
        if aspect_ratio_idc == 255 {
            bits.skip_bits(32)?;
        }
    }
    if bits.read_bit()? {
        bits.read_bit()?; // overscan_appropriate_flag
    }
    if !bits.read_bit()? {
        return Err("nvenc_vui_missing_color_signal");
    }
    bits.read_bits(3)?; // video_format
    let full_range = bits.read_bit()?;
    if !bits.read_bit()? {
        return Err("nvenc_vui_missing_color_description");
    }
    let color = SpsColorSignal {
        full_range,
        primaries: bits.read_bits(8)? as u8,
        transfer: bits.read_bits(8)? as u8,
        matrix: bits.read_bits(8)? as u8,
    };
    if bits.read_bit()? {
        bits.read_ue()?; // chroma_sample_loc_type_top_field
        bits.read_ue()?; // chroma_sample_loc_type_bottom_field
    }
    if bits.read_bit()? {
        bits.skip_bits(64)?; // num_units_in_tick and time_scale
        bits.read_bit()?; // fixed_frame_rate_flag
    }
    let nal_hrd = bits.read_bit()?;
    if nal_hrd {
        parse_hrd(&mut bits)?;
    }
    let vcl_hrd = bits.read_bit()?;
    if vcl_hrd {
        parse_hrd(&mut bits)?;
    }
    if nal_hrd || vcl_hrd {
        bits.read_bit()?; // low_delay_hrd_flag
    }
    bits.read_bit()?; // pic_struct_present_flag
    if bits.read_bit()? {
        bits.read_bit()?; // motion_vectors_over_pic_boundaries_flag
        for _ in 0..6 {
            bits.read_ue()?;
        }
    }
    bits.finish_rbsp()?;
    Ok(color)
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
fn parse_hrd(bits: &mut BitReader<'_>) -> Result<(), &'static str> {
    let cpb_count_minus_one = bits.read_ue()?;
    if cpb_count_minus_one > 31 {
        return Err("invalid_h264_sps");
    }
    bits.skip_bits(8)?; // bit_rate_scale and cpb_size_scale
    for _ in 0..=cpb_count_minus_one {
        bits.read_ue()?; // bit_rate_value_minus1
        bits.read_ue()?; // cpb_size_value_minus1
        bits.read_bit()?; // cbr_flag
    }
    bits.skip_bits(20) // four five-bit delay/offset lengths
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
struct BitReader<'a> {
    bytes: &'a [u8],
    bit_offset: usize,
}

#[cfg(any(amaterasu_ffmpeg_nvenc, test))]
impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            bit_offset: 0,
        }
    }

    fn read_bit(&mut self) -> Result<bool, &'static str> {
        if self.bit_offset >= self.bytes.len().saturating_mul(8) {
            return Err("invalid_h264_sps");
        }
        let byte = self.bytes[self.bit_offset / 8];
        let bit = (byte >> (7 - self.bit_offset % 8)) & 1 != 0;
        self.bit_offset += 1;
        Ok(bit)
    }

    fn read_bits(&mut self, count: usize) -> Result<u32, &'static str> {
        if count > 32 {
            return Err("invalid_h264_sps");
        }
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.read_bit()?);
        }
        Ok(value)
    }

    fn skip_bits(&mut self, count: usize) -> Result<(), &'static str> {
        for _ in 0..count {
            self.read_bit()?;
        }
        Ok(())
    }

    fn read_ue(&mut self) -> Result<u32, &'static str> {
        let mut leading_zeroes = 0;
        while !self.read_bit()? {
            leading_zeroes += 1;
            if leading_zeroes > 31 {
                return Err("invalid_h264_sps");
            }
        }
        let suffix = self.read_bits(leading_zeroes)?;
        Ok(((1u32 << leading_zeroes) - 1) + suffix)
    }

    fn read_se(&mut self) -> Result<i32, &'static str> {
        let code_num = self.read_ue()?;
        let magnitude = i32::try_from(code_num.div_ceil(2)).map_err(|_| "invalid_h264_sps")?;
        Ok(if code_num % 2 == 0 {
            -magnitude
        } else {
            magnitude
        })
    }

    fn finish_rbsp(&mut self) -> Result<(), &'static str> {
        if !self.read_bit()? {
            return Err("invalid_h264_sps");
        }
        while !self.bit_offset.is_multiple_of(8) {
            if self.read_bit()? {
                return Err("invalid_h264_sps");
            }
        }
        if self.bit_offset != self.bytes.len() * 8 {
            return Err("invalid_h264_sps");
        }
        Ok(())
    }
}

#[cfg(amaterasu_ffmpeg_nvenc)]
pub struct Encoder {
    native: NonNull<c_void>,
    normalizer: Normalizer,
    packets: VecDeque<Packet>,
    input_bytes: usize,
    draining: bool,
    color_mode: ColorMode,
    length_prefixed: bool,
}

#[cfg(amaterasu_ffmpeg_nvenc)]
pub fn open(
    selected_identity: &EncoderIdentity,
    configuration: &Configuration,
) -> Result<Encoder, &'static str> {
    if !selected(selected_identity) {
        return Err("encoder_identity_changed");
    }
    validate_configuration(configuration)?;
    let input_bytes = (configuration.width as usize)
        .checked_mul(configuration.height as usize)
        .and_then(|pixels| pixels.checked_add(pixels / 2))
        .ok_or("invalid_configuration")?;

    let mut error_code = 0;
    // SAFETY: open copies scalar settings into a new FFmpeg context. Its
    // returned handle is worker-confined and released exactly once by Drop.
    let raw = unsafe {
        amaterasu_ffmpeg_nvenc_open(
            configuration.width as i32,
            configuration.height as i32,
            configuration.fps as i32,
            configuration.bitrate as i32,
            configuration.max_reference_frames as i32,
            i32::from(configuration.color_mode == ColorMode::Bt709Full),
            &mut error_code,
        )
    };
    let native = NonNull::new(raw).ok_or(match error_code {
        1 => "nvenc_device_unavailable",
        2 => "nvenc_allocation_failed",
        3 => "nvenc_open_failed",
        4 => "nvenc_configuration_unsupported",
        _ => "nvenc_open_failed",
    })?;
    let mut normalizer = Normalizer::default();
    let mut header_data = std::ptr::null();
    let mut header_length = 0usize;
    // SAFETY: FFmpeg owns extradata for the lifetime of this open context;
    // Normalizer copies the bounded header before the context can be closed.
    let header_status = unsafe {
        amaterasu_ffmpeg_nvenc_header(native.as_ptr(), &mut header_data, &mut header_length)
    };
    if header_status != 0
        || header_data.is_null()
        || header_length == 0
        || header_length > 1024 * 1024
    {
        // SAFETY: native was just created and ownership has not yet moved into
        // Encoder; release it here on this initialization failure path.
        unsafe { amaterasu_ffmpeg_nvenc_close(native.as_ptr()) };
        return Err("nvenc_headers_unavailable");
    }
    // SAFETY: the bounded slice is FFmpeg-owned and remains valid until close.
    let header = unsafe { std::slice::from_raw_parts(header_data, header_length) };
    if normalizer.header(header).is_err()
        || validate_header_color(header, configuration.color_mode).is_err()
    {
        // SAFETY: native was just created and ownership has not yet moved into
        // Encoder; release it here on malformed or unsupported codec headers.
        unsafe { amaterasu_ffmpeg_nvenc_close(native.as_ptr()) };
        return Err("nvenc_headers_unsupported");
    }
    Ok(Encoder {
        native,
        normalizer,
        packets: VecDeque::new(),
        input_bytes,
        draining: false,
        color_mode: configuration.color_mode,
        length_prefixed: header.first() == Some(&1),
    })
}

#[cfg(amaterasu_ffmpeg_nvenc)]
impl Encoder {
    pub fn submit(
        &mut self,
        bytes: Vec<u8>,
        timestamp_100ns: i64,
        force_idr: bool,
    ) -> Result<(), &'static str> {
        if self.draining
            || bytes.len() != self.input_bytes
            || timestamp_100ns < 0
            || self.packets.len() >= MAX_PENDING_PACKETS
        {
            return Err("nvenc_input_invalid");
        }
        // SAFETY: the frame bytes remain live until FFmpeg has copied them to
        // its owned AVFrame and the synchronous send call has returned.
        let native = self.native.as_ptr();
        let send = || unsafe {
            amaterasu_ffmpeg_nvenc_submit(
                native,
                bytes.as_ptr(),
                bytes.len(),
                timestamp_100ns,
                i32::from(force_idr),
            )
        };
        let mut result = send();
        if result == SEND_RETRY_AFTER_RECEIVE {
            // FFmpeg has not consumed the frame. Read its pending output before
            // retrying the same owned input exactly once.
            self.collect_available_packets()?;
            result = send();
        }
        if result != 0 {
            return Err("nvenc_submit_failed");
        }
        self.collect_available_packets()
    }

    fn collect_available_packets(&mut self) -> Result<(), &'static str> {
        loop {
            if self.packets.len() >= MAX_PENDING_PACKETS {
                return Err("nvenc_packet_queue_full");
            }
            match self.receive_native()? {
                NativeReceive::Packet(packet) => self.packets.push_back(packet),
                NativeReceive::Again => return Ok(()),
                NativeReceive::Eof => return Err("nvenc_unexpected_eof"),
            }
        }
    }

    fn receive_native(&mut self) -> Result<NativeReceive, &'static str> {
        let mut data = std::ptr::null();
        let mut length = 0usize;
        let mut timestamp_100ns = 0i64;
        let mut native_keyframe = 0i32;
        // SAFETY: output pointers are valid for this call. FFmpeg retains the
        // AVPacket until its next receive; packet bytes are copied below now.
        let status = unsafe {
            amaterasu_ffmpeg_nvenc_receive(
                self.native.as_ptr(),
                &mut data,
                &mut length,
                &mut timestamp_100ns,
                &mut native_keyframe,
            )
        };
        match status {
            RECEIVE_AGAIN => Ok(NativeReceive::Again),
            RECEIVE_EOF => Ok(NativeReceive::Eof),
            RECEIVE_PACKET => {
                if data.is_null() || length == 0 || length > MAX_PACKET_BYTES || timestamp_100ns < 0
                {
                    return Err("nvenc_output_invalid");
                }
                // SAFETY: C guarantees this borrowed range is valid until the
                // next native receive call. Normalize copies it before then.
                let borrowed = unsafe { std::slice::from_raw_parts(data, length) };
                validate_packet_color(borrowed, self.color_mode, self.length_prefixed)?;
                let (bytes, keyframe) = self.normalizer.sample(borrowed)?;
                if keyframe != (native_keyframe != 0) {
                    return Err("nvenc_keyframe_mismatch");
                }
                Ok(NativeReceive::Packet(Packet {
                    bytes,
                    timestamp_100ns,
                    keyframe,
                }))
            }
            _ => Err("nvenc_receive_failed"),
        }
    }

    pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        if let Some(packet) = self.packets.pop_front() {
            return Ok(Some(packet));
        }
        match self.receive_native()? {
            NativeReceive::Packet(packet) => Ok(Some(packet)),
            NativeReceive::Again if self.draining => Err("nvenc_drain_incomplete"),
            NativeReceive::Again => Ok(None),
            NativeReceive::Eof if self.draining => Ok(None),
            NativeReceive::Eof => Err("nvenc_unexpected_eof"),
        }
    }

    pub fn drain(&mut self) -> Result<(), &'static str> {
        if self.draining {
            return Ok(());
        }
        // Submission drains available FFmpeg packets on every frame, so the
        // null-frame send can enter draining mode without discarding output.
        // SAFETY: native remains exclusively owned by this worker.
        let mut result = unsafe { amaterasu_ffmpeg_nvenc_drain(self.native.as_ptr()) };
        if result == SEND_RETRY_AFTER_RECEIVE {
            self.collect_available_packets()?;
            result = unsafe { amaterasu_ffmpeg_nvenc_drain(self.native.as_ptr()) };
        }
        if result != 0 {
            return Err("nvenc_drain_failed");
        }
        self.draining = true;
        Ok(())
    }
}

#[cfg(amaterasu_ffmpeg_nvenc)]
impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: this is the unique successful open handle, destroyed once
        // before the module can unload the linked FFmpeg API.
        unsafe { amaterasu_ffmpeg_nvenc_close(self.native.as_ptr()) };
    }
}

#[cfg(amaterasu_ffmpeg_nvenc)]
enum NativeReceive {
    Packet(Packet),
    Again,
    Eof,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injectable_probe_only_advertises_when_open_probe_succeeds() {
        assert!(description_with_probe(|_| false).is_none());
        let encoder = description_with_probe(|_| true).unwrap();
        assert!(encoder.hardware);
        assert_eq!(encoder.codec, "h264");
        assert!(encoder.identity == identity());
        assert!(description_with_probe(|mode| mode == ColorMode::Bt709Limited).is_none());
    }

    #[test]
    fn hardware_identity_requires_exact_backend_identity() {
        let expected = identity();
        assert!(selected(&expected));
        let mut changed = expected;
        changed.friendly_name.push_str(" (untrusted)");
        assert!(!selected(&changed));
    }

    #[test]
    fn supports_bt709_range_modes_and_rejects_other_color_or_reference_modes() {
        let mut configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "bitrate": 15_000_000
        }))
        .unwrap();
        assert_eq!(validate_configuration(&configuration), Ok(()));

        configuration.color_mode = ColorMode::Bt709Full;
        assert_eq!(validate_configuration(&configuration), Ok(()));
        configuration.color_mode = ColorMode::Bt601Full;
        assert_eq!(
            validate_configuration(&configuration),
            Err("nvenc_color_mode_unsupported")
        );
        configuration.color_mode = ColorMode::Bt709Limited;
        configuration.max_reference_frames = 1;
        assert_eq!(validate_configuration(&configuration), Ok(()));
        configuration.max_reference_frames = 2;
        assert!(validate_configuration(&configuration).is_err());
    }

    #[test]
    fn parses_and_validates_bt709_full_range_from_h264_sps_vui() {
        let full = baseline_sps(true);
        let limited = baseline_sps(false);
        assert_eq!(
            parse_sps_color_signal(&full),
            Ok(SpsColorSignal {
                full_range: true,
                primaries: 1,
                transfer: 1,
                matrix: 1,
            })
        );
        assert_eq!(validate_header_color(&full, ColorMode::Bt709Full), Ok(()));
        assert_eq!(
            validate_header_color(&limited, ColorMode::Bt709Limited),
            Ok(())
        );
        assert_eq!(
            validate_header_color(&limited, ColorMode::Bt709Full),
            Err("nvenc_vui_mismatch")
        );

        let mut packet = vec![0, 0, 0, 1];
        packet.extend_from_slice(&full);
        assert_eq!(
            validate_packet_color(&packet, ColorMode::Bt709Full, false),
            Ok(())
        );
        assert_eq!(
            validate_packet_color(&packet, ColorMode::Bt709Limited, false),
            Err("nvenc_vui_mismatch")
        );
        assert_eq!(
            validate_packet_color(&[0, 0, 0, 2, 0x65, 0x80], ColorMode::Bt709Full, true),
            Ok(())
        );
    }

    #[test]
    fn rejects_missing_or_malformed_sps_color_description() {
        assert_eq!(
            validate_header_color(&[0, 0, 0, 2, 0x68, 0x80], ColorMode::Bt709Full),
            Err("nvenc_vui_mismatch")
        );
        assert!(parse_sps_color_signal(&[0x67, 66, 0, 31]).is_err());
        let no_vui = baseline_sps_without_vui();
        assert_eq!(
            validate_header_color(&no_vui, ColorMode::Bt709Full),
            Err("nvenc_vui_mismatch")
        );
        let wrong_primaries = baseline_sps_with_vui(Some(true), [6, 1, 1]);
        assert_eq!(
            validate_header_color(&wrong_primaries, ColorMode::Bt709Full),
            Err("nvenc_vui_mismatch")
        );

        let full = baseline_sps(true);
        assert!(parse_sps_color_signal(&full[..full.len() - 1]).is_err());
        let mut malformed_tail = full.clone();
        *malformed_tail.last_mut().unwrap() = 0;
        assert!(parse_sps_color_signal(&malformed_tail).is_err());
        let mut malformed_prevention = full.clone();
        malformed_prevention.extend_from_slice(&[0, 0, 3, 4]);
        assert!(parse_sps_color_signal(&malformed_prevention).is_err());
        malformed_prevention.pop();
        assert!(parse_sps_color_signal(&malformed_prevention).is_err());
    }

    #[test]
    fn signed_exp_golomb_rejects_unsupported_code_width_without_panicking() {
        let maximum_code_number = [0, 0, 0, 0, 0x80];
        assert_eq!(
            BitReader::new(&maximum_code_number).read_se(),
            Err("invalid_h264_sps")
        );
    }

    #[test]
    fn validates_avcc_extradata_and_repeated_sps_packets() {
        let sps = baseline_sps(true);
        let mut avcc = vec![1, 66, 0, 31, 0xff, 0xe1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&sps);
        avcc.extend_from_slice(&[1, 0, 2, 0x68, 0x80]);
        assert_eq!(validate_header_color(&avcc, ColorMode::Bt709Full), Ok(()));

        let mut packet = vec![0, 0, 0, 1];
        packet.extend_from_slice(&sps);
        packet.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x80]);
        assert_eq!(
            validate_packet_color(&packet, ColorMode::Bt709Full, false),
            Ok(())
        );
        assert_eq!(
            validate_packet_color(&packet, ColorMode::Bt709Limited, false),
            Err("nvenc_vui_mismatch")
        );

        // The first four-byte AVCC length resembles an Annex B start code;
        // framing from the accepted extradata must still expose the later SPS.
        let mut ambiguous_avcc = vec![0, 0, 1, 0, 0x41];
        ambiguous_avcc.resize(4 + 256, 0x80);
        let limited_sps = baseline_sps(false);
        ambiguous_avcc.extend_from_slice(&(limited_sps.len() as u32).to_be_bytes());
        ambiguous_avcc.extend_from_slice(&limited_sps);
        assert_eq!(
            validate_packet_color(&ambiguous_avcc, ColorMode::Bt709Full, true),
            Err("nvenc_vui_mismatch")
        );
    }

    fn baseline_sps(full_range: bool) -> Vec<u8> {
        baseline_sps_with_vui(Some(full_range), [1, 1, 1])
    }

    #[cfg(amaterasu_ffmpeg_nvenc)]
    #[test]
    #[ignore = "opens the native NVIDIA encoder and submits real 1080p frames"]
    fn encodes_one_reference_frames_on_native_nvidia_hardware() {
        assert_eq!(std::env::var("AMATERASU_NATIVE_NVENC").as_deref(), Ok("1"));
        let configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width":1920,"height":1080,"fps":60,"bitrate":15000000,"max_reference_frames":1
        }))
        .unwrap();
        let mut encoder = open(&identity(), &configuration).unwrap();
        let mut pixels = vec![128; 1920 * 1080 * 3 / 2];
        pixels[..1920 * 1080].fill(32);
        let started = std::time::Instant::now();
        let mut packets = Vec::new();
        for index in 0..120 {
            encoder
                .submit(pixels.clone(), index * 10_000_000 / 60, index == 0)
                .unwrap();
            while let Some(packet) = encoder.receive().unwrap() {
                packets.push(packet);
            }
        }
        encoder.drain().unwrap();
        while let Some(packet) = encoder.receive().unwrap() {
            packets.push(packet);
        }
        assert_eq!(packets.len(), 120);
        assert!(packets[0].keyframe);
        assert!(
            packets
                .windows(2)
                .all(|pair| pair[0].timestamp_100ns < pair[1].timestamp_100ns)
        );
        eprintln!(
            "encoded 120 real 1080p one-reference frames in {:?}",
            started.elapsed()
        );
        if let Some(path) = std::env::var_os("AMATERASU_NVENC_EVIDENCE_FILE") {
            use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .unwrap();
            for packet in packets {
                output.write_all(&packet.bytes).unwrap();
            }
        }
    }

    fn baseline_sps_without_vui() -> Vec<u8> {
        baseline_sps_with_vui(None, [1, 1, 1])
    }

    fn baseline_sps_with_vui(full_range: Option<bool>, color_ids: [u8; 3]) -> Vec<u8> {
        baseline_sps_with_references(full_range, color_ids, 1)
    }

    #[test]
    fn rejects_sps_that_exceeds_moonlight_reference_limit() {
        let nal = baseline_sps_with_references(Some(false), [1, 1, 1], 2);
        assert_eq!(
            parse_sps_color_signal(&nal),
            Err("nvenc_reference_frames_unsupported")
        );
    }

    fn baseline_sps_with_references(
        full_range: Option<bool>,
        color_ids: [u8; 3],
        references: u32,
    ) -> Vec<u8> {
        let mut bits = TestBitWriter::default();
        bits.write_bits(66, 8); // Baseline profile
        bits.write_bits(0, 8); // constraint flags
        bits.write_bits(31, 8); // level_idc
        bits.write_ue(0); // SPS id
        bits.write_ue(0); // log2_max_frame_num_minus4
        bits.write_ue(0); // pic_order_cnt_type
        bits.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4
        bits.write_ue(references); // max_num_ref_frames
        bits.write_bit(false); // gaps_in_frame_num_value_allowed_flag
        bits.write_ue(119); // width: 1920
        bits.write_ue(67); // height in 16-line macroblock map units
        bits.write_bit(true); // frame_mbs_only_flag
        bits.write_bit(true); // direct_8x8_inference_flag
        bits.write_bit(false); // frame_cropping_flag
        bits.write_bit(full_range.is_some()); // vui_parameters_present_flag
        if let Some(full_range) = full_range {
            bits.write_bit(false); // aspect_ratio_info_present_flag
            bits.write_bit(false); // overscan_info_present_flag
            bits.write_bit(true); // video_signal_type_present_flag
            bits.write_bits(5, 3); // video_format
            bits.write_bit(full_range); // video_full_range_flag
            bits.write_bit(true); // colour_description_present_flag
            bits.write_bits(u32::from(color_ids[0]), 8);
            bits.write_bits(u32::from(color_ids[1]), 8);
            bits.write_bits(u32::from(color_ids[2]), 8);
            for _ in 0..6 {
                bits.write_bit(false); // remaining optional VUI syntax absent
            }
        }
        bits.write_bit(true); // rbsp_stop_one_bit
        while bits.bit_len % 8 != 0 {
            bits.write_bit(false);
        }
        let mut nal = vec![0x67];
        let mut zeroes = 0;
        for byte in bits.bytes {
            if zeroes >= 2 && byte <= 3 {
                nal.push(3);
                zeroes = 0;
            }
            nal.push(byte);
            zeroes = if byte == 0 { zeroes + 1 } else { 0 };
        }
        nal
    }

    #[derive(Default)]
    struct TestBitWriter {
        bytes: Vec<u8>,
        bit_len: usize,
    }

    impl TestBitWriter {
        fn write_bit(&mut self, bit: bool) {
            if self.bit_len.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if bit {
                let mask = 1 << (7 - self.bit_len % 8);
                *self.bytes.last_mut().unwrap() |= mask;
            }
            self.bit_len += 1;
        }

        fn write_bits(&mut self, value: u32, count: usize) {
            for shift in (0..count).rev() {
                self.write_bit(value & (1 << shift) != 0);
            }
        }

        fn write_ue(&mut self, value: u32) {
            let code_num = value + 1;
            let bit_count = (u32::BITS - code_num.leading_zeros()) as usize;
            for _ in 1..bit_count {
                self.write_bit(false);
            }
            self.write_bits(code_num, bit_count);
        }
    }
}

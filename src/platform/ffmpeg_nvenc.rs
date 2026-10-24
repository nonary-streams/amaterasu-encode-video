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
#[cfg(amaterasu_ffmpeg_nvenc)]
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
unsafe extern "C" {
    fn amaterasu_ffmpeg_nvenc_open(
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        max_reference_frames: i32,
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
fn description_with_probe(probe: impl FnOnce() -> bool) -> Option<EncoderDescription> {
    probe().then(|| EncoderDescription {
        identity: identity(),
        name: ENCODER_NAME.into(),
        codec: "h264",
        hardware: true,
        vendor_id: Some(0x10de),
    })
}

pub fn description() -> Option<EncoderDescription> {
    #[cfg(amaterasu_ffmpeg_nvenc)]
    {
        return description_with_probe(probe_hardware);
    }
    #[cfg(not(amaterasu_ffmpeg_nvenc))]
    None
}

#[cfg(amaterasu_ffmpeg_nvenc)]
fn probe_hardware() -> bool {
    let mut error_code = 0;
    // SAFETY: probe opens a private throwaway context with fixed bounded
    // settings and no frame data; it retains no handles after returning.
    let raw = unsafe { amaterasu_ffmpeg_nvenc_open(640, 480, 30, 1_000_000, 0, &mut error_code) };
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
    if configuration.color_mode != ColorMode::Bt709Limited {
        return Err("nvenc_color_mode_unsupported");
    }
    if configuration.max_reference_frames != 0 {
        return Err("nvenc_reference_frames_unsupported");
    }
    Ok(())
}

#[cfg(amaterasu_ffmpeg_nvenc)]
pub struct Encoder {
    native: NonNull<c_void>,
    normalizer: Normalizer,
    packets: VecDeque<Packet>,
    input_bytes: usize,
    draining: bool,
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
    if normalizer.header(header).is_err() {
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
        let result = unsafe {
            amaterasu_ffmpeg_nvenc_submit(
                self.native.as_ptr(),
                bytes.as_ptr(),
                bytes.len(),
                timestamp_100ns,
                i32::from(force_idr),
            )
        };
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
        // SAFETY: output pointers are valid for this call. FFmpeg retains the
        // AVPacket until its next receive; packet bytes are copied below now.
        let status = unsafe {
            amaterasu_ffmpeg_nvenc_receive(
                self.native.as_ptr(),
                &mut data,
                &mut length,
                &mut timestamp_100ns,
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
                let (bytes, keyframe) = self.normalizer.sample(borrowed)?;
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
        if unsafe { amaterasu_ffmpeg_nvenc_drain(self.native.as_ptr()) } != 0 {
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
        assert!(description_with_probe(|| false).is_none());
        let encoder = description_with_probe(|| true).unwrap();
        assert!(encoder.hardware);
        assert_eq!(encoder.codec, "h264");
        assert_eq!(encoder.identity, identity());
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
    fn unsupported_color_and_reference_configuration_fails_closed() {
        let mut configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "bitrate": 15_000_000
        }))
        .unwrap();
        assert_eq!(validate_configuration(&configuration), Ok(()));

        configuration.color_mode = ColorMode::Bt709Full;
        assert_eq!(
            validate_configuration(&configuration),
            Err("nvenc_color_mode_unsupported")
        );
        configuration.color_mode = ColorMode::Bt709Limited;
        configuration.max_reference_frames = 1;
        assert_eq!(
            validate_configuration(&configuration),
            Err("nvenc_reference_frames_unsupported")
        );
    }
}

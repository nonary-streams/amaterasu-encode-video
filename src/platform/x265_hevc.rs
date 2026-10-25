//! Opt-in 8-bit x265 Main-profile HEVC backend. The worker owns the shim and
//! copies every borrowed access unit before requesting another packet.
#[cfg(amaterasu_x265_hevc)]
use crate::hevc::Normalizer;
use crate::model::{Configuration, EncoderIdentity, Packet};
#[cfg(any(amaterasu_x265_hevc, test))]
use amaterasu_media::{ColorMode, VideoCodec};
#[cfg(amaterasu_x265_hevc)]
use std::{collections::VecDeque, ffi::c_void, ptr::NonNull};

const ENCODER_ID: &str = "amaterasu.software.linux.x265.hevc";
const ENCODER_URL: &str = "x265://libx265";
const ENCODER_NAME: &str = "x265 software HEVC Main (8-bit)";
#[cfg(any(amaterasu_x265_hevc, test))]
const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
#[cfg(amaterasu_x265_hevc)]
const MAX_PENDING_PACKETS: usize = 16;
#[cfg(amaterasu_x265_hevc)]
const RECEIVE_AGAIN: i32 = 0;
#[cfg(amaterasu_x265_hevc)]
const RECEIVE_PACKET: i32 = 1;
#[cfg(amaterasu_x265_hevc)]
const RECEIVE_EOF: i32 = 2;
#[cfg(amaterasu_x265_hevc)]
const SEND_RETRY_AFTER_RECEIVE: i32 = 3;

#[cfg(amaterasu_x265_hevc)]
unsafe extern "C" {
    fn amaterasu_x265_hevc_open(
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        max_reference_frames: i32,
        full_range: i32,
        error_code: *mut i32,
    ) -> *mut c_void;
    fn amaterasu_x265_hevc_close(encoder: *mut c_void);
    fn amaterasu_x265_hevc_header(
        encoder: *mut c_void,
        data: *mut *const u8,
        length: *mut usize,
    ) -> i32;
    fn amaterasu_x265_hevc_submit(
        encoder: *mut c_void,
        nv12: *const u8,
        length: usize,
        timestamp_100ns: i64,
        force_idr: i32,
    ) -> i32;
    fn amaterasu_x265_hevc_drain(encoder: *mut c_void) -> i32;
    fn amaterasu_x265_hevc_receive(
        encoder: *mut c_void,
        data: *mut *const u8,
        length: *mut usize,
        timestamp_100ns: *mut i64,
        keyframe: *mut i32,
    ) -> i32;
}

#[cfg(target_os = "linux")]
fn backend_identity() -> EncoderIdentity {
    EncoderIdentity {
        clsid: ENCODER_ID.into(),
        hardware_url: Some(ENCODER_URL.into()),
        friendly_name: ENCODER_NAME.into(),
    }
}

#[cfg(target_os = "linux")]
pub fn selected(identity: &EncoderIdentity) -> bool {
    identity == &backend_identity()
}

#[cfg(test)]
fn probe_configuration() -> Configuration {
    Configuration {
        video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
        slices_per_frame: 1,
        min_threads: 2,
        qp: 28,
        sw_preset: None,
        sw_tune: None,
        nvenc_settings: Default::default(),
        width: 640,
        height: 480,
        fps: 30,
        bitrate: 1_000_000,
        codec: VideoCodec::Hevc,
        color_mode: ColorMode::Bt709Limited,
        max_reference_frames: 0,
        nvenc_preset: None,
        nvenc_h264_cavlc: false,
        nvenc_spatial_aq: false,
        amd_av1_latency_mode: Default::default(),
    }
}

#[cfg(any(amaterasu_x265_hevc, test))]
fn validate_configuration(configuration: &Configuration) -> Result<(), &'static str> {
    if configuration.codec() != VideoCodec::Hevc
        || !matches!(
            configuration.color_mode,
            ColorMode::Bt709Limited | ColorMode::Bt709Full
        )
        || configuration.max_reference_frames > 1
        || configuration.nvenc_preset.is_some()
        || configuration.nvenc_h264_cavlc
        || configuration.nvenc_spatial_aq
        || configuration.amd_av1_latency_mode != crate::model::AmdAv1LatencyMode::Auto
    {
        return Err("hevc_configuration_unsupported");
    }
    configuration.validate()
}

#[cfg(not(amaterasu_x265_hevc))]
pub struct Encoder;

#[cfg(not(amaterasu_x265_hevc))]
impl Encoder {
    pub fn open(_: &EncoderIdentity, _: &Configuration) -> Result<Self, &'static str> {
        Err("hevc_feature_disabled")
    }
    pub fn submit(&mut self, _: Vec<u8>, _: i64, _: bool) -> Result<(), &'static str> {
        Err("hevc_encoder_unavailable")
    }
    pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        Err("hevc_encoder_unavailable")
    }
    pub fn drain(&mut self) -> Result<(), &'static str> {
        Err("hevc_encoder_unavailable")
    }
}

#[cfg(amaterasu_x265_hevc)]
pub struct Encoder {
    native: NonNull<c_void>,
    normalizer: Normalizer,
    color_mode: ColorMode,
    packets: VecDeque<Packet>,
    input_bytes: usize,
    draining: bool,
}

#[cfg(amaterasu_x265_hevc)]
fn open(configuration: &Configuration) -> Result<NonNull<c_void>, &'static str> {
    validate_configuration(configuration)?;
    let mut error_code = 0;
    // SAFETY: bounds are validated above; the shim copies/owns its frame
    // scratch and x265 handles, and returns a unique worker-owned pointer.
    let raw = unsafe {
        amaterasu_x265_hevc_open(
            configuration.width as i32,
            configuration.height as i32,
            configuration.fps as i32,
            configuration.bitrate as i32,
            configuration.max_reference_frames as i32,
            i32::from(configuration.color_mode == ColorMode::Bt709Full),
            &mut error_code,
        )
    };
    NonNull::new(raw).ok_or(match error_code {
        1 => "hevc_configuration_unsupported",
        2 => "hevc_x265_abi_unsupported",
        3 => "hevc_allocation_failed",
        4 => "hevc_profile_unsupported",
        5 => "hevc_open_failed",
        6 => "hevc_headers_unavailable",
        _ => "hevc_open_failed",
    })
}

#[cfg(amaterasu_x265_hevc)]
impl Encoder {
    pub fn open(
        identity: &EncoderIdentity,
        configuration: &Configuration,
    ) -> Result<Self, &'static str> {
        if identity != &backend_identity() {
            return Err("encoder_identity_changed");
        }
        let native = open(configuration)?;
        let mut normalizer = Normalizer::default();
        let mut data = std::ptr::null();
        let mut length = 0usize;
        // SAFETY: header memory belongs to the live shim context and is copied
        // into bounded Rust parameter-set storage before any further call.
        let status = unsafe { amaterasu_x265_hevc_header(native.as_ptr(), &mut data, &mut length) };
        if status != 0 || data.is_null() || length == 0 || length > 1024 * 1024 {
            // SAFETY: native is uniquely owned here; initialization failed.
            unsafe { amaterasu_x265_hevc_close(native.as_ptr()) };
            return Err("hevc_headers_unavailable");
        }
        // SAFETY: the shim guarantees this bounded range lives until close.
        let header = unsafe { std::slice::from_raw_parts(data, length) };
        if normalizer.header(header).is_err()
            || normalizer
                .validate_main_bt709(configuration.color_mode)
                .is_err()
        {
            // SAFETY: native is uniquely owned here; initialization failed.
            unsafe { amaterasu_x265_hevc_close(native.as_ptr()) };
            return Err("hevc_headers_unsupported");
        }
        Ok(Self {
            native,
            normalizer,
            color_mode: configuration.color_mode,
            packets: VecDeque::new(),
            input_bytes: configuration.width as usize * configuration.height as usize * 3 / 2,
            draining: false,
        })
    }

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
            return Err("hevc_input_invalid");
        }
        let native = self.native.as_ptr();
        let submit = || {
            // SAFETY: input remains live during the call; shim copies NV12
            // chroma into owned planar scratch before returning.
            unsafe {
                amaterasu_x265_hevc_submit(
                    native,
                    bytes.as_ptr(),
                    bytes.len(),
                    timestamp_100ns,
                    i32::from(force_idr),
                )
            }
        };
        let mut result = submit();
        if result == SEND_RETRY_AFTER_RECEIVE {
            self.collect_available_packets()?;
            result = submit();
        }
        if result != 0 {
            return Err("hevc_submit_failed");
        }
        self.collect_available_packets()
    }

    fn collect_available_packets(&mut self) -> Result<(), &'static str> {
        loop {
            if self.packets.len() >= MAX_PENDING_PACKETS {
                return Err("hevc_packet_queue_full");
            }
            match self.receive_native()? {
                NativeReceive::Packet(packet) => self.packets.push_back(packet),
                NativeReceive::Again => return Ok(()),
                NativeReceive::Eof => return Err("hevc_unexpected_eof"),
            }
        }
    }

    fn receive_native(&mut self) -> Result<NativeReceive, &'static str> {
        let mut data = std::ptr::null();
        let mut length = 0usize;
        let mut timestamp_100ns = 0i64;
        let mut native_keyframe = 0;
        // SAFETY: output pointers are writable; the returned borrowed bytes
        // remain valid until the following receive and are copied below.
        let status = unsafe {
            amaterasu_x265_hevc_receive(
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
                    return Err("hevc_output_invalid");
                }
                // SAFETY: borrowed range is valid until the next shim receive.
                let borrowed = unsafe { std::slice::from_raw_parts(data, length) };
                let (bytes, keyframe) = self.normalizer.sample(borrowed)?;
                self.normalizer.validate_main_bt709(self.color_mode)?;
                if keyframe != (native_keyframe != 0) {
                    return Err("hevc_keyframe_mismatch");
                }
                Ok(NativeReceive::Packet(Packet {
                    bytes,
                    timestamp_100ns,
                    keyframe,
                }))
            }
            _ => Err("hevc_receive_failed"),
        }
    }

    pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        if let Some(packet) = self.packets.pop_front() {
            return Ok(Some(packet));
        }
        match self.receive_native()? {
            NativeReceive::Packet(packet) => Ok(Some(packet)),
            NativeReceive::Again if self.draining => Err("hevc_drain_incomplete"),
            NativeReceive::Again => Ok(None),
            NativeReceive::Eof if self.draining => Ok(None),
            NativeReceive::Eof => Err("hevc_unexpected_eof"),
        }
    }

    pub fn drain(&mut self) -> Result<(), &'static str> {
        if self.draining {
            return Ok(());
        }
        let native = self.native.as_ptr();
        let mut result = unsafe { amaterasu_x265_hevc_drain(native) };
        let mut attempts = 0;
        while result == SEND_RETRY_AFTER_RECEIVE && attempts < 4 {
            self.collect_available_packets()?;
            result = unsafe { amaterasu_x265_hevc_drain(native) };
            attempts += 1;
        }
        if result != 0 {
            return Err("hevc_drain_failed");
        }
        self.draining = true;
        Ok(())
    }
}

#[cfg(amaterasu_x265_hevc)]
impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: this is the unique shim handle, destroyed once on the worker.
        unsafe { amaterasu_x265_hevc_close(self.native.as_ptr()) };
    }
}

#[cfg(amaterasu_x265_hevc)]
enum NativeReceive {
    Packet(Packet),
    Again,
    Eof,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_bt709_full_and_rejects_other_color_or_reference_modes() {
        let mut configuration = probe_configuration();
        assert!(validate_configuration(&configuration).is_ok());
        configuration.color_mode = ColorMode::Bt709Full;
        assert!(validate_configuration(&configuration).is_ok());
        configuration.color_mode = ColorMode::Bt601Full;
        assert_eq!(
            validate_configuration(&configuration),
            Err("hevc_configuration_unsupported")
        );
        configuration.color_mode = ColorMode::Bt709Limited;
        configuration.max_reference_frames = 2;
        assert_eq!(
            validate_configuration(&configuration),
            Err("hevc_configuration_unsupported")
        );
    }
}

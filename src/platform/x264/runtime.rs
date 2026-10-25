use super::*;
use crate::bitstream::Normalizer;
use amaterasu_module_sdk::{LogEmitter, LogLevel};
use libloading::Library;
use std::{
    collections::VecDeque,
    ffi::{CStr, CString, c_char, c_int, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::NonNull,
};

type X264LogCallback = unsafe extern "C" fn(*mut c_void, c_int, *const c_char);

// x264's public structs are opaque in Rust. Only the C adapter compiled against
// the installed versioned header accesses their layout. Function signatures use
// pointer-compatible opaque types at this ABI boundary.
#[repr(C)]
struct Functions {
    default_preset: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    apply_profile: unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int,
    param_cleanup: unsafe extern "C" fn(*mut c_void),
    picture_init: unsafe extern "C" fn(*mut c_void),
    encoder_open: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    encoder_close: unsafe extern "C" fn(*mut c_void),
    encoder_encode: unsafe extern "C" fn(
        *mut c_void,
        *mut *mut c_void,
        *mut c_int,
        *mut c_void,
        *mut c_void,
    ) -> c_int,
    delayed_frames: unsafe extern "C" fn(*mut c_void) -> c_int,
    maximum_delayed_frames: unsafe extern "C" fn(*mut c_void) -> c_int,
    encoder_parameters: unsafe extern "C" fn(*mut c_void, *mut c_void),
}

#[repr(C)]
struct NativeConfiguration {
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    reference_frames: u32,
    slices_per_frame: i32,
    min_threads: i32,
    color_primaries: u32,
    transfer_characteristics: u32,
    matrix_coefficients: u32,
    full_range: u32,
}

#[repr(C)]
struct Output {
    data: *const u8,
    len: usize,
    timestamp: i64,
}

unsafe extern "C" {
    fn amaterasu_x264_build() -> c_int;
    fn amaterasu_x264_open(
        functions: *const Functions,
        config: *const NativeConfiguration,
        preset: *const c_char,
        tune: *const c_char,
        log_callback: Option<X264LogCallback>,
        log_context: *mut c_void,
        status: *mut c_int,
    ) -> *mut c_void;
    fn amaterasu_x264_close(encoder: *mut c_void);
    fn amaterasu_x264_delayed_frames(encoder: *mut c_void) -> c_int;
    fn amaterasu_x264_encode(
        encoder: *mut c_void,
        input: *const u8,
        timestamp: i64,
        force_idr: c_int,
        output: *mut Output,
    ) -> c_int;
}

struct CodecLibrary {
    functions: Functions,
    _library: Library,
}

impl CodecLibrary {
    fn load() -> Result<Self, &'static str> {
        // SAFETY: adapter returns a compile-time x264 ABI version without inputs.
        let version = unsafe { amaterasu_x264_build() };
        Self::load_names(version, library_names(version))
    }

    fn load_names(version: c_int, candidates: Vec<String>) -> Result<Self, &'static str> {
        for name in candidates {
            // SAFETY: opt-in native codec loading uses the platform's trusted
            // library search. No caller-supplied library path enters this loader.
            let Ok(library) = (unsafe { Library::new(name) }) else {
                continue;
            };
            // SAFETY: every function signature matches x264.h. The versioned
            // encoder_open symbol must match the header used by the adapter.
            let functions = unsafe {
                (|| {
                    Ok::<_, libloading::Error>(Functions {
                        default_preset: *library.get(b"x264_param_default_preset\0")?,
                        apply_profile: *library.get(b"x264_param_apply_profile\0")?,
                        param_cleanup: *library.get(b"x264_param_cleanup\0")?,
                        picture_init: *library.get(b"x264_picture_init\0")?,
                        encoder_open: *library
                            .get(format!("x264_encoder_open_{version}\0").as_bytes())?,
                        encoder_close: *library.get(b"x264_encoder_close\0")?,
                        encoder_encode: *library.get(b"x264_encoder_encode\0")?,
                        delayed_frames: *library.get(b"x264_encoder_delayed_frames\0")?,
                        maximum_delayed_frames: *library
                            .get(b"x264_encoder_maximum_delayed_frames\0")?,
                        encoder_parameters: *library.get(b"x264_encoder_parameters\0")?,
                    })
                })()
            };
            if let Ok(functions) = functions {
                return Ok(Self {
                    functions,
                    _library: library,
                });
            }
        }
        Err("x264_runtime_unavailable")
    }
}

fn library_names(version: c_int) -> Vec<String> {
    if cfg!(windows) {
        vec![
            format!("libx264-{version}.dll"),
            format!("x264-{version}.dll"),
            "libx264.dll".into(),
        ]
    } else if cfg!(target_os = "macos") {
        vec![format!("libx264.{version}.dylib"), "libx264.dylib".into()]
    } else {
        vec![format!("libx264.so.{version}"), "libx264.so".into()]
    }
}

pub fn availability() -> Result<(), &'static str> {
    CodecLibrary::load().map(|_| ())
}

pub struct Encoder {
    native: NonNull<c_void>,
    _library: CodecLibrary,
    _log_emitter: Option<Box<LogEmitter>>,
    input_bytes: usize,
    normalizer: Normalizer,
    packets: VecDeque<Packet>,
    draining: bool,
}

impl Encoder {
    pub fn open(identity: &EncoderIdentity, config: &Configuration) -> Result<Self, &'static str> {
        Self::open_with_log_emitter(identity, config, None)
    }

    pub fn open_with_log_emitter(
        identity: &EncoderIdentity,
        config: &Configuration,
        emitter: Option<LogEmitter>,
    ) -> Result<Self, &'static str> {
        if identity != &super::identity() {
            return Err("encoder_identity_changed");
        }
        config.validate()?;
        if config.codec() != amaterasu_media::VideoCodec::H264 {
            return Err("codec_unsupported");
        }
        let library = CodecLibrary::load()?;
        let preset =
            CString::new(config.software_preset()).map_err(|_| "invalid_software_configuration")?;
        let tune =
            CString::new(config.software_tune()).map_err(|_| "invalid_software_configuration")?;
        let vui = super::vui_color(config.color_mode);
        let native_config = NativeConfiguration {
            width: config.width,
            height: config.height,
            fps: config.fps,
            bitrate: config.bitrate,
            reference_frames: config.max_reference_frames,
            slices_per_frame: config.slices_per_frame,
            min_threads: config.min_threads,
            color_primaries: vui.color_primaries,
            transfer_characteristics: vui.transfer_characteristics,
            matrix_coefficients: vui.matrix_coefficients,
            full_range: vui.full_range,
        };
        let log_emitter = emitter.map(Box::new);
        let (log_callback, log_context) = match &log_emitter {
            Some(emitter) => (
                Some(x264_log_callback as X264LogCallback),
                (emitter.as_ref() as *const LogEmitter as *mut LogEmitter).cast(),
            ),
            None => (None, std::ptr::null_mut()),
        };
        let mut status = 0;
        // SAFETY: functions belong to the retained library; config and strings
        // remain live during open. The adapter copies parameters and functions.
        let native = unsafe {
            amaterasu_x264_open(
                &library.functions,
                &native_config,
                preset.as_ptr(),
                tune.as_ptr(),
                log_callback,
                log_context,
                &mut status,
            )
        };
        let native = NonNull::new(native).ok_or(if status == 1 {
            "invalid_software_configuration"
        } else {
            "software_encoder_open_failed"
        })?;
        Ok(Self {
            native,
            _library: library,
            _log_emitter: log_emitter,
            input_bytes: config.width as usize * config.height as usize * 3 / 2,
            normalizer: Normalizer::default(),
            packets: VecDeque::new(),
            draining: false,
        })
    }

    pub fn submit(
        &mut self,
        bytes: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<(), &'static str> {
        if self.draining
            || bytes.len() != self.input_bytes
            || timestamp < 0
            || self.packets.len() >= 16
        {
            return Err("software_encoder_input_invalid");
        }
        self.encode(Some(&bytes), timestamp, force)
    }

    fn encode(
        &mut self,
        bytes: Option<&[u8]>,
        timestamp: i64,
        force: bool,
    ) -> Result<(), &'static str> {
        let mut output = Output {
            data: std::ptr::null(),
            len: 0,
            timestamp: 0,
        };
        // SAFETY: native is exclusively owned here. Validated tightly packed
        // input remains live through the call; null input flushes delayed frames.
        let result = unsafe {
            amaterasu_x264_encode(
                self.native.as_ptr(),
                bytes.map_or(std::ptr::null(), <[u8]>::as_ptr),
                timestamp,
                c_int::from(force),
                &mut output,
            )
        };
        if result < 0 {
            return Err("software_encoder_failed");
        }
        if result == 0 {
            return Ok(());
        }
        if output.data.is_null()
            || output.len == 0
            || output.len > 16 * 1024 * 1024
            || output.timestamp < 0
            || self.packets.len() >= 16
        {
            return Err("software_encoder_output_invalid");
        }
        // SAFETY: x264 guarantees contiguous encoded NALs, verified by adapter;
        // its allocation lasts until the next encode. Normalize copies now.
        let borrowed = unsafe { std::slice::from_raw_parts(output.data, output.len) };
        let (bytes, keyframe) = self.normalizer.sample(borrowed)?;
        self.packets.push_back(Packet {
            bytes,
            timestamp_100ns: output.timestamp,
            keyframe,
        });
        Ok(())
    }

    pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        if let Some(packet) = self.packets.pop_front() {
            return Ok(Some(packet));
        }
        if self.draining {
            // SAFETY: native remains live and exclusively owned by this worker.
            let delayed = unsafe { amaterasu_x264_delayed_frames(self.native.as_ptr()) };
            if delayed < 0 {
                return Err("software_encoder_failed");
            }
            if delayed > 0 {
                self.encode(None, 0, false)?;
                // Returning None while frames are still delayed would emit an
                // early EOS. The synchronous flush must produce the next frame.
                return self
                    .packets
                    .pop_front()
                    .map(Some)
                    .ok_or("software_encoder_drain_failed");
            }
        }
        Ok(None)
    }

    pub fn drain(&mut self) -> Result<(), &'static str> {
        self.draining = true;
        Ok(())
    }
}

fn x264_level(level: c_int) -> LogLevel {
    match level {
        0 => LogLevel::Error,
        1 => LogLevel::Warning,
        2 => LogLevel::Info,
        _ => LogLevel::Debug,
    }
}

unsafe extern "C" fn x264_log_callback(context: *mut c_void, level: c_int, message: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if context.is_null() || message.is_null() {
            return;
        }
        // SAFETY: the C adapter invokes this only with its live boxed LogEmitter
        // and a NUL-terminated stack message valid until the callback returns.
        let emitter = unsafe { &*context.cast::<LogEmitter>() };
        // SAFETY: the adapter terminates its fixed-size message buffer.
        let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        let message = message.trim_end_matches(['\r', '\n']);
        if !message.is_empty() {
            let _ = emitter.emit(x264_level(level), "encode.x264", message);
        }
    }));
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: exactly one successful open transfers this handle; close is
        // called once before the retained library and function pointers unload.
        unsafe { amaterasu_x264_close(self.native.as_ptr()) };
    }
}

#[cfg(test)]
mod tests;

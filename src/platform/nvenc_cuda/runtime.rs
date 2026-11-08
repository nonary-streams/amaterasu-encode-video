//! Rust side of the NVENC/CUDA adapter boundary. The adapter handle is
//! created by `Encoder::open` on the encoder worker thread and closed by
//! `Drop` on the same thread. Encoding is pipelined over the adapter's slots:
//! the worker thread converts and submits a frame while a dedicated output
//! thread waits for the previous frame's bitstream. The two threads only ever
//! touch different slots. Borrowed output bytes are copied before release.
use super::{
    codec::Codec,
    description::{self, CodecCaps, DeviceRecord},
    output::OutputFramer,
    result_code,
    settings::{self, NativeSettings},
    versions::{self, StructVersions},
};
use crate::{
    input::{DmaBufImportSupport, DmaBufInput, GpuInput, InputSupport},
    model::{Configuration, EncoderDescription, EncoderIdentity, Packet},
    worker::NativeStageTimings,
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    ffi::{CStr, CString, c_char, c_void},
    os::fd::{AsRawFd, IntoRawFd},
    ptr::NonNull,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::JoinHandle,
    time::Instant,
};

const MAX_DEVICES: usize = 16;
const MAX_PLANES: usize = 4;
const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
const MAX_PENDING_PACKETS: usize = 16;
const MAX_MODIFIERS_PER_FORMAT: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeStatus {
    /// The adapter stage that failed (`AM_STAGE_*` in native/internal.h).
    pub stage: u32,
    /// The NVENC, CUDA, EGL or GL error that stage reported, or 0.
    pub native_code: i32,
}

/// The most recent failed open in this process, kept so operators and tests
/// can see which native step refused instead of only the coarse error code.
static LAST_OPEN_FAILURE: Mutex<Option<NativeStatus>> = Mutex::new(None);

pub fn last_open_failure() -> Option<NativeStatus> {
    *LAST_OPEN_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn record_open_failure(status: NativeStatus) {
    *LAST_OPEN_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(status);
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativeCodecCaps {
    hdr10: u32,
    supported: u32,
    max_width: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NativeDevice {
    pci_bus_id: [c_char; 32],
    name: [c_char; 256],
    codecs: [NativeCodecCaps; 3],
    gpu_conversion: u32,
    dmabuf_import: u32,
}

impl NativeDevice {
    const EMPTY: Self = Self {
        pci_bus_id: [0; 32],
        name: [0; 256],
        codecs: [NativeCodecCaps {
            hdr10: 0,
            supported: 0,
            max_width: 0,
            max_height: 0,
        }; 3],
        gpu_conversion: 0,
        dmabuf_import: 0,
    };

    fn record(&self) -> DeviceRecord {
        DeviceRecord {
            pci_bus_id: c_string(&self.pci_bus_id),
            name: c_string(&self.name),
            codecs: self.codecs.map(|caps| CodecCaps {
                hdr10: caps.hdr10 != 0,
                supported: caps.supported != 0,
                max_width: caps.max_width,
                max_height: caps.max_height,
            }),
            gpu_conversion: self.gpu_conversion != 0,
            dmabuf_import: self.dmabuf_import != 0,
        }
    }
}

/// Reads a NUL-terminated adapter string; a missing terminator yields "".
fn c_string(characters: &[c_char]) -> String {
    let bytes: Vec<u8> = characters
        .iter()
        .map(|character| *character as u8)
        .collect();
    CStr::from_bytes_until_nul(&bytes)
        .map(|text| text.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Mirrors `struct am_nvcuda_dmabuf`. Descriptors stay owned by the
/// `DmaBufInput` it was built from.
#[repr(C)]
struct NativeDmaBuf {
    width: u32,
    height: u32,
    fourcc: u32,
    plane_count: u32,
    modifier: u64,
    plane_fds: [i32; MAX_PLANES],
    fence_fds: [i32; MAX_PLANES],
    offsets: [u32; MAX_PLANES],
    pitches: [u32; MAX_PLANES],
    source_retained: u32,
    reserved: u32,
}

impl NativeDmaBuf {
    fn borrow(frame: &DmaBufInput) -> Result<Self, &'static str> {
        if frame.planes.is_empty() || frame.planes.len() > MAX_PLANES {
            return Err("nvenc_cuda_invalid_argument");
        }
        let mut native = Self {
            width: frame.width,
            height: frame.height,
            fourcc: frame.fourcc,
            plane_count: frame.planes.len() as u32,
            modifier: frame.modifier,
            plane_fds: [-1; MAX_PLANES],
            fence_fds: [-1; MAX_PLANES],
            offsets: [0; MAX_PLANES],
            pitches: [0; MAX_PLANES],
            source_retained: u32::from(frame.source_retained),
            reserved: 0,
        };
        for (index, plane) in frame.planes.iter().enumerate() {
            native.plane_fds[index] = plane.fd.as_raw_fd();
            native.fence_fds[index] = plane
                .acquire_fence
                .as_ref()
                .map_or(-1, |fence| fence.as_raw_fd());
            native.offsets[index] = plane.offset;
            native.pitches[index] = plane.pitch;
        }
        Ok(native)
    }
}

#[cfg(test)]
#[repr(C)]
#[derive(Debug, Default, PartialEq, Eq)]
struct AbiSizes {
    status: u32,
    struct_versions: u32,
    device: u32,
    settings: u32,
    dmabuf: u32,
    test_buffer: u32,
}

/// Mirrors `struct am_nvcuda_test_buffer`; used only by the GPU tests. The
/// adapter reads the handles back when the buffer is released.
#[cfg(test)]
#[repr(C)]
#[allow(dead_code)]
pub(super) struct TestBuffer {
    pub device: *mut c_void,
    pub buffer: *mut c_void,
    pub render_fd: i32,
    pub dmabuf_fd: i32,
    pub pitch: u32,
    pub offset: u32,
    pub modifier: u64,
}

unsafe extern "C" {
    fn amaterasu_nvcuda_get_timings(
        encoder: *mut c_void,
        slot: u32,
        timings: *mut NativeStageTimings,
    );
    fn amaterasu_nvcuda_header_versions(versions: *mut StructVersions) -> i32;
    fn amaterasu_nvcuda_driver_api(packed_version: *mut u32, status: *mut NativeStatus) -> i32;
    fn amaterasu_nvcuda_enumerate(
        candidates: *const StructVersions,
        candidate_count: u32,
        devices: *mut NativeDevice,
        capacity: u32,
        device_count: *mut u32,
        status: *mut NativeStatus,
    ) -> i32;
    fn amaterasu_nvcuda_open(
        pci_bus_id: *const c_char,
        candidates: *const StructVersions,
        candidate_count: u32,
        settings: *const NativeSettings,
        status: *mut NativeStatus,
        result: *mut i32,
    ) -> *mut c_void;
    fn amaterasu_nvcuda_import_modifiers(
        encoder: *mut c_void,
        fourcc: u32,
        modifiers: *mut u64,
        capacity: u32,
        modifier_count: *mut u32,
        implicit: *mut u32,
    ) -> i32;
    fn amaterasu_nvcuda_upload_nv12(
        encoder: *mut c_void,
        slot: u32,
        nv12: *const u8,
        length: usize,
        status: *mut NativeStatus,
    ) -> i32;
    fn amaterasu_nvcuda_upload_bgra(
        encoder: *mut c_void,
        slot: u32,
        bgra: *const u8,
        length: usize,
        status: *mut NativeStatus,
    ) -> i32;
    fn amaterasu_nvcuda_import_dmabuf(
        encoder: *mut c_void,
        slot: u32,
        frame: *mut NativeDmaBuf,
        status: *mut NativeStatus,
    ) -> i32;
    fn amaterasu_nvcuda_submit(
        encoder: *mut c_void,
        slot: u32,
        timestamp_100ns: i64,
        force_idr: i32,
        status: *mut NativeStatus,
    ) -> i32;
    fn amaterasu_nvcuda_lock(
        encoder: *mut c_void,
        slot: u32,
        data: *mut *const u8,
        length: *mut usize,
        picture_type: *mut u32,
        status: *mut NativeStatus,
    ) -> i32;
    fn amaterasu_nvcuda_release_output(
        encoder: *mut c_void,
        slot: u32,
        status: *mut NativeStatus,
    ) -> i32;
    fn amaterasu_nvcuda_take_gpu_timing(
        encoder: *mut c_void,
        slot: u32,
        completed_ago_us: u64,
    ) -> i32;
    fn amaterasu_nvcuda_cuda_input(encoder: *mut c_void, device_uuid: *mut u8) -> i32;
    fn amaterasu_nvcuda_import_cuda(
        encoder: *mut c_void,
        slot: u32,
        pointer: u64,
        pitch: u32,
        width: u32,
        height: u32,
        ready: *mut c_void,
        status: *mut NativeStatus,
    ) -> i32;
    fn amaterasu_nvcuda_bind_thread(encoder: *mut c_void) -> i32;
    fn amaterasu_nvcuda_unbind_thread(encoder: *mut c_void);
    fn amaterasu_nvcuda_drain(encoder: *mut c_void, status: *mut NativeStatus) -> i32;
    fn amaterasu_nvcuda_close(encoder: *mut c_void);
    #[cfg(test)]
    fn amaterasu_nvcuda_test_conversion_stream(asynchronous: i32, fail_copy: i32) -> i32;
    #[cfg(test)]
    fn amaterasu_nvcuda_test_stream_enabled(encoder: *const c_void) -> i32;
    #[cfg(test)]
    fn amaterasu_nvcuda_test_failed_retirement(mapping_unproven: i32) -> i32;
    #[cfg(test)]
    fn amaterasu_nvcuda_test_stream_setup(mode: i32) -> i32;
}

#[cfg(test)]
unsafe extern "C" {
    fn amaterasu_nvcuda_abi_sizes(sizes: *mut AbiSizes);
    pub(super) fn amaterasu_nvcuda_test_allocate_gbm(
        render_node: *const c_char,
        width: u32,
        height: u32,
        top_xrgb: u32,
        bottom_xrgb: u32,
        format: u32,
        buffer: *mut TestBuffer,
    ) -> i32;
    pub(super) fn amaterasu_nvcuda_test_release_gbm(buffer: *mut TestBuffer);
    pub(super) fn amaterasu_nvcuda_test_cuda_frame(
        width: u32,
        height: u32,
        top: u32,
        bottom: u32,
        pointer: *mut u64,
        pitch: *mut u32,
        event: *mut *mut c_void,
    ) -> i32;
    pub(super) fn amaterasu_nvcuda_test_release_cuda_frame(pointer: u64, event: *mut c_void);
}

fn check(result: i32) -> Result<(), &'static str> {
    if result == 0 {
        Ok(())
    } else {
        Err(result_code(result))
    }
}

fn header_versions() -> Result<StructVersions, &'static str> {
    let mut header = StructVersions::default();
    // SAFETY: GPU-free; writes one plain record owned by this frame.
    check(unsafe { amaterasu_nvcuda_header_versions(&mut header) })?;
    Ok(header)
}

/// Loads the driver libraries (once per process) and offers the compiled
/// header's API versions that this driver can accept.
fn negotiated_candidates() -> Result<Vec<StructVersions>, &'static str> {
    let header = header_versions()?;
    let mut packed = 0;
    let mut status = NativeStatus::default();
    // SAFETY: both out-pointers are live for the call.
    check(unsafe { amaterasu_nvcuda_driver_api(&mut packed, &mut status) })?;
    let candidates = versions::candidates(&header, packed);
    if candidates.is_empty() {
        return Err("nvenc_cuda_driver_too_old");
    }
    Ok(candidates)
}

/// Probes every CUDA device; any failure publishes nothing.
pub fn descriptions() -> Vec<EncoderDescription> {
    let Ok(candidates) = negotiated_candidates() else {
        return Vec::new();
    };
    let mut devices = [NativeDevice::EMPTY; MAX_DEVICES];
    let mut count = 0u32;
    let mut status = NativeStatus::default();
    // SAFETY: candidates and devices are live arrays of the stated lengths;
    // the adapter writes at most `capacity` device records.
    let result = unsafe {
        amaterasu_nvcuda_enumerate(
            candidates.as_ptr(),
            candidates.len() as u32,
            devices.as_mut_ptr(),
            MAX_DEVICES as u32,
            &mut count,
            &mut status,
        )
    };
    if result != 0 {
        return Vec::new();
    }
    let records: Vec<DeviceRecord> = devices[..(count as usize).min(MAX_DEVICES)]
        .iter()
        .map(NativeDevice::record)
        .collect();
    description::descriptions(&records)
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    (code[0] as u32) | (code[1] as u32) << 8 | (code[2] as u32) << 16 | (code[3] as u32) << 24
}

/// Packed RGB formats the shader samples as `.rgb`; alpha is ignored.
const IMPORT_FOURCCS: [u32; 8] = [
    fourcc(b"XR24"),
    fourcc(b"AR24"),
    fourcc(b"XB24"),
    fourcc(b"AB24"),
    fourcc(b"XR30"),
    fourcc(b"AR30"),
    fourcc(b"XB30"),
    fourcc(b"AB30"),
];

/// Adapter slots: one frame converts and submits while another encodes.
const SLOTS: u32 = 2;

/// The adapter handle as seen by the output thread. The adapter keeps each
/// slot's state separate; the output thread only locks and releases slots the
/// worker has submitted, and the worker only reuses slots handed back.
#[derive(Clone, Copy)]
struct OutputHandle(NonNull<c_void>);
// SAFETY: NVENC permits locking output on another thread in synchronous mode.
// The adapter state each thread touches is disjoint per slot, and Encoder's
// Drop joins the output thread before the handle is closed.
unsafe impl Send for OutputHandle {}

/// A submitted picture waiting for its bitstream.
struct Submission {
    slot: u32,
    timestamp_100ns: i64,
    force_idr: bool,
}

/// State shared by the worker and output threads.
#[derive(Default)]
struct PipelineState {
    idle: Vec<u32>,
    /// Released slots whose GPU timing the worker thread has not read yet.
    completed: VecDeque<(u32, Instant)>,
    packets: VecDeque<Packet>,
    failure: Option<&'static str>,
}

#[derive(Default)]
struct Pipeline {
    state: Mutex<PipelineState>,
    changed: Condvar,
    notifier: Mutex<Option<amaterasu_module_sdk::WorkNotifier>>,
}

impl Pipeline {
    fn lock(&self) -> std::sync::MutexGuard<'_, PipelineState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify(&self) {
        self.changed.notify_all();
        if let Some(notifier) = self
            .notifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            notifier.notify();
        }
    }
}

impl crate::worker::PacketTap for Pipeline {
    fn take_packet(&self) -> Result<Option<Packet>, &'static str> {
        let mut state = self.lock();
        match state.packets.pop_front() {
            Some(packet) => Ok(Some(packet)),
            None => state.failure.map_or(Ok(None), Err),
        }
    }
}

pub struct Encoder {
    native: NonNull<c_void>,
    pipeline: Arc<Pipeline>,
    submissions: Option<mpsc::Sender<Submission>>,
    output_thread: Option<JoinHandle<()>>,
    /// Finished frames' stage timings, reported once each.
    stage_timings: RefCell<VecDeque<NativeStageTimings>>,
    input_support: InputSupport,
    draining: bool,
}

impl Encoder {
    #[cfg(test)]
    pub(super) fn stream_ordering_enabled(&self) -> bool {
        // SAFETY: encoder is live and accessed on its owning worker thread.
        unsafe { amaterasu_nvcuda_test_stream_enabled(self.native.as_ptr()) != 0 }
    }

    /// One finished frame's stages, oldest first. Pipelining completes a frame
    /// after its submission returns, so these trail submissions by a frame.
    pub fn native_stage_timings(&self) -> Option<NativeStageTimings> {
        self.stage_timings.borrow_mut().pop_front()
    }

    pub fn open(
        identity: &EncoderIdentity,
        configuration: &Configuration,
    ) -> Result<Self, &'static str> {
        let codec = Codec::from_clsid(&identity.clsid).ok_or("encoder_identity_changed")?;
        let bus_id = identity
            .hardware_url
            .as_deref()
            .and_then(description::pci_bus_id)
            .ok_or("encoder_identity_changed")?;
        settings::validate(configuration, codec)?;
        let candidates = negotiated_candidates()?;
        let native_settings = settings::native_settings(configuration, codec);
        let bus_id = CString::new(bus_id).map_err(|_| "encoder_identity_changed")?;
        let mut status = NativeStatus::default();
        let mut result = 0;
        // SAFETY: every pointer is live for the call; the adapter copies the
        // settings and candidates. The returned handle is owned by `Encoder`
        // and closed exactly once by Drop on this worker thread.
        let raw = unsafe {
            amaterasu_nvcuda_open(
                bus_id.as_ptr(),
                candidates.as_ptr(),
                candidates.len() as u32,
                &native_settings,
                &mut status,
                &mut result,
            )
        };
        let Some(native) = NonNull::new(raw) else {
            record_open_failure(status);
            return Err(result_code(result));
        };
        let pipeline = Arc::new(Pipeline::default());
        pipeline.lock().idle = (0..SLOTS).rev().collect();
        let (submissions, queued) = mpsc::channel();
        let output = OutputHandle(native);
        let output_pipeline = Arc::clone(&pipeline);
        let framer = OutputFramer::new(codec);
        let output_thread = std::thread::Builder::new()
            .name("amaterasu-nvenc-output".into())
            .spawn(move || run_output_thread(output, &output_pipeline, framer, &queued))
            .map_err(|_| "nvenc_cuda_output_thread_failed");
        let output_thread = match output_thread {
            Ok(thread) => thread,
            Err(error) => {
                // SAFETY: the handle has no other owner yet; close it once here.
                unsafe { amaterasu_nvcuda_close(native.as_ptr()) };
                return Err(error);
            }
        };
        let mut encoder = Self {
            native,
            pipeline,
            submissions: Some(submissions),
            output_thread: Some(output_thread),
            stage_timings: RefCell::new(VecDeque::new()),
            input_support: InputSupport::default(),
            draining: false,
        };
        encoder.input_support = InputSupport {
            gpu_bgra_conversion: !configuration.is_hdr(),
            dmabuf: encoder.import_support(configuration.is_hdr()),
            cuda_device: encoder
                .cuda_input_device()
                .filter(|_| configuration.is_hdr()),
        };
        Ok(encoder)
    }

    pub fn input_support(&self) -> InputSupport {
        self.input_support.clone()
    }

    fn import_support(&self, hdr: bool) -> Option<Arc<DmaBufImportSupport>> {
        let mut support = DmaBufImportSupport::default();
        for fourcc in IMPORT_FOURCCS {
            if hdr
                && ![
                    u32::from_le_bytes(*b"XR30"),
                    u32::from_le_bytes(*b"AR30"),
                    u32::from_le_bytes(*b"XB30"),
                    u32::from_le_bytes(*b"AB30"),
                ]
                .contains(&fourcc)
            {
                continue;
            }
            let mut modifiers = [0u64; MAX_MODIFIERS_PER_FORMAT];
            let mut count = 0u32;
            let mut implicit = 0u32;
            // SAFETY: live handle on its owning thread; the modifier array
            // holds `capacity` entries and the counts are live out-pointers.
            let result = unsafe {
                amaterasu_nvcuda_import_modifiers(
                    self.native.as_ptr(),
                    fourcc,
                    modifiers.as_mut_ptr(),
                    MAX_MODIFIERS_PER_FORMAT as u32,
                    &mut count,
                    &mut implicit,
                )
            };
            if result == 0 {
                let listed = (count as usize).min(MAX_MODIFIERS_PER_FORMAT);
                support.add_format(fourcc, implicit != 0, modifiers[..listed].iter().copied());
            }
        }
        (!support.is_empty()).then(|| Arc::new(support))
    }

    /// Packets the output thread finished, readable from any thread. GPU timing
    /// and slot reuse stay on the worker, which reads them on the next submit.
    pub(crate) fn packet_tap(&self) -> Arc<dyn crate::worker::PacketTap> {
        Arc::clone(&self.pipeline) as Arc<dyn crate::worker::PacketTap>
    }

    /// Wakes the module as soon as the output thread finishes a packet.
    pub fn set_output_notifier(&mut self, notifier: amaterasu_module_sdk::WorkNotifier) {
        *self
            .pipeline
            .notifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(notifier);
    }

    pub fn submit(
        &mut self,
        nv12: Vec<u8>,
        timestamp_100ns: i64,
        force_idr: bool,
    ) -> Result<(), &'static str> {
        self.ensure_accepting()?;
        let slot = self.acquire_slot()?;
        let mut status = NativeStatus::default();
        // SAFETY: the bytes stay live for this synchronous host-to-device copy
        // into an idle slot owned by this worker thread.
        let uploaded = check(unsafe {
            amaterasu_nvcuda_upload_nv12(
                self.native.as_ptr(),
                slot,
                nv12.as_ptr(),
                nv12.len(),
                &mut status,
            )
        });
        self.submit_slot(slot, uploaded, timestamp_100ns, force_idr)
    }

    pub fn submit_gpu(
        &mut self,
        input: GpuInput,
        timestamp_100ns: i64,
        force_idr: bool,
    ) -> Result<(), &'static str> {
        self.ensure_accepting()?;
        let slot = self.acquire_slot()?;
        let converted = match input {
            GpuInput::Bgra(bgra) => self.upload_bgra(slot, &bgra),
            GpuInput::DmaBuf(frame) => self.import_dmabuf(slot, frame),
            GpuInput::Cuda(frame) => self.import_cuda(slot, frame),
        };
        self.submit_slot(slot, converted, timestamp_100ns, force_idr)
    }

    /// Submits a converted slot and hands it to the output thread. A failed
    /// conversion or submission returns the slot unused.
    fn submit_slot(
        &mut self,
        slot: u32,
        converted: Result<(), &'static str>,
        timestamp_100ns: i64,
        force_idr: bool,
    ) -> Result<(), &'static str> {
        let submitted = converted.and_then(|()| {
            let mut status = NativeStatus::default();
            // SAFETY: live handle on its owning thread; the slot holds a
            // converted input and is not visible to the output thread.
            check(unsafe {
                amaterasu_nvcuda_submit(
                    self.native.as_ptr(),
                    slot,
                    timestamp_100ns,
                    i32::from(force_idr),
                    &mut status,
                )
            })
        });
        if let Err(error) = submitted {
            self.pipeline.lock().idle.push(slot);
            return Err(error);
        }
        let queued = self.submissions.as_ref().is_some_and(|submissions| {
            submissions
                .send(Submission {
                    slot,
                    timestamp_100ns,
                    force_idr,
                })
                .is_ok()
        });
        if queued {
            Ok(())
        } else {
            Err("nvenc_cuda_output_thread_failed")
        }
    }

    /// Waits for an idle slot. Two frames may be in flight; a third waits for
    /// the oldest bitstream, which keeps backpressure within one frame.
    fn acquire_slot(&mut self) -> Result<u32, &'static str> {
        loop {
            self.read_completed_timings();
            let mut state = self.pipeline.lock();
            if let Some(failure) = state.failure {
                return Err(failure);
            }
            if state.packets.len() >= MAX_PENDING_PACKETS {
                return Err("nvenc_cuda_packet_queue_full");
            }
            if let Some(slot) = state.idle.pop() {
                return Ok(slot);
            }
            if state.completed.is_empty() {
                drop(
                    self.pipeline
                        .changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
            }
        }
    }

    /// Reads finished slots' GPU timing on this thread, which owns the
    /// conversion context, then makes those slots available for reuse.
    fn read_completed_timings(&mut self) {
        let completed: Vec<_> = self.pipeline.lock().completed.drain(..).collect();
        if completed.is_empty() {
            return;
        }
        for &(slot, finished_at) in &completed {
            let ago = finished_at.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            let mut timings = NativeStageTimings::default();
            // SAFETY: the output thread released this slot; only this worker
            // thread touches it until it is marked idle below.
            unsafe {
                amaterasu_nvcuda_take_gpu_timing(self.native.as_ptr(), slot, ago);
                amaterasu_nvcuda_get_timings(self.native.as_ptr(), slot, &mut timings);
            }
            let mut reported = self.stage_timings.borrow_mut();
            if reported.len() >= MAX_PENDING_PACKETS {
                reported.pop_front();
            }
            reported.push_back(timings);
        }
        self.pipeline
            .lock()
            .idle
            .extend(completed.into_iter().map(|(slot, _)| slot));
    }

    fn upload_bgra(&mut self, slot: u32, bgra: &[u8]) -> Result<(), &'static str> {
        let mut status = NativeStatus::default();
        // SAFETY: the bytes stay live until the GL upload call returns, and
        // GL copies client memory before glTexSubImage2D returns.
        check(unsafe {
            amaterasu_nvcuda_upload_bgra(
                self.native.as_ptr(),
                slot,
                bgra.as_ptr(),
                bgra.len(),
                &mut status,
            )
        })
    }

    /// Converts the frame into the slot's input buffer, then closes the
    /// duplicated descriptors before encoding so the producer's buffer is
    /// released as early as possible.
    fn import_dmabuf(&mut self, slot: u32, mut frame: DmaBufInput) -> Result<(), &'static str> {
        let mut native = NativeDmaBuf::borrow(&frame)?;
        let mut status = NativeStatus::default();
        // SAFETY: `frame` owns every descriptor in `native` for the call. The
        // adapter orders fences before its reads. A borrowed source is fully
        // read before return; a retained source stays referenced until its
        // encoded output, whose completion follows the queued reads.
        let result = unsafe {
            amaterasu_nvcuda_import_dmabuf(self.native.as_ptr(), slot, &mut native, &mut status)
        };
        release_consumed_fences(&mut frame, &native);
        drop(frame);
        check(result)
    }

    /// Queues a GPU wait on the producer's event and the conversion of its
    /// CUDA frame into the slot; no CPU wait and no GL context is involved.
    fn import_cuda(
        &mut self,
        slot: u32,
        frame: crate::input::CudaInput,
    ) -> Result<(), &'static str> {
        let mut status = NativeStatus::default();
        // SAFETY: the session retains the producer frame until this picture's
        // encoded output, keeping its memory and event valid while queued.
        check(unsafe {
            amaterasu_nvcuda_import_cuda(
                self.native.as_ptr(),
                slot,
                frame.pointer,
                frame.pitch,
                frame.width,
                frame.height,
                frame.ready as *mut c_void,
                &mut status,
            )
        })
    }

    fn cuda_input_device(&self) -> Option<[u8; 16]> {
        let mut uuid = [0u8; 16];
        // SAFETY: live handle on its owning thread and a 16-byte output.
        (unsafe { amaterasu_nvcuda_cuda_input(self.native.as_ptr(), uuid.as_mut_ptr()) } == 1)
            .then_some(uuid)
    }

    fn ensure_accepting(&self) -> Result<(), &'static str> {
        if self.draining {
            Err("nvenc_cuda_draining")
        } else {
            Ok(())
        }
    }

    /// Delivers finished packets in order; an output failure is reported
    /// only after the packets completed before it.
    pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        self.read_completed_timings();
        let mut state = self.pipeline.lock();
        if let Some(packet) = state.packets.pop_front() {
            return Ok(Some(packet));
        }
        match state.failure {
            Some(failure) => Err(failure),
            None => Ok(None),
        }
    }

    /// Waits for every submitted picture's output, then ends the stream;
    /// queued packets remain available to receive.
    pub fn drain(&mut self) -> Result<(), &'static str> {
        if self.draining {
            return Ok(());
        }
        loop {
            self.read_completed_timings();
            let state = self.pipeline.lock();
            if let Some(failure) = state.failure {
                return Err(failure);
            }
            if state.idle.len() == SLOTS as usize {
                break;
            }
            if state.completed.is_empty() {
                drop(
                    self.pipeline
                        .changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
            }
        }
        let mut status = NativeStatus::default();
        // SAFETY: live handle on its owning thread with no slot in flight.
        check(unsafe { amaterasu_nvcuda_drain(self.native.as_ptr(), &mut status) })?;
        self.draining = true;
        Ok(())
    }
}

/// Locks each submitted slot's bitstream in submission order, frames it into
/// a packet, releases the slot and wakes the module. Exits when the encoder
/// closes the submission channel, after every queued picture has finished.
fn run_output_thread(
    handle: OutputHandle,
    pipeline: &Pipeline,
    mut framer: OutputFramer,
    queued: &mpsc::Receiver<Submission>,
) {
    amaterasu_module_sdk::threading::configure_worker_priority(
        amaterasu_module_sdk::threading::ThreadPriority::High,
    );
    // SAFETY: the handle stays open until Encoder::drop joins this thread.
    let bound = unsafe { amaterasu_nvcuda_bind_thread(handle.0.as_ptr()) } == 0;
    while let Ok(submission) = queued.recv() {
        let result = if bound {
            finish_submission(handle, &mut framer, &submission)
        } else {
            Err("nvenc_cuda_output_thread_failed")
        };
        let finished_at = Instant::now();
        let mut state = pipeline.lock();
        match result {
            Ok(packet) if state.failure.is_none() => state.packets.push_back(packet),
            Ok(_) => {}
            Err(error) => {
                state.failure.get_or_insert(error);
            }
        }
        state.completed.push_back((submission.slot, finished_at));
        drop(state);
        pipeline.notify();
    }
    if bound {
        // SAFETY: balances the successful bind on this thread.
        unsafe { amaterasu_nvcuda_unbind_thread(handle.0.as_ptr()) };
    }
}

fn finish_submission(
    handle: OutputHandle,
    framer: &mut OutputFramer,
    submission: &Submission,
) -> Result<Packet, &'static str> {
    let mut data = std::ptr::null();
    let mut length = 0usize;
    let mut picture_type = 0u32;
    let mut status = NativeStatus::default();
    // SAFETY: this submitted slot belongs to the output thread until it is
    // reported completed; the out-pointers are live for the call.
    let locked = check(unsafe {
        amaterasu_nvcuda_lock(
            handle.0.as_ptr(),
            submission.slot,
            &mut data,
            &mut length,
            &mut picture_type,
            &mut status,
        )
    });
    let framed = locked.and_then(|()| {
        if data.is_null() || length == 0 || length > MAX_PACKET_BYTES {
            Err("nvenc_cuda_invalid_output")
        } else {
            // SAFETY: the locked bitstream stays valid until release_output;
            // the framer copies it into an owned packet first.
            let bitstream = unsafe { std::slice::from_raw_parts(data, length) };
            framer.frame(bitstream, picture_type)
        }
    });
    // SAFETY: unlocks (when locked) and unmaps this slot's input in all cases.
    let released =
        unsafe { amaterasu_nvcuda_release_output(handle.0.as_ptr(), submission.slot, &mut status) };
    let (bytes, keyframe) = framed?;
    check(released)?;
    if submission.force_idr && !keyframe {
        return Err("nvenc_cuda_forced_idr_missing");
    }
    Ok(Packet {
        ready_at: Some(Instant::now()),
        bytes,
        timestamp_100ns: submission.timestamp_100ns,
        keyframe,
    })
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Closing the channel lets the output thread finish every submitted
        // picture and exit; the handle must outlive it.
        self.submissions.take();
        if let Some(thread) = self.output_thread.take() {
            let _ = thread.join();
        }
        // SAFETY: the unique handle, closed once, on the thread that opened it,
        // after the output thread has exited.
        unsafe { amaterasu_nvcuda_close(self.native.as_ptr()) };
    }
}

/// A fence the adapter handed to EGL (`EGL_ANDROID_native_fence_sync` takes
/// ownership on success) is reported as -1 and must not be closed here.
fn release_consumed_fences(frame: &mut DmaBufInput, native: &NativeDmaBuf) {
    for (plane, fence_fd) in frame.planes.iter_mut().zip(native.fence_fds) {
        if fence_fd < 0
            && let Some(fence) = plane.acquire_fence.take()
        {
            let _owned_by_egl = fence.into_raw_fd();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::DmaBufPlaneInput;
    use std::os::fd::OwnedFd;

    #[test]
    fn conversion_copy_uses_stream_or_synchronous_fallback_and_unmaps_on_failure() {
        for asynchronous in [0, 1] {
            for fail_copy in [0, 1, 2, 3] {
                // SAFETY: isolated test driver with no GPU handles or shared callers.
                assert_eq!(
                    unsafe { amaterasu_nvcuda_test_conversion_stream(asynchronous, fail_copy) },
                    1
                );
            }
        }
        // SAFETY: exercises synchronous fallback with a failed context wait.
        assert_eq!(unsafe { amaterasu_nvcuda_test_conversion_stream(0, 4) }, 1);
    }

    #[test]
    fn failed_cuda_retirement_does_not_free_in_flight_resources() {
        // SAFETY: synthetic callbacks create no actual GPU resources.
        for mapping_unproven in [0, 1, 2] {
            assert_eq!(
                unsafe { amaterasu_nvcuda_test_failed_retirement(mapping_unproven) },
                1
            );
        }
    }

    #[test]
    fn nvenc_stream_setup_falls_back_when_stream_support_is_unavailable() {
        for mode in [0, 1, 2] {
            // SAFETY: test function uses only synthetic CUDA/NVENC callbacks.
            assert_eq!(unsafe { amaterasu_nvcuda_test_stream_setup(mode) }, 1);
        }
    }

    /// A pipe's read end stands in for a DMA-BUF or sync_file descriptor;
    /// writing to the other end fails with EPIPE once every reader closed,
    /// regardless of descriptor-number reuse by concurrent tests.
    fn stand_in() -> (OwnedFd, std::io::PipeWriter) {
        let (reader, writer) = std::io::pipe().unwrap();
        (reader.into(), writer)
    }

    fn reader_closed(writer: &mut std::io::PipeWriter) -> bool {
        use std::io::Write;
        writer.write(&[0]).is_err()
    }

    #[test]
    fn mirrored_records_match_the_adapter_layout() {
        let mut sizes = AbiSizes::default();
        // SAFETY: GPU-free; writes one plain record.
        unsafe { amaterasu_nvcuda_abi_sizes(&mut sizes) };
        assert_eq!(
            sizes,
            AbiSizes {
                status: size_of::<NativeStatus>() as u32,
                struct_versions: size_of::<StructVersions>() as u32,
                device: size_of::<NativeDevice>() as u32,
                settings: size_of::<NativeSettings>() as u32,
                dmabuf: size_of::<NativeDmaBuf>() as u32,
                test_buffer: size_of::<TestBuffer>() as u32,
            }
        );
    }

    #[test]
    fn compiled_header_versions_follow_the_struct_version_encoding() {
        let header = header_versions().unwrap();
        let api = versions::ApiVersion::from_nvenc(header.api);
        assert!(
            api.major >= 13,
            "compiled header must provide AV1 bit-depth fields"
        );
        assert_eq!(header.for_api(api), header);
        for version in [header.config, header.initialize, header.pic_params] {
            assert_eq!(version & 0x7000_0000, 0x7000_0000);
            assert_eq!(version & 0x8000_0000, 0x8000_0000);
        }
    }

    #[test]
    fn native_strings_stop_at_the_terminator() {
        let mut device = NativeDevice::EMPTY;
        for (slot, byte) in device.pci_bus_id.iter_mut().zip(b"0000:01:00.0\0junk") {
            *slot = *byte as c_char;
        }
        device.codecs[2] = NativeCodecCaps {
            hdr10: 0,
            supported: 1,
            max_width: 8192,
            max_height: 8192,
        };
        device.gpu_conversion = 1;
        let record = device.record();
        assert_eq!(record.pci_bus_id, "0000:01:00.0");
        assert_eq!(record.name, "");
        assert!(record.codecs[2].supported && !record.codecs[0].supported);
        assert!(record.gpu_conversion && !record.dmabuf_import);
    }

    #[test]
    fn dmabuf_records_borrow_descriptors_and_fences_moved_to_egl_stay_open() {
        let (plane, mut plane_writer) = stand_in();
        let (kept_fence, mut kept_writer) = stand_in();
        let (moved_fence, mut moved_writer) = stand_in();
        let (second_plane, _second_writer) = stand_in();
        let (plane_fd, kept_fd, moved_fd) = (
            plane.as_raw_fd(),
            kept_fence.as_raw_fd(),
            moved_fence.as_raw_fd(),
        );
        let mut frame = DmaBufInput {
            width: 64,
            height: 32,
            fourcc: fourcc(b"XR24"),
            modifier: 0,
            planes: vec![
                DmaBufPlaneInput {
                    fd: plane,
                    acquire_fence: Some(kept_fence),
                    offset: 0,
                    pitch: 256,
                },
                DmaBufPlaneInput {
                    fd: second_plane,
                    acquire_fence: Some(moved_fence),
                    offset: 8192,
                    pitch: 256,
                },
            ],
            source_retained: true,
        };
        let mut native = NativeDmaBuf::borrow(&frame).unwrap();
        assert_eq!(native.plane_count, 2);
        assert_eq!(native.source_retained, 1);
        assert_eq!(native.plane_fds[0], plane_fd);
        assert_eq!(native.fence_fds[..2], [kept_fd, moved_fd]);
        assert_eq!(native.plane_fds[2..], [-1, -1]);
        assert_eq!(native.offsets[1], 8192);

        native.fence_fds[1] = -1;
        release_consumed_fences(&mut frame, &native);
        assert!(frame.planes[0].acquire_fence.is_some());
        assert!(frame.planes[1].acquire_fence.is_none());
        drop(frame);
        assert!(reader_closed(&mut plane_writer));
        assert!(reader_closed(&mut kept_writer));
        // The adapter's EGL sync owns this one now; release it as EGL would.
        assert!(!reader_closed(&mut moved_writer));
        // SAFETY: this test took the descriptor's ownership from the frame.
        drop(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(moved_fd) });
        assert!(reader_closed(&mut moved_writer));
    }

    #[test]
    fn import_formats_are_the_packed_rgb_fourccs() {
        assert_eq!(IMPORT_FOURCCS[0], 0x3432_5258);
        assert_eq!(IMPORT_FOURCCS[4], 0x3033_5258);
    }
}

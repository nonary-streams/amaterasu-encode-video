//! A single outstanding frame keeps CPU conversion off the input/runtime loop.
//! The joined worker owns its FFmpeg context; only owned pixels cross threads.
use super::{OwnedFrame, Progress, Pump, elapsed_micros};
use crate::model::Configuration;
use amaterasu_media::{
    CaptureApiPrefix, ColorMode, CpuFrame, EncoderApi, FrameSink, MEDIA_BUSY, MEDIA_PENDING,
    PIXEL_BGRA8, PIXEL_NV12,
};
use amaterasu_module_sdk::abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status};
use std::{
    ffi::c_void,
    ptr::NonNull,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    thread::{self, JoinHandle},
    time::Instant,
};

unsafe extern "C" {
    fn am_video_converter_open() -> *mut c_void;
    fn am_video_converter_close(context: *mut c_void);
    fn am_video_convert(
        context: *mut c_void,
        source: *const u8,
        source_width: i32,
        source_height: i32,
        source_stride: i32,
        destination: *mut u8,
        width: i32,
        height: i32,
        bt709: i32,
        full_range: i32,
    ) -> i32;
}

struct Converter(NonNull<c_void>);
trait FrameConverter {
    fn convert(&mut self, job: &Job) -> Result<OwnedFrame, &'static str>;
}
impl Converter {
    fn new() -> Result<Self, &'static str> {
        // SAFETY: creates an opaque context, used and destroyed on this worker.
        NonNull::new(unsafe { am_video_converter_open() })
            .map(Self)
            .ok_or("conversion_allocation_failed")
    }
}
impl FrameConverter for Converter {
    fn convert(&mut self, job: &Job) -> Result<OwnedFrame, &'static str> {
        let configuration = &job.configuration;
        let mut bytes = Vec::new();
        let length = configuration.width as usize * configuration.height as usize * 3 / 2;
        bytes
            .try_reserve_exact(length)
            .map_err(|_| "conversion_allocation_failed")?;
        bytes.resize(length, 0);
        let source = &job.source;
        let stride = i32::try_from(source.stride).map_err(|_| "invalid_capture_stride")?;
        let bt709 = matches!(
            configuration.color_mode,
            ColorMode::Bt709Full | ColorMode::Bt709Limited
        );
        let full_range = matches!(
            configuration.color_mode,
            ColorMode::Bt709Full | ColorMode::Bt601Full
        );
        // SAFETY: validated owned BGRA rows and a fully allocated NV12 destination
        // survive the synchronous call. C retains neither pixel pointer.
        let result = unsafe {
            am_video_convert(
                self.0.as_ptr(),
                source.bytes.as_ptr(),
                source.width as i32,
                source.height as i32,
                stride,
                bytes.as_mut_ptr(),
                configuration.width as i32,
                configuration.height as i32,
                i32::from(bt709),
                i32::from(full_range),
            )
        };
        if result != 0 {
            return Err("frame_conversion_failed");
        }
        Ok(OwnedFrame {
            width: configuration.width,
            height: configuration.height,
            stride: configuration.width,
            format: PIXEL_NV12,
            bytes,
        })
    }
}
impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: uniquely owned context is released on its creating worker.
        unsafe { am_video_converter_close(self.0.as_ptr()) };
    }
}

struct Job {
    video: u64,
    configuration: Configuration,
    source: OwnedFrame,
}
struct Prepared {
    video: u64,
    pixels: OwnedFrame,
    conversion_us: u64,
}
type Completion = Result<Prepared, &'static str>;

#[derive(Default)]
pub(super) struct Processors {
    streams: std::collections::BTreeMap<u64, (u64, Processor)>,
}
impl Processors {
    pub fn pump(
        &mut self,
        capture: &CaptureApiPrefix,
        encoder: &EncoderApi,
        input: &Pump<'_>,
    ) -> Result<Progress, &'static str> {
        if self
            .streams
            .get(&input.video)
            .is_some_and(|(session, _)| *session != input.encoder)
        {
            self.remove(input.video);
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = self.streams.entry(input.video) {
            entry.insert((input.encoder, Processor::new()?));
        }
        let result = self
            .streams
            .get_mut(&input.video)
            .unwrap()
            .1
            .pump(capture, encoder, input);
        if result.is_err() {
            self.remove(input.video);
        }
        result
    }

    pub fn remove(&mut self, video: u64) {
        self.streams.remove(&video);
    }
}

pub(super) struct Processor {
    sender: Option<SyncSender<Job>>,
    receiver: Option<Receiver<Completion>>,
    worker: Option<JoinHandle<()>>,
    outstanding: bool,
    pending: Option<Prepared>,
}
impl Processor {
    pub fn new() -> Result<Self, &'static str> {
        Self::with_converter(Converter::new)
    }
    fn with_converter<C: FrameConverter + 'static>(
        factory: impl FnOnce() -> Result<C, &'static str> + Send + 'static,
    ) -> Result<Self, &'static str> {
        let (sender, jobs) = mpsc::sync_channel::<Job>(1);
        let (results, receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("amaterasu-convert".into())
            .spawn(move || {
                let mut converter = factory();
                while let Ok(job) = jobs.recv() {
                    let started = Instant::now();
                    let converted = (|| {
                        let pixels = converter.as_mut().map_err(|error| *error)?.convert(&job)?;
                        Ok(Prepared {
                            video: job.video,
                            pixels,
                            conversion_us: elapsed_micros(started),
                        })
                    })();
                    if results.send(converted).is_err() {
                        break;
                    }
                }
            })
            .map_err(|_| "conversion_worker_failed")?;
        Ok(Self {
            sender: Some(sender),
            receiver: Some(receiver),
            worker: Some(worker),
            outstanding: false,
            pending: None,
        })
    }

    pub fn pump(
        &mut self,
        capture: &CaptureApiPrefix,
        encoder: &EncoderApi,
        input: &Pump<'_>,
    ) -> Result<Progress, &'static str> {
        let mut progress = Progress::default();
        if self.pending.is_none() && self.outstanding {
            match self.receiver.as_ref().unwrap().try_recv() {
                Ok(result) => self.pending = Some(result?),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return Err("conversion_worker_failed"),
            }
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|frame| frame.video != input.video)
        {
            self.pending = None;
            self.outstanding = false;
        }
        if let Some(prepared) = self.pending.as_ref() {
            let pixels = &prepared.pixels;
            let frame = CpuFrame {
                width: pixels.width,
                height: pixels.height,
                format: pixels.format,
                stride: pixels.stride,
                data: pixels.bytes.as_ptr(),
                len: pixels.bytes.len(),
                timestamp_us: input.timestamp_us,
                sequence: input.sequence,
            };
            let started = Instant::now();
            // SAFETY: prepared pixels remain owned through submission; the
            // encoder copies accepted input before returning.
            let status = unsafe {
                (encoder.submit)(
                    encoder.base.context,
                    input.encoder,
                    &frame,
                    u32::from(input.force_idr),
                )
            };
            progress.encoder_submit_us = elapsed_micros(started);
            if !matches!(status, STATUS_OK | MEDIA_BUSY) {
                return Err("frame_rejected");
            }
            progress.dropped = status == MEDIA_BUSY;
            if status == STATUS_OK {
                progress.submitted = true;
                progress.frame = Some(super::FrameMetadata {
                    width: frame.width,
                    height: frame.height,
                    stride: frame.stride,
                    timestamp_us: frame.timestamp_us,
                    sequence: frame.sequence,
                });
                let prepared = self.pending.take().unwrap();
                progress.preprocessing_us = prepared.conversion_us;
                progress.owned_frame = Some(prepared.pixels);
                self.outstanding = false;
            }
        }
        progress.capture_deferred = input.capture_due && self.outstanding;
        if input.capture_due && !self.outstanding {
            let mut request = CaptureRequest {
                source: None,
                status: MEDIA_PENDING,
            };
            let sink = FrameSink {
                context: (&mut request as *mut CaptureRequest).cast(),
                write: copy_frame,
            };
            let started = Instant::now();
            // SAFETY: request is a live stack allocation until the synchronous
            // provider call returns; the callback copies borrowed pixels.
            let result = unsafe { (capture.read_frame)(capture.base.context, input.capture, sink) };
            progress.capture_callback_us = elapsed_micros(started);
            progress.capture_callback_count = 1;
            if !matches!(result, STATUS_OK | MEDIA_BUSY | MEDIA_PENDING) {
                return Err("capture_failed");
            }
            if !matches!(request.status, STATUS_OK | MEDIA_PENDING) {
                return Err("frame_rejected");
            }
            if let Some(source) = request.source {
                progress.capture_frame_count = 1;
                self.sender
                    .as_ref()
                    .unwrap()
                    .try_send(Job {
                        video: input.video,
                        configuration: input.configuration.clone(),
                        source,
                    })
                    .map_err(|_| "conversion_worker_failed")?;
                self.outstanding = true;
            }
        }
        Ok(progress)
    }
}
impl Drop for Processor {
    fn drop(&mut self) {
        // Close both channels before joining so a completed conversion can never
        // block on delivery during unload. No native code outlives this join.
        self.sender.take();
        self.receiver.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct CaptureRequest {
    source: Option<OwnedFrame>,
    status: Status,
}
unsafe extern "C" fn copy_frame(context: *mut c_void, frame: *const CpuFrame) -> Status {
    if context.is_null() || frame.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the provider retains its borrowed frame and this live stack
    // context through the synchronous callback. Copy before returning.
    let request = unsafe { &mut *context.cast::<CaptureRequest>() };
    let frame = unsafe { &*frame };
    request.status = STATUS_INVALID_ARGUMENT;
    if frame.format != PIXEL_BGRA8 || frame.width > 8192 || frame.height > 8192 {
        return STATUS_OK;
    }
    let Ok(bytes) = (unsafe { frame.bytes() }) else {
        return STATUS_OK;
    };
    let mut owned = Vec::new();
    if owned.try_reserve_exact(bytes.len()).is_err() {
        return STATUS_OK;
    }
    owned.extend_from_slice(bytes);
    request.source = Some(OwnedFrame {
        width: frame.width,
        height: frame.height,
        format: frame.format,
        stride: frame.stride,
        bytes: owned,
    });
    request.status = STATUS_OK;
    STATUS_OK
}

#[cfg(test)]
mod tests;

//! Borrowed DMA-BUF delivery from a v1.4 capture provider straight into a v1.1
//! encoder. No pixels enter video-owned memory: the producer lends its
//! descriptors for one synchronous callback, and the encoder duplicates any
//! descriptor it keeps before `submit_dmabuf` returns. A v1.5 provider also
//! reports its newest completed presentation, which drives push pacing.
mod resource;
use super::{FrameMetadata, PresentationQuery, Progress, Pump, elapsed_micros};
use amaterasu_media::*;
use amaterasu_module_sdk::abi::*;
pub(super) use resource::{CachedSelection, deliver as deliver_resource};
use std::{ffi::c_void, time::Instant};

/// The capture table advertises v1.4 and is large enough to hold
/// `read_dmabuf_frame`. Smaller tables are never read past their size.
pub fn capture_tail_available(header: AbiHeader) -> bool {
    header.minor >= CAPTURE_DMABUF_INTERFACE_MINOR && header.size >= CAPTURE_API_V1_4_SIZE
}

/// The capture table advertises v1.5 and is large enough to hold
/// `latest_presentation`. Smaller tables are never read past their size.
pub fn presentation_tail_available(header: AbiHeader) -> bool {
    header.minor >= CAPTURE_PRESENTATION_INTERFACE_MINOR && header.size >= CAPTURE_API_V1_5_SIZE
}

/// Asks a v1.5 provider for its newest completed presentation. The query is
/// non-blocking and delivers no frame. `MEDIA_UNSUPPORTED` keeps the stream on
/// timer pacing; any other failure is a capture failure.
pub fn latest_presentation(
    capture: &CaptureApiV1_5,
    stream: u64,
) -> Result<PresentationQuery, &'static str> {
    let mut presentation = PresentationStatus::default();
    // SAFETY: `presentation` is a live, writable stack value of the exact ABI
    // type for this synchronous call, and the provider retains nothing.
    let status =
        unsafe { (capture.latest_presentation)(capture.base.context, stream, &mut presentation) };
    match status {
        STATUS_OK if presentation.sequence == 0 => Ok(PresentationQuery::Pending),
        STATUS_OK => Ok(PresentationQuery::Latest(presentation)),
        MEDIA_PENDING | MEDIA_BUSY => Ok(PresentationQuery::Pending),
        MEDIA_UNSUPPORTED => Ok(PresentationQuery::Unavailable),
        _ => Err("capture_failed"),
    }
}

/// The encoder table advertises v1.1 and is large enough to hold `submit_dmabuf`.
pub fn lease_tail_available(header: AbiHeader) -> bool {
    header.minor >= CAPTURE_LEASE_INTERFACE_MINOR && header.size >= CAPTURE_API_V1_6_SIZE
}

pub fn encoder_lease_tail_available(header: AbiHeader) -> bool {
    header.minor >= ENCODER_LEASE_INTERFACE_MINOR && header.size >= ENCODER_API_V1_2_SIZE
}

pub fn require_lease_path(capture: AbiHeader, encoder: AbiHeader) -> Result<(), &'static str> {
    if !lease_tail_available(capture) {
        return Err("capture_frame_lease_required");
    }
    if !encoder_lease_tail_available(encoder) {
        return Err("encoder_frame_lease_required");
    }
    Ok(())
}

pub fn encoder_tail_available(header: AbiHeader) -> bool {
    header.minor >= ENCODER_DMABUF_INTERFACE_MINOR && header.size >= ENCODER_API_V1_1_SIZE
}

/// Why this capture/encoder table pair cannot exchange DMA-BUF frames.
pub fn path_unavailable(capture: AbiHeader, encoder: AbiHeader) -> Option<&'static str> {
    if !capture_tail_available(capture) {
        Some("capture_dmabuf_interface_unavailable")
    } else if !encoder_tail_available(encoder) {
        Some("encoder_dmabuf_interface_unavailable")
    } else {
        None
    }
}

/// Selects the additive ownership path without changing the legacy ABI route.
/// A configured GPU processor is mandatory, so it cannot silently be skipped.
pub fn use_lease_path(
    capture: AbiHeader,
    encoder: AbiHeader,
    processor: bool,
) -> Result<bool, &'static str> {
    let capture_lease = lease_tail_available(capture);
    let encoder_lease = encoder_lease_tail_available(encoder);
    if processor && !capture_lease {
        return Err("gpu_processor_requires_frame_lease");
    }
    if processor && !encoder_lease {
        return Err("gpu_processor_requires_encoder_lease");
    }
    Ok(capture_lease && encoder_lease)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// The capture callback has not run.
    NotDelivered,
    Accepted,
    /// The encoder accepted nothing; the presentation is dropped like a busy CPU frame.
    Busy,
    /// This stream cannot use DMA-BUF frames; the reason selects CPU capture.
    Unsupported(&'static str),
    Rejected,
}

/// No source constraint preserves legacy operation. Explicit processing plans
/// accept only a capture-provided, recognized sRGB or HDR10 color tag.
fn source_color_matches(required_hdr: Option<bool>, color: u32) -> bool {
    match required_hdr {
        None => true,
        Some(false) => color == DMABUF_COLOR_SRGB,
        Some(true) => color == DMABUF_COLOR_BT2020_PQ,
    }
}

struct Submission<'a> {
    api: &'a EncoderApiV1_1,
    encoder: u64,
    width: u32,
    height: u32,
    force_idr: bool,
    /// Require a capture-supplied, recognized source color on each frame.
    source_hdr_required: Option<bool>,
    output_hdr_required: Option<bool>,
    timestamp_us: u64,
    sequence: u64,
    outcome: Outcome,
    fresh: Option<bool>,
    /// Provider sequence of the delivered presentation.
    presentation: Option<u64>,
    frame: Option<FrameMetadata>,
    encoder_submit_us: u64,
}

impl Submission<'_> {
    /// Forwards one borrowed presentation with video's own stream clock.
    ///
    /// # Safety
    /// Every descriptor in `frame` must remain open until this call returns.
    unsafe fn forward(&mut self, frame: &DmaBufFrame) -> Outcome {
        self.presentation = Some(frame.sequence);
        if frame.validate().is_err()
            || !source_color_matches(self.source_hdr_required, frame.color)
            || !source_color_matches(self.output_hdr_required, frame.color)
        {
            return Outcome::Rejected;
        }
        if frame.width != self.width || frame.height != self.height {
            // The GPU path never scales; PipeWire capture fits mismatched sizes.
            return Outcome::Unsupported("dmabuf_size_mismatch");
        }
        let mut forwarded = *frame;
        forwarded.timestamp_us = self.timestamp_us;
        forwarded.sequence = self.sequence;
        let started = Instant::now();
        // SAFETY: `forwarded` copies descriptor numbers only. The producer keeps
        // them open until its callback returns, which encloses this call, and the
        // encoder duplicates anything it retains before returning.
        let status = unsafe {
            (self.api.submit_dmabuf)(
                self.api.base.context,
                self.encoder,
                &forwarded,
                u32::from(self.force_idr),
            )
        };
        self.encoder_submit_us = elapsed_micros(started);
        match status {
            STATUS_OK => {
                self.frame = Some(FrameMetadata {
                    width: frame.width,
                    height: frame.height,
                    stride: frame.planes[0].pitch,
                    timestamp_us: self.timestamp_us,
                    sequence: self.sequence,
                });
                Outcome::Accepted
            }
            MEDIA_BUSY => Outcome::Busy,
            MEDIA_UNSUPPORTED => Outcome::Unsupported("encoder_dmabuf_unsupported"),
            _ => Outcome::Rejected,
        }
    }
}

unsafe extern "C" fn submit_borrowed_frame(
    context: *mut c_void,
    frame: *const DmaBufFrame,
    freshness: u32,
) -> Status {
    if context.is_null() || frame.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    let fresh = match freshness {
        CAPTURE_FRAME_FRESH => true,
        CAPTURE_FRAME_CACHED_REUSE => false,
        _ => return STATUS_INVALID_ARGUMENT,
    };
    // SAFETY: `deliver` passes its live stack submission as the sink context,
    // and the provider keeps `frame` readable until this callback returns.
    let submission = unsafe { &mut *context.cast::<Submission<'_>>() };
    let frame = unsafe { &*frame };
    submission.fresh = Some(fresh);
    // SAFETY: the provider lends every descriptor through this callback.
    submission.outcome = unsafe { submission.forward(frame) };
    // The borrow ends here whatever the encoder decided; nothing is retained.
    STATUS_OK
}

/// Reads at most one DMA-BUF presentation and submits it to the encoder inside
/// the capture callback. `repeat` marks a keepalive re-read of the latest
/// presentation, which providers may satisfy with cached reuse.
pub fn deliver(
    capture: &CaptureApiV1_4,
    encoder: &EncoderApiV1_1,
    input: &Pump<'_>,
    repeat: bool,
) -> Result<Progress, &'static str> {
    let mut submission = Submission {
        api: encoder,
        encoder: input.encoder,
        width: input.configuration.width,
        height: input.configuration.height,
        force_idr: input.force_idr,
        source_hdr_required: input
            .configuration
            .gpu_processing
            .as_ref()
            .map(|selection| selection.source_hdr_required),
        output_hdr_required: input.configuration.gpu_processing.as_ref().map(|_| {
            input.configuration.video_stream_format.dynamic_range
                == amaterasu_module_sdk::DynamicRange::Hdr
        }),
        timestamp_us: input.timestamp_us,
        sequence: input.sequence,
        outcome: Outcome::NotDelivered,
        fresh: None,
        presentation: None,
        frame: None,
        encoder_submit_us: 0,
    };
    let sink = DmaBufFrameSink {
        context: (&mut submission as *mut Submission<'_>).cast(),
        write: submit_borrowed_frame,
    };
    let started = Instant::now();
    // SAFETY: the stack sink and its submission outlive this synchronous read;
    // the callback forwards borrowed descriptors without retaining them.
    let status = unsafe { (capture.read_dmabuf_frame)(capture.base.context, input.capture, sink) };
    let capture_callback_us = elapsed_micros(started);
    let capture_fallback = match status {
        STATUS_OK | MEDIA_PENDING | MEDIA_BUSY => None,
        MEDIA_UNSUPPORTED => Some("capture_dmabuf_unsupported"),
        _ => return Err("capture_failed"),
    };
    let delivered = submission.outcome != Outcome::NotDelivered;
    let dmabuf_fallback = match submission.outcome {
        Outcome::Rejected => return Err("frame_rejected"),
        Outcome::Unsupported(reason) => Some(reason),
        _ => capture_fallback,
    };
    Ok(Progress {
        dropped: submission.outcome == Outcome::Busy,
        submitted: submission.outcome == Outcome::Accepted,
        repeat_attempted: repeat,
        capture_reused: submission.fresh == Some(false),
        dmabuf_fresh: submission.fresh,
        dmabuf_fallback,
        dmabuf_presentation: submission.presentation,
        frame: submission.frame,
        capture_callback_us,
        capture_callback_count: 1,
        capture_frame_count: u64::from(delivered),
        encoder_submit_us: submission.encoder_submit_us,
        ..Progress::default()
    })
}

struct LeasedSubmission<'a> {
    common: Submission<'a>,
    encoder: &'a EncoderApiV1_2,
    processor: Option<&'a GpuProcessorApi>,
    video: u64,
    calls: usize,
    invalid: bool,
}

impl LeasedSubmission<'_> {
    /// The encoder retains this immutable lease if work continues after submit.
    /// It must release all accepted input before stream close completes.
    unsafe fn forward(&mut self, lease: &DmaBufFrameLease) -> Outcome {
        let frame = &lease.frame;
        self.common.presentation = Some(frame.sequence);
        if lease.validate().is_err()
            || !source_color_matches(self.common.output_hdr_required, frame.color)
        {
            return Outcome::Rejected;
        }
        if frame.width != self.common.width || frame.height != self.common.height {
            return Outcome::Unsupported("dmabuf_size_mismatch");
        }
        let mut forwarded = *lease;
        forwarded.frame.timestamp_us = self.common.timestamp_us;
        forwarded.frame.sequence = self.common.sequence;
        let started = Instant::now();
        // SAFETY: the retained input owner and both bindings remain live during
        // submit. An accepting encoder acquires its own reference before return.
        let status = unsafe {
            (self.encoder.submit_dmabuf_leased)(
                self.encoder.base.context,
                self.common.encoder,
                &forwarded,
                u32::from(self.common.force_idr),
            )
        };
        self.common.encoder_submit_us = elapsed_micros(started);
        match status {
            STATUS_OK => {
                self.common.frame = Some(FrameMetadata {
                    width: frame.width,
                    height: frame.height,
                    stride: frame.planes[0].pitch,
                    timestamp_us: self.common.timestamp_us,
                    sequence: self.common.sequence,
                });
                Outcome::Accepted
            }
            MEDIA_BUSY => Outcome::Busy,
            MEDIA_UNSUPPORTED => Outcome::Unsupported("encoder_dmabuf_lease_unsupported"),
            _ => Outcome::Rejected,
        }
    }
}

unsafe extern "C" fn submit_leased_frame(
    context: *mut c_void,
    lease: *const DmaBufFrameLease,
    freshness: u32,
) -> Status {
    if context.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the read owns this stack context for the synchronous callback.
    let submission = unsafe { &mut *context.cast::<LeasedSubmission<'_>>() };
    submission.calls += 1;
    if submission.calls != 1
        || lease.is_null()
        || !matches!(freshness, CAPTURE_FRAME_FRESH | CAPTURE_FRAME_CACHED_REUSE)
    {
        submission.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: capture lends this descriptor through the callback.
    let lease = unsafe { &*lease };
    if !source_color_matches(submission.common.source_hdr_required, lease.frame.color) {
        submission.common.outcome = Outcome::Rejected;
        return STATUS_OK;
    }
    submission.common.fresh = Some(freshness == CAPTURE_FRAME_FRESH);
    // SAFETY: capture and processor bindings outlive the synchronous pump; the
    // temporary retained reference is dropped before the callback returns.
    let retained = match unsafe { super::gpu_processor::ScopedLease::retain(lease) } {
        Ok(retained) => retained,
        Err(status) => {
            submission.common.outcome = if status == MEDIA_BUSY {
                Outcome::Busy
            } else {
                Outcome::Rejected
            };
            return STATUS_OK;
        }
    };
    submission.common.outcome = if let Some(processor) = submission.processor {
        // SAFETY: retained frame and processor table stay alive throughout the
        // synchronous processing callback and encoder submission.
        match unsafe {
            super::gpu_processor::dispatch(
                processor,
                submission.video,
                retained.descriptor(),
                freshness,
                |output| {
                    let outcome = submission.forward(output);
                    submission.common.outcome = outcome;
                    match outcome {
                        Outcome::Accepted => STATUS_OK,
                        Outcome::Busy => MEDIA_BUSY,
                        Outcome::Unsupported(_) => MEDIA_UNSUPPORTED,
                        _ => STATUS_INVALID_ARGUMENT,
                    }
                },
            )
        } {
            super::processor::Outcome::Accepted => Outcome::Accepted,
            super::processor::Outcome::Busy => Outcome::Busy,
            super::processor::Outcome::Failed(MEDIA_UNSUPPORTED) => {
                if matches!(submission.common.outcome, Outcome::Unsupported(_)) {
                    submission.common.outcome
                } else {
                    Outcome::Unsupported("gpu_processor_unsupported")
                }
            }
            _ => Outcome::Rejected,
        }
    } else {
        // SAFETY: this retained frame outlives synchronous submission.
        unsafe { submission.forward(retained.descriptor()) }
    };
    STATUS_OK
}

/// Exchanges immutable GPU ownership with capture, an optional processor, and
/// a lease-aware encoder. No video-owned reference survives this call.
pub fn deliver_leased(
    capture: &CaptureApiV1_6,
    encoder: &EncoderApiV1_2,
    processor: Option<&GpuProcessorApi>,
    input: &Pump<'_>,
    repeat: bool,
) -> Result<Progress, &'static str> {
    // SAFETY: EncoderApiV1_2 is an additive extension of the exact v1.1 prefix.
    let legacy_encoder = unsafe { &*(encoder as *const EncoderApiV1_2).cast::<EncoderApiV1_1>() };
    let mut submission = LeasedSubmission {
        common: Submission {
            api: legacy_encoder,
            encoder: input.encoder,
            width: input.configuration.width,
            height: input.configuration.height,
            force_idr: input.force_idr,
            source_hdr_required: input
                .configuration
                .gpu_processing
                .as_ref()
                .map(|selection| selection.source_hdr_required),
            output_hdr_required: input.configuration.gpu_processing.as_ref().map(|_| {
                input.configuration.video_stream_format.dynamic_range
                    == amaterasu_module_sdk::DynamicRange::Hdr
            }),
            timestamp_us: input.timestamp_us,
            sequence: input.sequence,
            outcome: Outcome::NotDelivered,
            fresh: None,
            presentation: None,
            frame: None,
            encoder_submit_us: 0,
        },
        encoder,
        processor,
        video: input.video,
        calls: 0,
        invalid: false,
    };
    let sink = DmaBufLeaseSink {
        context: (&mut submission as *mut LeasedSubmission<'_>).cast(),
        write: submit_leased_frame,
    };
    let started = Instant::now();
    // SAFETY: the stack sink remains alive through this synchronous read.
    let status =
        unsafe { (capture.read_dmabuf_frame_leased)(capture.base.context, input.capture, sink) };
    if submission.invalid
        || submission.calls > 1
        || (status == STATUS_OK && submission.calls != 1)
        || (submission.calls == 1 && status != STATUS_OK)
    {
        return Err("capture_lease_protocol");
    }
    let common = submission.common;
    let fallback = match status {
        STATUS_OK | MEDIA_BUSY | MEDIA_PENDING => None,
        MEDIA_UNSUPPORTED => Some("capture_dmabuf_lease_unsupported"),
        _ => return Err("capture_failed"),
    };
    let dmabuf_fallback = match common.outcome {
        Outcome::Rejected => return Err("frame_rejected"),
        Outcome::Unsupported(reason) => Some(reason),
        _ => fallback,
    };
    Ok(Progress {
        dropped: common.outcome == Outcome::Busy,
        submitted: common.outcome == Outcome::Accepted,
        repeat_attempted: repeat,
        capture_reused: common.fresh == Some(false),
        dmabuf_fresh: common.fresh,
        dmabuf_fallback,
        dmabuf_presentation: common.presentation,
        frame: common.frame,
        capture_callback_us: elapsed_micros(started),
        capture_callback_count: 1,
        capture_frame_count: u64::from(submission.calls == 1),
        encoder_submit_us: common.encoder_submit_us,
        ..Progress::default()
    })
}

#[cfg(test)]
mod tests;

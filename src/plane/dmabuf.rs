//! Borrowed DMA-BUF delivery from a v1.4 capture provider straight into a v1.1
//! encoder. No pixels enter video-owned memory: the producer lends its
//! descriptors for one synchronous callback, and the encoder duplicates any
//! descriptor it keeps before `submit_dmabuf` returns. A v1.5 provider also
//! reports its newest completed presentation, which drives push pacing.
use super::{FrameMetadata, PresentationQuery, Progress, Pump, elapsed_micros};
use amaterasu_media::*;
use amaterasu_module_sdk::abi::*;
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

struct Submission<'a> {
    api: &'a EncoderApiV1_1,
    encoder: u64,
    width: u32,
    height: u32,
    force_idr: bool,
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
        if frame.validate().is_err() {
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

#[cfg(test)]
mod tests;

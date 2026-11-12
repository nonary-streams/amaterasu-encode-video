//! Marks each frame's stages inside the video pipeline. The session continues
//! the same flow from the video stream handle and the frame sequence, which
//! the encoded packet already carries.
use amaterasu_module_sdk::trace::{Stage, TraceTime, Tracer, Track, flow_id, names};

#[derive(Clone, Copy)]
pub struct FrameTrace {
    tracer: Tracer,
    track: Track,
    capture_started: Stage,
    encoder_submitted: Stage,
    packet_queued: Stage,
}

impl FrameTrace {
    pub fn new(tracer: Tracer) -> Self {
        Self {
            track: tracer.track(names::VIDEO_TRACK),
            capture_started: tracer.stage(names::VIDEO_CAPTURE_STARTED),
            encoder_submitted: tracer.stage(names::VIDEO_ENCODER_SUBMITTED),
            packet_queued: tracer.stage(names::VIDEO_PACKET_QUEUED),
            tracer,
        }
    }

    pub fn now(&self) -> TraceTime {
        self.tracer.now()
    }

    /// The encoder accepted the frame whose capture began at `capture_started`.
    /// A pump that submits nothing starts no flow.
    pub fn submitted(&self, stream: u64, sequence: u64, capture_started: TraceTime) {
        let flow = flow_id(stream, sequence);
        self.tracer
            .mark_at(self.track, flow, self.capture_started, capture_started);
        self.tracer.mark(self.track, flow, self.encoder_submitted);
    }

    /// The frame's first encoded bytes are queued for the consumer.
    pub fn packet_queued(&self, stream: u64, sequence: u64) {
        self.tracer
            .mark(self.track, flow_id(stream, sequence), self.packet_queued);
    }
}

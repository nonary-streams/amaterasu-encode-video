//! Deferred, metadata-only capture observations. Native frame memory never enters
//! this queue or the broker. In particular, no broker call runs in a borrowed
//! capture or encoder callback.
use crate::plane::{AcceptedFrame, AcceptedPacket};
use amaterasu_media::{CODEC_AV1, CODEC_H264, CODEC_HEVC, PIXEL_BGRA8, PIXEL_NV12};
use amaterasu_module_sdk::{
    Command, FrameMemory, HookEvent, HookMode, HookPoint, HookSubject, HookVersion, HookVideoCodec,
    ModuleResult, PixelFormat, Response,
};
use std::collections::VecDeque;

pub(super) const CAPTURE_TOPIC: &str = "amaterasu.video.capture-observation/1";
pub(super) const FRAME_TOPIC: &str = "amaterasu.video.frame-observation/1";
const MAX_QUEUED_ATTEMPTS: usize = 8;

#[derive(Default)]
struct ObservationBatch {
    capture_before: Option<HookEvent>,
    capture_after: Option<HookEvent>,
    frame_events: Vec<HookEvent>,
    packet_after: Option<HookEvent>,
}

/// Publication is best effort and never changes capture or encode results.
/// The signed module policy must both enable this source and grant the exact
/// `publish` topic. A missing grant disables observation after one failed send.
pub(super) struct Observations {
    capture_enabled: bool,
    frame_enabled: bool,
    next_id: u64,
    pending: VecDeque<ObservationBatch>,
    dropped: u64,
}

impl Observations {
    pub(super) fn new(capture_enabled: bool, frame_enabled: bool) -> Self {
        Self {
            capture_enabled,
            frame_enabled,
            next_id: 0,
            pending: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Called immediately before the native plane reads a due capture. The
    /// returned marker is completed only after the plane has relinquished all
    /// borrowed frame and DMA-BUF callback lifetimes.
    pub(super) fn before_capture(
        &mut self,
        width: u32,
        height: u32,
        memory: FrameMemory,
    ) -> Option<usize> {
        if !self.capture_enabled {
            return None;
        }
        if self.pending.len() >= MAX_QUEUED_ATTEMPTS {
            self.dropped = self.dropped.saturating_add(1);
            return None;
        }
        let before = self.event(
            HookPoint::BeforeCapture,
            HookSubject::Capture {
                width,
                height,
                memory,
            },
        )?;
        self.pending.push_back(ObservationBatch {
            capture_before: Some(before),
            ..ObservationBatch::default()
        });
        Some(self.pending.len() - 1)
    }

    /// A successful pump that never reached the native read removes its marker
    /// (for example, when an optional DMA-BUF path falls back to CPU). A failed
    /// pump calls `discard_capture`. `after_capture` means the native read
    /// returned, not that a fresh image or encoded packet was accepted.
    pub(super) fn after_capture(&mut self, marker: Option<usize>, callback_completed: bool) {
        let Some(marker) = marker else { return };
        if !callback_completed {
            self.discard_capture(Some(marker));
            return;
        }
        let Some(before) = self
            .pending
            .get(marker)
            .and_then(|attempt| attempt.capture_before.as_ref())
        else {
            return;
        };
        let (width, height, memory) = match &before.subject {
            HookSubject::Capture {
                width,
                height,
                memory,
            } => (*width, *height, *memory),
            _ => return,
        };
        if let Some(after) = self.event(
            HookPoint::AfterCapture,
            HookSubject::Capture {
                width,
                height,
                memory,
            },
        ) && let Some(attempt) = self.pending.get_mut(marker)
        {
            attempt.capture_after = Some(after);
        }
    }

    /// An error does not report whether a provider reached its borrowed-frame
    /// callback. Suppress the marker rather than claim a read that may not have
    /// happened; stream error status remains authoritative.
    pub(super) fn discard_capture(&mut self, marker: Option<usize>) {
        if marker.is_some_and(|marker| marker + 1 == self.pending.len()) {
            self.pending.pop_back();
        }
    }

    /// Records only confirmed encoder acceptance. A busy or rejected submit
    /// is never described as an accepted frame. Packet output may lag the
    /// accepted input or consist of multiple fragments with one timestamp.
    pub(super) fn record_pump(
        &mut self,
        marker: Option<usize>,
        accepted_frame: Option<AcceptedFrame>,
        encoded_packet: Option<AcceptedPacket>,
    ) {
        if !self.frame_enabled {
            return;
        }
        let mut frame_events = Vec::new();
        if let Some(frame) = accepted_frame
            && let Some(timestamp_100ns) = frame.timestamp_us.checked_mul(10)
        {
            let subject = HookSubject::VideoFrame {
                width: frame.width,
                height: frame.height,
                format: match frame.format {
                    PIXEL_BGRA8 => PixelFormat::Bgra,
                    PIXEL_NV12 => PixelFormat::Nv12,
                    _ => PixelFormat::Unknown,
                },
                memory: frame.memory,
                timestamp_100ns,
            };
            for point in [
                HookPoint::BeforeFrame,
                HookPoint::BeforeEncode,
                HookPoint::AfterFrame,
            ] {
                if let Some(event) = self.event(point, subject.clone()) {
                    frame_events.push(event);
                }
            }
        }
        let packet_after = encoded_packet.and_then(|packet| {
            let timestamp_100ns = packet.timestamp_us.checked_mul(10)?;
            self.event(
                HookPoint::AfterEncode,
                HookSubject::EncodedPacket {
                    codec: match packet.codec {
                        CODEC_H264 => HookVideoCodec::H264,
                        CODEC_HEVC => HookVideoCodec::Hevc,
                        CODEC_AV1 => HookVideoCodec::Av1,
                        _ => HookVideoCodec::Unknown,
                    },
                    bytes: packet.bytes,
                    timestamp_100ns,
                    keyframe: packet.keyframe,
                },
            )
        });
        if frame_events.is_empty() && packet_after.is_none() {
            return;
        }
        let target = if let Some(marker) = marker {
            marker
        } else if self.pending.len() < MAX_QUEUED_ATTEMPTS {
            self.pending.push_back(ObservationBatch::default());
            self.pending.len() - 1
        } else {
            self.dropped = self.dropped.saturating_add(1);
            return;
        };
        if let Some(batch) = self.pending.get_mut(target) {
            batch.frame_events.extend(frame_events);
            batch.packet_after = packet_after;
        }
    }

    fn event(&mut self, point: HookPoint, subject: HookSubject) -> Option<HookEvent> {
        let Some(id) = self.next_id.checked_add(1) else {
            self.clear();
            return None;
        };
        self.next_id = id;
        let event = HookEvent {
            version: HookVersion::CURRENT,
            event_id: id,
            point,
            mode: HookMode::Observe,
            subject,
        };
        if event.validate().is_err() {
            self.dropped = self.dropped.saturating_add(1);
            return None;
        }
        Some(event)
    }

    /// At most six small broker sends per module tick. Never retries an
    /// uncertain send: the broker sequence covers only calls that reach it,
    /// and this observer cannot affect stream correctness.
    pub(super) fn flush(&mut self, send: &mut dyn FnMut(Command) -> ModuleResult<Response>) {
        if !self.capture_enabled && !self.frame_enabled {
            return;
        }
        let Some(batch) = self.pending.pop_front() else {
            return;
        };
        for event in batch
            .capture_before
            .into_iter()
            .chain(batch.capture_after)
            .chain(batch.frame_events)
            .chain(batch.packet_after)
        {
            if !self.publish(event, send) {
                break;
            }
        }
    }

    fn publish(
        &mut self,
        event: HookEvent,
        send: &mut dyn FnMut(Command) -> ModuleResult<Response>,
    ) -> bool {
        let capture = matches!(
            event.point,
            HookPoint::BeforeCapture | HookPoint::AfterCapture
        );
        if (capture && !self.capture_enabled) || (!capture && !self.frame_enabled) {
            return true;
        }
        let Ok(payload) = serde_json::to_value(event) else {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        };
        match send(Command::Publish {
            topic: if capture { CAPTURE_TOPIC } else { FRAME_TOPIC }.into(),
            payload,
        }) {
            Ok(Response::Ok) => true,
            Ok(Response::Error { code, .. }) if code == "permission_denied" => {
                if capture {
                    self.capture_enabled = false;
                } else {
                    self.frame_enabled = false;
                }
                if !self.capture_enabled && !self.frame_enabled {
                    self.pending.clear();
                }
                // Each exact topic has its own signed grant. A denied capture
                // topic must not suppress an independently granted frame topic.
                true
            }
            _ => {
                self.dropped = self.dropped.saturating_add(1);
                false
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.capture_enabled = false;
        self.frame_enabled = false;
        self.pending.clear();
    }

    #[cfg(test)]
    pub(super) fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_source_never_allocates_or_calls_the_broker() {
        let mut observations = Observations::new(false, false);
        assert_eq!(
            observations.before_capture(640, 480, FrameMemory::CpuBorrowed),
            None
        );
        observations.flush(&mut |_| panic!("disabled observer called broker"));
        assert_eq!(observations.next_id, 0);
    }

    #[test]
    fn capture_markers_publish_in_order_after_native_return() {
        let mut observations = Observations::new(true, false);
        let marker = observations.before_capture(640, 480, FrameMemory::GpuOpaque);
        observations.after_capture(marker, true);
        let mut points = Vec::new();
        observations.flush(&mut |command| {
            let Command::Publish { topic, payload } = command else {
                panic!("wrong command")
            };
            assert_eq!(topic, CAPTURE_TOPIC);
            let event: HookEvent = serde_json::from_value(payload).unwrap();
            assert_eq!(event.mode, HookMode::Observe);
            assert_eq!(
                event.subject,
                HookSubject::Capture {
                    width: 640,
                    height: 480,
                    memory: FrameMemory::GpuOpaque,
                }
            );
            points.push((event.event_id, event.point));
            Ok(Response::Ok)
        });
        assert_eq!(
            points,
            [(1, HookPoint::BeforeCapture), (2, HookPoint::AfterCapture)]
        );
    }

    #[test]
    fn queue_pressure_and_permission_loss_never_block_stream_work() {
        let mut observations = Observations::new(true, false);
        for _ in 0..MAX_QUEUED_ATTEMPTS {
            assert!(
                observations
                    .before_capture(16, 16, FrameMemory::CpuBorrowed)
                    .is_some()
            );
        }
        assert_eq!(
            observations.before_capture(16, 16, FrameMemory::CpuBorrowed),
            None
        );
        assert_eq!(observations.dropped(), 1);
        let mut calls = 0;
        observations.flush(&mut |_| {
            calls += 1;
            Ok(Response::error("permission_denied", ""))
        });
        assert_eq!(calls, 1);
        assert!(observations.pending.is_empty());
        observations.flush(&mut |_| panic!("revoked observer called broker"));
    }

    #[test]
    fn stop_discards_pending_metadata_without_publishing() {
        let mut observations = Observations::new(true, false);
        observations.before_capture(32, 32, FrameMemory::CpuBorrowed);
        observations.clear();
        observations.flush(&mut |_| panic!("stopped observer called broker"));
    }

    #[test]
    fn unused_capture_attempt_never_claims_a_native_read() {
        let mut observations = Observations::new(true, false);
        let marker = observations.before_capture(32, 32, FrameMemory::GpuOpaque);
        observations.after_capture(marker, false);
        observations.flush(&mut |_| panic!("fallback did not read a native frame"));
    }

    #[test]
    fn failed_plane_before_native_read_discards_the_before_marker() {
        let mut observations = Observations::new(true, false);
        let marker = observations.before_capture(32, 32, FrameMemory::CpuBorrowed);
        observations.discard_capture(marker);
        observations.flush(&mut |_| panic!("failed plane did not read a native frame"));
    }

    #[test]
    fn accepted_frame_and_admitted_packet_keep_timestamps_and_stage_order() {
        let mut observations = Observations::new(false, true);
        observations.record_pump(
            None,
            Some(AcceptedFrame {
                width: 1280,
                height: 720,
                format: PIXEL_NV12,
                memory: FrameMemory::CpuOwned,
                timestamp_us: 17,
                sequence: 4,
            }),
            Some(AcceptedPacket {
                codec: CODEC_HEVC,
                bytes: 93,
                timestamp_us: 11,
                keyframe: true,
            }),
        );
        let mut events = Vec::new();
        observations.flush(&mut |command| {
            let Command::Publish { topic, payload } = command else {
                panic!("wrong command")
            };
            assert_eq!(topic, FRAME_TOPIC);
            events.push(serde_json::from_value::<HookEvent>(payload).unwrap());
            Ok(Response::Ok)
        });
        assert_eq!(events.len(), 4);
        assert_eq!(
            events.iter().map(|event| event.point).collect::<Vec<_>>(),
            [
                HookPoint::BeforeFrame,
                HookPoint::BeforeEncode,
                HookPoint::AfterFrame,
                HookPoint::AfterEncode,
            ]
        );
        assert!(events[..3].iter().all(|event| event.subject
            == HookSubject::VideoFrame {
                width: 1280,
                height: 720,
                format: PixelFormat::Nv12,
                memory: FrameMemory::CpuOwned,
                timestamp_100ns: 170,
            }));
        assert_eq!(
            events[3].subject,
            HookSubject::EncodedPacket {
                codec: HookVideoCodec::Hevc,
                bytes: 93,
                timestamp_100ns: 110,
                keyframe: true,
            }
        );
    }

    #[test]
    fn absent_or_overflowed_frame_metadata_emits_no_false_stage() {
        let mut observations = Observations::new(false, true);
        observations.record_pump(None, None, None);
        observations.record_pump(
            None,
            Some(AcceptedFrame {
                width: 640,
                height: 480,
                format: PIXEL_BGRA8,
                memory: FrameMemory::CpuBorrowed,
                timestamp_us: u64::MAX,
                sequence: 1,
            }),
            None,
        );
        observations.flush(&mut |_| panic!("no valid observation exists"));
    }

    #[test]
    fn denied_capture_topic_does_not_suppress_independently_granted_frame_topic() {
        let mut observations = Observations::new(true, true);
        let marker = observations.before_capture(640, 480, FrameMemory::CpuBorrowed);
        observations.after_capture(marker, true);
        observations.record_pump(
            marker,
            Some(AcceptedFrame {
                width: 640,
                height: 480,
                format: PIXEL_BGRA8,
                memory: FrameMemory::CpuBorrowed,
                timestamp_us: 3,
                sequence: 1,
            }),
            None,
        );
        let mut topics = Vec::new();
        observations.flush(&mut |command| {
            let Command::Publish { topic, .. } = command else {
                panic!("wrong command")
            };
            topics.push(topic.clone());
            if topic == CAPTURE_TOPIC {
                Ok(Response::Error {
                    code: "permission_denied".into(),
                    message: "denied".into(),
                })
            } else {
                Ok(Response::Ok)
            }
        });
        assert_eq!(topics.len(), 4);
        assert_eq!(topics[0], CAPTURE_TOPIC);
        assert!(topics[1..].iter().all(|topic| topic == FRAME_TOPIC));
        assert!(!observations.capture_enabled);
        assert!(observations.frame_enabled);
    }

    #[test]
    fn stopped_frame_observer_discards_pending_packet_without_publish() {
        let mut observations = Observations::new(false, true);
        observations.record_pump(
            None,
            None,
            Some(AcceptedPacket {
                codec: CODEC_H264,
                bytes: 8,
                timestamp_us: 2,
                keyframe: false,
            }),
        );
        observations.clear();
        observations.flush(&mut |_| panic!("stopped observer called broker"));
    }

    #[test]
    fn frame_queue_pressure_drops_metadata_without_affecting_later_work() {
        let mut observations = Observations::new(false, true);
        let packet = AcceptedPacket {
            codec: CODEC_H264,
            bytes: 8,
            timestamp_us: 2,
            keyframe: false,
        };
        for _ in 0..MAX_QUEUED_ATTEMPTS + 1 {
            observations.record_pump(None, None, Some(packet));
        }
        assert_eq!(observations.dropped(), 1);
        let mut sent = 0;
        observations.flush(&mut |_| {
            sent += 1;
            Ok(Response::Ok)
        });
        assert_eq!(sent, 1);
        observations.record_pump(None, None, Some(packet));
        assert_eq!(observations.dropped(), 1);
    }
}

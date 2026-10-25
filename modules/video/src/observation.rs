//! Deferred, metadata-only capture observations. Native frame memory never enters
//! this queue or the broker. In particular, no broker call runs in a borrowed
//! capture or encoder callback.
use amaterasu_module_sdk::{
    Command, FrameMemory, HookEvent, HookMode, HookPoint, HookSubject, HookVersion, ModuleResult,
    Response,
};
use std::collections::VecDeque;

pub(super) const TOPIC: &str = "amaterasu.video.capture-observation/1";
const MAX_QUEUED_ATTEMPTS: usize = 8;

struct CaptureAttempt {
    before: HookEvent,
    after: Option<HookEvent>,
}

/// Publication is best effort and never changes capture or encode results.
/// The signed module policy must both enable this source and grant the exact
/// `publish` topic. A missing grant disables observation after one failed send.
pub(super) struct Observations {
    enabled: bool,
    next_id: u64,
    pending: VecDeque<CaptureAttempt>,
    dropped: u64,
}

impl Observations {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
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
        if !self.enabled {
            return None;
        }
        if self.pending.len() >= MAX_QUEUED_ATTEMPTS {
            self.dropped = self.dropped.saturating_add(1);
            return None;
        }
        let before = self.event(HookPoint::BeforeCapture, width, height, memory)?;
        self.pending.push_back(CaptureAttempt {
            before,
            after: None,
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
        let Some(before) = self.pending.get(marker).map(|attempt| &attempt.before) else {
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
        if let Some(after) = self.event(HookPoint::AfterCapture, width, height, memory)
            && let Some(attempt) = self.pending.get_mut(marker)
        {
            attempt.after = Some(after);
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

    fn event(
        &mut self,
        point: HookPoint,
        width: u32,
        height: u32,
        memory: FrameMemory,
    ) -> Option<HookEvent> {
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
            subject: HookSubject::Capture {
                width,
                height,
                memory,
            },
        };
        if event.validate().is_err() {
            self.dropped = self.dropped.saturating_add(1);
            return None;
        }
        Some(event)
    }

    /// At most two small broker sends per module tick. Never retries an
    /// uncertain send: consumers use the broker's own publisher sequence for
    /// gap detection, and this observer cannot affect stream correctness.
    pub(super) fn flush(&mut self, send: &mut dyn FnMut(Command) -> ModuleResult<Response>) {
        if !self.enabled {
            return;
        }
        let Some(attempt) = self.pending.pop_front() else {
            return;
        };
        if !self.publish(attempt.before, send) {
            return;
        }
        if let Some(after) = attempt.after {
            self.publish(after, send);
        }
    }

    fn publish(
        &mut self,
        event: HookEvent,
        send: &mut dyn FnMut(Command) -> ModuleResult<Response>,
    ) -> bool {
        let Ok(payload) = serde_json::to_value(event) else {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        };
        match send(Command::Publish {
            topic: TOPIC.into(),
            payload,
        }) {
            Ok(Response::Ok) => true,
            Ok(Response::Error { code, .. }) if code == "permission_denied" => {
                self.clear();
                false
            }
            _ => {
                self.dropped = self.dropped.saturating_add(1);
                false
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.enabled = false;
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
        let mut observations = Observations::new(false);
        assert_eq!(
            observations.before_capture(640, 480, FrameMemory::CpuBorrowed),
            None
        );
        observations.flush(&mut |_| panic!("disabled observer called broker"));
        assert_eq!(observations.next_id, 0);
    }

    #[test]
    fn capture_markers_publish_in_order_after_native_return() {
        let mut observations = Observations::new(true);
        let marker = observations.before_capture(640, 480, FrameMemory::GpuOpaque);
        observations.after_capture(marker, true);
        let mut points = Vec::new();
        observations.flush(&mut |command| {
            let Command::Publish { topic, payload } = command else {
                panic!("wrong command")
            };
            assert_eq!(topic, TOPIC);
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
        let mut observations = Observations::new(true);
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
        let mut observations = Observations::new(true);
        observations.before_capture(32, 32, FrameMemory::CpuBorrowed);
        observations.clear();
        observations.flush(&mut |_| panic!("stopped observer called broker"));
    }

    #[test]
    fn unused_capture_attempt_never_claims_a_native_read() {
        let mut observations = Observations::new(true);
        let marker = observations.before_capture(32, 32, FrameMemory::GpuOpaque);
        observations.after_capture(marker, false);
        observations.flush(&mut |_| panic!("fallback did not read a native frame"));
    }

    #[test]
    fn failed_plane_before_native_read_discards_the_before_marker() {
        let mut observations = Observations::new(true);
        let marker = observations.before_capture(32, 32, FrameMemory::CpuBorrowed);
        observations.discard_capture(marker);
        observations.flush(&mut |_| panic!("failed plane did not read a native frame"));
    }
}

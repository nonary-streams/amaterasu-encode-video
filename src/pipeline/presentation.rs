//! Push-driven pacing for DMA-BUF streams whose capture provider reports its
//! newest completed presentation (capture v1.5). Every pump tick asks the
//! provider for that presentation. A newer one is submitted as soon as the
//! negotiated frame-rate cap admits it; presentations that arrive while the
//! cap holds coalesce to the newest. Without a new presentation only the
//! keepalive repeat rule submits. Providers without the query keep timer pacing.
use crate::plane::PresentationQuery;
use amaterasu_media::PresentationStatus;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// A mapped presentation timestamp further behind the stream clock than this
/// belongs to a discontinuous provider clock and re-anchors the mapping.
const MAX_TIMESTAMP_LAG_US: u64 = 200_000;
/// Fraction of a frame interval by which a presentation may precede its cap
/// slot. Nothing wakes the pipeline exactly at a slot, so holding a frame that
/// arrived just early delays it until the next host pass. The slot grid itself
/// still advances by whole intervals, so the sustained rate stays capped.
const EARLY_ADMISSION_DIVISOR: u32 = 8;

/// Maps the provider's presentation clock onto the video stream clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClockAnchor {
    presentation_us: u64,
    stream_us: u64,
    last_presentation_us: u64,
}

/// What one pump delivered while presentation pacing was active.
#[derive(Clone, Copy, Debug)]
pub(super) struct PresentationDelivery {
    /// The read pushed a newer presentation. A timer probe before the first
    /// accepted frame is neither a push nor a repeat.
    pub pushed: bool,
    /// The read was a keepalive or forced-keyframe repeat.
    pub repeat: bool,
    /// Provider sequence of the presentation the read delivered, if any.
    pub delivered: Option<u64>,
    /// The encoder accepted the delivered presentation.
    pub submitted: bool,
}

#[derive(Default)]
pub(super) struct PresentationPacing {
    /// The latest pump tick paced this stream by presentations.
    active: bool,
    /// Newest presentation actually accepted by the encoder.
    delivered_sequence: u64,
    /// Newest observed presentation that has not been delivered yet.
    pending: Option<PresentationStatus>,
    /// Earliest instant the frame-rate cap admits another submit.
    next_submit_at: Option<Instant>,
    /// How far ahead of `next_submit_at` a presentation is still admitted.
    early_admission: Duration,
    clock: Option<ClockAnchor>,
    fresh_pushes: u64,
    repeats: u64,
    coalesced: u64,
    /// A newer presentation arrived between metadata query and actual read.
    newer_during_read: u64,
    /// Distinct increasing presentation values observed by the consumer.
    observed_presentations: u64,
    latest_observed_sequence: u64,
    /// Queries that advanced by more than one sequence value. Sequence values
    /// need not be consecutive, so this is not a count of lost frames.
    sequence_jump_events: u64,
    /// Distinct observed presentations held at least once by the rate cap.
    cap_hold_events: u64,
    last_cap_held_sequence: u64,
}

impl PresentationPacing {
    /// Records one presentation query and returns whether this tick is
    /// presentation-paced. A newer presentation replaces an undelivered one,
    /// which counts as coalesced.
    pub(super) fn observe(&mut self, query: PresentationQuery) -> bool {
        self.active = query != PresentationQuery::Unavailable;
        let PresentationQuery::Latest(presentation) = query else {
            return self.active;
        };
        if presentation.sequence > self.latest_observed_sequence {
            if self.latest_observed_sequence != 0
                && presentation
                    .sequence
                    .saturating_sub(self.latest_observed_sequence)
                    > 1
            {
                self.sequence_jump_events = self.sequence_jump_events.saturating_add(1);
            }
            self.latest_observed_sequence = presentation.sequence;
            self.observed_presentations = self.observed_presentations.saturating_add(1);
        }
        let newest_known = self
            .pending
            .map_or(self.delivered_sequence, |pending| pending.sequence);
        if presentation.sequence > newest_known {
            if self.pending.is_some() {
                self.coalesced = self.coalesced.saturating_add(1);
            }
            self.pending = Some(presentation);
        }
        self.active
    }

    /// Leaves presentation pacing, for example after a CPU capture fallback.
    pub(super) fn deactivate(&mut self) {
        self.active = false;
    }

    /// The frame-rate cap admits a submit at `now`.
    pub(super) fn submit_allowed(&self, now: Instant) -> bool {
        self.next_submit_at.is_none_or(|slot| {
            now.checked_add(self.early_admission)
                .is_some_and(|t| t >= slot)
        })
    }

    /// A newer presentation waits and the frame-rate cap admits it.
    pub(super) fn fresh_due(&self, now: Instant) -> bool {
        self.pending.is_some() && self.submit_allowed(now)
    }

    /// Records cap holds once per presentation, rather than once per host tick.
    pub(super) fn record_cap_hold(&mut self, now: Instant) {
        if let Some(pending) = self.pending
            && pending.sequence > self.last_cap_held_sequence
            && !self.submit_allowed(now)
        {
            self.last_cap_held_sequence = pending.sequence;
            self.cap_hold_events = self.cap_hold_events.saturating_add(1);
        }
    }

    /// The stream timestamp for submitting the pending presentation, mapped
    /// from its provider timestamp so variable cadence survives. Zero or
    /// unusable provider timestamps fall back to `stream_now_us`.
    pub(super) fn fresh_timestamp(&mut self, stream_now_us: u64, interval: Duration) -> u64 {
        let Some(pending) = self.pending else {
            return stream_now_us;
        };
        self.map_timestamp(pending.timestamp_us, stream_now_us, interval)
    }

    fn map_timestamp(
        &mut self,
        presentation_us: u64,
        stream_now_us: u64,
        interval: Duration,
    ) -> u64 {
        if presentation_us == 0 {
            self.clock = None;
            return stream_now_us;
        }
        let max_lead_us = u64::try_from(interval.as_micros()).unwrap_or(u64::MAX);
        let mapped = self.clock.and_then(|anchor| {
            if presentation_us < anchor.last_presentation_us {
                return None;
            }
            let mapped = anchor
                .stream_us
                .checked_add(presentation_us.checked_sub(anchor.presentation_us)?)?;
            // A completed presentation cannot be more than one frame ahead of
            // its observation, nor far behind the stream clock.
            (mapped <= stream_now_us.saturating_add(max_lead_us)
                && stream_now_us.saturating_sub(mapped) <= MAX_TIMESTAMP_LAG_US)
                .then_some(mapped)
        });
        if let Some(mapped) = mapped {
            if let Some(anchor) = self.clock.as_mut() {
                anchor.last_presentation_us = presentation_us;
            }
            return mapped;
        }
        self.clock = Some(ClockAnchor {
            presentation_us,
            stream_us: stream_now_us,
            last_presentation_us: presentation_us,
        });
        stream_now_us
    }

    /// Only an accepted presentation consumes pending work. A busy encoder
    /// can reject the last new frame, and capture can temporarily return an
    /// older frame than its latest presentation query. Keep the newest pending
    /// until it is actually submitted or superseded by a later presentation.
    pub(super) fn record(
        &mut self,
        submitted_at: Instant,
        interval: Duration,
        delivery: PresentationDelivery,
    ) {
        if delivery.submitted
            && let Some(delivered) = delivery.delivered
        {
            let requested = if delivery.pushed {
                self.pending.map_or(0, |pending| pending.sequence)
            } else {
                0
            };
            if requested != 0 && delivered > requested {
                self.newer_during_read = self.newer_during_read.saturating_add(1);
            }
            self.delivered_sequence = self.delivered_sequence.max(delivered);
            if self
                .pending
                .is_some_and(|pending| pending.sequence <= self.delivered_sequence)
            {
                self.pending = None;
            }
        }
        if !delivery.submitted {
            return;
        }
        if delivery.repeat {
            self.repeats = self.repeats.saturating_add(1);
        } else if delivery.pushed {
            self.fresh_pushes = self.fresh_pushes.saturating_add(1);
        }
        self.advance_rate_cap(submitted_at, interval);
    }

    /// Keeps submits on a 1/fps grid. Each submit claims the next grid slot,
    /// so tick jitter neither drifts the grid nor lets the sustained rate exceed
    /// the negotiated fps. A submit late by more than half an interval cannot
    /// make the next one sooner than half an interval, and a submit after a
    /// whole missed slot re-anchors the grid instead of bursting to catch up.
    fn advance_rate_cap(&mut self, submitted_at: Instant, interval: Duration) {
        self.early_admission = interval / EARLY_ADMISSION_DIVISOR;
        let fresh_grid = submitted_at.checked_add(interval);
        self.next_submit_at = match self.next_submit_at {
            Some(slot)
                if slot
                    .checked_add(interval)
                    .is_some_and(|next| submitted_at < next) =>
            {
                let next = slot.checked_add(interval);
                let earliest = submitted_at.checked_add(interval / 2);
                next.max(earliest)
            }
            _ => fresh_grid,
        };
    }

    pub(super) fn write_status(&self, response: &mut Value) {
        response["pacing"] = json!(if self.active { "presentation" } else { "timer" });
        response["presentation_pacing"] = json!({
            "fresh_pushes": self.fresh_pushes,
            "repeats": self.repeats,
            "coalesced": self.coalesced,
            "newer_during_read": self.newer_during_read,
            "observed_presentations": self.observed_presentations,
            "latest_observed_sequence": self.latest_observed_sequence,
            "sequence_jump_events": self.sequence_jump_events,
            "cap_hold_events": self.cap_hold_events,
        });
    }
}

#[cfg(test)]
impl PresentationPacing {
    /// Moves the frame-rate cap's next slot, standing in for elapsed time.
    pub(super) fn set_next_submit_at(&mut self, slot: Instant) {
        self.next_submit_at = Some(slot);
    }
}

/// The frame interval of the negotiated client frame rate.
pub(super) fn frame_interval(fps: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1)))
}

#[cfg(test)]
mod early_admission_tests {
    use super::*;

    fn submitted() -> PresentationDelivery {
        PresentationDelivery {
            pushed: false,
            repeat: false,
            delivered: None,
            submitted: true,
        }
    }

    #[test]
    fn a_presentation_just_before_its_slot_is_not_held() {
        let interval = frame_interval(120);
        let start = Instant::now();
        let mut pacing = PresentationPacing::default();
        pacing.record(start, interval, submitted());

        assert!(pacing.submit_allowed(start + interval - interval / 16));
        assert!(!pacing.submit_allowed(start + interval / 2));
    }

    #[test]
    fn early_admission_does_not_raise_the_sustained_rate() {
        let interval = frame_interval(120);
        let start = Instant::now();
        let mut pacing = PresentationPacing::default();
        pacing.record(start, interval, submitted());
        let mut now = start;
        let frames = 1_000u32;
        for _ in 0..frames {
            while !pacing.submit_allowed(now) {
                now += Duration::from_micros(10);
            }
            pacing.record(now, interval, submitted());
        }
        // Every submission lands as early as the cap allows, yet the elapsed
        // time still covers all but the tolerance of a single interval.
        let floor = interval * frames - interval / EARLY_ADMISSION_DIVISOR;
        assert!(now - start >= floor, "{:?} < {floor:?}", now - start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_nanos(16_666_666);

    fn presented(sequence: u64, timestamp_us: u64) -> PresentationQuery {
        PresentationQuery::Latest(PresentationStatus {
            sequence,
            timestamp_us,
        })
    }

    fn fresh_submit(sequence: u64) -> PresentationDelivery {
        PresentationDelivery {
            pushed: true,
            repeat: false,
            delivered: Some(sequence),
            submitted: true,
        }
    }

    #[test]
    fn unavailable_query_keeps_timer_pacing_and_pending_has_no_news() {
        let mut pacing = PresentationPacing::default();
        assert!(!pacing.observe(PresentationQuery::Unavailable));
        assert!(pacing.observe(PresentationQuery::Pending));
        assert!(!pacing.fresh_due(Instant::now()));
        let mut status = json!({});
        pacing.write_status(&mut status);
        assert_eq!(status["pacing"], "presentation");
        pacing.deactivate();
        pacing.write_status(&mut status);
        assert_eq!(status["pacing"], "timer");
    }

    #[test]
    fn rate_cap_holds_on_a_grid_without_drifting_from_tick_jitter() {
        let start = Instant::now();
        let mut pacing = PresentationPacing::default();
        pacing.record(start, INTERVAL, fresh_submit(1));
        // A presentation may land up to an eighth of an interval early.
        let earliest = start + INTERVAL - INTERVAL / EARLY_ADMISSION_DIVISOR;
        assert!(!pacing.submit_allowed(earliest - Duration::from_micros(1)));
        assert!(pacing.submit_allowed(earliest));

        // Submitting 1.9 ms late (a host tick) still claims the original grid.
        pacing.record(
            start + INTERVAL + Duration::from_micros(1_900),
            INTERVAL,
            fresh_submit(2),
        );
        assert_eq!(pacing.next_submit_at, Some(start + INTERVAL * 2));

        // A submit late by more than half an interval keeps half an interval.
        let late = start + INTERVAL * 2 + Duration::from_millis(12);
        pacing.record(late, INTERVAL, fresh_submit(3));
        assert_eq!(pacing.next_submit_at, Some(late + INTERVAL / 2));

        // After a whole missed slot the grid re-anchors without a burst.
        let resumed = late + INTERVAL * 3;
        pacing.record(resumed, INTERVAL, fresh_submit(4));
        assert_eq!(pacing.next_submit_at, Some(resumed + INTERVAL));
    }

    #[test]
    fn source_observations_and_cap_holds_are_counted_once_per_presentation() {
        let start = Instant::now();
        let mut pacing = PresentationPacing::default();
        pacing.observe(presented(100, 1_000));
        pacing.record(start, INTERVAL, fresh_submit(100));
        pacing.observe(presented(102, 2_000));
        pacing.record_cap_hold(start + Duration::from_millis(1));
        pacing.record_cap_hold(start + Duration::from_millis(2));
        pacing.observe(presented(102, 2_000));
        pacing.observe(presented(103, 3_000));
        pacing.record_cap_hold(start + Duration::from_millis(3));

        let mut status = json!({});
        pacing.write_status(&mut status);
        let counters = &status["presentation_pacing"];
        assert_eq!(counters["observed_presentations"], 3);
        assert_eq!(counters["latest_observed_sequence"], 103);
        assert_eq!(counters["sequence_jump_events"], 1);
        assert_eq!(counters["cap_hold_events"], 2);
        assert_eq!(counters["coalesced"], 1);
    }

    #[test]
    fn aligned_120hz_source_does_not_lose_frames_to_two_millisecond_ticks() {
        let start = Instant::now();
        let interval = frame_interval(120);
        let mut pacing = PresentationPacing::default();
        let mut submitted = 0_u64;
        for tick in 0..5_000_u64 {
            let elapsed = Duration::from_millis(tick * 2);
            let sequence = (elapsed.as_nanos() / interval.as_nanos()) as u64 + 1;
            pacing.observe(presented(sequence, elapsed.as_micros() as u64));
            let now = start + elapsed;
            pacing.record_cap_hold(now);
            if pacing.fresh_due(now) {
                pacing.record(now, interval, fresh_submit(sequence));
                submitted += 1;
            }
        }
        assert_eq!(submitted, 1_200);
        assert_eq!(pacing.coalesced, 0);
        assert_eq!(pacing.sequence_jump_events, 0);
    }

    #[test]
    fn newer_presentations_coalesce_while_the_cap_holds() {
        let start = Instant::now();
        let mut pacing = PresentationPacing::default();
        pacing.observe(presented(1, 1_000));
        pacing.record(start, INTERVAL, fresh_submit(1));
        pacing.observe(presented(1, 1_000));
        assert!(pacing.pending.is_none());

        pacing.observe(presented(2, 5_000));
        pacing.observe(presented(3, 9_000));
        assert!(!pacing.fresh_due(start + Duration::from_millis(9)));
        assert_eq!(pacing.coalesced, 1);
        assert!(pacing.fresh_due(start + INTERVAL));
        assert_eq!(pacing.pending.unwrap().sequence, 3);

        // The read may deliver an even newer presentation than was queried.
        pacing.record(start + INTERVAL, INTERVAL, fresh_submit(4));
        assert_eq!(pacing.delivered_sequence, 4);
        pacing.observe(presented(4, 13_000));
        assert!(pacing.pending.is_none());
        assert_eq!(pacing.fresh_pushes, 2);
    }

    #[test]
    fn dropped_presentation_remains_pending_without_advancing_the_cap() {
        let start = Instant::now();
        let mut pacing = PresentationPacing::default();
        pacing.observe(presented(5, 1_000));
        pacing.record(
            start,
            INTERVAL,
            PresentationDelivery {
                submitted: false,
                ..fresh_submit(5)
            },
        );
        assert_eq!(pacing.pending.unwrap().sequence, 5);
        assert!(pacing.fresh_due(start));
        assert!(pacing.submit_allowed(start));
        assert_eq!(pacing.fresh_pushes, 0);
        pacing.record(start, INTERVAL, fresh_submit(5));
        assert!(pacing.pending.is_none());
        assert_eq!(pacing.fresh_pushes, 1);
    }

    #[test]
    fn a_newer_presentation_replaces_a_backpressured_frame() {
        let start = Instant::now();
        let mut pacing = PresentationPacing::default();
        pacing.observe(presented(5, 1_000));
        pacing.record(
            start,
            INTERVAL,
            PresentationDelivery {
                submitted: false,
                ..fresh_submit(5)
            },
        );
        pacing.observe(presented(6, 2_000));
        assert_eq!(pacing.pending.unwrap().sequence, 6);
        assert_eq!(pacing.coalesced, 1);
        pacing.record(start, INTERVAL, fresh_submit(6));
        assert!(pacing.pending.is_none());
        assert_eq!(pacing.delivered_sequence, 6);
    }

    #[test]
    fn an_older_accepted_capture_does_not_consume_the_newest_presentation() {
        let start = Instant::now();
        let mut pacing = PresentationPacing::default();
        pacing.observe(presented(8, 1_000));
        pacing.record(start, INTERVAL, fresh_submit(7));
        assert_eq!(pacing.pending.unwrap().sequence, 8);
        assert!(!pacing.fresh_due(start));
        assert!(pacing.fresh_due(start + INTERVAL));
        pacing.record(start + INTERVAL, INTERVAL, fresh_submit(8));
        assert!(pacing.pending.is_none());
    }

    #[test]
    fn timestamps_follow_presentation_cadence_and_reanchor_on_discontinuity() {
        let mut pacing = PresentationPacing::default();
        assert_eq!(pacing.map_timestamp(500_000, 40_000, INTERVAL), 40_000);
        // Uneven VRR spacing is preserved relative to the anchor.
        assert_eq!(pacing.map_timestamp(507_000, 48_000, INTERVAL), 47_000);
        assert_eq!(pacing.map_timestamp(530_000, 71_000, INTERVAL), 70_000);
        // More than one frame ahead of its observation re-anchors.
        assert_eq!(pacing.map_timestamp(600_000, 80_000, INTERVAL), 80_000);
        // A regressing provider clock re-anchors.
        assert_eq!(pacing.map_timestamp(10_000, 90_000, INTERVAL), 90_000);
        // A mapping far behind the stream clock re-anchors.
        assert_eq!(pacing.map_timestamp(20_000, 400_000, INTERVAL), 400_000);
        // Zero is unknown and uses the stream clock.
        assert_eq!(pacing.map_timestamp(0, 410_000, INTERVAL), 410_000);
        assert_eq!(pacing.clock, None);
    }
}

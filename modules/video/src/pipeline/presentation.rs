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
    /// Newest presentation handed to the encoder, accepted or dropped.
    delivered_sequence: u64,
    /// Newest observed presentation that has not been delivered yet.
    pending: Option<PresentationStatus>,
    /// Earliest instant the frame-rate cap admits another submit.
    next_submit_at: Option<Instant>,
    clock: Option<ClockAnchor>,
    fresh_pushes: u64,
    repeats: u64,
    coalesced: u64,
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
        self.next_submit_at.is_none_or(|slot| now >= slot)
    }

    /// A newer presentation waits and the frame-rate cap admits it.
    pub(super) fn fresh_due(&self, now: Instant) -> bool {
        self.pending.is_some() && self.submit_allowed(now)
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

    /// Applies one pump's outcome: a delivered presentation is consumed even
    /// when encoder backpressure dropped it, and an accepted submit advances
    /// the frame-rate cap.
    pub(super) fn record(
        &mut self,
        completed_at: Instant,
        interval: Duration,
        delivery: PresentationDelivery,
    ) {
        if let Some(delivered) = delivery.delivered {
            let requested = if delivery.pushed {
                self.pending.map_or(0, |pending| pending.sequence)
            } else {
                0
            };
            self.delivered_sequence = self.delivered_sequence.max(delivered).max(requested);
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
        self.advance_rate_cap(completed_at, interval);
    }

    /// Keeps submits on a 1/fps grid. Each submit claims the next grid slot,
    /// so tick jitter neither drifts the grid nor lets the sustained rate exceed
    /// the negotiated fps. A submit late by more than half an interval cannot
    /// make the next one sooner than half an interval, and a submit after a
    /// whole missed slot re-anchors the grid instead of bursting to catch up.
    fn advance_rate_cap(&mut self, submitted_at: Instant, interval: Duration) {
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
        assert!(!pacing.submit_allowed(start + INTERVAL - Duration::from_micros(1)));
        assert!(pacing.submit_allowed(start + INTERVAL));

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
    fn dropped_presentation_is_consumed_without_advancing_the_cap() {
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
        assert!(pacing.pending.is_none());
        assert!(pacing.submit_allowed(start));
        assert_eq!(pacing.fresh_pushes, 0);
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

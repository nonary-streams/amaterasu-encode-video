use std::time::Duration;

const NANOS_PER_SECOND: u128 = 1_000_000_000;
const MAX_SNAP_WINDOW_NS: u128 = 2_000_000;
const MAX_HOLD_NS: u128 = 200_000_000;
const MAX_DURATION_NS: u128 = u64::MAX as u128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceClock {
    anchor_timestamp_us: u64,
    last_timestamp_us: u64,
    anchor_monotonic: Duration,
}

/// Tracks WGC capture slots and retains a missed slot as a possible re-anchor.
pub(super) struct WgcPacing {
    enabled: bool,
    fps: u32,
    group_anchor: Option<Duration>,
    pub(super) group_frames: u64,
    last_missed_slot: Option<Duration>,
    source_clock: Option<SourceClock>,
}

impl WgcPacing {
    pub(super) fn new(enabled: bool, fps: u32) -> Self {
        Self {
            enabled,
            fps,
            group_anchor: None,
            group_frames: 0,
            last_missed_slot: None,
            source_clock: None,
        }
    }

    pub(super) fn capture_arrived(
        &mut self,
        source_timestamp_us: u64,
        observed_at: Duration,
    ) -> Option<Duration> {
        let raw_anchor = self.map_source_timestamp(source_timestamp_us, observed_at);
        if let Some(missed_slot) = self.last_missed_slot.take() {
            self.group_anchor = Some(
                phase_preserving_anchor(missed_slot, raw_anchor, self.fps, self.enabled)
                    .unwrap_or(raw_anchor),
            );
            self.group_frames = 1;
        } else if self.group_anchor.is_some() {
            self.group_frames = self.group_frames.checked_add(1)?;
        } else {
            self.group_anchor = Some(raw_anchor);
            self.group_frames = 1;
        }
        self.next_slot()
    }

    fn map_source_timestamp(&mut self, timestamp_us: u64, observed_at: Duration) -> Duration {
        if timestamp_us == 0 {
            self.source_clock = None;
            return observed_at;
        }

        let mapped = self.source_clock.and_then(|clock| {
            if timestamp_us <= clock.last_timestamp_us {
                return None;
            }
            let elapsed_us = timestamp_us.checked_sub(clock.anchor_timestamp_us)?;
            let mapped = clock
                .anchor_monotonic
                .checked_add(Duration::from_micros(elapsed_us))?;
            // A capture timestamp may lag its callback, but a timestamp more
            // than one frame ahead cannot represent an already acquired frame.
            let max_future_ns = NANOS_PER_SECOND / u128::from(self.fps.max(1));
            (mapped.as_nanos() <= MAX_DURATION_NS
                && mapped.as_nanos().abs_diff(observed_at.as_nanos()) <= MAX_HOLD_NS
                && mapped.as_nanos() <= observed_at.as_nanos().saturating_add(max_future_ns))
            .then_some(mapped)
        });

        if let Some(mapped) = mapped {
            self.source_clock = self.source_clock.map(|mut clock| {
                clock.last_timestamp_us = timestamp_us;
                clock
            });
            return mapped;
        }

        // Invalid source time falls back to the local monotonic observation and
        // starts a fresh mapping. Future source timestamps can then recover.
        self.source_clock = Some(SourceClock {
            anchor_timestamp_us: timestamp_us,
            last_timestamp_us: timestamp_us,
            anchor_monotonic: observed_at,
        });
        observed_at
    }

    pub(super) fn capture_missed(&mut self, target: Duration) {
        if self.group_anchor.is_some() {
            // Preserve the first bust slot until the next frame arrives. Replacing
            // it on each empty poll would extend the 200 ms trust window forever.
            self.last_missed_slot.get_or_insert(target);
            self.group_anchor = None;
            self.group_frames = 0;
        }
    }

    /// Advances an already established output grid for a cached delivery.
    /// Reuse never changes the source-arrival anchor or clears a pending miss.
    pub(super) fn cached_reuse_slot(&mut self) -> Option<Duration> {
        self.group_anchor?;
        self.group_frames = self.group_frames.checked_add(1)?;
        self.next_slot()
    }

    fn next_slot(&self) -> Option<Duration> {
        let anchor = self.group_anchor?;
        if self.fps == 0 {
            return None;
        }
        let frames = u128::from(self.group_frames);
        let frame_offset_ns = frames.checked_mul(NANOS_PER_SECOND)? / u128::from(self.fps);
        duration_from_nanos(anchor.as_nanos().checked_add(frame_offset_ns)?)
    }
}

/// Keeps the next target in the future so a delayed pump cannot spin through
/// already expired slots on consecutive module ticks.
pub(super) fn next_slot_after_completion(
    projected_slot: Duration,
    completed_at: Duration,
    fps: u32,
) -> Option<(Duration, bool)> {
    if fps == 0 {
        return None;
    }
    if projected_slot <= completed_at {
        let interval_ns = NANOS_PER_SECOND / u128::from(fps);
        let next_ns = completed_at.as_nanos().checked_add(interval_ns)?;
        Some((duration_from_nanos(next_ns)?, true))
    } else {
        Some((projected_slot, false))
    }
}

/// Returns the prior cadence grid's projected anchor when it is safe to keep.
fn phase_preserving_anchor(
    previous_slot: Duration,
    raw_anchor: Duration,
    fps: u32,
    enabled: bool,
) -> Option<Duration> {
    if !enabled || fps == 0 {
        return None;
    }

    let interval_ns = NANOS_PER_SECOND / u128::from(fps);
    if interval_ns == 0 {
        return None;
    }
    let elapsed_ns = raw_anchor
        .as_nanos()
        .checked_sub(previous_slot.as_nanos())?;
    let intervals = elapsed_ns
        .checked_add(interval_ns / 2)?
        .checked_div(interval_ns)?;
    let projected_ns = intervals
        .checked_mul(NANOS_PER_SECOND)?
        .checked_div(u128::from(fps))?;
    let grid_ns = previous_slot.as_nanos().checked_add(projected_ns)?;
    let phase_error_ns = grid_ns.abs_diff(raw_anchor.as_nanos());
    let snap_window_ns = MAX_SNAP_WINDOW_NS.min(interval_ns / 4);
    let hold_limit_ns = intervals.checked_add(1)?.checked_mul(interval_ns)?;

    if phase_error_ns <= snap_window_ns && hold_limit_ns <= MAX_HOLD_NS {
        duration_from_nanos(grid_ns)
    } else {
        None
    }
}

fn duration_from_nanos(nanoseconds: u128) -> Option<Duration> {
    Some(Duration::from_nanos(u64::try_from(nanoseconds).ok()?))
}

#[cfg(test)]
mod tests {
    use super::{WgcPacing, next_slot_after_completion, phase_preserving_anchor};
    use std::time::Duration;

    fn duration(nanoseconds: u64) -> Duration {
        Duration::from_nanos(nanoseconds)
    }

    #[test]
    fn phase_near_the_prior_grid_snaps_to_the_projected_slot() {
        let anchor =
            phase_preserving_anchor(duration(100_000_000), duration(133_500_000), 60, true);

        assert_eq!(anchor, Some(duration(133_333_333)));
    }

    #[test]
    fn phase_outside_two_milliseconds_uses_the_raw_anchor() {
        let anchor =
            phase_preserving_anchor(duration(100_000_000), duration(130_000_000), 60, true);

        assert_eq!(anchor, None);
    }

    #[test]
    fn hold_over_two_hundred_milliseconds_uses_the_raw_anchor() {
        let anchor =
            phase_preserving_anchor(duration(100_000_000), duration(300_000_000), 60, true);

        assert_eq!(anchor, None);
    }

    #[test]
    fn disabled_smoothing_uses_the_raw_anchor() {
        let anchor =
            phase_preserving_anchor(duration(100_000_000), duration(133_500_000), 60, false);

        assert_eq!(anchor, None);
    }

    #[test]
    fn consecutive_empty_polls_keep_the_first_missed_slot() {
        let mut pacing = WgcPacing::new(true, 60);
        assert_eq!(
            pacing.capture_arrived(100_000, duration(0)),
            Some(duration(16_666_666))
        );

        pacing.capture_missed(duration(16_666_666));
        pacing.capture_missed(duration(33_333_332));
        assert_eq!(pacing.last_missed_slot, Some(duration(16_666_666)));
        assert_eq!(
            pacing.capture_arrived(133_500, duration(33_500_000)),
            Some(duration(49_999_998))
        );
    }

    #[test]
    fn cached_reuse_advances_the_existing_grid_without_reanchoring() {
        let mut pacing = WgcPacing::new(true, 60);
        assert_eq!(
            pacing.capture_arrived(100_000, duration(0)),
            Some(duration(16_666_666))
        );

        assert_eq!(pacing.cached_reuse_slot(), Some(duration(33_333_333)));
        assert_eq!(pacing.group_frames, 2);
        assert_eq!(pacing.group_anchor, Some(duration(0)));
        assert_eq!(pacing.last_missed_slot, None);
    }

    #[test]
    fn cached_reuse_preserves_a_pending_miss_for_a_later_fresh_frame() {
        let mut pacing = WgcPacing::new(true, 60);
        pacing.capture_arrived(100_000, duration(0));
        pacing.capture_missed(duration(16_666_666));

        assert_eq!(pacing.cached_reuse_slot(), None);
        assert_eq!(pacing.last_missed_slot, Some(duration(16_666_666)));
        assert_eq!(
            pacing.capture_arrived(133_500, duration(33_500_000)),
            Some(duration(49_999_998))
        );
    }

    #[test]
    fn source_timestamp_removes_callback_delay_from_phase_reanchor() {
        let mut pacing = WgcPacing::new(true, 60);
        assert_eq!(
            pacing.capture_arrived(100_000, duration(0)),
            Some(duration(16_666_666))
        );
        pacing.capture_missed(duration(16_666_666));

        // The source frame timestamp advances by 33.5 ms, but its callback is
        // observed 8 ms later. Pacing follows source time and snaps within 2 ms.
        assert_eq!(
            pacing.capture_arrived(133_500, duration(41_500_000)),
            Some(duration(49_999_998))
        );
        assert_eq!(pacing.group_anchor, Some(duration(33_333_332)));
    }

    #[test]
    fn disabled_smoothing_keeps_raw_source_anchor_after_a_missed_slot() {
        let mut pacing = WgcPacing::new(false, 60);
        assert_eq!(
            pacing.capture_arrived(100_000, duration(0)),
            Some(duration(16_666_666))
        );
        pacing.capture_missed(duration(16_666_666));

        // The mapped source time is 33.5 ms even though its callback arrives 8 ms later.
        assert_eq!(
            pacing.capture_arrived(133_500, duration(41_500_000)),
            Some(duration(50_166_666))
        );
        assert_eq!(pacing.group_anchor, Some(duration(33_500_000)));
    }

    #[test]
    fn zero_and_regressing_source_timestamps_fall_back_to_observed_time() {
        let mut pacing = WgcPacing::new(true, 60);
        pacing.capture_arrived(100_000, duration(0));
        pacing.capture_missed(duration(16_666_666));

        // Zero is invalid and uses the local observation; the next valid source
        // timestamp starts a new source-clock mapping at its own observation.
        pacing.capture_arrived(0, duration(40_000_000));
        assert_eq!(pacing.group_anchor, Some(duration(40_000_000)));
        assert_eq!(pacing.source_clock, None);

        pacing.capture_arrived(200_000, duration(50_000_000));
        assert_eq!(
            pacing.source_clock.unwrap().anchor_monotonic,
            duration(50_000_000)
        );
        pacing.capture_arrived(190_000, duration(60_000_000));
        assert_eq!(
            pacing.source_clock.unwrap().anchor_monotonic,
            duration(60_000_000)
        );
        assert_eq!(pacing.source_clock.unwrap().anchor_timestamp_us, 190_000);
    }

    #[test]
    fn unrepresentable_source_delta_falls_back_and_reanchors() {
        let mut pacing = WgcPacing::new(true, 60);
        assert_eq!(
            pacing.map_source_timestamp(1, duration(10_000_000)),
            duration(10_000_000)
        );

        let observed_at = duration(20_000_000);
        assert_eq!(
            pacing.map_source_timestamp(u64::MAX, observed_at),
            observed_at
        );
        assert_eq!(pacing.source_clock.unwrap().anchor_timestamp_us, u64::MAX);
        assert_eq!(pacing.source_clock.unwrap().anchor_monotonic, observed_at);
    }

    #[test]
    fn discontinuous_source_clock_reanchors_before_it_can_stall_capture() {
        let mut pacing = WgcPacing::new(true, 60);
        pacing.capture_arrived(100_000, duration(10_000_000));

        let observed_at = duration(30_000_000);
        assert_eq!(
            pacing.map_source_timestamp(10_000_000, observed_at),
            observed_at
        );
        assert_eq!(pacing.source_clock.unwrap().anchor_timestamp_us, 10_000_000);

        // A later timestamp from the new clock remains usable.
        assert_eq!(
            pacing.map_source_timestamp(10_016_000, duration(46_000_000)),
            duration(46_000_000)
        );
    }

    #[test]
    fn delayed_source_clock_reanchors_to_current_observation() {
        let mut pacing = WgcPacing::new(true, 60);
        pacing.capture_arrived(100_000, duration(10_000_000));

        assert_eq!(
            pacing.map_source_timestamp(116_000, duration(500_000_000)),
            duration(500_000_000)
        );
        assert_eq!(
            pacing.source_clock.unwrap().anchor_monotonic,
            duration(500_000_000)
        );
    }

    #[test]
    fn source_clock_ahead_of_callback_reanchors_before_next_capture() {
        let mut pacing = WgcPacing::new(true, 60);
        pacing.capture_arrived(100_000, duration(10_000_000));
        pacing.capture_missed(duration(26_666_666));

        // This 150 ms source jump is inside the old symmetric 200 ms window,
        // yet that frame has already arrived at the 30 ms callback.
        let next = pacing.capture_arrived(250_000, duration(30_000_000));
        assert_eq!(next, Some(duration(46_666_666)));
        assert_eq!(
            pacing.source_clock.unwrap().anchor_monotonic,
            duration(30_000_000)
        );
    }

    #[test]
    fn completion_past_projected_slot_schedules_one_future_interval() {
        let (slot, was_missed) =
            next_slot_after_completion(duration(20_000_000), duration(25_000_000), 60).unwrap();

        assert!(was_missed);
        assert_eq!(slot, duration(41_666_666));
    }
}

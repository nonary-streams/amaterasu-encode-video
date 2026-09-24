use std::time::Duration;

const NANOS_PER_SECOND: u128 = 1_000_000_000;
const MAX_SNAP_WINDOW_NS: u128 = 2_000_000;
const MAX_HOLD_NS: u128 = 200_000_000;

/// Tracks WGC capture slots and retains a missed slot as a possible re-anchor.
pub(super) struct WgcPacing {
    enabled: bool,
    fps: u32,
    group_anchor: Option<Duration>,
    pub(super) group_frames: u64,
    last_missed_slot: Option<Duration>,
}

impl WgcPacing {
    pub(super) fn new(enabled: bool, fps: u32) -> Self {
        Self {
            enabled,
            fps,
            group_anchor: None,
            group_frames: 0,
            last_missed_slot: None,
        }
    }

    pub(super) fn capture_arrived(&mut self, raw_anchor: Duration) -> Option<Duration> {
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

    pub(super) fn capture_missed(&mut self, target: Duration) {
        if self.group_anchor.is_some() {
            // Preserve the first bust slot until the next frame arrives. Replacing
            // it on each empty poll would extend the 200 ms trust window forever.
            self.last_missed_slot.get_or_insert(target);
            self.group_anchor = None;
            self.group_frames = 0;
        }
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
            pacing.capture_arrived(duration(0)),
            Some(duration(16_666_666))
        );

        pacing.capture_missed(duration(16_666_666));
        pacing.capture_missed(duration(33_333_332));
        assert_eq!(pacing.last_missed_slot, Some(duration(16_666_666)));
        assert_eq!(
            pacing.capture_arrived(duration(33_500_000)),
            Some(duration(49_999_998))
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

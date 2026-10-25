use super::owned_capture::{linux_providers, owned_setup, start, status};
use super::*;
use crate::plane::FramePath;
use amaterasu_media::PresentationStatus;

/// A running owned-output stream on the DMA-BUF path whose vkms table carries
/// the v1.5 presentation query. Its first frame went out through the timer
/// probe, the keepalive deadline and the 1/fps timer are far away, and the
/// frame-rate cap is open.
fn presentation_stream() -> (Pipeline, Runtime, FakePlane) {
    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    plane.presentation_api = true;
    start(&mut pipeline, &mut runtime, &mut plane);
    let stream = pipeline.streams.get_mut(&1).unwrap();
    assert!(matches!(stream.stage, Stage::Running));
    assert_eq!(stream.frame_path, FramePath::DmaBuf);
    assert!(
        stream.last_output_at.is_some(),
        "the timer probe sent a frame"
    );
    stream.force_idr = false;
    idle_timers(stream);
    open_rate_cap(stream);
    (pipeline, runtime, plane)
}

fn idle_timers(stream: &mut Stream) {
    let later = Instant::now() + Duration::from_secs(60);
    stream.next_frame = later;
    stream.next_repeat_at = Some(later);
}

fn open_rate_cap(stream: &mut Stream) {
    stream.presentation.set_next_submit_at(Instant::now());
}

fn hold_rate_cap(stream: &mut Stream) {
    stream
        .presentation
        .set_next_submit_at(Instant::now() + Duration::from_secs(60));
}

fn present(plane: &mut FakePlane, sequence: u64, timestamp_us: u64) {
    plane.presentation = PresentationStatus {
        sequence,
        timestamp_us,
    };
}

/// One pump, returning its (capture due, force IDR, repeat due, timestamp).
fn pump_once(
    pipeline: &mut Pipeline,
    runtime: &mut Runtime,
    plane: &mut FakePlane,
) -> (bool, bool, bool, u64) {
    let before = plane.inputs.len();
    tick(pipeline, runtime, plane, 1);
    assert_eq!(plane.inputs.len(), before + 1, "the running stream pumped");
    let input = plane.inputs.last().unwrap();
    (input.1, input.2, input.3, input.6)
}

fn counters(pipeline: &Pipeline) -> (u64, u64, u64) {
    let counters = &status(pipeline)["presentation_pacing"];
    (
        counters["fresh_pushes"].as_u64().unwrap(),
        counters["repeats"].as_u64().unwrap(),
        counters["coalesced"].as_u64().unwrap(),
    )
}

#[test]
fn fresh_presentation_is_pushed_on_the_next_tick_not_the_next_timer_slot() {
    let (mut pipeline, mut runtime, mut plane) = presentation_stream();
    assert_eq!(status(&pipeline)["pacing"], "presentation");
    assert_eq!(status(&pipeline)["frame_path"], "dmabuf");
    let (capture_due, _, repeat_due, _) = pump_once(&mut pipeline, &mut runtime, &mut plane);
    assert!(!capture_due && !repeat_due, "nothing was presented");

    present(&mut plane, 1, 2_000_000);
    let submitted = pipeline.streams[&1].submitted;
    let (capture_due, _, repeat_due, _) = pump_once(&mut pipeline, &mut runtime, &mut plane);
    assert!(
        capture_due,
        "the presentation is pushed although the timer is a minute away"
    );
    assert!(!repeat_due);
    assert_eq!(pipeline.streams[&1].submitted, submitted + 1);
    assert_eq!(counters(&pipeline), (1, 0, 0));
    assert!(plane.presentation_queries >= 2);
}

#[test]
fn rate_cap_holds_newer_presentations_and_pushes_only_the_newest() {
    let (mut pipeline, mut runtime, mut plane) = presentation_stream();
    present(&mut plane, 1, 1_000_000);
    assert!(pump_once(&mut pipeline, &mut runtime, &mut plane).0);
    let stream = pipeline.streams.get_mut(&1).unwrap();
    idle_timers(stream);
    hold_rate_cap(stream);

    for (sequence, timestamp_us) in [(2, 1_004_000), (3, 1_008_000)] {
        present(&mut plane, sequence, timestamp_us);
        let (capture_due, _, repeat_due, _) = pump_once(&mut pipeline, &mut runtime, &mut plane);
        assert!(
            !capture_due && !repeat_due,
            "the cap holds presentation {sequence}"
        );
    }
    let submitted = pipeline.streams[&1].submitted;
    open_rate_cap(pipeline.streams.get_mut(&1).unwrap());
    assert!(pump_once(&mut pipeline, &mut runtime, &mut plane).0);
    assert_eq!(pipeline.streams[&1].submitted, submitted + 1);
    // Presentation 2 was superseded before the cap admitted a submit.
    assert_eq!(counters(&pipeline), (2, 0, 1));

    // Presentation 3 was delivered; nothing newer remains to push.
    open_rate_cap(pipeline.streams.get_mut(&1).unwrap());
    let (capture_due, _, repeat_due, _) = pump_once(&mut pipeline, &mut runtime, &mut plane);
    assert!(!capture_due && !repeat_due);
}

#[test]
fn a_fresh_push_closes_the_rate_cap_for_one_client_frame_interval() {
    let (mut pipeline, mut runtime, mut plane) = presentation_stream();
    present(&mut plane, 1, 1_000_000);
    assert!(pump_once(&mut pipeline, &mut runtime, &mut plane).0);
    // The next presentation lands at once; the 60 fps cap has not reopened.
    present(&mut plane, 2, 1_001_000);
    let started = Instant::now();
    let (capture_due, _, _, _) = pump_once(&mut pipeline, &mut runtime, &mut plane);
    if started.elapsed() < Duration::from_millis(15) {
        assert!(
            !capture_due,
            "a second push within one interval exceeds 60 fps"
        );
    }
}

#[test]
fn idle_stream_submits_only_keepalive_repeats() {
    let (mut pipeline, mut runtime, mut plane) = presentation_stream();
    present(&mut plane, 1, 1_000_000);
    assert!(pump_once(&mut pipeline, &mut runtime, &mut plane).0);
    let stream = pipeline.streams.get_mut(&1).unwrap();
    idle_timers(stream);
    open_rate_cap(stream);
    let submitted = pipeline.streams[&1].submitted;
    for _ in 0..5 {
        let (capture_due, _, repeat_due, _) = pump_once(&mut pipeline, &mut runtime, &mut plane);
        assert!(
            !capture_due && !repeat_due,
            "nothing new and no keepalive due"
        );
    }
    assert_eq!(pipeline.streams[&1].submitted, submitted);

    // The keepalive deadline (minimum_fps_target) re-reads the presentation.
    pipeline.streams.get_mut(&1).unwrap().next_repeat_at = Some(Instant::now());
    let (capture_due, _, repeat_due, _) = pump_once(&mut pipeline, &mut runtime, &mut plane);
    assert!(!capture_due && repeat_due);
    assert_eq!(pipeline.streams[&1].submitted, submitted + 1);
    assert_eq!(counters(&pipeline), (1, 1, 0));
    assert!(
        pipeline.streams[&1]
            .next_repeat_at
            .is_some_and(|deadline| deadline > Instant::now()),
        "the keepalive deadline advanced"
    );
    let (_, _, repeat_due, _) = pump_once(&mut pipeline, &mut runtime, &mut plane);
    assert!(!repeat_due);
}

#[test]
fn forced_keyframe_repeats_the_latest_presentation_once_the_cap_allows() {
    let (mut pipeline, mut runtime, mut plane) = presentation_stream();
    let stream = pipeline.streams.get_mut(&1).unwrap();
    stream.force_idr = true;
    hold_rate_cap(stream);
    let (capture_due, _, repeat_due, _) = pump_once(&mut pipeline, &mut runtime, &mut plane);
    assert!(
        !capture_due && !repeat_due,
        "even a keyframe respects the fps cap"
    );
    assert!(pipeline.streams[&1].force_idr);

    open_rate_cap(pipeline.streams.get_mut(&1).unwrap());
    let (capture_due, force_idr, repeat_due, _) =
        pump_once(&mut pipeline, &mut runtime, &mut plane);
    assert!(!capture_due && repeat_due && force_idr);
    assert!(
        !pipeline.streams[&1].force_idr,
        "the accepted repeat cleared it"
    );

    // A pending keyframe rides on the next fresh push when one is ready.
    let stream = pipeline.streams.get_mut(&1).unwrap();
    stream.force_idr = true;
    open_rate_cap(stream);
    present(&mut plane, 4, 1_000_000);
    let (capture_due, force_idr, _, _) = pump_once(&mut pipeline, &mut runtime, &mut plane);
    assert!(capture_due && force_idr);
    assert_eq!(plane.frame_paths.last(), Some(&FramePath::DmaBuf));
}

#[test]
fn fresh_timestamps_follow_presentation_spacing_and_stay_monotonic() {
    let (mut pipeline, mut runtime, mut plane) = presentation_stream();
    let mut timestamps = vec![pipeline.streams[&1].last_output_timestamp_us.unwrap()];
    for (sequence, presented_us) in [(1, 5_000_000), (2, 5_008_000), (3, 5_011_000)] {
        open_rate_cap(pipeline.streams.get_mut(&1).unwrap());
        present(&mut plane, sequence, presented_us);
        let (capture_due, _, _, timestamp_us) = pump_once(&mut pipeline, &mut runtime, &mut plane);
        assert!(capture_due);
        timestamps.push(timestamp_us);
    }
    // Uneven 8 ms and 3 ms presentation gaps survive into the stream clock.
    assert_eq!(timestamps[2] - timestamps[1], 8_000);
    assert_eq!(timestamps[3] - timestamps[2], 3_000);

    // A regressing provider clock re-anchors, and a keepalive repeat follows
    // the stream clock; neither moves output time backwards.
    open_rate_cap(pipeline.streams.get_mut(&1).unwrap());
    present(&mut plane, 4, 10);
    timestamps.push(pump_once(&mut pipeline, &mut runtime, &mut plane).3);
    let stream = pipeline.streams.get_mut(&1).unwrap();
    open_rate_cap(stream);
    stream.next_repeat_at = Some(Instant::now());
    let (_, _, repeat_due, timestamp_us) = pump_once(&mut pipeline, &mut runtime, &mut plane);
    assert!(repeat_due);
    timestamps.push(timestamp_us);
    assert!(
        timestamps.windows(2).all(|pair| pair[0] < pair[1]),
        "{timestamps:?}"
    );
    assert_eq!(
        pipeline.streams[&1].last_output_timestamp_us,
        timestamps.last().copied()
    );
}

#[test]
fn capture_v1_4_provider_keeps_timer_pacing() {
    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    start(&mut pipeline, &mut runtime, &mut plane);
    assert_eq!(pipeline.streams[&1].frame_path, FramePath::DmaBuf);
    assert_eq!(status(&pipeline)["pacing"], "timer");
    assert_eq!(counters(&pipeline), (0, 0, 0));

    present(&mut plane, 7, 1_000_000);
    idle_timers(pipeline.streams.get_mut(&1).unwrap());
    let (capture_due, _, repeat_due, _) = pump_once(&mut pipeline, &mut runtime, &mut plane);
    assert!(
        !capture_due && !repeat_due,
        "only the timer schedules v1.4 reads"
    );
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    assert!(pump_once(&mut pipeline, &mut runtime, &mut plane).0);
    assert_eq!(status(&pipeline)["pacing"], "timer");
}

#[test]
fn cpu_streams_never_query_presentations() {
    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    plane.presentation_api = true;
    plane.dmabuf_ready = false;
    start(&mut pipeline, &mut runtime, &mut plane);
    assert_eq!(pipeline.streams[&1].frame_path, FramePath::Cpu);
    assert_eq!(plane.presentation_queries, 0);
    assert_eq!(status(&pipeline)["pacing"], "timer");
}

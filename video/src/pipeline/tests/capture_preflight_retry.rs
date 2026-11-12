use super::*;
use amaterasu_media::{MEDIA_BUSY, MEDIA_PENDING};
use amaterasu_module_sdk::{DynamicRange, VideoCodec, VideoStreamFormat, capture::SourceClass};

fn selected_open(provider: CaptureProvider, format: VideoStreamFormat) -> Operation {
    let mut configuration = open_configuration();
    configuration.video_stream_format = format;
    configuration.normalize_codec().unwrap();
    serde_json::from_value(json!({
        "action":"open", "operation_id":"pending-preflight", "output":"display-1",
        "capture_provider":provider, "encoder":encoder_identity(), "configuration":configuration,
        "source_capture_context":{"route_key":"desktop"}
    }))
    .unwrap()
}

fn setup_selected(
    provider: CaptureProvider,
    format: VideoStreamFormat,
) -> (Pipeline, Runtime, FakePlane) {
    let (mut pipeline, mut runtime, mut plane) = setup_with_providers([provider].into());
    pipeline.source_preflight_routes.insert(
        "desktop".into(),
        crate::model::SourcePreflightRoute {
            provider,
            source_class: SourceClass::DesktopOutput,
        },
    );
    plane.source_format_open = true;
    let operation = selected_open(provider, format);
    if let Operation::OpenWithOptions { specification } = &operation {
        runtime.encoder_probe_replies.insert(
            encoder_identity().clsid.clone(),
            probe_reply(
                &json!(encoder_identity()),
                &json!(specification.configuration),
                true,
            ),
        );
    }
    pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
    (pipeline, runtime, plane)
}

fn poll_until_first_query(pipeline: &mut Pipeline, runtime: &mut Runtime, plane: &mut FakePlane) {
    for _ in 0..20 {
        if !plane.source_queries.is_empty() {
            return;
        }
        tick(pipeline, runtime, plane, 1);
    }
    panic!("preflight was not queried");
}

#[test]
fn open_retries_busy_and_pending_for_selected_replacement_providers_and_exact_formats() {
    for provider in [CaptureProvider::Dxgi, CaptureProvider::Wgc] {
        for dynamic_range in [DynamicRange::Sdr, DynamicRange::Hdr] {
            let format = VideoStreamFormat {
                codec: VideoCodec::Hevc,
                bit_depth: 10,
                dynamic_range,
            };
            let (mut pipeline, mut runtime, mut plane) = setup_selected(provider, format);
            plane.source_preflight_statuses = [MEDIA_BUSY, MEDIA_PENDING].into();
            poll_until_first_query(&mut pipeline, &mut runtime, &mut plane);
            assert_eq!(runtime.open_effects, 0);
            assert_eq!(plane.source_queries.len(), 1);
            tick(&mut pipeline, &mut runtime, &mut plane, 1);
            assert_eq!(plane.source_queries.len(), 2);
            assert_eq!(runtime.open_effects, 0);
            tick(&mut pipeline, &mut runtime, &mut plane, 20);
            assert_eq!(plane.source_queries.len(), 3);
            assert!(
                plane
                    .source_queries
                    .iter()
                    .all(|query| query == &plane.source_queries[0])
            );
            let query = &plane.source_queries[0];
            assert_eq!(query.0, provider);
            assert_eq!(query.1.route.as_deref(), Some("display-1"));
            assert_eq!(query.1.mode.bit_depth, Some(10));
            assert_eq!(query.1.mode.hdr, dynamic_range == DynamicRange::Hdr);
            let captures: Vec<_> = runtime
                .commands
                .iter()
                .filter_map(|command| match command {
                    Command::Request {
                        service, payload, ..
                    } if service == provider.service() && payload["action"] == "open" => {
                        Some(payload)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(captures.len(), 1);
            assert_eq!(
                captures[0]["source_format"],
                json!({"bit_depth":10,"dynamic_range":dynamic_range})
            );
            assert_eq!(runtime.completed[0].1["state"], "running");
        }
    }
}

#[test]
fn cancellation_timeout_and_shutdown_stop_preflight_without_opening_capture() {
    for reason in ["cancel", "timeout", "shutdown"] {
        let (mut pipeline, mut runtime, mut plane) =
            setup_selected(CaptureProvider::Dxgi, VideoStreamFormat::H264_SDR);
        plane.source_preflight_statuses = [MEDIA_BUSY].into();
        poll_until_first_query(&mut pipeline, &mut runtime, &mut plane);
        match reason {
            "cancel" => runtime.cancelled = true,
            "timeout" => {
                pipeline
                    .streams
                    .get_mut(&1)
                    .unwrap()
                    .capture_preflight_started = Some(Instant::now() - Duration::from_secs(6))
            }
            "shutdown" => {
                pipeline.prepare_shutdown(20_000);
                pipeline
                    .quiesce(&mut plane, &mut |command| runtime.send(command))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        tick(&mut pipeline, &mut runtime, &mut plane, 20);
        assert_eq!(plane.source_queries.len(), 1, "{reason}");
        assert_eq!(runtime.open_effects, 0, "{reason}");
        assert!(pipeline.streams.is_empty(), "{reason}");
    }
}

#[test]
fn identical_pending_queries_do_not_prevent_two_streams_from_progressing() {
    let (mut pipeline, mut runtime, mut plane) =
        setup_selected(CaptureProvider::Dxgi, VideoStreamFormat::H264_SDR);
    let mut second = selected_open(CaptureProvider::Dxgi, VideoStreamFormat::H264_SDR);
    let Operation::OpenWithOptions { specification } = &mut second else {
        unreachable!();
    };
    specification.operation_id = Some("second-preflight".into());
    pipeline.handle("alice", 100, second, &mut |command| runtime.send(command));
    plane.source_preflight_statuses = [MEDIA_BUSY, MEDIA_BUSY, MEDIA_PENDING].into();
    tick(&mut pipeline, &mut runtime, &mut plane, 30);
    assert_eq!(plane.source_queries.len(), 5);
    assert!(
        plane
            .source_queries
            .iter()
            .all(|query| query == &plane.source_queries[0])
    );
    assert_eq!(runtime.completed.len(), 2);
    assert!(
        runtime
            .completed
            .iter()
            .all(|(_, result)| result["state"] == "running")
    );
    let capture_opens = runtime
        .commands
        .iter()
        .filter(|command| {
            matches!(command,
                Command::Request { service, payload, .. }
                    if service == CAPTURE && payload["action"] == "open"
            )
        })
        .count();
    assert_eq!(capture_opens, 2);
}

#[test]
fn first_ready_response_after_deadline_cannot_start_capture() {
    let (mut pipeline, mut runtime, mut plane) =
        setup_selected(CaptureProvider::Dxgi, VideoStreamFormat::H264_SDR);
    plane.source_preflight_delay = Duration::from_millis(5_010);
    poll_until_first_query(&mut pipeline, &mut runtime, &mut plane);
    assert_eq!(plane.source_queries.len(), 1);
    assert_eq!(runtime.open_effects, 0);
    assert_eq!(
        pipeline.streams[&1].error,
        Some("capture_source_preflight_timeout")
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 20);
    assert!(pipeline.streams.is_empty());
    assert_eq!(plane.source_queries.len(), 1);
}

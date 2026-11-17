//! First-hand stream reports: the video service publishes its own failures
//! and fallbacks under the operation it was opened with.
use super::*;
use amaterasu_module_sdk::stream_lifecycle::{
    IssueDisposition, LifecycleLayer, StreamBinding, StreamIssue, StreamReport,
};

fn reports(commands: &[Command]) -> Vec<StreamReport> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::Publish { topic, payload } if topic == "amaterasu.stream.report/1" => {
                Some(serde_json::from_value(payload.clone()).unwrap())
            }
            _ => None,
        })
        .collect()
}

fn issues(commands: &[Command]) -> Vec<StreamIssue> {
    reports(commands)
        .into_iter()
        .filter_map(|report| match report {
            StreamReport::Issue(issue) => Some(issue),
            StreamReport::Binding(_) => None,
        })
        .collect()
}

fn open_from_session(encoder_selection: EncoderSelection) -> Operation {
    let mut request = open();
    let Operation::Open {
        encoder,
        operation_id,
        ..
    } = &mut request
    else {
        unreachable!();
    };
    *encoder = encoder_selection;
    *operation_id = Some("ss-2a-video".into());
    request
}

#[test]
fn encoder_unavailable_cleanup_is_published_under_the_opening_operation() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.enable_stream_issue_reports();
    let request = open_from_session(EncoderSelection::Index(1));
    pipeline.handle("alice", 100, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 8);

    let published = reports(&runtime.commands);
    assert_eq!(
        published.first(),
        Some(&StreamReport::Binding(StreamBinding::new(
            "ss-2a-video",
            "video-test-1"
        ))),
        "providers called under the stream operation report into the session's stream"
    );
    let raw = runtime.commands.iter().find_map(|command| match command {
        Command::Publish { payload, .. } if payload["kind"] == "issue" => Some(payload),
        _ => None,
    });
    assert_eq!(raw.unwrap()["kind"], "issue");

    let issues = issues(&runtime.commands);
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.operation, "ss-2a-video");
    assert_eq!(issue.layer, LifecycleLayer::EncodeVideo);
    assert_eq!(issue.caused_by, Some(LifecycleLayer::EncodeVideo));
    assert_eq!(issue.code, "encoder_unavailable");
    assert_eq!(issue.phase, "select-encoder");
    assert_eq!(issue.disposition, IssueDisposition::CriticalFailure);
    assert!(issue.site.contains("pipeline.rs:"), "{}", issue.site);
}

#[test]
fn software_encoder_fallback_is_a_demoted_warning_with_the_provider_code() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.enable_stream_issue_reports();
    runtime.encoder_enumeration_override = Some(json!({
        "ok": true, "enabled": true, "encoders": [
            {"identity":{"clsid":LINUX_FFMPEG_NVENC_ID,"hardware_url":"ffmpeg://h264_nvenc","friendly_name":"NVENC"},"name":"NVENC","codec":"h264","hardware":true},
            {"identity":{"clsid":SOFTWARE_ENCODER_ID,"hardware_url":null,"friendly_name":"x264"},"name":"x264","codec":"h264","hardware":false}
        ]
    }));
    runtime.encoder_open_error_once = Some("nvenc_device_unavailable");
    let request = open_from_session(EncoderSelection::Backend(EncoderBackend::Auto));
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 12);

    assert!(matches!(pipeline.streams[&1].stage, Stage::Running));
    let issues = issues(&runtime.commands);
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(issue.operation, "video-test-1-encoder-attempt-0");
    assert_eq!(issue.layer, LifecycleLayer::EncodeVideo);
    assert_eq!(issue.caused_by, Some(LifecycleLayer::EncodeVideo));
    assert_eq!(issue.code, "encoder_open_failed");
    assert_eq!(issue.reason, "nvenc_device_unavailable");
    assert_eq!(issue.fallback.as_deref(), Some("software_encoder"));
    assert_eq!(issue.disposition, IssueDisposition::DemotedWarning);
    assert!(issue.site.contains("pipeline.rs:"), "{}", issue.site);
}

#[test]
fn failed_software_encoder_fallback_remains_a_critical_failure() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.enable_stream_issue_reports();
    runtime.encoder_enumeration_override = Some(json!({"ok": true, "enabled": true, "encoders": [
        {"identity":{"clsid":LINUX_FFMPEG_NVENC_ID,"hardware_url":"ffmpeg://h264_nvenc","friendly_name":"NVENC"},"name":"NVENC","codec":"h264","hardware":true},
        {"identity":{"clsid":SOFTWARE_ENCODER_ID,"hardware_url":null,"friendly_name":"x264"},"name":"x264","codec":"h264","hardware":false}
    ]}));
    runtime.encoder_open_error = Some("nvenc_device_unavailable");
    let request = open_from_session(EncoderSelection::Backend(EncoderBackend::Auto));
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 20);
    let issues = issues(&runtime.commands);
    assert!(issues.iter().any(StreamIssue::is_critical), "{issues:?}");
    assert!(
        !issues
            .iter()
            .any(|issue| issue.code == "encoder_open_failed"
                && issue.disposition == IssueDisposition::DemotedWarning),
        "{issues:?}"
    );
}

#[test]
fn encoder_probe_fallback_warning_requires_a_running_candidate() {
    for final_candidate_usable in [true, false] {
        let (mut pipeline, mut runtime, mut plane) = setup();
        pipeline.enable_stream_issue_reports();
        let mut request = auto_open();
        let Operation::Open {
            configuration,
            operation_id,
            ..
        } = &mut request
        else {
            unreachable!()
        };
        *operation_id = Some("ss-probe-video".into());
        let configuration = json!(configuration);
        let first = candidate_description("first-hardware", true);
        let second = candidate_description("second-hardware", true);
        runtime.encoder_enumeration_override =
            Some(json!({"ok":true,"enabled":true,"encoders":[first,second]}));
        runtime.encoder_probe_replies.insert(
            "first-hardware".into(),
            probe_reply(&first["identity"], &configuration, false),
        );
        if !final_candidate_usable {
            runtime.encoder_probe_replies.insert(
                "second-hardware".into(),
                probe_reply(&second["identity"], &configuration, false),
            );
        }
        pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
        tick(&mut pipeline, &mut runtime, &mut plane, 30);
        let issues = issues(&runtime.commands);
        assert_eq!(
            issues
                .iter()
                .any(|issue| issue.code == "encoder_probe_rejected"
                    && issue.disposition == IssueDisposition::DemotedWarning),
            final_candidate_usable,
            "{issues:?}"
        );
        if !final_candidate_usable {
            assert!(issues.iter().any(StreamIssue::is_critical), "{issues:?}");
        }
    }
}

#[test]
fn runtime_encoder_fallback_warning_requires_successful_restart() {
    for restart_succeeds in [true, false] {
        let (mut pipeline, mut runtime, mut plane) = setup();
        pipeline.enable_stream_issue_reports();
        runtime.encoder_enumeration_override = Some(json!({"ok":true,"enabled":true,"encoders":[
            {"identity":{"clsid":LINUX_FFMPEG_VAAPI_ID,"hardware_url":"ffmpeg://h264_vaapi","friendly_name":"VAAPI"},"name":"VAAPI","codec":"h264","hardware":true},
            {"identity":{"clsid":SOFTWARE_ENCODER_ID,"hardware_url":null,"friendly_name":"x264"},"name":"x264","codec":"h264","hardware":false}
        ]}));
        pipeline.handle(
            "alice",
            99,
            open_from_session(EncoderSelection::Backend(EncoderBackend::Auto)),
            &mut |command| runtime.send(command),
        );
        tick(&mut pipeline, &mut runtime, &mut plane, 12);
        assert!(matches!(pipeline.streams[&1].stage, Stage::Running));
        if !restart_succeeds {
            runtime.encoder_open_error = Some("encoder_unavailable");
        }
        plane.failure_code_once = Some("encoder_failed");
        pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
        tick(&mut pipeline, &mut runtime, &mut plane, 20);
        let issues = issues(&runtime.commands);
        assert_eq!(
            issues.iter().any(|issue| issue.code == "encoder_failed"
                && issue.disposition == IssueDisposition::DemotedWarning),
            restart_succeeds,
            "{issues:?}"
        );
        if !restart_succeeds {
            assert!(issues.iter().any(StreamIssue::is_critical), "{issues:?}");
        }
    }
}

#[test]
fn capture_refusal_is_reported_by_video_without_provider_reporting() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.enable_stream_issue_reports();
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Dxgi, "capture_disabled");
    let request = open_from_session(EncoderSelection::Identity(encoder_identity()));
    pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 20);
    let issues = issues(&runtime.commands);
    assert!(
        issues.iter().any(
            |issue| issue.disposition == IssueDisposition::CriticalFailure
                && issue.caused_by == Some(LifecycleLayer::Capture)
                && issue.reason == "capture_disabled"
                && issue.valid()
        ),
        "{issues:?}"
    );
}

#[test]
fn alternate_capture_is_reported_as_a_warning_by_video() {
    let dxgi = format!("00000000:00000001/{}", r"\\.\DISPLAY1");
    let wgc = r"\\.\DISPLAY1";
    let (mut pipeline, mut runtime, mut plane) =
        setup_with_providers([CaptureProvider::Dxgi, CaptureProvider::Wgc].into());
    pipeline.enable_stream_issue_reports();
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Dxgi, "unsupported_platform");
    runtime
        .capture_outputs
        .insert(CaptureProvider::Wgc, vec![capture_output(wgc, wgc)]);
    pipeline.handle(
        "alice",
        99,
        canonical_open("auto", CaptureProvider::Dxgi, &dxgi),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 20);
    assert!(matches!(pipeline.streams[&1].stage, Stage::Running));
    let issues = issues(&runtime.commands);
    assert!(
        issues.iter().any(
            |issue| issue.disposition == IssueDisposition::DemotedWarning
                && issue.caused_by == Some(LifecycleLayer::Capture)
                && issue.reason == "unsupported_platform"
                && issue.valid()
        ),
        "{issues:?}"
    );
    assert!(!issues.iter().any(StreamIssue::is_critical), "{issues:?}");
}

#[test]
fn failed_alternate_capture_does_not_report_a_successful_fallback() {
    let dxgi = format!("00000000:00000001/{}", r"\\.\DISPLAY1");
    let wgc = r"\\.\DISPLAY1";
    let (mut pipeline, mut runtime, mut plane) =
        setup_with_providers([CaptureProvider::Dxgi, CaptureProvider::Wgc].into());
    pipeline.enable_stream_issue_reports();
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Dxgi, "unsupported_platform");
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Wgc, "capture_disabled");
    runtime
        .capture_outputs
        .insert(CaptureProvider::Wgc, vec![capture_output(wgc, wgc)]);
    pipeline.handle(
        "alice",
        99,
        canonical_open("auto", CaptureProvider::Dxgi, &dxgi),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 30);
    let issues = issues(&runtime.commands);
    assert!(
        issues
            .iter()
            .any(|issue| issue.disposition == IssueDisposition::CriticalFailure),
        "{issues:?}"
    );
    assert!(
        !issues
            .iter()
            .any(|issue| issue.code == "capture_provider_failed"
                && issue.disposition == IssueDisposition::DemotedWarning),
        "{issues:?}"
    );
}

#[test]
fn owned_capture_fallback_is_reported_only_when_the_stream_recovers() {
    for fallback_succeeds in [true, false] {
        let (mut pipeline, mut runtime, mut plane) =
            super::owned_capture::owned_setup(super::owned_capture::linux_providers());
        pipeline.enable_stream_issue_reports();
        runtime
            .capture_open_errors
            .insert(CaptureProvider::Vkms, "frame_helper_unavailable");
        if !fallback_succeeds {
            runtime
                .capture_open_errors
                .insert(CaptureProvider::Pipewire, "capture_disabled");
        }
        let request =
            serde_json::from_value(json!({"action":"open", "operation_id":"ss-owned-video",
            "capture_provider":"pipewire", "output":"kwin:Virtual-9", "encoder":"auto",
            "configuration":{"width":1920,"height":1080,"fps":60,"bitrate":5000000}}))
            .unwrap();
        pipeline.handle("alice", 99, request, &mut |command| runtime.send(command));
        tick(&mut pipeline, &mut runtime, &mut plane, 30);
        let issues = issues(&runtime.commands);
        assert_eq!(
            issues
                .iter()
                .any(|issue| issue.code == "vkms_capture_open_failed"
                    && issue.disposition == IssueDisposition::DemotedWarning),
            fallback_succeeds,
            "{issues:?}"
        );
        if fallback_succeeds {
            let fallback = issues
                .iter()
                .find(|issue| issue.code == "vkms_capture_open_failed")
                .unwrap();
            let failed_operation = runtime
                .commands
                .iter()
                .find_map(|command| match command {
                    Command::Request {
                        service, payload, ..
                    } if service.contains("vkms") && payload["action"] == "open" => {
                        payload["operation_id"].as_str()
                    }
                    _ => None,
                })
                .unwrap();
            assert_eq!(fallback.operation, failed_operation);
        }
        if !fallback_succeeds {
            assert!(issues.iter().any(StreamIssue::is_critical), "{issues:?}");
        }
    }
}

#[test]
fn skipped_dmabuf_selection_does_not_recover_a_later_cpu_capture_failure() {
    let (mut pipeline, mut runtime, mut plane) =
        super::owned_capture::owned_setup(super::owned_capture::linux_providers());
    pipeline.enable_stream_issue_reports();
    plane.dmabuf_ready = false;
    super::owned_capture::start(&mut pipeline, &mut runtime, &mut plane);
    let stream = &pipeline.streams[&1];
    assert_eq!(stream.provider, CaptureProvider::Pipewire);
    let operation = stream.capture_operation.clone();
    let warning = issues(&runtime.commands)
        .into_iter()
        .find(|issue| issue.code == "dmabuf_capture_unavailable")
        .unwrap();
    assert_eq!(warning.disposition, IssueDisposition::DemotedWarning);
    assert_eq!(warning.caused_by, None);
    let mut store = amaterasu_module_sdk::stream_lifecycle::StreamLifecycleStore::new();
    store.open("launch-1");
    store.alias("launch-1", &stream.operation);
    store.commit("video", warning);
    store.commit(
        "capture",
        StreamIssue::critical(operation, LifecycleLayer::Capture, "read", "capture_failed"),
    );
    assert!(!store.stream("launch-1").unwrap().healthy());
}

#[test]
fn reports_are_off_until_enabled_and_requested_endings_are_not_failures() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let request = open_from_session(EncoderSelection::Index(1));
    pipeline.handle("alice", 100, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(reports(&runtime.commands).is_empty());
    assert!(pipeline.retired_issues.is_empty(), "nothing is kept either");

    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.enable_stream_issue_reports();
    let request = open_from_session(EncoderSelection::Identity(encoder_identity()));
    pipeline.handle("alice", 101, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert!(matches!(pipeline.streams[&1].stage, Stage::Running));
    pipeline.handle(
        "alice",
        102,
        Operation::Close { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert!(
        issues(&runtime.commands).is_empty(),
        "a caller's close is not a failure"
    );
}

#[test]
fn noncritical_provider_reports_cannot_start_cleanup_or_release_live_video() {
    use amaterasu_module_sdk::stream_lifecycle::StreamFailure;
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.enable_stream_issue_reports();
    pipeline.handle(
        "alice",
        101,
        open_from_session(EncoderSelection::Identity(encoder_identity())),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    let stream = pipeline.streams.get_mut(&1).unwrap();
    assert_eq!(stream.stage, Stage::Running);
    let capture = stream.capture;
    let encoder = stream.session;
    for report in [
        StreamFailure::advisory("optional_metadata_unavailable"),
        StreamFailure::demoted("optional_feedback_unavailable", "keep_video"),
        StreamFailure::critical("invalid/code"),
    ] {
        stream.cleanup(report);
        assert_eq!(stream.stage, Stage::Running);
        assert_eq!(stream.capture, capture);
        assert_eq!(stream.session, encoder);
        assert_eq!(stream.error, None);
        assert!(!stream.unresolved);
    }
    // A drain warning has continuation authority only after an owner drain.
    stream.drain_failed("drain_timeout");
    assert_eq!(stream.stage, Stage::Running);
    stream.cleanup(StreamFailure::critical("capture_source_lost"));
    assert_eq!(stream.stage, Stage::FindEncoder);
    assert_eq!(stream.error, Some("capture_source_lost"));
    assert_eq!(
        stream.capture, capture,
        "retirement still needs exact capture proof"
    );
    assert_eq!(
        stream.session, encoder,
        "retirement still needs exact encoder proof"
    );
    pipeline.publish_stream_issues(&mut |command| runtime.send(command));
    let reports = issues(&runtime.commands);
    assert!(
        reports
            .iter()
            .any(|issue| issue.code == "optional_metadata_unavailable" && !issue.is_critical())
    );
    assert!(
        reports
            .iter()
            .any(|issue| issue.code == "capture_source_lost" && issue.is_critical())
    );
}

#[test]
fn noncritical_reports_cannot_install_a_video_cleanup_barrier() {
    use amaterasu_module_sdk::stream_lifecycle::StreamFailure;
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle(
        "alice",
        101,
        open_from_session(EncoderSelection::Identity(encoder_identity())),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    let stream = pipeline.streams.get_mut(&1).unwrap();
    stream.cleanup(StreamFailure::critical("native_encoder_lost"));
    stream.fail_retirement(StreamFailure::advisory("optional_cleanup_diagnostic_lost"));
    assert_eq!(stream.stage, Stage::FindEncoder);
    assert_eq!(stream.error, Some("native_encoder_lost"));
    for _ in 0..4 {
        failed_step(stream, StreamFailure::advisory("optional_diagnostic_lost"));
    }
    assert_eq!(stream.stage, Stage::FindEncoder);
    assert_eq!(stream.retries, 0);
    assert!(!stream.unresolved);
    assert_eq!(stream.error, Some("native_encoder_lost"));
    for _ in 0..3 {
        failed_step(stream, StreamFailure::critical("native_close_unconfirmed"));
    }
    assert!(
        stream.unresolved,
        "critical native retirement failures retain the cleanup fence"
    );
    assert_eq!(stream.error, Some("cleanup_failed"));
}

#[test]
fn uncertain_parent_liveness_does_not_invent_an_owner_stop() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.enable_stream_issue_reports();
    pipeline.handle(
        "alice",
        101,
        open_from_session(EncoderSelection::Identity(encoder_identity())),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    pipeline.streams.get_mut(&1).unwrap().parent = Some(999);
    pipeline.tick(&mut plane, &mut |command| {
        if matches!(command, Command::CheckRequest { request: 999 }) {
            Err("lost broker reply".into())
        } else {
            runtime.send(command)
        }
    });
    assert_eq!(pipeline.streams[&1].stage, Stage::Running);
    assert_eq!(pipeline.streams[&1].parent, Some(999));
    assert_eq!(pipeline.streams[&1].error, None);
    pipeline.tick(&mut plane, &mut |command| {
        if matches!(command, Command::CheckRequest { request: 999 }) {
            Ok(Response::error("cancelled", ""))
        } else {
            runtime.send(command)
        }
    });
    assert_ne!(pipeline.streams[&1].stage, Stage::Running);
    assert_eq!(pipeline.streams[&1].error, Some("request_ended"));
    pipeline.publish_stream_issues(&mut |command| runtime.send(command));
    assert!(
        !issues(&runtime.commands)
            .iter()
            .any(StreamIssue::is_critical)
    );
}

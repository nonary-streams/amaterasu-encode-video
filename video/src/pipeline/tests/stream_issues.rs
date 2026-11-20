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
    assert_eq!(issue.operation, "ss-2a-video");
    assert_eq!(issue.layer, LifecycleLayer::EncodeVideo);
    assert_eq!(issue.caused_by, Some(LifecycleLayer::EncodeVideo));
    assert_eq!(issue.code, "encoder_open_failed");
    assert_eq!(issue.reason, "nvenc_device_unavailable");
    assert_eq!(issue.fallback.as_deref(), Some("software_encoder"));
    assert_eq!(issue.disposition, IssueDisposition::DemotedWarning);
    assert!(issue.site.contains("pipeline.rs:"), "{}", issue.site);
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
fn a_terminal_processing_failure_preserves_its_native_evidence_during_cleanup() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.enable_stream_issue_reports();
    let request = open_from_session(EncoderSelection::Identity(encoder_identity()));
    pipeline.handle("alice", 100, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert_eq!(pipeline.streams[&1].stage, Stage::Running);

    let failure = NativeStreamFailure {
        service: "amaterasu.rtx-hdr/1".into(),
        call: "process".into(),
        status: amaterasu_media::MEDIA_UNSUPPORTED,
        capture_generation: Some(9),
    };
    plane.failure_code_once = Some("frame_processor_failed");
    plane.frame_failure = Some((LifecycleLayer::FrameProcessing, failure.clone()));
    plane.hdr_failure = Some("processor_hdr_metadata_unavailable");
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].error, Some("frame_processor_failed"));
    tick(&mut pipeline, &mut runtime, &mut plane, 8);

    let published = issues(&runtime.commands);
    let first = published
        .iter()
        .find(|issue| issue.code == "frame_processor_failed")
        .unwrap();
    assert_eq!(first.operation, "ss-2a-video");
    assert_eq!(first.caused_by, Some(LifecycleLayer::FrameProcessing));
    assert_eq!(first.native_failure.as_deref(), Some(&failure));
    assert!(
        !published
            .iter()
            .any(|issue| issue.code == "processor_hdr_metadata_unavailable")
    );
}

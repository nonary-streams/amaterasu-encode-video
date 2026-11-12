use super::*;
use amaterasu_module_sdk::capture::{CaptureNotice, PendingSwap, SwapDecision, SwapRequest};

fn running() -> (Pipeline, Runtime, FakePlane) {
    let (mut pipeline, mut runtime, mut plane) =
        setup_with_providers([CaptureProvider::Dxgi, CaptureProvider::Wgc].into());
    pipeline.handle("alice", 99, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(pipeline.streams[&1].stage, Stage::Running);
    plane.event_api = true;
    (pipeline, runtime, plane)
}

fn swap(provider: Option<&str>, output: Option<&str>) -> CaptureNotice {
    CaptureNotice {
        swap_request: Some(PendingSwap {
            id: 1,
            request: SwapRequest {
                preferred_provider: provider.map(str::to_owned),
                output: output.map(str::to_owned),
                reason: "source_unavailable".into(),
            },
        }),
        ..CaptureNotice::default()
    }
}

#[test]
fn frame_notifications_gate_reads_and_hold_ready_until_the_rate_cap_allows() {
    let (mut pipeline, mut runtime, mut plane) = running();
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert!(!plane.inputs.last().unwrap().1);
    plane.notices.push_back(CaptureNotice {
        frame_ready: true,
        ..CaptureNotice::default()
    });
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now() + Duration::from_secs(10);
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert!(!plane.inputs.last().unwrap().1);
    assert!(pipeline.streams[&1].capture_event_ready);
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert!(plane.inputs.last().unwrap().1);
}

#[test]
fn an_unregistered_swap_is_rejected_and_capture_keeps_running() {
    let (mut pipeline, mut runtime, mut plane) = running();
    plane
        .notices
        .push_back(swap(Some("unregistered"), Some("other-output")));
    let old_capture = pipeline.streams[&1].capture;
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].stage, Stage::Running);
    assert_eq!(pipeline.streams[&1].capture, old_capture);
    assert_eq!(
        plane.swap_decisions.last().unwrap().2,
        SwapDecision::Rejected {
            reason: "capture_swap_provider_unavailable".into()
        }
    );
}

#[test]
fn accepted_swap_retires_the_old_source_and_reopens_capture_without_reopening_encoding() {
    let (mut pipeline, mut runtime, mut plane) = running();
    let old_encoder = pipeline.streams[&1].session;
    let previous = runtime.commands.len();
    plane
        .notices
        .push_back(swap(Some("wgc"), Some("wgc-output")));
    tick(&mut pipeline, &mut runtime, &mut plane, 10);
    let stream = &pipeline.streams[&1];
    assert_eq!(stream.stage, Stage::Running);
    assert_eq!(stream.provider, CaptureProvider::Wgc);
    assert_eq!(stream.session, old_encoder);
    assert_eq!(stream.capture_output, "wgc-output");
    assert_eq!(plane.swap_decisions[0].2, SwapDecision::Accepted);
    assert!(plane.retired_events.contains(&1));
    let commands = &runtime.commands[previous..];
    let old_close = commands.iter().position(|command| matches!(command, Command::Request { service, payload, .. } if service == CAPTURE && payload["action"] == "close")).unwrap();
    let new_open = commands.iter().position(|command| matches!(command, Command::Request { service, payload, .. } if service == CaptureProvider::Wgc.service() && payload["action"] == "open")).unwrap();
    assert!(old_close < new_open);
    assert!(
        !commands.iter().any(
            |command| matches!(command, Command::Request { service, .. } if service == ENCODE)
        )
    );
}

#[test]
fn coordinator_can_choose_an_allowed_provider_but_requires_an_explicit_output_mapping() {
    let (mut pipeline, mut runtime, mut plane) = running();
    plane.notices.push_back(swap(None, None));
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(
        plane.swap_decisions.last().unwrap().2,
        SwapDecision::Rejected {
            reason: "capture_swap_output_required".into()
        }
    );
    plane.notices.push_back(swap(None, Some("mapped-output")));
    tick(&mut pipeline, &mut runtime, &mut plane, 10);
    assert_ne!(pipeline.streams[&1].provider, CaptureProvider::Dxgi);
    assert_eq!(pipeline.streams[&1].stage, Stage::Running);
}

#[test]
fn gpu_transition_is_explicitly_rejected_when_it_would_require_an_encoder_restart() {
    let (mut pipeline, mut runtime, mut plane) = running();
    pipeline.streams.get_mut(&1).unwrap().frame_path = FramePath::DmaBuf;
    plane
        .notices
        .push_back(swap(Some("wgc"), Some("other-output")));
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(
        plane.swap_decisions.last().unwrap().2,
        SwapDecision::Rejected {
            reason: "capture_swap_requires_pipeline_restart".into()
        }
    );
    assert_eq!(pipeline.streams[&1].provider, CaptureProvider::Dxgi);
}

#[test]
fn failure_to_settle_the_request_never_opens_a_replacement() {
    let (mut pipeline, mut runtime, mut plane) = running();
    plane.swap_reply_error = true;
    plane
        .notices
        .push_back(swap(Some("wgc"), Some("other-output")));
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].provider, CaptureProvider::Dxgi);
    assert_ne!(pipeline.streams[&1].stage, Stage::Running);
    assert!(pipeline.streams[&1].capture_swap_target.is_none());
}

#[test]
fn successive_swaps_close_each_generation_under_distinct_operation_ids() {
    let (mut pipeline, mut runtime, mut plane) = running();
    plane
        .notices
        .push_back(swap(Some("wgc"), Some("wgc-output")));
    tick(&mut pipeline, &mut runtime, &mut plane, 10);
    plane
        .notices
        .push_back(swap(Some("dxgi"), Some("dxgi-output")));
    tick(&mut pipeline, &mut runtime, &mut plane, 10);
    assert_eq!(pipeline.streams[&1].stage, Stage::Running);
    assert_eq!(pipeline.streams[&1].provider, CaptureProvider::Dxgi);
    let closes: Vec<_> = runtime
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Request {
                service,
                payload,
                operation_id,
                ..
            } if service != ENCODE && payload["action"] == "close" => Some(operation_id),
            _ => None,
        })
        .collect();
    assert_eq!(closes.len(), 2);
    assert_ne!(closes[0], closes[1]);
    assert_eq!(plane.swap_decisions.len(), 2);
}

#[test]
fn failed_replacement_open_cleans_up_the_existing_encoder() {
    let (mut pipeline, mut runtime, mut plane) = running();
    runtime
        .capture_open_errors
        .insert(CaptureProvider::Wgc, "source_unavailable");
    plane
        .notices
        .push_back(swap(Some("wgc"), Some("wgc-output")));
    tick(&mut pipeline, &mut runtime, &mut plane, 25);
    assert!(pipeline.streams.is_empty());
    assert!(runtime.commands.iter().any(|command| matches!(command,
        Command::Request { service, payload, .. } if service == ENCODE && payload["action"] == "close"
    )));
}

#[test]
fn terminal_capture_loss_cleans_up_instead_of_repeating_stale_pixels_forever() {
    let (mut pipeline, mut runtime, mut plane) = running();
    plane.notices.push_back(CaptureNotice {
        closed: true,
        stop_reason: Some("source_lost".into()),
        ..CaptureNotice::default()
    });
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_ne!(pipeline.streams[&1].stage, Stage::Running);
    assert_eq!(pipeline.streams[&1].error, Some("capture_source_stopped"));
    assert!(plane.swap_decisions.is_empty());
}

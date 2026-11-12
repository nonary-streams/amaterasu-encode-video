use super::*;

fn identified(operation_id: &str) -> Operation {
    let mut operation = open();
    if let Operation::Open {
        operation_id: identity,
        ..
    } = &mut operation
    {
        *identity = Some(operation_id.into());
    }
    operation
}

fn identified_wgc(operation_id: &str, smoothing: bool) -> Operation {
    let mut operation = identified(operation_id);
    if let Operation::Open {
        capture_provider,
        wgc_pacing_smoothing,
        ..
    } = &mut operation
    {
        *capture_provider = CaptureProvider::Wgc;
        *wgc_pacing_smoothing = smoothing;
    }
    operation
}

fn invoke(
    pipeline: &mut Pipeline,
    runtime: &mut Runtime,
    caller: &str,
    request: u64,
    operation: Operation,
) {
    pipeline.handle(caller, request, operation, &mut |command| {
        runtime.send(command)
    });
}

#[test]
fn opening_retry_and_lookup_preserve_original_parent_even_when_reply_is_lost() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        99,
        identified("launch-1"),
    );
    runtime.lost_complete = true;
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        100,
        identified("launch-1"),
    );
    assert_eq!(runtime.completed.last().unwrap().1["state"], "opening");
    assert_eq!(pipeline.streams[&1].parent, Some(99));
    assert_eq!(pipeline.next_id, 1);
    runtime.lost_complete = false;
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        101,
        Operation::Lookup {
            operation_id: "launch-1".into(),
        },
    );
    assert_eq!(runtime.completed.last().unwrap().1["stream"], 1);
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(runtime.open_effects, 2);
    assert!(pipeline.streams[&1].stage == Stage::Running);
    runtime.lost_complete = true;
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        102,
        identified("launch-1"),
    );
    assert!(pipeline.streams[&1].stage == Stage::Running);
    assert!(pipeline.streams[&1].ending_parent.is_none());
}

#[test]
fn operation_scope_conflicts_and_closed_tombstones_do_not_reallocate() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        99,
        identified("launch-1"),
    );
    let mut changed = identified("launch-1");
    if let Operation::Open { configuration, .. } = &mut changed {
        configuration.fps = 30;
    }
    invoke(&mut pipeline, &mut runtime, "alice", 100, changed);
    assert_eq!(
        runtime.completed.last().unwrap().1["code"],
        "operation_conflict"
    );
    invoke(
        &mut pipeline,
        &mut runtime,
        "bob",
        101,
        Operation::Lookup {
            operation_id: "launch-1".into(),
        },
    );
    assert_eq!(runtime.completed.last().unwrap().1["code"], "not_found");
    invoke(
        &mut pipeline,
        &mut runtime,
        "bob",
        102,
        identified("launch-1"),
    );
    assert_eq!(pipeline.streams.len(), 2);
    assert_eq!(pipeline.streams[&2].owner, "bob");
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        103,
        Operation::Close { stream: 1 },
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 24);
    assert!(!pipeline.streams.contains_key(&1));
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        104,
        identified("launch-1"),
    );
    assert_eq!(runtime.completed.last().unwrap().1["code"], "stream_closed");
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        105,
        Operation::Lookup {
            operation_id: "launch-1".into(),
        },
    );
    assert_eq!(runtime.completed.last().unwrap().1["code"], "not_found");
    assert_eq!(pipeline.next_id, 2);
}

#[test]
fn losing_original_completion_cleans_resources_even_after_successful_opening_retry() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        99,
        identified("launch-1"),
    );
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        100,
        identified("launch-1"),
    );
    runtime.lost_complete = true;
    tick(&mut pipeline, &mut runtime, &mut plane, 14);
    assert!(pipeline.streams.is_empty());
    assert_eq!(runtime.open_effects, 2);
    let closes = runtime.commands.iter().filter(|command| matches!(command, Command::Request {payload,..} if payload["action"] == "close")).count();
    assert_eq!(closes, 2);
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        101,
        identified("launch-1"),
    );
    assert_eq!(runtime.completed.last().unwrap().1["code"], "stream_closed");
}

#[test]
fn operation_history_is_bounded_and_bad_identifiers_never_allocate() {
    let (mut pipeline, mut runtime, _) = setup();
    for id in ["", "with space", "line\nfeed", "é", &"x".repeat(129)] {
        invoke(&mut pipeline, &mut runtime, "alice", 99, identified(id));
        assert_eq!(
            runtime.completed.last().unwrap().1["code"],
            "invalid_operation_id"
        );
        assert!(pipeline.streams.is_empty());
    }
    for index in 0..128 {
        invoke(
            &mut pipeline,
            &mut runtime,
            "alice",
            index + 100,
            identified(&format!("op-{index}")),
        );
        // Simulate completed cleanup; retained history, not active streams, is under test.
        pipeline.streams.clear();
    }
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        300,
        identified("next"),
    );
    assert_eq!(
        runtime.completed.last().unwrap().1["code"],
        "operation_limit"
    );
    assert_eq!(pipeline.operations.len(), 128);
    invoke(&mut pipeline, &mut runtime, "alice", 301, open());
    assert_eq!(pipeline.streams.len(), 1);
}

#[test]
fn packet_boundary_mode_is_forwarded_confirmed_and_part_of_retry_identity() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = identified("boundaries");
    if let Operation::Open {
        packet_boundaries, ..
    } = &mut request
    {
        *packet_boundaries = true;
    }
    invoke(&mut pipeline, &mut runtime, "alice", 99, request);
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(
        runtime.completed.last().unwrap().1["packet_boundaries"],
        true
    );
    assert!(runtime.commands.iter().any(|command| matches!(command, Command::Request {service,payload,..} if service == ENCODE && payload["action"] == "open" && payload["packet_boundaries"] == true)));
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        100,
        identified("boundaries"),
    );
    assert_eq!(
        runtime.completed.last().unwrap().1["code"],
        "operation_conflict"
    );
}

#[test]
fn opted_in_video_does_not_activate_an_encoder_without_boundary_confirmation() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    let mut request = identified("boundaries");
    if let Operation::Open {
        packet_boundaries, ..
    } = &mut request
    {
        *packet_boundaries = true;
    }
    invoke(&mut pipeline, &mut runtime, "alice", 99, request);
    tick(&mut pipeline, &mut runtime, &mut plane, 3);
    for payload in runtime.pending.values_mut() {
        payload.as_object_mut().unwrap().remove("packet_boundaries");
    }
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(
        pipeline.streams[&1].error,
        Some("packet_boundaries_unavailable")
    );
    assert_ne!(pipeline.streams[&1].stage as u8, Stage::Running as u8);
    tick(&mut pipeline, &mut runtime, &mut plane, 10);
    assert!(pipeline.streams.is_empty());
}

#[test]
fn wgc_pacing_smoothing_is_snapshotted_and_part_of_open_retry_identity() {
    let (mut pipeline, mut runtime, _) = setup_with_providers([CaptureProvider::Wgc].into());

    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        99,
        identified_wgc("wgc-smoothing", false),
    );
    assert_eq!(pipeline.next_id, 1);
    assert_eq!(pipeline.streams.len(), 1);
    assert_eq!(pipeline.streams[&1].provider, CaptureProvider::Wgc);
    assert!(!pipeline.streams[&1].wgc_pacing_smoothing);

    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        100,
        identified_wgc("wgc-smoothing", false),
    );
    let retry_response = &runtime.completed.last().unwrap().1;
    assert_eq!(retry_response["stream"], 1);
    assert_eq!(retry_response["wgc_pacing_smoothing"], false);
    assert_eq!(pipeline.next_id, 1);
    assert_eq!(pipeline.streams.len(), 1);
    assert!(!pipeline.streams[&1].wgc_pacing_smoothing);

    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        101,
        identified_wgc("wgc-smoothing", true),
    );
    assert_eq!(
        runtime.completed.last().unwrap().1["code"],
        "operation_conflict"
    );
    assert_eq!(pipeline.next_id, 1);
    assert_eq!(pipeline.streams.len(), 1);
    assert!(!pipeline.streams[&1].wgc_pacing_smoothing);

    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        102,
        identified_wgc("wgc-smoothing-enabled", true),
    );
    invoke(
        &mut pipeline,
        &mut runtime,
        "alice",
        103,
        identified_wgc("wgc-smoothing-enabled", true),
    );
    let second_response = &runtime.completed.last().unwrap().1;
    assert_eq!(second_response["stream"], 2);
    assert_eq!(second_response["wgc_pacing_smoothing"], true);
    assert_eq!(pipeline.next_id, 2);
    assert_eq!(pipeline.streams.len(), 2);
    assert!(!pipeline.streams[&1].wgc_pacing_smoothing);
    assert!(pipeline.streams[&2].wgc_pacing_smoothing);
}

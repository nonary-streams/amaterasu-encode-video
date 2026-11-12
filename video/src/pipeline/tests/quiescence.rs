use super::*;

#[test]
fn legacy_quiesce_waits_for_encoder_close_before_entering_native_retirement() {
    use std::{cell::Cell, rc::Rc};
    struct RetirementPlane {
        encoder_closed: Rc<Cell<bool>>,
        native_retired: Rc<Cell<bool>>,
    }
    impl Plane for RetirementPlane {
        fn verify_geometry(
            &mut self,
            _: CaptureProvider,
            _: u64,
            _: Option<CaptureGeometry>,
        ) -> Result<GeometryProgress, &'static str> {
            Ok(GeometryProgress::Ready)
        }
        fn pump(&mut self, _: Pump<'_>) -> Result<Progress, &'static str> {
            Ok(Progress::default())
        }
        fn drain(&mut self, _: u64) -> Result<bool, &'static str> {
            Ok(true)
        }
        fn finish_stream(&mut self, _: u64) {
            assert!(
                self.encoder_closed.get(),
                "a synchronous native barrier cannot precede asynchronous encoder Close"
            );
            self.native_retired.set(true);
        }
    }
    let (mut pipeline, mut runtime, _) = setup();
    let encoder_closed = Rc::new(Cell::new(false));
    let native_retired = Rc::new(Cell::new(false));
    let mut plane = RetirementPlane {
        encoder_closed: encoder_closed.clone(),
        native_retired: native_retired.clone(),
    };
    let mut encoder_close = None;
    let mut send = |command: Command| {
        if matches!(&command, Command::Request { service, payload, .. }
            if service == CAPTURE && payload["action"] == "close")
        {
            assert!(
                native_retired.get(),
                "capture ownership must survive native lease retirement"
            );
        }
        let close_request = matches!(&command, Command::Request { service, payload, .. }
            if service == ENCODE && payload["action"] == "close");
        let close_result =
            matches!(&command, Command::TakeResult { request } if Some(*request) == encoder_close);
        let response = runtime.send(command)?;
        if close_request && let Response::Accepted { id } = &response {
            encoder_close = Some(*id);
        }
        if close_result
            && let Response::Result { payload } = &response
            && payload["ok"] == true
        {
            encoder_closed.set(true);
        }
        Ok(response)
    };
    pipeline.handle("alice", 91, open(), &mut send);
    for _ in 0..4 {
        pipeline.tick(&mut plane, &mut send);
    }
    assert_eq!(pipeline.streams[&1].stage, Stage::Running);
    assert!(!pipeline.quiesce(&mut plane, &mut send).unwrap());
    assert!(!native_retired.get());
    for _ in 0..20 {
        pipeline.tick(&mut plane, &mut send);
    }
    assert!(pipeline.quiesce(&mut plane, &mut send).unwrap());
    assert!(native_retired.get());
}

#[test]
fn source_activation_keeps_its_native_connection_while_packet_queue_is_borrowed() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.source_encoders.insert(
        "fixture".into(),
        crate::model::SourceEncoderRegistration {
            control_service: "example.encode/1".into(),
            native_service: "example.encode-provider/1".into(),
            source_ids: [42].into(),
        },
    );
    pipeline.handle(
        "alice",
        91,
        Operation::OpenSource {
            packet_boundaries: true,
            operation_id: Some("borrowed-source".into()),
            source_id: 42,
            encoder_options: None,
            encoder_provider: "fixture".into(),
            encoder: EncoderSelection::Identity(encoder_identity()),
            configuration: Box::new(open_configuration()),
        },
        &mut |command| runtime.send(command),
    );
    let queues = pipeline.queues.clone();
    let borrowed = queues.lock().unwrap();
    tick(&mut pipeline, &mut runtime, &mut plane, 5);
    assert_eq!(pipeline.streams[&1].stage, Stage::Activating);
    assert_eq!(plane.source_starts, [(1, 51, 42)]);
    // The first release precedes Encoder admission. None may occur after the
    // source has started while activation waits for another packet borrower.
    let releases = plane.retired_events.len();
    tick(&mut pipeline, &mut runtime, &mut plane, 3);
    assert_eq!(plane.retired_events.len(), releases);
    drop(borrowed);
    tick(&mut pipeline, &mut runtime, &mut plane, 2);
    assert_eq!(pipeline.streams[&1].stage, Stage::Running);
    assert_eq!(plane.source_starts, [(1, 51, 42)]);
    assert!(!plane.source_pumps.is_empty());
}

#[test]
fn busy_packet_queue_does_not_postpone_the_drain_deadline() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 91, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    pipeline.handle(
        "alice",
        92,
        Operation::Drain { stream: 1 },
        &mut |command| runtime.send(command),
    );
    output::expire(pipeline.streams.get_mut(&1).unwrap());
    let queues = pipeline.queues.clone();
    let borrowed = queues.lock().unwrap();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].stage, Stage::FindEncoder);
    assert_eq!(pipeline.streams[&1].error, Some("drain_timeout"));
    drop(borrowed);
}

#[test]
fn held_packet_queue_keeps_close_and_shutdown_ticks_nonblocking() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 91, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    let queues = pipeline.queues.clone();
    let (entered, borrowed) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let borrower = std::thread::spawn(move || {
        let _guard = queues.lock().unwrap();
        entered.send(()).unwrap();
        // Finite fixture ownership: the baseline blocking lock must also exit.
        let _ = released.recv_timeout(Duration::from_millis(250));
    });
    borrowed.recv().unwrap();
    let started = Instant::now();
    pipeline.handle(
        "alice",
        92,
        Operation::Close { stream: 1 },
        &mut |command| runtime.send(command),
    );
    let initially_ready = pipeline
        .quiesce(&mut plane, &mut |command| runtime.send(command))
        .unwrap();
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    let ready_while_borrowed = pipeline
        .quiesce(&mut plane, &mut |command| runtime.send(command))
        .unwrap();
    assert!(pipeline.streams[&1].cleanup_complete);
    let elapsed = started.elapsed();
    let _ = release.send(());
    borrower.join().unwrap();
    assert!(
        elapsed < Duration::from_millis(100),
        "packet callback blocked cleanup for {elapsed:?}"
    );
    assert!(!initially_ready && !ready_while_borrowed);
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert!(
        pipeline
            .quiesce(&mut plane, &mut |command| runtime.send(command))
            .unwrap()
    );
}

#[test]
fn observation_parent_must_end_before_shutdown_can_forget_its_child() {
    let (mut pipeline, mut runtime, mut plane) =
        owned_capture::owned_setup([CaptureProvider::Pipewire].into());
    owned_capture::start(&mut pipeline, &mut runtime, &mut plane);
    pipeline.observations.begin(
        "alice",
        91,
        1,
        &pipeline.epoch,
        &pipeline.streams,
        &mut |command| runtime.send(command),
    );
    runtime.parent_failures = 1;
    assert!(
        !pipeline.observations.quiesce(&mut |command| match command {
            Command::CheckRequest { .. } => Err("lost check reply".into()),
            command => runtime.send(command),
        })
    );
    assert!(!runtime.ended.contains(&91));
    assert!(
        pipeline
            .observations
            .quiesce(&mut |command| runtime.send(command))
    );
    assert!(runtime.ended.contains(&91));
}

#[test]
fn portal_selection_releases_provider_result_before_completing_parent() {
    let mut control = portal::Control::default();
    let mut runtime = Runtime::default();
    control.select(
        "alice",
        91,
        crate::model::PortalSourceType::Monitor,
        "fixture-selection".into(),
        true,
        &mut |command| runtime.send(command),
    );
    control.tick(&mut |command| runtime.send(command));
    let released = runtime
        .commands
        .iter()
        .position(|command| matches!(command, Command::ReleaseResult { request: 1 }))
        .expect("provider result must be released");
    let completed = runtime
        .commands
        .iter()
        .position(|command| matches!(command, Command::Complete { request: 91, .. }))
        .unwrap();
    assert!(released < completed);
}

#[test]
fn announcement_keeps_active_frames_until_quiesce_retires_each_owned_stream() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    for (owner, request) in [("alice", 91), ("bob", 92)] {
        pipeline.handle(owner, request, open(), &mut |command| runtime.send(command));
    }
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert!(
        pipeline
            .streams
            .values()
            .all(|stream| stream.stage == Stage::Running)
    );
    let before = plane.calls;
    pipeline.prepare_shutdown(5_000);
    tick(&mut pipeline, &mut runtime, &mut plane, 2);
    assert!(plane.calls > before);
    assert!(plane.finished_streams.is_empty());
    assert!(
        !pipeline
            .quiesce(&mut plane, &mut |command| runtime.send(command))
            .unwrap()
    );
    assert!(plane.finished_streams.is_empty());
    tick(&mut pipeline, &mut runtime, &mut plane, 24);
    assert!(
        pipeline
            .quiesce(&mut plane, &mut |command| runtime.send(command))
            .unwrap()
    );
    assert!(pipeline.streams.is_empty());
    assert!(plane.finished_streams.contains(&1));
    assert!(plane.finished_streams.contains(&2));
    assert_eq!(
        runtime
            .commands
            .iter()
            .filter(|command| {
                matches!(command, Command::Request { payload, .. } if payload["action"] == "close")
            })
            .count(),
        4
    );
}

#[test]
fn shutdown_does_not_retry_an_existing_failed_cleanup_or_extend_its_deadline() {
    let (mut pipeline, mut runtime, mut plane) = setup();
    pipeline.handle("alice", 91, open(), &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    runtime.fail_encoder_cleanup = true;
    pipeline.handle(
        "alice",
        92,
        Operation::Close { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 24);
    assert_eq!(pipeline.streams[&1].stage, Stage::Failed);
    let before = runtime.commands.len();
    pipeline.prepare_shutdown(0);
    let deadline = pipeline.shutdown_deadline;
    pipeline.prepare_shutdown(20_000);
    assert_eq!(pipeline.shutdown_deadline, deadline);
    assert!(
        pipeline
            .quiesce(&mut plane, &mut |command| runtime.send(command))
            .is_err()
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert!(
        pipeline
            .quiesce(&mut plane, &mut |command| runtime.send(command))
            .is_err()
    );
    assert_eq!(runtime.commands.len(), before);
    assert!(pipeline.streams.contains_key(&1));
}

#[test]
fn portal_release_failure_retains_result_and_blocks_shutdown_readiness() {
    let mut control = portal::Control::default();
    let mut runtime = Runtime::default();
    control.select(
        "alice",
        91,
        crate::model::PortalSourceType::Monitor,
        "fixture-selection".into(),
        true,
        &mut |command| runtime.send(command),
    );
    runtime.release_failures = 2;
    control.tick(&mut |command| runtime.send(command));
    assert!(runtime.completed.is_empty());
    assert!(!control.quiesce(&mut |command| runtime.send(command)));
    assert!(control.quiesce(&mut |command| runtime.send(command)));
    assert_eq!(runtime.commands.iter().filter(|command| {
        matches!(command, Command::Request { payload, .. } if payload["action"] == "select_source")
    }).count(), 1);
}

#[test]
fn lost_portal_admission_is_recovered_and_cancelled_under_the_original_operation() {
    let mut control = portal::Control::default();
    let mut runtime = Runtime {
        lose_accept: true,
        ..Runtime::default()
    };
    control.select(
        "alice",
        91,
        crate::model::PortalSourceType::Monitor,
        "fixture-selection".into(),
        true,
        &mut |command| runtime.send(command),
    );
    assert!(runtime.completed.is_empty());
    assert!(control.quiesce(&mut |command| runtime.send(command)));
    assert_eq!(runtime.operations.len(), 1);
    assert!(
        runtime
            .commands
            .iter()
            .any(|command| { matches!(command, Command::Cancel { request: 1 }) })
    );
    let ids: Vec<_> = runtime
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Request { operation_id, .. } => Some(operation_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1]);
}

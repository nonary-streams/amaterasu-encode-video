use super::*;
use crate::plane::FramePath;

const OWNED_OUTPUT: &str = "kwin:Virtual-9";
const NVENC_H264: &str = "amaterasu.hardware.nvenc-cuda.h264";

fn owned_geometry() -> CaptureGeometry {
    CaptureGeometry::new(
        DesktopRect::new(0, 0, 1920, 1080).unwrap(),
        DesktopRect::new(0, 0, 1920, 1080).unwrap(),
    )
    .unwrap()
}

fn nvenc_entry(clsid: &str, codec: &str, inputs: Option<Value>) -> Value {
    let mut entry = json!({
        "identity":{"clsid":clsid,"hardware_url":null,"friendly_name":clsid},
        "name":clsid,"codec":codec,"hardware":true
    });
    if let Some(inputs) = inputs {
        entry["inputs"] = inputs;
    }
    entry
}

fn registry(entries: Vec<Value>) -> Value {
    json!({"ok":true,"enabled":true,"encoders":entries})
}

fn dmabuf_registry() -> Value {
    registry(vec![nvenc_entry(
        NVENC_H264,
        "h264",
        Some(json!(["cpu", "dmabuf"])),
    )])
}

/// Pipeline, runtime and plane for an owned output whose vkms and encoder
/// tables both carry DMA-BUF tails.
pub(super) fn owned_setup(providers: BTreeSet<CaptureProvider>) -> (Pipeline, Runtime, FakePlane) {
    let (pipeline, mut runtime, mut plane) = setup_with_providers(providers);
    runtime.encoder_enumeration_override = Some(dmabuf_registry());
    plane.owned_geometry = Some(owned_geometry());
    plane.dmabuf_ready = true;
    (pipeline, runtime, plane)
}

pub(super) fn linux_providers() -> BTreeSet<CaptureProvider> {
    [CaptureProvider::Pipewire, CaptureProvider::Vkms].into()
}

fn owned_open(provider: CaptureProvider) -> Operation {
    serde_json::from_value(json!({
        "action":"open",
        "capture_provider":provider,
        "output":OWNED_OUTPUT,
        "encoder":"auto",
        "configuration":{"width":1920,"height":1080,"fps":60,"bitrate":5000000}
    }))
    .unwrap()
}

pub(super) fn start(pipeline: &mut Pipeline, runtime: &mut Runtime, plane: &mut FakePlane) {
    pipeline.handle(
        "alice",
        77,
        owned_open(CaptureProvider::Pipewire),
        &mut |command| runtime.send(command),
    );
    tick(pipeline, runtime, plane, 12);
}

fn capture_opens(runtime: &Runtime) -> Vec<(String, String)> {
    runtime
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Request {
                service,
                payload,
                operation_id,
                ..
            } if capture_provider_for_service(service).is_some() && payload["action"] == "open" => {
                Some((service.clone(), operation_id.clone()))
            }
            _ => None,
        })
        .collect()
}

fn requests(runtime: &Runtime, service: &str, action: &str) -> usize {
    runtime
        .commands
        .iter()
        .filter(|command| {
            matches!(command, Command::Request { service: requested, payload, .. }
                if requested == service && payload["action"] == action)
        })
        .count()
}

pub(super) fn status(pipeline: &Pipeline) -> Value {
    pipeline.streams[&1].response(1)
}

#[test]
fn owned_output_uses_dmabuf_only_when_capture_encoder_and_inputs_all_allow_it() {
    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    start(&mut pipeline, &mut runtime, &mut plane);
    let stream = &pipeline.streams[&1];
    assert!(matches!(stream.stage, Stage::Running));
    assert_eq!(stream.provider, CaptureProvider::Vkms);
    assert_eq!(stream.frame_path, FramePath::DmaBuf);
    assert_eq!(
        capture_opens(&runtime),
        [(
            CaptureProvider::Vkms.service().to_owned(),
            "video-test-1-capture-attempt-0".to_owned()
        )]
    );
    assert_eq!(plane.frame_paths.last(), Some(&FramePath::DmaBuf));
    let reply = status(&pipeline);
    assert_eq!(reply["frame_path"], "dmabuf");
    assert_eq!(reply["capture_provider"], "vkms");
    assert!(reply["vkms_fallback_reason"].is_null());
    assert!(reply["dmabuf_frames"]["fresh"].as_u64().unwrap() >= 1);

    for (dmabuf_ready, inputs, providers, reason) in [
        (
            false,
            Some(json!(["cpu", "dmabuf"])),
            linux_providers(),
            "capture_dmabuf_interface_unavailable",
        ),
        (
            true,
            None,
            linux_providers(),
            "encoder_dmabuf_input_unavailable",
        ),
        (
            true,
            Some(json!(["cpu"])),
            linux_providers(),
            "encoder_dmabuf_input_unavailable",
        ),
        (
            true,
            Some(json!(["cpu", "dmabuf"])),
            [CaptureProvider::Pipewire].into(),
            "capture_vkms_not_configured",
        ),
    ] {
        let (mut pipeline, mut runtime, mut plane) = owned_setup(providers);
        plane.dmabuf_ready = dmabuf_ready;
        runtime.encoder_enumeration_override =
            Some(registry(vec![nvenc_entry(NVENC_H264, "h264", inputs)]));
        start(&mut pipeline, &mut runtime, &mut plane);
        let stream = &pipeline.streams[&1];
        assert!(matches!(stream.stage, Stage::Running), "{reason}");
        assert_eq!(stream.provider, CaptureProvider::Pipewire, "{reason}");
        assert_eq!(stream.frame_path, FramePath::Cpu, "{reason}");
        assert_eq!(status(&pipeline)["vkms_fallback_reason"], reason);
        assert_eq!(
            requests(&runtime, CaptureProvider::Vkms.service(), "open"),
            0,
            "{reason}"
        );
        assert!(plane.frame_paths.iter().all(|path| *path == FramePath::Cpu));
    }
}

#[test]
fn required_frame_lease_prevents_owned_output_cpu_fallbacks() {
    for fail_at_open in [false, true] {
        let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
        pipeline.require_frame_lease = true;
        if fail_at_open {
            runtime
                .capture_open_errors
                .insert(CaptureProvider::Vkms, "lease_source_unavailable");
        } else {
            plane.dmabuf_ready = false;
        }
        start(&mut pipeline, &mut runtime, &mut plane);
        tick(&mut pipeline, &mut runtime, &mut plane, 16);
        assert!(pipeline.streams.is_empty());
        assert!(
            capture_opens(&runtime)
                .iter()
                .all(|(service, _)| service != CaptureProvider::Pipewire.service())
        );
        assert!(!plane.frame_paths.contains(&FramePath::Cpu));
    }
}

#[test]
fn required_frame_lease_prevents_runtime_cpu_fallback() {
    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    pipeline.require_frame_lease = true;
    start(&mut pipeline, &mut runtime, &mut plane);
    assert_eq!(status(&pipeline)["frame_lease_required"], true);
    plane.dmabuf_fallback_once = Some("capture_dmabuf_lease_unsupported");
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 24);
    assert!(pipeline.streams.is_empty());
    assert!(!plane.frame_paths.contains(&FramePath::Cpu));
    assert!(
        capture_opens(&runtime)
            .iter()
            .all(|(service, _)| service != CaptureProvider::Pipewire.service())
    );
}

#[test]
fn explicit_encoder_identity_keeps_cpu_capture_because_inputs_are_unknown() {
    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    let mut request = owned_open(CaptureProvider::Pipewire);
    let Operation::Open { encoder, .. } = &mut request else {
        unreachable!();
    };
    *encoder = EncoderSelection::Identity(encoder_identity());
    pipeline.handle("alice", 77, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert_eq!(pipeline.streams[&1].provider, CaptureProvider::Pipewire);
    assert_eq!(
        status(&pipeline)["vkms_fallback_reason"],
        "encoder_inputs_unknown"
    );
}

#[test]
fn vkms_refusal_reopens_the_owned_output_through_pipewire_with_reason() {
    for refusal in ["open_reply", "admission"] {
        let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
        if refusal == "open_reply" {
            runtime
                .capture_open_errors
                .insert(CaptureProvider::Vkms, "frame_helper_unavailable");
        } else {
            runtime
                .capture_admission_errors
                .insert(CaptureProvider::Vkms, "permission_denied");
        }
        start(&mut pipeline, &mut runtime, &mut plane);
        let stream = &pipeline.streams[&1];
        assert!(matches!(stream.stage, Stage::Running), "{refusal}");
        assert_eq!(stream.provider, CaptureProvider::Pipewire);
        assert_eq!(stream.frame_path, FramePath::Cpu);
        let opens = capture_opens(&runtime);
        assert_eq!(opens.len(), 2, "{refusal}");
        assert_eq!(opens[0].0, CaptureProvider::Vkms.service());
        assert_eq!(opens[1].0, CaptureProvider::Pipewire.service());
        assert_ne!(opens[0].1, opens[1].1);
        assert_eq!(
            status(&pipeline)["vkms_fallback_reason"],
            if refusal == "open_reply" {
                "frame_helper_unavailable"
            } else {
                "permission_denied"
            }
        );
        assert_eq!(runtime.completed[0].1["capture_provider"], "pipewire");
        assert!(plane.frame_paths.iter().all(|path| *path == FramePath::Cpu));
    }
}

#[test]
fn runtime_dmabuf_unsupported_reopens_pipewire_once_and_keeps_the_encoder() {
    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    start(&mut pipeline, &mut runtime, &mut plane);
    assert_eq!(pipeline.streams[&1].frame_path, FramePath::DmaBuf);
    pipeline.streams.get_mut(&1).unwrap().force_idr = false;

    plane.dmabuf_fallback_once = Some("encoder_dmabuf_unsupported");
    pipeline.streams.get_mut(&1).unwrap().next_frame = Instant::now();
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    let stream = &pipeline.streams[&1];
    assert!(matches!(stream.stage, Stage::ReplaceCapture));
    assert_eq!(stream.frame_path, FramePath::Cpu);
    assert!(pipeline.queues.lock().unwrap().geometry("alice", 1).is_ok());

    tick(&mut pipeline, &mut runtime, &mut plane, 6);
    let stream = &pipeline.streams[&1];
    assert!(matches!(stream.stage, Stage::Running));
    assert_eq!(stream.provider, CaptureProvider::Pipewire);
    assert_eq!(
        status(&pipeline)["vkms_fallback_reason"],
        "encoder_dmabuf_unsupported"
    );
    assert_eq!(
        requests(&runtime, CaptureProvider::Vkms.service(), "close"),
        1
    );
    assert_eq!(requests(&runtime, ENCODE, "open"), 1);
    assert_eq!(requests(&runtime, ENCODE, "close"), 0);
    let opens = capture_opens(&runtime);
    assert_eq!(opens.len(), 2);
    assert_eq!(opens[1].0, CaptureProvider::Pipewire.service());
    assert!(pipeline.queues.lock().unwrap().geometry("alice", 1).is_ok());
    assert_eq!(plane.frame_paths.last(), Some(&FramePath::Cpu));
    // The first CPU frame after the switch is a forced keyframe.
    let first_cpu_input = plane
        .frame_paths
        .iter()
        .rposition(|path| *path == FramePath::DmaBuf)
        .unwrap()
        + 1;
    assert!(plane.inputs[first_cpu_input].2);

    // A second unsupported report cannot occur on the CPU path; a later
    // failure follows the normal cleanup rules.
    assert!(!crate::pipeline::owned_capture::begin_cpu_capture_fallback(
        pipeline.streams.get_mut(&1).unwrap(),
        "capture_dmabuf_unsupported"
    ));
}

#[test]
fn pinned_vkms_fails_with_the_reason_instead_of_falling_back() {
    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    plane.dmabuf_ready = false;
    pipeline.handle(
        "alice",
        77,
        owned_open(CaptureProvider::Vkms),
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert!(pipeline.streams.is_empty());
    assert_eq!(
        pipeline.recent[0].1["error"],
        "capture_dmabuf_interface_unavailable"
    );
    assert!(capture_opens(&runtime).is_empty());

    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    plane.dmabuf_fallback_once = Some("capture_dmabuf_unsupported");
    pipeline.handle(
        "alice",
        77,
        owned_open(CaptureProvider::Vkms),
        &mut |command| runtime.send(command),
    );
    // Auto probing adds selection round trips before normal cleanup.
    tick(&mut pipeline, &mut runtime, &mut plane, 24);
    assert!(pipeline.streams.is_empty());
    assert_eq!(pipeline.recent[0].1["error"], "capture_dmabuf_unsupported");
    assert_eq!(
        requests(&runtime, CaptureProvider::Vkms.service(), "close"),
        1
    );
    assert_eq!(requests(&runtime, ENCODE, "close"), 1);
}

#[test]
fn dmabuf_repeats_reread_without_owned_pixels() {
    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    start(&mut pipeline, &mut runtime, &mut plane);
    let stream = pipeline.streams.get_mut(&1).unwrap();
    assert!(stream.last_pixels.is_none());
    assert!(stream.has_repeat_source());
    stream.next_frame = Instant::now() + Duration::from_secs(60);
    stream.next_repeat_at = Some(Instant::now());
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    let input = plane.inputs.last().unwrap();
    assert!(!input.1, "capture is not due");
    assert!(input.3, "repeat is due");
    assert_eq!(plane.frame_paths.last(), Some(&FramePath::DmaBuf));
    assert!(
        status(&pipeline)["dmabuf_frames"]["reused"]
            .as_u64()
            .unwrap()
            >= 1
    );
}

#[test]
fn auto_selection_prefers_dmabuf_hardware_only_for_a_ready_owned_capture() {
    let payload = registry(vec![
        nvenc_entry("cpu-hardware", "h264", None),
        nvenc_entry(NVENC_H264, "h264", Some(json!(["cpu", "dmabuf"]))),
    ]);
    let configuration: Configuration = serde_json::from_value(json!({
        "width":1920,"height":1080,"fps":60,"bitrate":5000000
    }))
    .unwrap();
    let preferred = resolve_encoder_selection(None, &configuration, &payload, true).unwrap();
    assert_eq!(preferred.identity.clsid, NVENC_H264);
    assert!(preferred.accepts_dmabuf);
    let ordinary = resolve_encoder_selection(None, &configuration, &payload, false).unwrap();
    assert_eq!(ordinary.identity.clsid, "cpu-hardware");
    assert!(!ordinary.accepts_dmabuf);

    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    runtime.encoder_enumeration_override = Some(payload.clone());
    start(&mut pipeline, &mut runtime, &mut plane);
    assert_eq!(
        pipeline.streams[&1].encoder.as_ref().unwrap().clsid,
        NVENC_H264
    );
    assert_eq!(pipeline.streams[&1].frame_path, FramePath::DmaBuf);

    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    runtime.encoder_enumeration_override = Some(payload);
    plane.dmabuf_ready = false;
    start(&mut pipeline, &mut runtime, &mut plane);
    assert_eq!(
        pipeline.streams[&1].encoder.as_ref().unwrap().clsid,
        "cpu-hardware"
    );
}

#[test]
fn encoder_inputs_are_parsed_tolerantly() {
    for (inputs, accepts) in [
        (None, false),
        (Some(json!("dmabuf")), false),
        (Some(json!([])), false),
        (Some(json!(["cpu", 5, "future-kind"])), false),
        (Some(json!([5, "dmabuf"])), true),
    ] {
        let entry = nvenc_entry(NVENC_H264, "h264", inputs.clone());
        assert_eq!(encoder_accepts_dmabuf(&entry), accepts, "{inputs:?}");
    }
}

#[test]
fn av1_selects_hardware_and_never_falls_back_to_software() {
    let mut configuration: Configuration = serde_json::from_value(json!({
        "width":1920,"height":1080,"fps":60,"bitrate":5000000,"codec":"av1"
    }))
    .unwrap();
    let x264 = json!({
        "identity":{"clsid":SOFTWARE_ENCODER_ID,"hardware_url":null,"friendly_name":"x264"},
        "name":"x264","codec":"h264","hardware":false
    });
    let av1 = registry(vec![
        x264.clone(),
        nvenc_entry(NVENC_H264, "h264", Some(json!(["cpu", "dmabuf"]))),
        nvenc_entry(
            "amaterasu.hardware.nvenc-cuda.av1",
            "av1",
            Some(json!(["cpu", "dmabuf"])),
        ),
    ]);
    let choice = resolve_encoder_selection(None, &configuration, &av1, true).unwrap();
    assert_eq!(choice.identity.clsid, "amaterasu.hardware.nvenc-cuda.av1");
    assert!(x264_fallback_for_auto(None, &configuration, &choice.identity, &av1).is_none());

    let software_only = registry(vec![x264]);
    assert!(matches!(
        resolve_encoder_selection(None, &configuration, &software_only, false),
        Err("av1_encoder_unavailable")
    ));
    // An AV1 identity can never encode another codec.
    configuration.codec = amaterasu_media::VideoCodec::H264;
    assert!(!encoder_supports_configuration(
        &choice.identity,
        &configuration
    ));

    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    runtime.encoder_enumeration_override = Some(software_only);
    let mut request = owned_open(CaptureProvider::Pipewire);
    let Operation::Open { configuration, .. } = &mut request else {
        unreachable!();
    };
    configuration.codec = amaterasu_media::VideoCodec::Av1;
    pipeline.handle("alice", 77, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert!(pipeline.streams.is_empty());
    assert_eq!(pipeline.recent[0].1["error"], "av1_encoder_unavailable");
    assert!(capture_opens(&runtime).is_empty());
}

#[test]
fn owned_outputs_past_the_first_connector_pool_skip_portal_selection() {
    let (mut pipeline, mut runtime, mut plane) =
        setup_with_providers([CaptureProvider::Pipewire].into());
    plane.owned_geometry = Some(owned_geometry());
    let mut request = owned_open(CaptureProvider::Pipewire);
    let Operation::Open { output, .. } = &mut request else {
        unreachable!();
    };
    *output = "kwin:Virtual-16".into();
    pipeline.handle("alice", 77, request, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 12);
    assert!(matches!(pipeline.streams[&1].stage, Stage::Running));
    assert_eq!(pipeline.streams[&1].capture_output, "kwin:Virtual-16");
}

#[test]
fn hevc_and_av1_with_multiple_references_keep_dmabuf_hardware_and_probe_codec() {
    for (codec, media_codec) in [("hevc", VideoCodec::Hevc), ("av1", VideoCodec::Av1)] {
        let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
        let clsid = format!("{NVENC_CUDA_ENCODER_PREFIX}{codec}");
        runtime.encoder_enumeration_override = Some(registry(vec![nvenc_entry(
            &clsid,
            codec,
            Some(json!(["cpu", "dmabuf"])),
        )]));
        let mut operation = owned_open(CaptureProvider::Pipewire);
        let Operation::Open { configuration, .. } = &mut operation else {
            unreachable!()
        };
        configuration.codec = media_codec;
        configuration.max_reference_frames = 4;
        pipeline.handle("alice", 77, operation, &mut |command| runtime.send(command));
        tick(&mut pipeline, &mut runtime, &mut plane, 16);
        let stream = &pipeline.streams[&1];
        assert!(stream.stage == Stage::Running);
        assert_eq!(stream.frame_path, FramePath::DmaBuf);
        assert_eq!(stream.encoder.as_ref().unwrap().clsid, clsid);
        assert_eq!(requests(&runtime, ENCODE, "probe"), 1);
        let probe = runtime
            .commands
            .iter()
            .find_map(|command| match command {
                Command::Request {
                    service, payload, ..
                } if service == ENCODE && payload["action"] == "probe" => Some(payload),
                _ => None,
            })
            .unwrap();
        assert_eq!(probe["configuration"]["codec"], codec);
        assert_eq!(
            probe["configuration"]["video_stream_format"]["codec"],
            codec
        );
        assert_eq!(probe["configuration"]["max_reference_frames"], 4);
    }
}

#[test]
fn registered_dmabuf_provider_uses_its_own_binding_and_presentation_without_fallback() {
    for ready in [true, false] {
        let settings: Settings = serde_json::from_value(json!({
            "enabled": true,
            "capture_providers": [],
            "external_capture_providers": {"acme":{"service":"acme.capture/1", "frame_transport":"dmabuf"}}
        })).unwrap();
        assert!(settings.validate().is_ok());
        let mut pipeline = Pipeline::new(
            settings,
            "test".into(),
            Arc::new(Mutex::new(Queues::default())),
        );
        let mut runtime = Runtime {
            encoder_enumeration_override: Some(dmabuf_registry()),
            ..Runtime::default()
        };
        let mut plane = FakePlane {
            dmabuf_ready: ready,
            presentation_api: true,
            ..FakePlane::default()
        };
        let provider = serde_json::from_value(json!("acme")).unwrap();
        pipeline.handle("alice", 77, owned_open(provider), &mut |command| {
            runtime.send(command)
        });
        tick(&mut pipeline, &mut runtime, &mut plane, 20);
        if ready {
            let stream = &pipeline.streams[&1];
            assert!(matches!(stream.stage, Stage::Running));
            assert_eq!(stream.provider.service(), "acme.capture/1");
            assert_eq!(stream.frame_path, FramePath::DmaBuf);
            assert!(plane.frame_paths.contains(&FramePath::DmaBuf));
            assert!(
                plane
                    .inputs
                    .iter()
                    .any(|input| input.0.service() == "acme.capture/1")
            );
            assert!(plane.presentation_queries > 0);
            assert!(runtime.commands.iter().any(|command| matches!(command, Command::Request {service, payload, ..} if service == "acme.capture/1" && payload["action"] == "open")));
        } else {
            assert!(pipeline.streams.is_empty());
            assert_eq!(
                pipeline.recent[0].1["error"],
                "capture_dmabuf_interface_unavailable"
            );
            assert!(!runtime.commands.iter().any(|command| matches!(command, Command::Request {service, payload, ..} if service == "acme.capture/1" && payload["action"] == "open")));
        }
    }
}

#[test]
fn vkms_service_can_be_selected_by_a_registered_dmabuf_alias() {
    let settings: Settings = serde_json::from_value(json!({
        "enabled": true,
        "capture_providers": [],
        "external_capture_providers": {"virtual_screen":{"service":"amaterasu.capture-vkms/1", "frame_transport":"dmabuf"}}
    })).unwrap();
    assert!(settings.validate().is_ok());
    let mut pipeline = Pipeline::new(
        settings,
        "test".into(),
        Arc::new(Mutex::new(Queues::default())),
    );
    let mut runtime = Runtime {
        encoder_enumeration_override: Some(dmabuf_registry()),
        ..Runtime::default()
    };
    let mut plane = FakePlane {
        dmabuf_ready: true,
        presentation_api: true,
        ..FakePlane::default()
    };
    let provider = serde_json::from_value(json!("virtual_screen")).unwrap();
    pipeline.handle("alice", 77, owned_open(provider), &mut |command| {
        runtime.send(command)
    });
    tick(&mut pipeline, &mut runtime, &mut plane, 20);
    let stream = &pipeline.streams[&1];
    assert!(matches!(stream.stage, Stage::Running));
    assert_eq!(stream.provider.identity(), "virtual_screen");
    assert_eq!(stream.provider.service(), "amaterasu.capture-vkms/1");
    assert_eq!(stream.frame_path, FramePath::DmaBuf);
    assert!(
        plane
            .inputs
            .iter()
            .any(|input| input.0.identity() == "virtual_screen"
                && input.0.service() == "amaterasu.capture-vkms/1")
    );
    assert!(plane.presentation_queries > 0);
    assert!(runtime.commands.iter().any(|command| matches!(command, Command::Request {service, payload, ..} if service == "amaterasu.capture-vkms/1" && payload["action"] == "open")));
}

#[test]
fn explicit_encoder_identity_checks_dmabuf_capability_and_keeps_the_identity() {
    let entry = nvenc_entry(NVENC_H264, "h264", Some(json!(["cpu", "dmabuf"])));
    let identity: EncoderIdentity = serde_json::from_value(entry["identity"].clone()).unwrap();
    let cases = [
        (registry(vec![entry.clone()]), Some(true), None),
        (
            registry(vec![nvenc_entry(NVENC_H264, "h264", Some(json!(["cpu"])))]),
            Some(false),
            Some("encoder_dmabuf_input_unavailable"),
        ),
        (
            registry(vec![nvenc_entry(NVENC_H264, "h264", None)]),
            None,
            Some("encoder_inputs_unknown"),
        ),
        (
            registry(vec![nvenc_entry(
                NVENC_H264,
                "h264",
                Some(json!(["dmabuf", 1])),
            )]),
            None,
            Some("encoder_inputs_unknown"),
        ),
        (
            registry(vec![entry.clone(), entry.clone()]),
            None,
            Some("encoder_inputs_unknown"),
        ),
        (
            registry(vec![nvenc_entry(
                "amaterasu.other",
                "h264",
                Some(json!(["dmabuf"])),
            )]),
            None,
            Some("encoder_inputs_unknown"),
        ),
        (
            json!({"ok":true,"enabled":true,"encoders":"invalid"}),
            None,
            Some("encoder_inputs_unknown"),
        ),
    ];
    for (enumeration, capability, reason) in cases {
        for provider in [CaptureProvider::Vkms, CaptureProvider::Pipewire] {
            let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
            runtime.encoder_enumeration_override = Some(enumeration.clone());
            let mut operation = owned_open(provider);
            let Operation::Open { encoder, .. } = &mut operation else {
                unreachable!()
            };
            *encoder = EncoderSelection::Identity(identity.clone());
            pipeline.handle("alice", 77, operation, &mut |command| runtime.send(command));
            tick(&mut pipeline, &mut runtime, &mut plane, 25);
            if capability == Some(true) {
                let stream = &pipeline.streams[&1];
                assert_eq!(stream.provider, CaptureProvider::Vkms);
                assert_eq!(stream.encoder.as_ref(), Some(&identity));
                assert_eq!(stream.frame_path, FramePath::DmaBuf);
            } else if provider == CaptureProvider::Pipewire {
                let stream = &pipeline.streams[&1];
                assert_eq!(stream.provider, CaptureProvider::Pipewire);
                assert_eq!(stream.encoder.as_ref(), Some(&identity));
                assert_eq!(stream.response(1)["vkms_fallback_reason"], reason.unwrap());
            } else {
                assert!(pipeline.streams.is_empty());
                assert_eq!(pipeline.recent[0].1["error"], reason.unwrap());
            }
        }
    }
}

#[test]
fn external_dmabuf_alias_accepts_matching_explicit_encoder_identity() {
    let settings: Settings = serde_json::from_value(json!({
        "enabled": true, "capture_providers": [],
        "external_capture_providers": {"virtual_screen":{"service":"amaterasu.capture-vkms/1", "frame_transport":"dmabuf"}}
    })).unwrap();
    let mut pipeline = Pipeline::new(
        settings,
        "test".into(),
        Arc::new(Mutex::new(Queues::default())),
    );
    let mut runtime = Runtime {
        encoder_enumeration_override: Some(dmabuf_registry()),
        ..Runtime::default()
    };
    let mut plane = FakePlane {
        dmabuf_ready: true,
        ..FakePlane::default()
    };
    let provider = serde_json::from_value(json!("virtual_screen")).unwrap();
    let mut operation = owned_open(provider);
    let expected: EncoderIdentity = serde_json::from_value(
        nvenc_entry(NVENC_H264, "h264", Some(json!(["dmabuf"])))["identity"].clone(),
    )
    .unwrap();
    let Operation::Open { encoder, .. } = &mut operation else {
        unreachable!()
    };
    *encoder = EncoderSelection::Identity(expected.clone());
    pipeline.handle("alice", 77, operation, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 25);
    let stream = &pipeline.streams[&1];
    assert_eq!(stream.provider.identity(), "virtual_screen");
    assert_eq!(stream.encoder.as_ref(), Some(&expected));
    assert_eq!(stream.frame_path, FramePath::DmaBuf);
}

fn begin_explicit_encoder_inspection() -> (Pipeline, Runtime, FakePlane) {
    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    runtime.hold_encoder_enumeration = true;
    let mut operation = owned_open(CaptureProvider::Pipewire);
    let Operation::Open { encoder, .. } = &mut operation else {
        unreachable!()
    };
    *encoder = EncoderSelection::Identity(
        serde_json::from_value(
            nvenc_entry(NVENC_H264, "h264", Some(json!(["dmabuf"])))["identity"].clone(),
        )
        .unwrap(),
    );
    pipeline.handle("alice", 77, operation, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 1);
    assert_eq!(pipeline.streams[&1].stage, Stage::InspectEncoder);
    (pipeline, runtime, plane)
}

#[test]
fn timed_out_explicit_encoder_inspection_settles_before_pipewire_open() {
    let (mut pipeline, mut runtime, mut plane) = begin_explicit_encoder_inspection();
    let pending = pipeline.streams[&1].pending.unwrap();
    pipeline.streams.get_mut(&1).unwrap().opened_at = Instant::now() - Duration::from_secs(6);
    tick(&mut pipeline, &mut runtime, &mut plane, 15);
    assert_eq!(pipeline.streams[&1].provider, CaptureProvider::Pipewire);
    assert_eq!(
        pipeline.streams[&1].response(1)["vkms_fallback_reason"],
        "encoder_inputs_unknown"
    );
    let cancel = runtime
        .commands
        .iter()
        .position(|cmd| matches!(cmd, Command::Cancel {request} if *request == pending))
        .unwrap();
    let release = runtime
        .commands
        .iter()
        .position(|cmd| matches!(cmd, Command::ReleaseResult {request} if *request == pending))
        .unwrap();
    let open = runtime.commands.iter().position(|cmd| matches!(cmd, Command::Request {service, payload, ..} if service == CaptureProvider::Pipewire.service() && payload["action"] == "open")).unwrap();
    assert!(cancel < release && release < open);
}

#[test]
fn timed_out_explicit_encoder_inspection_cancellation_failure_blocks_open() {
    let (mut pipeline, mut runtime, mut plane) = begin_explicit_encoder_inspection();
    let pending = pipeline.streams[&1].pending.unwrap();
    runtime.cancel_failures = 3;
    pipeline.streams.get_mut(&1).unwrap().opened_at = Instant::now() - Duration::from_secs(6);
    tick(&mut pipeline, &mut runtime, &mut plane, 4);
    assert_eq!(pipeline.streams[&1].stage, Stage::Failed);
    assert_eq!(pipeline.streams[&1].unsettled, Some(pending));
    assert_eq!(
        pipeline.streams[&1].error,
        Some("request_cancellation_unconfirmed")
    );
    assert!(
        !runtime.commands.iter().any(
            |cmd| matches!(cmd, Command::Request {payload, ..} if payload["action"] == "open")
        )
    );
    pipeline.handle(
        "alice",
        78,
        Operation::Close { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(pipeline.streams.is_empty());
    assert!(
        !runtime.commands.iter().any(
            |cmd| matches!(cmd, Command::Request {payload, ..} if payload["action"] == "open")
        )
    );
}

#[test]
fn uncertain_explicit_encoder_inspection_waits_for_parent_end_before_retry() {
    let (mut pipeline, mut runtime, mut plane) = owned_setup(linux_providers());
    runtime.lose_accept = true;
    runtime.parent_failures = 3;
    let mut operation = owned_open(CaptureProvider::Pipewire);
    let Operation::Open { encoder, .. } = &mut operation else {
        unreachable!()
    };
    *encoder = EncoderSelection::Identity(
        serde_json::from_value(
            nvenc_entry(NVENC_H264, "h264", Some(json!(["dmabuf"])))["identity"].clone(),
        )
        .unwrap(),
    );
    pipeline.handle("alice", 77, operation, &mut |command| runtime.send(command));
    tick(&mut pipeline, &mut runtime, &mut plane, 5);
    assert_eq!(pipeline.streams[&1].stage, Stage::Failed);
    assert!(pipeline.streams[&1].uncertain.is_some());
    assert_eq!(
        runtime
            .commands
            .iter()
            .filter(|cmd| matches!(cmd, Command::Request { .. }))
            .count(),
        1
    );
    pipeline.handle(
        "alice",
        78,
        Operation::Close { stream: 1 },
        &mut |command| runtime.send(command),
    );
    tick(&mut pipeline, &mut runtime, &mut plane, 8);
    assert!(runtime.ended.contains(&77));
    assert!(pipeline.streams.is_empty());
    assert!(
        !runtime.commands.iter().any(
            |cmd| matches!(cmd, Command::Request {payload, ..} if payload["action"] == "open")
        )
    );
    let completion = runtime
        .commands
        .iter()
        .rposition(|cmd| matches!(cmd, Command::Complete { request: 77, .. }))
        .unwrap();
    let retry = runtime.commands.iter().rposition(|cmd| matches!(cmd, Command::Request {service, payload, ..} if service == ENCODE && payload["action"] == "enumerate")).unwrap();
    assert!(completion < retry);
}

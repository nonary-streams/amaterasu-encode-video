use super::*;
use crate::EncodeModule;
use amaterasu_media::*;
use amaterasu_module_sdk::{ManagedModule, Response, abi::*, encode::FormatId};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    path::{Path, PathBuf},
    sync::Mutex,
};

fn configuration() -> session::Configuration {
    session::Configuration {
        profile: session::Profile::Sdr8,
        width: 64,
        height: 48,
        fps: 60,
        bitrate: 100_000_000,
        clock_epoch: [7; 16],
        clock_origin_us: 1000,
    }
}
#[test]
fn pyrowave_configuration_rejects_unsupported_geometry_clocks_and_unbounded_frames() {
    let valid = configuration();
    assert!(valid.validate().is_ok());
    for field in ["width", "height", "fps", "bitrate"] {
        let mut bad = json!(valid);
        bad[field] = json!(0);
        assert!(
            serde_json::from_value::<session::Configuration>(bad)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let mut bad = valid.clone();
    bad.width = 65;
    assert!(bad.validate().is_err());
    bad = valid.clone();
    bad.clock_epoch = [0; 16];
    assert!(bad.validate().is_err());
    bad = valid;
    bad.fps = 1;
    bad.bitrate = 4_000_000_000;
    assert!(bad.validate().is_err());
}
#[test]
fn c_abi_layout_matches_the_pinned_header_on_64_bit_targets() {
    if size_of::<usize>() == 8 {
        assert_eq!(size_of::<library::CpuBuffer>(), 88);
        assert_eq!(std::mem::offset_of!(library::CpuBuffer, width), 72);
        assert_eq!(std::mem::offset_of!(library::CpuBuffer, format), 80);
    }
}

#[derive(Default)]
struct Endpoint {
    settings: Value,
    factories: BTreeMap<String, usize>,
    contracts: BTreeMap<String, String>,
    incoming: Option<Response>,
    completed: Option<Value>,
    reject_delivery: bool,
}
unsafe extern "C" fn dispatch(
    context: *mut c_void,
    bytes: BorrowedBytes,
    sink: ReplySink,
) -> Status {
    // SAFETY: the endpoint, bytes and synchronous response sink live for the call.
    let state = unsafe { &*context.cast::<Mutex<Endpoint>>() };
    let mut state = state.lock().unwrap();
    let request: Command = serde_json::from_slice(unsafe { bytes.read().unwrap() }).unwrap();
    let reply = match request {
        Command::GetSettings {} => Response::Result {
            payload: state.settings.clone(),
        },
        Command::Register { .. } | Command::CheckRequest { .. } => Response::Ok,
        Command::NativeRegisterTyped {
            service,
            contract,
            factory,
            ..
        } => {
            state.factories.insert(service.clone(), factory);
            state.contracts.insert(service, contract);
            Response::Ok
        }
        Command::Poll {} => state.incoming.take().unwrap_or(Response::Pending),
        Command::Complete { payload, .. } => {
            state.completed = Some(payload);
            if state.reject_delivery {
                Response::Pending
            } else {
                Response::Ok
            }
        }
        Command::Log { .. } => Response::Ok,
        _ => panic!("unexpected runtime request"),
    };
    // SAFETY: serialized response lives through the sink.
    unsafe { sink.send(&serde_json::to_vec(&reply).unwrap()) };
    STATUS_OK
}
fn request(
    state: &Mutex<Endpoint>,
    module: &mut EncodeModule,
    owner: &str,
    payload: Value,
) -> Value {
    state.lock().unwrap().incoming = Some(Response::Request {
        id: 1,
        caller: owner.into(),
        initiator: "untrusted-initiator".into(),
        service: CONTROL_SERVICE.into(),
        remaining_ms: 5000,
        payload,
    });
    module.tick().unwrap();
    state.lock().unwrap().completed.take().unwrap()
}
struct Binding(*const NativeInterface);
impl Drop for Binding {
    fn drop(&mut self) {
        // SAFETY: test owns exactly one binding and has retired its calls.
        unsafe {
            let api = &*self.0;
            api.release.unwrap()(api.context);
        }
    }
}
fn bind(state: &Mutex<Endpoint>, service: &str, owner: &str, size: u32) -> Binding {
    let address = state.lock().unwrap().factories[service];
    let query = NativeInterfaceQuery {
        header: AbiHeader::of::<NativeInterfaceQuery>(),
        interface_major: 1,
        min_minor: 0,
        min_size: size,
    };
    let mut output = std::ptr::null();
    // SAFETY: managed module pins the registered factory and context.
    unsafe {
        let factory = &*(address as *const NativeServiceFactory);
        assert_eq!(
            factory.bind.unwrap()(
                factory.context,
                BorrowedBytes::new(owner.as_bytes()),
                &query,
                &mut output
            ),
            STATUS_OK
        );
    }
    Binding(output)
}
fn native_id(value: FormatId) -> EncoderFormatId {
    EncoderFormatId {
        namespace_id: value.namespace_id,
        value: value.value,
        schema: value.schema,
    }
}
struct Received {
    bytes: Vec<u8>,
    flags: u32,
    sequence: u64,
    timestamp: u64,
    reject: bool,
}
unsafe extern "C" fn output(context: *mut c_void, chunk: *const EncoderOutputChunk) -> Status {
    // SAFETY: the SDK lends a valid chunk during receive; test context is live.
    let (received, chunk) = unsafe { (&mut *context.cast::<Received>(), &*chunk) };
    received.bytes = unsafe { chunk.bytes.read().unwrap() }.to_vec();
    received.flags = chunk.flags;
    received.sequence = chunk.submission_id;
    received.timestamp = chunk.timestamp_us;
    if received.reject {
        MEDIA_BUSY
    } else {
        STATUS_OK
    }
}
fn receive(api: &EncoderProviderApi, id: u64, received: &mut Received, max: usize) -> Status {
    // SAFETY: both binding and local sink remain live through receive.
    unsafe {
        (api.receive)(
            api.base.context,
            id,
            max,
            EncoderOutputSink {
                context: (received as *mut Received).cast(),
                write: output,
            },
        )
    }
}
fn none() -> FrameDescriptor {
    FrameDescriptor {
        kind: 0,
        version: 0,
        size: 0,
        reserved: 0,
        namespace: [0; 16],
        data: std::ptr::null(),
    }
}
unsafe extern "C" fn retain(_: *mut c_void) -> Status {
    panic!("synchronous consumer must not retain source input")
}
unsafe extern "C" fn release(_: *mut c_void, _: *const FrameDescriptor) {
    panic!("synchronous consumer must not release an unretained frame")
}

#[test]
#[ignore = "requires pinned PyroWave 0.6 library and a supported Vulkan GPU"]
fn registered_pyrowave_encoder_roundtrips_frames_and_retires_sessions() {
    let path = PathBuf::from(
        std::env::var_os("AMATERASU_PYROWAVE_LIBRARY")
            .expect("set AMATERASU_PYROWAVE_LIBRARY to the pinned native library"),
    );
    let decoder = library::Runtime::load(Path::new(&path)).unwrap();
    let state = Box::new(Mutex::new(Endpoint {
        settings: json!({"enabled":true,"pyrowave":{"library_path":path,"gpu_only":false}}),
        ..Default::default()
    }));
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (&*state as *const Mutex<Endpoint>).cast_mut().cast(),
        invoke: dispatch,
    };
    // SAFETY: boxed endpoint state outlives module, moved module and native bindings.
    let mut module =
        Box::new(EncodeModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap());
    let config = configuration();
    let enumeration = request(&state, &mut module, "video", json!({"action":"enumerate"}));
    assert_eq!(enumeration["profiles"][0]["bitstream"], "186f0393");
    assert_eq!(
        state.lock().unwrap().contracts[PROVIDER_SERVICE],
        amaterasu_module_sdk::media::native_contracts::ENCODER_PROVIDER
    );
    let probe = request(
        &state,
        &mut module,
        "video",
        json!({"action":"probe","configuration":config}),
    );
    assert_eq!(probe["usable"], true);
    let open = json!({"action":"open","operation_id":"stream-a","configuration":config});
    let reply = request(&state, &mut module, "video", open.clone());
    let id = reply["session_id"].as_u64().unwrap();
    assert_eq!(request(&state, &mut module, "video", open.clone()), reply);
    let mut conflict = open.clone();
    conflict["configuration"]["fps"] = json!(120);
    assert_eq!(
        request(&state, &mut module, "video", conflict)["code"],
        "operation_conflict"
    );
    let provider = bind(
        &state,
        PROVIDER_SERVICE,
        "video",
        ENCODER_PROVIDER_API_V1_0_SIZE,
    );
    let consumer = bind(
        &state,
        CONSUMER_SERVICE,
        "video",
        FRAME_RESOURCE_CONSUMER_API_V1_0_SIZE,
    );
    let stranger = bind(
        &state,
        PROVIDER_SERVICE,
        "other",
        ENCODER_PROVIDER_API_V1_0_SIZE,
    );
    // SAFETY: the runtime created these exact typed native tables.
    let (api, input, foreign) = unsafe {
        (
            &*provider.0.cast::<EncoderProviderApi>(),
            &*consumer.0.cast::<FrameResourceConsumerApi>(),
            &*stranger.0.cast::<EncoderProviderApi>(),
        )
    };
    let options = serde_json::to_vec(&config).unwrap();
    let mut native = EncoderSessionConfig {
        size: size_of::<EncoderSessionConfig>() as u32,
        mode: ENCODER_SESSION_SUBMITTED,
        codec: native_id(session::CODEC),
        framing: native_id(session::RECORDS),
        source_id: 6,
        config_generation: 1,
        options_schema: native_id(session::OPTIONS),
        options: BorrowedBytes::new(&options),
    };
    // SAFETY: descriptors/options remain borrowed for the synchronous calls.
    unsafe {
        assert_eq!(
            (foreign.configure_session)(foreign.base.context, id, &native),
            MEDIA_CLOSED
        );
        native.codec.schema += 1;
        assert_eq!(
            (api.configure_session)(api.base.context, id, &native),
            MEDIA_UNSUPPORTED
        );
        native.codec.schema -= 1;
        assert_eq!(
            (api.configure_session)(api.base.context, id, &native),
            STATUS_OK
        );
    }
    let caps = CpuResourceCapabilities {
        format: PIXEL_NV12,
        min_width: 64,
        max_width: 64,
        min_height: 48,
        max_height: 48,
    };
    let mut offer = CaptureResourceOffer {
        id: 10,
        ownership: FRAME_OWNERSHIP_BORROWED,
        accessibility: FRAME_ACCESS_CPU,
        reserved: 0,
        resource: FrameDescriptor {
            kind: FRAME_RESOURCE_CPU,
            version: 1,
            size: size_of::<CpuResourceCapabilities>() as u32,
            reserved: 0,
            namespace: [0; 16],
            data: (&caps as *const CpuResourceCapabilities).cast(),
        },
        acquire_sync: none(),
        completion_sync: none(),
    };
    // SAFETY: producer capability payload is readable for each configure call.
    unsafe {
        assert_eq!(
            (input.configure)(input.base.context, id, 1, 2, 9, &offer),
            STATUS_OK
        );
        offer.resource.version = 2;
        assert_eq!(
            (input.configure)(input.base.context, id, 1, 2, 9, &offer),
            MEDIA_UNSUPPORTED
        );
    }
    let (width, height) = (64, 48);
    let luma = (width * height) as usize;
    let mut pixels = vec![128u8; luma * 3 / 2];
    for y in 0..height {
        for x in 0..width {
            pixels[(y * width + x) as usize] = (40 + (x / 8) * 16 + (y / 8) * 8) as u8;
        }
    }
    let cpu = CpuFrame {
        width,
        height,
        format: PIXEL_NV12,
        stride: width,
        data: pixels.as_ptr(),
        len: pixels.len(),
        timestamp_us: 1100,
        sequence: 1,
    };
    let mut frame = CaptureResourceFrame {
        offer_generation: 9,
        offer_id: 10,
        timestamp_us: 1100,
        capture_pacing_timestamp_us: 1100,
        sequence: 1,
        width,
        height,
        freshness: 1,
        reserved: 0,
        color: FrameColorMetadata {
            version: 1,
            size: size_of::<FrameColorMetadata>() as u32,
            primaries: FRAME_PRIMARIES_BT709,
            transfer: FRAME_TRANSFER_SRGB,
            matrix: FRAME_MATRIX_BT709,
            range: FRAME_RANGE_FULL,
        },
        clock: FrameClockMetadata {
            version: 1,
            size: size_of::<FrameClockMetadata>() as u32,
            domain: FRAME_CLOCK_HOST_MONOTONIC,
            reserved: 0,
            epoch_id: [7; 16],
        },
        pacing_clock: Default::default(),
        resource: FrameDescriptor {
            kind: FRAME_RESOURCE_CPU,
            version: 1,
            size: size_of::<CpuFrame>() as u32,
            reserved: 0,
            namespace: [0; 16],
            data: (&cpu as *const CpuFrame).cast(),
        },
        acquire_sync: none(),
        context: std::ptr::null_mut(),
        retain,
        release,
    };
    // SAFETY: frame/CPU allocations and the binding remain live through submit.
    unsafe {
        assert_eq!(
            (input.submit)(input.base.context, id, &frame, 2),
            MEDIA_UNSUPPORTED
        );
        frame.clock.epoch_id = [8; 16];
        assert_eq!(
            (input.submit)(input.base.context, id, &frame, 0),
            MEDIA_UNSUPPORTED
        );
        frame.clock.epoch_id = [7; 16];
        assert_eq!(
            (input.submit)(
                input.base.context,
                id,
                &frame,
                FRAME_RESOURCE_SUBMIT_FORCE_IDR
            ),
            STATUS_OK
        );
        assert_eq!(
            (input.submit)(input.base.context, id, &frame, 0),
            MEDIA_BUSY
        );
    }
    let mut received = Received {
        bytes: Vec::new(),
        flags: 0,
        sequence: 0,
        timestamp: 0,
        reject: true,
    };
    assert_eq!(
        receive(foreign, id, &mut received, library::MAX_FRAME_BYTES),
        MEDIA_CLOSED
    );
    assert_eq!(
        receive(api, id, &mut received, library::MAX_FRAME_BYTES),
        MEDIA_BUSY
    );
    let first = received.bytes.clone();
    assert_eq!(received.sequence, 1);
    assert_eq!(received.timestamp, 100);
    assert!(received.flags & ENCODER_CHUNK_KEYFRAME != 0);
    assert_eq!(receive(api, id, &mut received, 1), MEDIA_BUSY);
    received.reject = false;
    assert_eq!(
        receive(api, id, &mut received, library::MAX_FRAME_BYTES),
        STATUS_OK
    );
    assert_eq!(received.bytes, first);
    let decoded = decoder.decode(&first, width, height).unwrap();
    assert!(
        decoded[..luma]
            .iter()
            .zip(&pixels[..luma])
            .all(|(a, b)| a.abs_diff(*b) <= 2)
    );
    assert!(decoded[luma..].iter().all(|value| value.abs_diff(128) <= 2));
    // Reusing an already accepted input is rejected after the output is consumed.
    unsafe {
        assert_eq!(
            (input.submit)(input.base.context, id, &frame, 0),
            MEDIA_UNSUPPORTED
        );
    }
    let bgra_caps = CpuResourceCapabilities {
        format: PIXEL_BGRA8,
        ..caps
    };
    offer.resource.version = 1;
    offer.resource.data = (&bgra_caps as *const CpuResourceCapabilities).cast();
    let bgra_pixels = [0, 0, 255, 255].repeat(luma);
    let bgra_cpu = CpuFrame {
        format: PIXEL_BGRA8,
        stride: width * 4,
        data: bgra_pixels.as_ptr(),
        len: bgra_pixels.len(),
        timestamp_us: 1200,
        sequence: 2,
        ..cpu
    };
    frame.resource.data = (&bgra_cpu as *const CpuFrame).cast();
    frame.color.matrix = FRAME_MATRIX_RGB;
    frame.sequence = 2;
    frame.timestamp_us = 1200;
    // SAFETY: BGRA input and exact producer capabilities are live through submit.
    unsafe {
        assert_eq!(
            (input.configure)(input.base.context, id, 1, 1, 9, &offer),
            STATUS_OK
        );
        assert_eq!((input.submit)(input.base.context, id, &frame, 0), STATUS_OK);
    }
    // Queued output remains owned after both source allocations are gone.
    drop(pixels);
    drop(bgra_pixels);
    // Drain retains the accepted picture before EOS and stops further admission.
    unsafe {
        assert_eq!((api.drain)(api.base.context, id), STATUS_OK);
        assert_eq!((api.drain)(api.base.context, id), STATUS_OK);
    }
    assert_eq!(
        receive(api, id, &mut received, library::MAX_FRAME_BYTES),
        STATUS_OK
    );
    assert_eq!(received.sequence, 2);
    assert_eq!(received.timestamp, 200);
    let decoded_red = decoder.decode(&received.bytes, width, height).unwrap();
    assert!(
        decoded_red[..luma]
            .iter()
            .all(|value| value.abs_diff(54) <= 2)
    );
    assert!(
        decoded_red[luma..luma + luma / 4]
            .iter()
            .all(|value| value.abs_diff(99) <= 2)
    );
    assert!(
        decoded_red[luma + luma / 4..]
            .iter()
            .all(|value| value.abs_diff(255) <= 2)
    );
    received.reject = true;
    assert_eq!(
        receive(api, id, &mut received, library::MAX_FRAME_BYTES),
        MEDIA_BUSY
    );
    received.reject = false;
    assert_eq!(
        receive(api, id, &mut received, library::MAX_FRAME_BYTES),
        STATUS_OK
    );
    assert_eq!(received.flags, ENCODER_CHUNK_END_OF_STREAM);
    assert_eq!(
        receive(api, id, &mut received, library::MAX_FRAME_BYTES),
        MEDIA_CLOSED
    );
    unsafe {
        (input.cancel)(input.base.context, id);
    }
    assert_eq!(
        request(&state, &mut module, "video", open)["code"],
        "session_closed"
    );
    // Lost broker delivery closes newly created sessions without reopening on retry.
    state.lock().unwrap().reject_delivery = true;
    let lost = json!({"action":"open","operation_id":"lost","configuration":config});
    let lost_id = request(&state, &mut module, "video", lost.clone())["session_id"]
        .as_u64()
        .unwrap();
    state.lock().unwrap().reject_delivery = false;
    assert_eq!(
        request(&state, &mut module, "video", lost)["code"],
        "session_closed"
    );
    assert_eq!(
        request(
            &state,
            &mut module,
            "other",
            json!({"action":"close","session_id":lost_id})
        )["code"],
        "session_not_found"
    );
    drop(stranger);
    drop(consumer);
    drop(provider);
    drop(module);
}

#[test]
fn disabled_pyrowave_never_loads_the_native_library_or_registers_its_services() {
    let state = Box::new(Mutex::new(Endpoint {
        settings: json!({"enabled":false,"pyrowave":{"library_path":"/missing/pyrowave.so"}}),
        ..Default::default()
    }));
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (&*state as *const Mutex<Endpoint>).cast_mut().cast(),
        invoke: dispatch,
    };
    // SAFETY: boxed endpoint remains live through module destruction.
    let module = EncodeModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap();
    assert_eq!(state.lock().unwrap().factories.len(), 1);
    assert!(module.pyrowave.is_none());
}
#[test]
fn enabled_pyrowave_fails_startup_on_a_missing_or_relative_library() {
    for path in ["/missing/pyrowave.so", "relative-library.so"] {
        let state = Box::new(Mutex::new(Endpoint {
            settings: json!({"enabled":true,"pyrowave":{"library_path":path,"gpu_only":false}}),
            ..Default::default()
        }));
        let endpoint = HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: (&*state as *const Mutex<Endpoint>).cast_mut().cast(),
            invoke: dispatch,
        };
        // SAFETY: boxed endpoint remains live for the attempted module startup.
        assert!(EncodeModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).is_err());
        assert!(state.lock().unwrap().factories.is_empty());
    }
}

#[cfg(feature = "pyrowave-gpu")]
#[test]
#[ignore = "requires pinned PyroWave Vulkan library and render-node access"]
fn pyrowave_gpu_exposes_actual_importable_sdr_and_hdr_formats() {
    let path =
        PathBuf::from(std::env::var_os("AMATERASU_PYROWAVE_LIBRARY").expect("pinned library path"));
    let runtime = library::Runtime::load_gpu(&path).unwrap();
    let gpu = runtime.gpu_context().unwrap();
    let sdr = gpu.offers(64, 48, false);
    let hdr = gpu.offers(64, 48, true);
    assert!(!sdr.is_empty(), "no actual SDR DMA-BUF imports available");
    assert!(
        !hdr.is_empty(),
        "no actual HDR RGB10 DMA-BUF imports available"
    );
    assert_ne!(gpu.identity.device_uuid, [0; 16]);
    assert_ne!(gpu.identity.render_major, 0);
    eprintln!(
        "selected render node {}:{}, SDR offers {}, HDR offers {}",
        gpu.identity.render_major,
        gpu.identity.render_minor,
        sdr.len(),
        hdr.len()
    );
}

#[cfg(feature = "pyrowave-gpu-tests")]
mod gpu_roundtrip;

#[cfg(feature = "pyrowave-gpu")]
mod capture_interop;

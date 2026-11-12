//! Operator-selected, separately built capture provider. No display creation,
//! helper installation or privilege changes occur in these opt-in checks.
use super::*;
use amaterasu_module_sdk::capture::ResourceConnection;
use libloading::Library;
use std::{
    ptr, thread,
    time::{Duration, Instant},
};

const CAPTURE_SERVICE: &str = "amaterasu.capture-vkms/1";

struct CapturePlugin {
    context: *mut c_void,
    api: ModuleApi,
    state: Box<Mutex<Endpoint>>,
    _library: Library,
}
impl CapturePlugin {
    fn load(settings: Value) -> Self {
        let path = std::env::var_os("AMATERASU_CAPTURE_LIBRARY")
            .expect("set AMATERASU_CAPTURE_LIBRARY to the independently built capture plugin");
        let state = Box::new(Mutex::new(Endpoint {
            settings,
            ..Default::default()
        }));
        let endpoint = HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: (&*state as *const Mutex<Endpoint>).cast_mut().cast(),
            invoke: dispatch,
        };
        // SAFETY: the operator-selected library is trusted native code. This
        // object pins it and the endpoint until all module callbacks retire.
        let library = unsafe { Library::new(path) }.unwrap();
        let entry = unsafe { library.get::<ModuleEntry>(b"amaterasu_module_v1\0") }.unwrap();
        let table = unsafe { entry().as_ref() }.expect("module table");
        assert!(table.header.supports::<ModuleApi>());
        let api = *table;
        let mut context = ptr::null_mut();
        assert_eq!(unsafe { (api.create)(&endpoint, &mut context) }, STATUS_OK);
        assert!(!context.is_null());
        Self {
            context,
            api,
            state,
            _library: library,
        }
    }
    fn request(&self, payload: Value) -> Value {
        self.state.lock().unwrap().incoming = Some(Response::Request {
            id: 1,
            caller: "video".into(),
            initiator: "test".into(),
            service: CAPTURE_SERVICE.into(),
            remaining_ms: 5000,
            payload,
        });
        // SAFETY: this module owns context and lifecycle calls are serialized.
        assert_eq!(unsafe { (self.api.tick)(self.context) }, STATUS_OK);
        self.state.lock().unwrap().completed.take().unwrap()
    }
}
impl Drop for CapturePlugin {
    fn drop(&mut self) {
        // SAFETY: bindings are scoped inside this object's lifetime. Destruction
        // joins the capture worker before the library and endpoint are released.
        unsafe { (self.api.destroy)(self.context) };
    }
}

#[test]
#[ignore = "requires the independently built capture library; no privileged helper is used"]
fn independent_capture_library_registers_the_same_typed_sdk_contract() {
    let capture = CapturePlugin::load(json!({"enabled":false}));
    assert_eq!(
        capture.state.lock().unwrap().contracts[CAPTURE_SERVICE],
        amaterasu_module_sdk::media::native_contracts::CAPTURE
    );
    let binding = bind(
        &capture.state,
        CAPTURE_SERVICE,
        "video",
        size_of::<CaptureApiV1_9>() as u32,
    );
    // SAFETY: a complete capture table was requested from the typed factory.
    let api = unsafe { &*binding.0.cast::<CaptureApiV1_9>() };
    assert!(api.base.base.base.header.minor >= 9);
    assert_eq!(
        capture.request(json!({"action":"status"}))["enabled"],
        false
    );
    drop(binding);
}

unsafe extern "C" fn capture_offer(
    context: *mut c_void,
    offer: *const CaptureResourceOffer,
) -> Status {
    // SAFETY: enumeration lends this exact descriptor and a writable local slot.
    let (slot, offer) = unsafe {
        (
            &mut *context.cast::<Option<DmaBufResourceCapabilities>>(),
            &*offer,
        )
    };
    if offer.resource.kind != FRAME_RESOURCE_DMABUF
        || offer.resource.version != 1
        || offer.resource.size as usize != size_of::<DmaBufResourceCapabilities>()
        || offer.resource.data.is_null()
        || offer.accessibility != FRAME_ACCESS_GPU
        || offer.ownership != FRAME_OWNERSHIP_RETAINABLE
    {
        return MEDIA_UNSUPPORTED;
    }
    *slot = Some(unsafe { ptr::read_unaligned(offer.resource.data.cast()) });
    STATUS_OK
}

fn await_capability(api: &CaptureApiV1_8, stream: u64) -> DmaBufResourceCapabilities {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let mut caps = None::<DmaBufResourceCapabilities>;
        let mut generation = 0;
        // SAFETY: sink storage remains live through synchronous enumeration.
        let status = unsafe {
            (api.enumerate_resources)(
                api.base.base.base.context,
                stream,
                CaptureResourceOfferSink {
                    context: (&mut caps as *mut Option<_>).cast(),
                    write: capture_offer,
                },
                &mut generation,
            )
        };
        if status == STATUS_OK {
            return caps.expect("capture GPU capability");
        }
        assert!(
            matches!(status, MEDIA_PENDING | MEDIA_BUSY),
            "capture enumeration failed: {status}"
        );
        assert!(
            Instant::now() < deadline,
            "capture produced no guarded GPU frame"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "requires an explicitly selected existing output, authorized protocol-4 helper and matching driver"]
fn live_capture_forwards_gpu_frames_through_the_unchanged_sdk_connection() {
    let output_name = std::env::var("AMATERASU_CAPTURE_OUTPUT")
        .expect("set AMATERASU_CAPTURE_OUTPUT to an already active kwin:Virtual-N output");
    let helper = std::env::var("AMATERASU_FRAME_HELPER")
        .expect("set AMATERASU_FRAME_HELPER to an operator-approved guarded helper");
    let capture = CapturePlugin::load(json!({"enabled":true,"capture_principals":["video"],
        "frame_helper":helper,"gpu_snapshot":false,"require_lease":true}));
    let opened = capture.request(json!({"action":"open","output":output_name,
        "operation_id":"pyrowave-live-gpu",
        "resource_capture_clock":{"domain":"host_monotonic","epoch_id":"07070707070707070707070707070707"},
        "resource_callback_contract":"cross_service_safe"}));
    assert_eq!(opened["ok"], true, "capture open: {opened}");
    let stream = opened["stream"].as_u64().unwrap();
    let capture_binding = bind(
        &capture.state,
        CAPTURE_SERVICE,
        "video",
        size_of::<CaptureApiV1_9>() as u32,
    );
    // SAFETY: capture v1.9 has the complete v1.8 resource prefix. The binding
    // and independently loaded library outlive the generic connection.
    let capture_api = unsafe { &*capture_binding.0.cast::<CaptureApiV1_8>() };
    let capability = await_capability(capture_api, stream);
    let hdr = capability.color == DMABUF_COLOR_BT2020_PQ;
    assert!(hdr || capability.color == DMABUF_COLOR_SRGB);
    if std::env::var("AMATERASU_CAPTURE_EXPECT_HDR").as_deref() == Ok("1") {
        assert!(hdr, "the selected presentation is not HDR10");
    }
    // Inspect the same mailbox presentation before direct forwarding retires
    // it. A delivered direct guard must not stay cached waiting for a flip.
    let hdr_api = unsafe { &*capture_binding.0.cast::<CaptureApiV1_9>() };
    let mut metadata = HdrMetadata::default();
    assert_eq!(
        unsafe { (hdr_api.hdr_metadata)(hdr_api.base.base.base.context, stream, &mut metadata) },
        STATUS_OK
    );
    assert_eq!(metadata.enabled != 0, hdr);
    let mut config = configuration();
    config.width = opened["width"].as_u64().unwrap().try_into().unwrap();
    config.height = opened["height"].as_u64().unwrap().try_into().unwrap();
    config.profile = if hdr {
        session::Profile::Hdr10
    } else {
        session::Profile::Sdr8
    };
    // An explicit epoch origin, not inferred from the first captured frame.
    config.clock_origin_us = 1;
    let path = PathBuf::from(
        std::env::var_os("AMATERASU_PYROWAVE_LIBRARY").expect("pinned native library"),
    );
    let decoder = library::Runtime::load_gpu(&path).unwrap();
    let state = Box::new(Mutex::new(Endpoint {
        settings: json!({"enabled":true,"pyrowave":{"library_path":path,"gpu_only":true}}),
        ..Default::default()
    }));
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (&*state as *const Mutex<Endpoint>).cast_mut().cast(),
        invoke: dispatch,
    };
    // SAFETY: endpoint state lives through module and native binding retirement.
    let mut module =
        EncodeModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap();
    let reply = request(
        &state,
        &mut module,
        "video",
        json!({"action":"open",
        "operation_id":"live-capture-encode","configuration":config}),
    );
    let id = reply["session_id"].as_u64().expect("encoder session");
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
    // SAFETY: typed factories return the exact complete SDK tables requested.
    let (api, input) = unsafe {
        (
            &*provider.0.cast::<EncoderProviderApi>(),
            &*consumer.0.cast::<FrameResourceConsumerApi>(),
        )
    };
    let options = serde_json::to_vec(&config).unwrap();
    let native = EncoderSessionConfig {
        size: size_of::<EncoderSessionConfig>() as u32,
        mode: ENCODER_SESSION_SUBMITTED,
        codec: native_id(if hdr {
            session::CODEC_HDR10
        } else {
            session::CODEC
        }),
        framing: native_id(session::RECORDS),
        source_id: stream,
        config_generation: 1,
        options_schema: native_id(session::OPTIONS),
        options: BorrowedBytes::new(&options),
    };
    assert_eq!(
        unsafe { (api.configure_session)(api.base.context, id, &native) },
        STATUS_OK
    );
    {
        // SAFETY: both bindings, library owners and open streams remain live.
        let mut connection =
            unsafe { ResourceConnection::new(capture_api, stream, input, id, FRAME_ACCESS_GPU) }
                .unwrap();
        connection
            .negotiate()
            .expect("compatible direct GPU capture import");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = connection
                .forward(FRAME_RESOURCE_SUBMIT_FORCE_IDR)
                .expect("capture-to-encoder SDK forwarding");
            if status == STATUS_OK {
                break;
            }
            assert!(
                matches!(status, MEDIA_PENDING | MEDIA_BUSY),
                "frame submission failed: {status}"
            );
            assert!(Instant::now() < deadline, "no frame delivered");
            thread::sleep(Duration::from_millis(5));
        }
        let mut received = Received {
            bytes: vec![],
            flags: 0,
            sequence: 0,
            timestamp: 0,
            reject: false,
        };
        assert_eq!(receive(api, id, &mut received, 16 * 1024 * 1024), STATUS_OK);
        assert!(!received.bytes.is_empty());
        assert!(received.sequence > 0);
        assert!(received.timestamp > 0);
        let decoded = decoder
            .decode(&received.bytes, config.width, config.height)
            .expect("independent capture bitstream decode");
        assert_eq!(
            decoded.len(),
            (config.width * config.height * 3 / 2) as usize
        );
        eprintln!(
            "qualified existing capture {}x{} HDR={} through GPU-only SDK input; {} encoded bytes",
            config.width,
            config.height,
            hdr,
            received.bytes.len()
        );
    }
    assert_eq!(
        capture.request(json!({"action":"close","stream":stream}))["ok"],
        true
    );
    drop(consumer);
    drop(provider);
    drop(capture_binding);
}

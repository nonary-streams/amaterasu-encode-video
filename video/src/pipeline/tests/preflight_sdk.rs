//! Exercise the unchanged pipeline through the public Capture preflight adapter.
use super::*;
use amaterasu_media::{
    CAPTURE_API_V1_0_SIZE, CAPTURE_API_V1_11_SIZE, CaptureApiPrefix, FrameSink, MEDIA_BUSY,
    MEDIA_PENDING, MEDIA_UNSUPPORTED,
};
use amaterasu_module_sdk::{
    abi::*,
    capture::{
        SourceClass, SourceModeCapabilities, SourcePreflightFactory, SourcePreflightProvider,
        SourcePreflightRequest, SourcePreflightResponse, source_preflight,
    },
};
use std::{ffi::c_void, sync::atomic::AtomicUsize};

fn ready(request: &SourcePreflightRequest) -> SourcePreflightResponse {
    SourcePreflightResponse::ready(
        request,
        vec![SourceModeCapabilities {
            source_class: request.source_class,
            min_width: 1,
            max_width: 8192,
            min_height: 1,
            max_height: 8192,
            min_refresh_millihz: 1_000,
            max_refresh_millihz: 1_000_000,
            sdr: true,
            hdr: true,
            bit_depths: vec![8, 10],
            format_open: true,
        }],
    )
}

struct Immediate(Arc<AtomicUsize>);
impl SourcePreflightProvider for Immediate {
    fn query(
        &self,
        caller: &str,
        request: &SourcePreflightRequest,
    ) -> Result<SourcePreflightResponse, Status> {
        assert_eq!(caller, "amaterasu.video");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ready(request))
    }
}

struct Deferred(Arc<AtomicUsize>);
impl SourcePreflightProvider for Deferred {
    fn query(
        &self,
        caller: &str,
        request: &SourcePreflightRequest,
    ) -> Result<SourcePreflightResponse, Status> {
        assert_eq!(caller, "amaterasu.video");
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(MEDIA_BUSY);
        }
        Ok(ready(request))
    }
}

// One completed observation is consumed by one caller, as in the DXGI worker.
// A second stream with the same query must enqueue a new observation.
struct SingleUse {
    calls: Arc<AtomicUsize>,
    observation: Mutex<Option<SourcePreflightRequest>>,
}
impl SourcePreflightProvider for SingleUse {
    fn query(
        &self,
        caller: &str,
        request: &SourcePreflightRequest,
    ) -> Result<SourcePreflightResponse, Status> {
        assert_eq!(caller, "amaterasu.video");
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut observation = self.observation.lock().unwrap();
        if let Some(previous) = observation.take() {
            assert_eq!(previous, *request);
            Ok(ready(request))
        } else {
            *observation = Some(request.clone());
            Err(MEDIA_BUSY)
        }
    }
}

unsafe extern "C" fn read_frame(_: *mut c_void, _: u64, _: FrameSink) -> Status {
    MEDIA_PENDING
}

unsafe extern "C" fn bind_prefix(
    context: *mut c_void,
    _: BorrowedBytes,
    query: *const NativeInterfaceQuery,
    output: *mut *const NativeInterface,
) -> Status {
    // SAFETY: the adapter supplies a live query and output for this call.
    let query = unsafe { &*query };
    if query.interface_major != 1 || query.min_minor > 0 || query.min_size > CAPTURE_API_V1_0_SIZE {
        return MEDIA_UNSUPPORTED;
    }
    // SAFETY: the fixture retains the complete prefix until every binding is released.
    unsafe { output.write(context.cast()) };
    STATUS_OK
}

struct SdkPlane {
    fake: FakePlane,
    binding: *const NativeInterface,
}

impl Plane for SdkPlane {
    fn source_preflight(
        &mut self,
        _: CaptureProvider,
        request: &SourcePreflightRequest,
    ) -> Result<SourcePreflightResponse, Status> {
        // SAFETY: the selected SDK factory returned this complete Capture binding;
        // it remains live until SdkPlane drops after the pipeline stops using it.
        unsafe { source_preflight(&*self.binding.cast::<CaptureApiPrefix>(), request) }
    }

    fn verify_geometry(
        &mut self,
        provider: CaptureProvider,
        capture: u64,
        expected: Option<CaptureGeometry>,
    ) -> Result<GeometryProgress, &'static str> {
        self.fake.verify_geometry(provider, capture, expected)
    }

    fn pump(&mut self, input: Pump<'_>) -> Result<Progress, &'static str> {
        self.fake.pump(input)
    }

    fn drain(&mut self, encoder: u64) -> Result<bool, &'static str> {
        self.fake.drain(encoder)
    }
}

impl Drop for SdkPlane {
    fn drop(&mut self) {
        // SAFETY: the SDK factory returned one uniquely owned binding, no
        // callback remains in flight, and its implementation outlives this Drop.
        let binding = unsafe { &*self.binding };
        if let Some(release) = binding.release {
            unsafe { release(binding.context) };
        }
    }
}

fn exercise_provider<P: SourcePreflightProvider + 'static>(
    provider: P,
    calls: Arc<AtomicUsize>,
    deferred: bool,
    concurrent: bool,
) {
    let prefix = CaptureApiPrefix {
        base: NativeInterface {
            header: AbiHeader {
                size: CAPTURE_API_V1_0_SIZE,
                major: 1,
                minor: 0,
            },
            context: std::ptr::null_mut(),
            release: None,
        },
        read_frame,
    };
    let original = NativeServiceFactory {
        header: AbiHeader::of::<NativeServiceFactory>(),
        context: std::ptr::from_ref(&prefix).cast_mut().cast(),
        bind: Some(bind_prefix),
    };
    // SAFETY: the original metadata-only Capture prefix outlives the adapter
    // and every binding; the provider performs only read-only observations.
    let factory = unsafe { SourcePreflightFactory::new(&original, Arc::new(provider)) };
    let query = NativeInterfaceQuery {
        header: AbiHeader::of::<NativeInterfaceQuery>(),
        interface_major: 1,
        min_minor: 11,
        min_size: CAPTURE_API_V1_11_SIZE,
    };
    let mut binding = std::ptr::null();
    // SAFETY: the factory, identity, typed query and output remain live through bind.
    let status = unsafe {
        factory.api.bind.unwrap()(
            factory.api.context,
            BorrowedBytes::new(b"amaterasu.video"),
            &query,
            &mut binding,
        )
    };
    assert_eq!(status, STATUS_OK);
    assert!(!binding.is_null());
    let mut plane = SdkPlane {
        fake: FakePlane::default(),
        binding,
    };
    let (mut pipeline, mut runtime, _) = setup();
    pipeline.source_preflight_routes.insert(
        "desktop".into(),
        crate::model::SourcePreflightRoute {
            provider: CaptureProvider::Dxgi,
            source_class: SourceClass::DesktopOutput,
        },
    );
    let operation = serde_json::from_value(json!({
        "action":"open", "operation_id":"sdk-replacement", "output":"display-1",
        "capture_provider":"dxgi", "encoder":encoder_identity(),
        "configuration":open_configuration(), "source_capture_context":{"route_key":"desktop"}
    }))
    .unwrap();
    pipeline.handle("alice", 99, operation, &mut |command| runtime.send(command));
    if concurrent {
        let second = serde_json::from_value(json!({
            "action":"open", "operation_id":"sdk-second", "output":"display-1",
            "capture_provider":"dxgi", "encoder":encoder_identity(),
            "configuration":open_configuration(), "source_capture_context":{"route_key":"desktop"}
        }))
        .unwrap();
        pipeline.handle("alice", 100, second, &mut |command| runtime.send(command));
    }
    let expected_streams = if concurrent { 2 } else { 1 };
    let deadline = Instant::now() + Duration::from_secs(3);
    while runtime.completed.len() < expected_streams {
        pipeline.tick(&mut plane, &mut |command| runtime.send(command));
        if deferred && calls.load(Ordering::SeqCst) == 1 {
            assert_eq!(runtime.open_effects, 0, "BUSY cannot admit capture");
        }
        assert!(Instant::now() < deadline, "pipeline did not complete");
        std::thread::yield_now();
    }
    assert!(
        runtime
            .completed
            .iter()
            .all(|(_, result)| result["state"] == "running")
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        expected_streams * if deferred { 2 } else { 1 }
    );
    let opens: Vec<_> = runtime
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Request {
                service, payload, ..
            } if service == CAPTURE && payload["action"] == "open" => Some(payload),
            _ => None,
        })
        .collect();
    assert_eq!(opens.len(), expected_streams);
    for open in opens {
        assert_eq!(open["output"], "display-1");
        assert_eq!(
            open["source_format"],
            json!({"bit_depth":8,"dynamic_range":"sdr"})
        );
    }
    drop(plane);
}

#[test]
fn unchanged_pipeline_opens_with_two_sdk_preflight_implementations() {
    let calls = Arc::new(AtomicUsize::new(0));
    exercise_provider(Immediate(calls.clone()), calls, false, false);
    let calls = Arc::new(AtomicUsize::new(0));
    exercise_provider(Deferred(calls.clone()), calls, true, false);
    let calls = Arc::new(AtomicUsize::new(0));
    exercise_provider(
        SingleUse {
            calls: calls.clone(),
            observation: Mutex::new(None),
        },
        calls,
        true,
        true,
    );
}

//! The unchanged native Video consumer keeps legacy capture while querying an
//! explicitly selected optional SDK decorator through a separate typed binding.
use super::*;
use amaterasu_module_sdk::{Command, Response, capture::*};
use std::{
    cell::{Cell, RefCell},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Ready(Arc<AtomicUsize>);
impl SourcePreflightProvider for Ready {
    fn query(
        &self,
        _: &str,
        request: &SourcePreflightRequest,
    ) -> Result<SourcePreflightResponse, Status> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(SourcePreflightResponse::ready(
            request,
            vec![SourceModeCapabilities {
                source_class: request.source_class,
                min_width: 1,
                max_width: 8192,
                min_height: 1,
                max_height: 8192,
                min_refresh_millihz: 1000,
                max_refresh_millihz: 1_000_000,
                sdr: true,
                hdr: false,
                bit_depths: vec![8],
                format_open: false,
            }],
        ))
    }
}
struct Fixture {
    factory: *const NativeServiceFactory,
    encoder: *const EncoderApi,
    next: Cell<u64>,
    bindings: RefCell<BTreeMap<u64, *const NativeInterface>>,
}
unsafe extern "C" fn bind_old(
    context: *mut c_void,
    _: BorrowedBytes,
    query: *const NativeInterfaceQuery,
    result: *mut *const NativeInterface,
) -> Status {
    // SAFETY: test retains the original prefix and complete query/result slots.
    let query = unsafe { &*query };
    if query.interface_major != 1 || query.min_minor > 0 || query.min_size > CAPTURE_API_V1_0_SIZE {
        return MEDIA_UNSUPPORTED;
    }
    unsafe { result.write(context.cast()) };
    STATUS_OK
}
unsafe extern "C" fn pixels(context: *mut c_void, _: u64, sink: FrameSink) -> Status {
    // SAFETY: prefix context retains its read counter through synchronous calls.
    unsafe { &*context.cast::<AtomicUsize>() }.fetch_add(1, Ordering::SeqCst);
    let bytes = [0u8; 16 * 16 * 4];
    let frame = CpuFrame {
        width: 16,
        height: 16,
        format: PIXEL_BGRA8,
        stride: 64,
        data: bytes.as_ptr(),
        len: bytes.len(),
        timestamp_us: 1,
        sequence: 1,
    };
    // SAFETY: initialized frame and pixels remain live through sink return.
    unsafe { sink.send(&frame) }
}
unsafe extern "C" fn submit_pixels(
    context: *mut c_void,
    _: u64,
    _: *const CpuFrame,
    _: u32,
) -> Status {
    // SAFETY: test retains this encoder counter until all plane bindings drop.
    unsafe { &*context.cast::<AtomicUsize>() }.fetch_add(1, Ordering::SeqCst);
    STATUS_OK
}
unsafe extern "C" fn receive(_: *mut c_void, _: u64, _: PacketSink) -> Status {
    MEDIA_PENDING
}
unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
    STATUS_OK
}
unsafe extern "C" fn invoke(context: *mut c_void, input: BorrowedBytes, sink: ReplySink) -> Status {
    // SAFETY: fixture, command borrow and reply sink remain live for this call.
    let fixture = unsafe { &*context.cast::<Fixture>() };
    let command: Command = serde_json::from_slice(unsafe { input.read() }.unwrap()).unwrap();
    let response = match command {
        Command::NativeBindTyped {
            service,
            contract,
            interface_major,
            min_minor,
            min_size,
        } => {
            let mut output = std::ptr::null();
            let status = if service == CaptureProvider::Vkms.service() {
                assert_eq!(contract, native_contracts::CAPTURE);
                let query = NativeInterfaceQuery {
                    header: AbiHeader::of::<NativeInterfaceQuery>(),
                    interface_major,
                    min_minor,
                    min_size,
                };
                let factory = unsafe { &*fixture.factory };
                unsafe {
                    factory.bind.unwrap()(
                        factory.context,
                        BorrowedBytes::new(b"amaterasu.video"),
                        &query,
                        &mut output,
                    )
                }
            } else {
                assert_eq!(service, ENCODE);
                assert_eq!(contract, native_contracts::ENCODER);
                output = fixture.encoder.cast();
                STATUS_OK
            };
            if status == STATUS_OK {
                let id = fixture.next.get() + 1;
                fixture.next.set(id);
                fixture.bindings.borrow_mut().insert(id, output);
                Response::NativeBound {
                    binding: id,
                    address: output as usize,
                }
            } else {
                Response::Error {
                    code: "unsupported".into(),
                    message: "unsupported".into(),
                }
            }
        }
        Command::NativeRelease { binding } => {
            let pointer = fixture.bindings.borrow_mut().remove(&binding).unwrap();
            let interface = unsafe { &*pointer };
            if let Some(release) = interface.release {
                unsafe { release(interface.context) }
            }
            Response::Ok
        }
        _ => panic!("unexpected runtime operation"),
    };
    unsafe { sink.send(&serde_json::to_vec(&response).unwrap()) };
    STATUS_OK
}

#[test]
fn unchanged_native_video_captures_old_prefix_with_or_without_preflight_decorator() {
    for decorated in [false, true] {
        let reads = AtomicUsize::new(0);
        let encoded = AtomicUsize::new(0);
        let queries = Arc::new(AtomicUsize::new(0));
        let old = CaptureApiPrefix {
            base: NativeInterface {
                header: AbiHeader {
                    major: 1,
                    minor: 0,
                    size: CAPTURE_API_V1_0_SIZE,
                },
                context: std::ptr::from_ref(&reads).cast_mut().cast(),
                release: None,
            },
            read_frame: pixels,
        };
        let inner = NativeServiceFactory {
            header: AbiHeader::of::<NativeServiceFactory>(),
            context: std::ptr::from_ref(&old).cast_mut().cast(),
            bind: Some(bind_old),
        };
        // SAFETY: inner binds metadata only, and every original object outlives
        // both decorated bindings and this unchanged native consumer.
        let wrapper =
            unsafe { SourcePreflightFactory::new(&inner, Arc::new(Ready(queries.clone()))) };
        let encoder = EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context: std::ptr::from_ref(&encoded).cast_mut().cast(),
                release: None,
            },
            submit: submit_pixels,
            receive,
            drain,
        };
        let fixture = Fixture {
            factory: if decorated { &wrapper.api } else { &inner },
            encoder: &encoder,
            next: Cell::new(0),
            bindings: RefCell::new(BTreeMap::new()),
        };
        let runtime = unsafe {
            RuntimeClient::from_endpoint(HostApi {
                header: AbiHeader::of::<HostApi>(),
                context: std::ptr::from_ref(&fixture).cast_mut().cast(),
                invoke,
            })
        };
        let routes = [(
            "third-party.capture-route/2".into(),
            crate::model::SourcePreflightRoute {
                provider: CaptureProvider::Vkms,
                source_class: SourceClass::ManagedOutput,
            },
        )]
        .into();
        let mut plane = NativePlane::bind(
            &runtime,
            Arc::new(Mutex::new(Queues::default())),
            &[CaptureProvider::Vkms].into(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
            None,
        )
        .unwrap()
        .with_source_preflight_routes(&runtime, &routes);
        let ordinary =
            unsafe { plane.captures[&CaptureProvider::Vkms].table::<CaptureApiPrefix>() }.unwrap();
        assert_eq!(
            ordinary.base.header.minor, 0,
            "ordinary capture must retain its true optional tails"
        );
        assert_eq!(ordinary.base.header.size, CAPTURE_API_V1_0_SIZE);
        let request = SourcePreflightRequest {
            source_class: SourceClass::ManagedOutput,
            route: None,
            mode: SourceMode {
                width: 16,
                height: 16,
                refresh_millihz: 60_000,
                hdr: false,
                bit_depth: Some(8),
            },
        };
        let readiness = plane.source_preflight(CaptureProvider::Vkms, &request);
        if decorated {
            assert!(readiness.unwrap().is_ready());
        } else {
            assert_eq!(readiness.err(), Some(MEDIA_UNSUPPORTED));
        }
        let configuration: Configuration = serde_json::from_value(
            serde_json::json!({"width":16,"height":16,"fps":60,"bitrate":100000}),
        )
        .unwrap();
        let result = plane
            .pump(Pump {
                video: 1,
                provider: CaptureProvider::Vkms,
                frame_path: FramePath::Cpu,
                capture: 2,
                encoder: 3,
                configuration: &configuration,
                capture_due: true,
                constant_rate: false,
                repeat_due: false,
                repeat_frame: None,
                timestamp_us: 1,
                sequence: 1,
                force_idr: false,
            })
            .unwrap();
        assert!(result.submitted);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(encoded.load(Ordering::SeqCst), 1);
        assert_eq!(queries.load(Ordering::SeqCst), usize::from(decorated));
        drop(plane);
        assert!(fixture.bindings.borrow().is_empty());
    }
}

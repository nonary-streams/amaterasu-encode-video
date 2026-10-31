//! Native pump qualification with a startup-selected external CPU provider.
use super::*;
use amaterasu_module_sdk::{Command, Response};
use serde_json::json;

struct Frames {
    bytes: Box<[u8]>,
    mode: u32,
    legacy_reads: usize,
    freshness_reads: usize,
    timestamp_reads: usize,
    submitted: usize,
}
impl Frames {
    fn frame(&self) -> CpuFrame {
        CpuFrame {
            width: 16,
            height: 16,
            format: PIXEL_BGRA8,
            stride: 64,
            data: self.bytes.as_ptr(),
            len: self.bytes.len(),
            timestamp_us: 99,
            sequence: 17,
        }
    }
}
unsafe extern "C" fn plain(context: *mut c_void, _: u64, sink: FrameSink) -> Status {
    // SAFETY: this context is owned by the test until all bindings are released.
    let frames = unsafe { &mut *context.cast::<Frames>() };
    frames.legacy_reads += 1;
    // SAFETY: the fixture frame bytes and sink live through synchronous delivery.
    unsafe { sink.send(&frames.frame()) }
}
unsafe extern "C" fn fresh(context: *mut c_void, _: u64, sink: FreshFrameSinkV1_2) -> Status {
    // SAFETY: test context and immutable bytes outlive every callback.
    let frames = unsafe { &mut *context.cast::<Frames>() };
    frames.freshness_reads += 1;
    if frames.mode == 2 || frames.mode == 5 {
        return MEDIA_UNSUPPORTED;
    }
    let freshness = if frames.mode == 1 {
        CAPTURE_FRAME_CACHED_REUSE
    } else {
        CAPTURE_FRAME_FRESH
    };
    let status = unsafe { sink.send(&frames.frame(), freshness) };
    if frames.mode == 3 {
        MEDIA_UNSUPPORTED
    } else {
        status
    }
}
unsafe extern "C" fn timestamp(context: *mut c_void, _: u64, sink: FreshFrameSinkV1_3) -> Status {
    // SAFETY: test context and immutable bytes outlive the synchronous callback.
    let frames = unsafe { &mut *context.cast::<Frames>() };
    frames.timestamp_reads += 1;
    if frames.mode == 4 || frames.mode == 5 {
        return MEDIA_UNSUPPORTED;
    }
    let frame = CaptureFrameV1_3 {
        frame: frames.frame(),
        capture_pacing_timestamp_us: 123,
    };
    let status = unsafe { sink.send(&frame, CAPTURE_FRAME_FRESH) };
    if frames.mode == 6 {
        MEDIA_UNSUPPORTED
    } else {
        status
    }
}
unsafe extern "C" fn encode(context: *mut c_void, _: u64, _: *const CpuFrame, _: u32) -> Status {
    // SAFETY: the test owns this context for the entire synchronous operation.
    unsafe { &mut *context.cast::<Frames>() }.submitted += 1;
    STATUS_OK
}
unsafe extern "C" fn pending(_: *mut c_void, _: u64, _: PacketSink) -> Status {
    MEDIA_PENDING
}
unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
    STATUS_OK
}
unsafe extern "C" fn geometry(_: *mut c_void, _: u64, _: *mut CaptureGeometry) -> Status {
    MEDIA_UNSUPPORTED
}

struct Host {
    capture: CaptureApiV1_3,
    encoder: EncoderApi,
    service: String,
}
unsafe extern "C" fn invoke(context: *mut c_void, bytes: BorrowedBytes, sink: ReplySink) -> Status {
    // SAFETY: RuntimeClient borrows both input and test Host for this call.
    let host = unsafe { &mut *context.cast::<Host>() };
    let Ok(bytes) = (unsafe { bytes.read() }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    let Ok(command) = serde_json::from_slice::<Command>(bytes) else {
        return STATUS_INVALID_ARGUMENT;
    };
    let response = match command {
        Command::NativeBindTyped {
            service, contract, ..
        } if service == host.service && contract == native_contracts::CAPTURE => {
            Response::NativeBound {
                binding: 1,
                address: &host.capture as *const CaptureApiV1_3 as usize,
            }
        }
        Command::NativeBindTyped {
            service, contract, ..
        } if service == ENCODE && contract == native_contracts::ENCODER => Response::NativeBound {
            binding: 2,
            address: &host.encoder as *const EncoderApi as usize,
        },
        Command::NativeRelease { .. } => Response::Ok,
        _ => Response::error("unexpected_request", "unconfigured service"),
    };
    // SAFETY: reply bytes remain live throughout this synchronous callback.
    unsafe { sink.send(&serde_json::to_vec(&response).unwrap()) };
    STATUS_OK
}

fn pump_external(mode: u32, legacy: bool) -> (Result<Progress, &'static str>, Box<Frames>) {
    let mut frames = Box::new(Frames {
        bytes: vec![7; 1024].into_boxed_slice(),
        mode,
        legacy_reads: 0,
        freshness_reads: 0,
        timestamp_reads: 0,
        submitted: 0,
    });
    let context = (&mut *frames as *mut Frames).cast();
    let mut host = Box::new(Host {
        capture: CaptureApiV1_3 {
            base: NativeInterface {
                header: AbiHeader {
                    size: if legacy {
                        CAPTURE_API_V1_0_SIZE
                    } else if mode >= 4 {
                        CAPTURE_API_V1_3_SIZE
                    } else {
                        CAPTURE_API_V1_2_SIZE
                    },
                    major: 1,
                    minor: if legacy {
                        0
                    } else if mode >= 4 {
                        CAPTURE_PACING_TIMESTAMP_INTERFACE_MINOR
                    } else {
                        CAPTURE_FRESHNESS_INTERFACE_MINOR
                    },
                },
                context,
                release: None,
            },
            read_frame: plain,
            geometry,
            read_frame_with_freshness: fresh,
            read_frame_with_capture_timestamp: timestamp,
        },
        encoder: EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context,
                release: None,
            },
            submit: encode,
            receive: pending,
            drain,
        },
        service: "vendor.capture/1".into(),
    });
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (&mut *host as *mut Host).cast(),
        invoke,
    };
    // SAFETY: the host, provider tables and buffers remain live through plane Drop.
    let runtime = unsafe { RuntimeClient::from_endpoint(endpoint) };
    let settings: crate::model::Settings = serde_json::from_value(
        json!({"capture_providers":[],"external_capture_providers":{"vendor":"vendor.capture/1"}}),
    )
    .unwrap();
    settings.validate().unwrap();
    let providers = settings.resolved_providers();
    let provider = *providers.first().unwrap();
    let configuration: Configuration =
        serde_json::from_value(json!({"width":16,"height":16,"fps":60,"bitrate":100000})).unwrap();
    let mut plane = NativePlane::bind(
        &runtime,
        Arc::new(Mutex::new(Queues::default())),
        &providers,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
        None,
    )
    .unwrap();
    let progress = plane.pump(Pump {
        video: 1,
        provider,
        frame_path: FramePath::Cpu,
        capture: 1,
        encoder: 1,
        configuration: &configuration,
        capture_due: true,
        constant_rate: false,
        repeat_due: false,
        repeat_frame: None,
        timestamp_us: 7,
        sequence: 2,
        force_idr: false,
    });
    drop(plane);
    drop(host);
    (progress, frames)
}

#[test]
fn external_provider_freshness_uses_advertised_tail_without_wgc_clock_assumptions() {
    for mode in [0, 1] {
        let (progress, frames) = pump_external(mode, false);
        let progress = progress.unwrap();
        assert!(progress.submitted);
        assert_eq!(progress.capture_reused, mode == 1);
        assert!(progress.capture_arrival.is_none());
        assert_eq!(frames.freshness_reads, 1);
        assert_eq!(frames.legacy_reads, 0);
        assert_eq!(frames.submitted, 1);
    }
}

#[test]
fn unsupported_optional_freshness_falls_back_only_before_delivery() {
    let (progress, frames) = pump_external(2, false);
    assert!(progress.unwrap().submitted);
    assert_eq!(frames.freshness_reads, 1);
    assert_eq!(frames.legacy_reads, 1);
    assert_eq!(frames.submitted, 1);
    let (progress, frames) = pump_external(3, false);
    assert_eq!(progress.err(), Some("capture_failed"));
    assert_eq!(frames.freshness_reads, 1);
    assert_eq!(frames.legacy_reads, 0);
    assert_eq!(frames.submitted, 1);
}

#[test]
fn legacy_external_capture_keeps_its_original_read_contract() {
    let (progress, frames) = pump_external(1, true);
    assert!(progress.unwrap().submitted);
    assert_eq!(frames.freshness_reads, 0);
    assert_eq!(frames.legacy_reads, 1);
    assert_eq!(frames.submitted, 1);
}

#[test]
fn external_timestamp_capability_and_fallback_preserve_single_submission() {
    let (progress, frames) = pump_external(7, false);
    let progress = progress.unwrap();
    assert!(progress.submitted);
    assert!(progress.capture_arrival.is_none());
    assert_eq!(
        (
            frames.timestamp_reads,
            frames.freshness_reads,
            frames.legacy_reads,
            frames.submitted
        ),
        (1, 0, 0, 1)
    );

    let (progress, frames) = pump_external(4, false);
    assert!(progress.unwrap().submitted);
    assert_eq!(
        (
            frames.timestamp_reads,
            frames.freshness_reads,
            frames.legacy_reads,
            frames.submitted
        ),
        (1, 1, 0, 1)
    );

    let (progress, frames) = pump_external(5, false);
    assert!(progress.unwrap().submitted);
    assert_eq!(
        (
            frames.timestamp_reads,
            frames.freshness_reads,
            frames.legacy_reads,
            frames.submitted
        ),
        (1, 1, 1, 1)
    );

    let (progress, frames) = pump_external(6, false);
    assert_eq!(progress.err(), Some("capture_failed"));
    assert_eq!(
        (
            frames.timestamp_reads,
            frames.freshness_reads,
            frames.legacy_reads,
            frames.submitted
        ),
        (1, 0, 0, 1)
    );
}

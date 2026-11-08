//! Independently built SDK adapters exercise the unchanged native Video plane.
//! Neutral-black transforms qualify negotiation/lifetimes, not a tone mapper.
use super::*;
use amaterasu_module_sdk::{
    Command, Response, capture::*, media::OwnedCpuFrame, processing::resource::*,
};
use std::{
    cell::{Cell, RefCell},
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Counts {
    reads: AtomicUsize,
    configured: AtomicUsize,
    processed: AtomicUsize,
    cancelled: AtomicUsize,
    shutdown: AtomicUsize,
    processor_binds: AtomicUsize,
    encoded: Mutex<Vec<(u32, Vec<u8>)>>,
}
fn color(hdr: bool, rgb: bool) -> FrameColorMetadata {
    FrameColorMetadata {
        version: 1,
        size: size_of::<FrameColorMetadata>() as u32,
        primaries: if hdr {
            FRAME_PRIMARIES_BT2020
        } else {
            FRAME_PRIMARIES_BT709
        },
        transfer: if hdr {
            FRAME_TRANSFER_PQ
        } else if rgb {
            FRAME_TRANSFER_SRGB
        } else {
            FRAME_TRANSFER_BT709
        },
        matrix: if hdr {
            FRAME_MATRIX_BT2020_NCL
        } else if rgb {
            FRAME_MATRIX_RGB
        } else {
            FRAME_MATRIX_BT709
        },
        range: if rgb {
            FRAME_RANGE_FULL
        } else {
            FRAME_RANGE_LIMITED
        },
    }
}
fn offer(format: u32, hdr: bool) -> CpuProcessingOffer {
    CpuProcessingOffer {
        id: 1,
        capabilities: CpuResourceCapabilitiesV2 {
            base: CpuResourceCapabilities {
                format,
                min_width: 16,
                max_width: 16,
                min_height: 16,
                max_height: 16,
            },
            color: color(hdr, format == PIXEL_BGRA8),
        },
    }
}
struct Source {
    counts: Arc<Counts>,
    provenance: bool,
}
impl CpuSourceProvider for Source {
    fn describe(&self, _: &str, _: u64) -> Result<CpuSourceDescription, Status> {
        Ok(CpuSourceDescription {
            generation: 1,
            width: 16,
            height: 16,
            formats: vec![CpuSourceFormat {
                pixel_format: PIXEL_P010,
                color: color(true, false),
            }],
            hdr: Some(HdrMetadata {
                enabled: 1,
                ..HdrMetadata::default()
            }),
            authoritative_color: self.provenance,
        })
    }
    fn read(
        &self,
        _: &str,
        _: u64,
        _: &CpuSourceFormat,
    ) -> Result<Option<CpuSourcePresentation>, Status> {
        self.counts.reads.fetch_add(1, Ordering::SeqCst);
        let bytes: Vec<u8> = (0..16 * 16 * 3 / 2)
            .flat_map(|i| if i < 256 { 64u16 << 6 } else { 512u16 << 6 }.to_le_bytes())
            .collect();
        Ok(Some(CpuSourcePresentation {
            receipt: None,
            width: 16,
            height: 16,
            stride: 32,
            pixel_format: PIXEL_P010,
            color: color(true, false),
            bytes: Arc::new(bytes).into(),
            timestamp_us: 11,
            capture_pacing_timestamp_us: 10,
            sequence: 1,
            freshness: CAPTURE_FRAME_FRESH,
        }))
    }
    fn geometry(&self, _: &str, _: u64) -> Result<CaptureGeometry, Status> {
        CaptureGeometry::new(
            DesktopRect::new(0, 0, 16, 16)?,
            DesktopRect::new(0, 0, 16, 16)?,
        )
    }
}
struct Planar {
    counts: Arc<Counts>,
    accept_hdr: bool,
}
impl CpuResourceProcessor for Planar {
    fn describe(&self, _: u64) -> Result<CpuProcessingDescription, Status> {
        Ok(CpuProcessingDescription {
            generation: 1,
            inputs: vec![offer(PIXEL_P010, self.accept_hdr)],
            outputs: vec![offer(PIXEL_NV12, false)],
        })
    }
    fn configure(
        &mut self,
        _: u64,
        input: CpuResourceCapabilitiesV2,
        output: CpuResourceCapabilitiesV2,
    ) -> Result<(), Status> {
        assert_eq!(input.color.transfer, FRAME_TRANSFER_PQ);
        assert_eq!(output.color.transfer, FRAME_TRANSFER_BT709);
        self.counts.configured.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn process(
        &mut self,
        _: u64,
        input: CpuResourceInput<'_>,
        _: CpuResourceCapabilitiesV2,
    ) -> Result<OwnedCpuFrame, Status> {
        self.counts.processed.fetch_add(1, Ordering::SeqCst);
        let bytes = input
            .pixels
            .pixels()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| (u16::from_le_bytes(*v) >> 8) as u8)
            .collect();
        let frame = input.pixels.descriptor();
        OwnedCpuFrame::new(
            16,
            16,
            PIXEL_NV12,
            16,
            bytes,
            frame.timestamp_us,
            frame.sequence,
        )
    }
    fn cancel(&mut self, _: u64) {
        self.counts.cancelled.fetch_add(1, Ordering::SeqCst);
    }
    fn shutdown(&mut self) {
        self.counts.shutdown.fetch_add(1, Ordering::SeqCst);
    }
}
struct Rgb(Arc<Counts>);
impl CpuResourceProcessor for Rgb {
    fn describe(&self, _: u64) -> Result<CpuProcessingDescription, Status> {
        Ok(CpuProcessingDescription {
            generation: 7,
            inputs: vec![offer(PIXEL_P010, true)],
            outputs: vec![offer(PIXEL_BGRA8, false)],
        })
    }
    fn configure(
        &mut self,
        _: u64,
        _: CpuResourceCapabilitiesV2,
        _: CpuResourceCapabilitiesV2,
    ) -> Result<(), Status> {
        self.0.configured.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn process(
        &mut self,
        _: u64,
        input: CpuResourceInput<'_>,
        _: CpuResourceCapabilitiesV2,
    ) -> Result<OwnedCpuFrame, Status> {
        self.0.processed.fetch_add(1, Ordering::SeqCst);
        let mut bytes = vec![0u8; 16 * 16 * 4];
        for (pixel, y) in bytes
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(input.pixels.pixels()[..512].as_chunks::<2>().0)
        {
            let level = (u32::from((u16::from_le_bytes(*y) >> 6).saturating_sub(64)) * 255 / 876)
                .min(255) as u8;
            *pixel = [level, level, level, 255];
        }
        let frame = input.pixels.descriptor();
        OwnedCpuFrame::new(
            16,
            16,
            PIXEL_BGRA8,
            64,
            bytes,
            frame.timestamp_us,
            frame.sequence,
        )
    }
    fn cancel(&mut self, _: u64) {
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
    }
    fn shutdown(&mut self) {
        self.0.shutdown.fetch_add(1, Ordering::SeqCst);
    }
}
struct Fixture {
    capture: *const NativeServiceFactory,
    processor: *const NativeServiceFactory,
    processor_contract: &'static str,
    encoder: *const EncoderApi,
    counts: Arc<Counts>,
    next: Cell<u64>,
    bindings: RefCell<BTreeMap<u64, *const NativeInterface>>,
}
unsafe extern "C" fn invoke(context: *mut c_void, input: BorrowedBytes, sink: ReplySink) -> Status {
    // SAFETY: each test retains its fixture and tables until all bindings drop.
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
            let (factory, expected) = if service == CaptureProvider::Wgc.service() {
                (fixture.capture, native_contracts::CAPTURE)
            } else if service == "example.resource-effects/1" {
                (fixture.processor, fixture.processor_contract)
            } else {
                assert_eq!(service, ENCODE);
                (std::ptr::null(), native_contracts::ENCODER)
            };
            if contract != expected {
                Response::Error {
                    code: "native_contract_mismatch".into(),
                    message: "native_contract_mismatch".into(),
                }
            } else {
                let mut pointer = std::ptr::null();
                let status = if factory.is_null() {
                    pointer = fixture.encoder.cast();
                    STATUS_OK
                } else {
                    if service == "example.resource-effects/1" {
                        fixture
                            .counts
                            .processor_binds
                            .fetch_add(1, Ordering::SeqCst);
                    }
                    let factory = unsafe { &*factory };
                    let query = NativeInterfaceQuery {
                        header: AbiHeader::of::<NativeInterfaceQuery>(),
                        interface_major,
                        min_minor,
                        min_size,
                    };
                    unsafe {
                        factory.bind.unwrap()(
                            factory.context,
                            BorrowedBytes::new(b"amaterasu.video"),
                            &query,
                            &mut pointer,
                        )
                    }
                };
                if status == STATUS_OK {
                    let id = fixture.next.get() + 1;
                    fixture.next.set(id);
                    fixture.bindings.borrow_mut().insert(id, pointer);
                    Response::NativeBound {
                        binding: id,
                        address: pointer as usize,
                    }
                } else {
                    Response::Error {
                        code: "unsupported".into(),
                        message: "unsupported".into(),
                    }
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
        _ => panic!("unexpected fixture request"),
    };
    unsafe { sink.send(&serde_json::to_vec(&response).unwrap()) };
    STATUS_OK
}
unsafe extern "C" fn encode(
    context: *mut c_void,
    _: u64,
    frame: *const CpuFrame,
    _: u32,
) -> Status {
    let counts = unsafe { &*context.cast::<Counts>() };
    let frame = unsafe { &*frame };
    counts
        .encoded
        .lock()
        .unwrap()
        .push((frame.format, unsafe { frame.bytes() }.unwrap().to_vec()));
    STATUS_OK
}
unsafe extern "C" fn receive(_: *mut c_void, _: u64, _: PacketSink) -> Status {
    MEDIA_PENDING
}
unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
    STATUS_OK
}
fn configuration() -> Configuration {
    serde_json::from_value(serde_json::json!({"width":16,"height":16,"fps":60,"bitrate":100000,"gpu_processing":{"service":"example.resource-effects/1","enabled":true,"source_hdr_required":true,"settings":{}}})).unwrap()
}
fn pump(config: &Configuration) -> Pump<'_> {
    Pump {
        video: 1,
        provider: CaptureProvider::Wgc,
        frame_path: FramePath::Cpu,
        capture: 2,
        encoder: 3,
        configuration: config,
        capture_due: true,
        constant_rate: false,
        repeat_due: false,
        repeat_frame: None,
        timestamp_us: 21,
        sequence: 31,
        force_idr: false,
    }
}
fn exercise<P: CpuResourceProcessor>(
    provider: P,
    counts: Arc<Counts>,
    provenance: bool,
    contract: &'static str,
    expected: Result<u32, &'static str>,
) {
    let capture = CpuSourceFactory::new(Arc::new(Source {
        counts: counts.clone(),
        provenance,
    }));
    // Use the public factory for a single independent provider instance; the
    // runtime owns the resulting table and enforces its registered family.
    let instance = RefCell::new(Some(provider));
    let processor = ResourceProcessorFactory::new(move |caller: &str| {
        assert_eq!(caller, "amaterasu.video");
        instance.borrow_mut().take().ok_or(MEDIA_BUSY)
    });
    let encoder = EncoderApi {
        base: NativeInterface {
            header: AbiHeader::of::<EncoderApi>(),
            context: Arc::as_ptr(&counts).cast_mut().cast(),
            release: None,
        },
        submit: encode,
        receive,
        drain,
    };
    let fixture = Fixture {
        capture: &capture.api,
        processor: &processor.api,
        processor_contract: contract,
        encoder: &encoder,
        counts: counts.clone(),
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
    let base = NativePlane::bind(
        &runtime,
        Arc::new(Mutex::new(Queues::default())),
        &[CaptureProvider::Wgc].into(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        None,
        None,
    )
    .unwrap();
    let result = base.with_resource_processor(&runtime, Some("example.resource-effects/1"));
    if contract != native_contracts::FRAME_RESOURCE_PROCESSOR {
        assert!(result.is_err());
        assert_eq!(counts.processor_binds.load(Ordering::SeqCst), 0);
        assert_eq!(counts.reads.load(Ordering::SeqCst), 0);
        assert!(fixture.bindings.borrow().is_empty());
        return;
    }
    let mut plane = result.unwrap();
    let config = configuration();
    let mut unsupported = config.gpu_processing.clone().unwrap();
    unsupported
        .settings
        .insert("gain".into(), serde_json::json!(1));
    assert_eq!(
        plane.configure_gpu(9, Some(&unsupported)),
        Err("resource_processor_stream_settings_unsupported")
    );
    unsupported.settings.clear();
    unsupported.live_settings = Some(crate::model::LiveProcessingSettings {
        target: amaterasu_module_sdk::extension_api::LifecycleTarget::Session {
            host_epoch: "epoch".into(),
            session_id: "session".into(),
            generation: 1,
        },
        provider: "provider".into(),
        revision: "revision".into(),
    });
    assert_eq!(
        plane.configure_gpu(9, Some(&unsupported)),
        Err("resource_processor_stream_settings_unsupported")
    );
    assert_eq!(counts.configured.load(Ordering::SeqCst), 0);
    plane
        .configure_gpu(1, config.gpu_processing.as_ref())
        .unwrap();
    assert_eq!(
        plane.update_gpu(1, &serde_json::json!({})),
        Err("resource_processor_live_updates_unsupported")
    );
    let result = plane.pump(pump(&config));
    match expected {
        Ok(format) => {
            let progress = result.unwrap();
            assert!(progress.submitted);
            let accepted = progress
                .owned_frame
                .expect("processed output is owned for repeats");
            let encoded = counts.encoded.lock().unwrap();
            assert_eq!(encoded.len(), 1);
            assert_eq!(encoded[0].0, format);
            if format == PIXEL_NV12 {
                assert!(encoded[0].1[..256].iter().all(|byte| *byte == 16));
            } else {
                assert!(
                    encoded[0]
                        .1
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .all(|pixel| *pixel == [0, 0, 0, 255])
                );
            }
            drop(encoded);
            let mut repeated = pump(&config);
            repeated.capture_due = false;
            repeated.repeat_due = true;
            repeated.repeat_frame = Some(&accepted);
            repeated.timestamp_us += 1;
            repeated.sequence += 1;
            assert!(plane.pump(repeated).unwrap().submitted);
            assert_eq!(
                counts.processed.load(Ordering::SeqCst),
                1,
                "repeat submits owned processed pixels without running the transform twice"
            );
            assert_eq!(counts.reads.load(Ordering::SeqCst), 1);
            assert_eq!(counts.encoded.lock().unwrap().len(), 2);
            assert_eq!(
                plane
                    .hdr_metadata(1, CaptureProvider::Wgc, 2)
                    .unwrap()
                    .unwrap()
                    .enabled,
                0
            );
        }
        Err(code) => {
            assert_eq!(result.err(), Some(code));
            assert!(counts.encoded.lock().unwrap().is_empty());
            assert_eq!(counts.reads.load(Ordering::SeqCst), 0);
        }
    }
    plane.finish_stream(1);
    let cancelled = counts.cancelled.load(Ordering::SeqCst);
    drop(plane);
    assert_eq!(counts.cancelled.load(Ordering::SeqCst), cancelled);
    assert!(fixture.bindings.borrow().is_empty());
    assert_eq!(counts.shutdown.load(Ordering::SeqCst), 1);
}
#[test]
fn unchanged_native_video_negotiates_cpu_p010_hdr_to_sdr_with_two_independent_processors() {
    let counts = Arc::new(Counts::default());
    exercise(
        Planar {
            counts: counts.clone(),
            accept_hdr: true,
        },
        counts,
        true,
        native_contracts::FRAME_RESOURCE_PROCESSOR,
        Ok(PIXEL_NV12),
    );
    let counts = Arc::new(Counts::default());
    exercise(
        Rgb(counts.clone()),
        counts,
        true,
        native_contracts::FRAME_RESOURCE_PROCESSOR,
        Ok(PIXEL_BGRA8),
    );
}
#[test]
fn source_provenance_and_processor_capabilities_are_required_before_capture_delivery() {
    let counts = Arc::new(Counts::default());
    exercise(
        Planar {
            counts: counts.clone(),
            accept_hdr: false,
        },
        counts,
        true,
        native_contracts::FRAME_RESOURCE_PROCESSOR,
        Err("resource_processor_negotiation_failed"),
    );
    let counts = Arc::new(Counts::default());
    exercise(
        Planar {
            counts: counts.clone(),
            accept_hdr: true,
        },
        counts,
        false,
        native_contracts::FRAME_RESOURCE_PROCESSOR,
        Err("capture_color_provenance_unavailable"),
    );
}
#[test]
fn an_equal_layout_or_service_alias_cannot_substitute_another_processor_family() {
    let counts = Arc::new(Counts::default());
    exercise(
        Planar {
            counts: counts.clone(),
            accept_hdr: true,
        },
        counts,
        true,
        native_contracts::FRAME_PROCESSOR,
        Err("wrong_family"),
    );
}

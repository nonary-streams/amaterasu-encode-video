use super::*;
use amaterasu_module_sdk::{
    Command, Response,
    capture::{
        CpuSourceDescription, CpuSourceFactory, CpuSourceFormat, CpuSourcePresentation,
        CpuSourceProvider,
    },
    frame_processing::{
        FrameProcessor, ProcessingFrame, ResourceOutputFrame, into_frame_processor_binding,
    },
};
use std::{
    cell::RefCell,
    ptr,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};
const KIND: u32 = FRAME_RESOURCE_VENDOR_BASE + 1;
const NONE: FrameDescriptor = FrameDescriptor {
    kind: FRAME_SYNC_NONE,
    version: 0,
    size: 0,
    reserved: 0,
    namespace: [0; 16],
    data: ptr::null(),
};
static HANDLE: u64 = 0x1234;
fn color() -> FrameColorMetadata {
    FrameColorMetadata {
        version: 1,
        size: size_of::<FrameColorMetadata>() as u32,
        primaries: FRAME_PRIMARIES_BT709,
        transfer: FRAME_TRANSFER_SRGB,
        matrix: FRAME_MATRIX_RGB,
        range: FRAME_RANGE_FULL,
    }
}
fn caps() -> CpuResourceCapabilitiesV2 {
    CpuResourceCapabilitiesV2 {
        base: CpuResourceCapabilities {
            format: PIXEL_BGRA8,
            min_width: 16,
            max_width: 16,
            min_height: 16,
            max_height: 16,
        },
        color: color(),
    }
}
fn offer(id: u64, kind: u32, data: *const c_void) -> CaptureResourceOffer {
    CaptureResourceOffer {
        id,
        ownership: FRAME_OWNERSHIP_RETAINABLE,
        accessibility: FRAME_ACCESS_CPU,
        reserved: 0,
        resource: FrameDescriptor {
            kind,
            version: if kind == FRAME_RESOURCE_CPU { 2 } else { 1 },
            size: if kind == FRAME_RESOURCE_CPU {
                size_of::<CpuResourceCapabilitiesV2>() as u32
            } else {
                8
            },
            reserved: 0,
            namespace: [0; 16],
            data,
        },
        acquire_sync: NONE,
        completion_sync: NONE,
    }
}
struct Source {
    pixels: Arc<Vec<u8>>,
    generation: Arc<AtomicU64>,
}
impl CpuSourceProvider for Source {
    fn describe(&self, caller: &str, stream: u64) -> Result<CpuSourceDescription, Status> {
        if caller != "amaterasu.video" || stream != 2 {
            return Err(MEDIA_CLOSED);
        }
        Ok(CpuSourceDescription {
            generation: self.generation.load(Ordering::Relaxed),
            width: 16,
            height: 16,
            formats: vec![CpuSourceFormat {
                pixel_format: PIXEL_BGRA8,
                color: color(),
            }],
            hdr: None,
            authoritative_color: false,
        })
    }
    fn read(
        &self,
        _: &str,
        _: u64,
        _: &CpuSourceFormat,
    ) -> Result<Option<CpuSourcePresentation>, Status> {
        Ok(Some(CpuSourcePresentation {
            receipt: None,
            width: 16,
            height: 16,
            stride: 64,
            pixel_format: PIXEL_BGRA8,
            color: color(),
            bytes: self.pixels.clone().into(),
            timestamp_us: 5,
            capture_pacing_timestamp_us: 111,
            sequence: 7,
            freshness: CAPTURE_FRAME_FRESH,
        }))
    }
    fn geometry(&self, _: &str, _: u64) -> Result<CaptureGeometry, Status> {
        Err(MEDIA_UNSUPPORTED)
    }
}
struct Effect {
    caps: Box<CpuResourceCapabilitiesV2>,
    pixels: *const u8,
    events: Rc<RefCell<Vec<&'static str>>>,
}
impl FrameProcessor for Effect {
    fn configure_stream(&mut self, _: u64, _: &[u8]) -> Result<(), Status> {
        Ok(())
    }
    fn update_stream(&mut self, _: u64, _: &[u8]) -> Result<(), Status> {
        Ok(())
    }
    fn input_offers(&mut self, _: u64) -> Result<(u64, Vec<CaptureResourceOffer>), Status> {
        Ok((
            1,
            vec![offer(
                1,
                FRAME_RESOURCE_CPU,
                ptr::from_ref(&*self.caps).cast(),
            )],
        ))
    }
    fn configure_input(
        &mut self,
        _: u64,
        _: u64,
        _: u64,
        _: u64,
        source: &CaptureResourceOffer,
    ) -> Result<(), Status> {
        if source.resource.kind == FRAME_RESOURCE_CPU && source.resource.version == 2 {
            Ok(())
        } else {
            Err(MEDIA_UNSUPPORTED)
        }
    }
    fn output_offers(&mut self, _: u64) -> Result<(u64, Vec<CaptureResourceOffer>), Status> {
        Ok((1, vec![offer(1, KIND, ptr::from_ref(&HANDLE).cast())]))
    }
    fn select_output(&mut self, _: u64, _: u64, _: u64) -> Result<(), Status> {
        Ok(())
    }
    fn process(
        &mut self,
        _: u64,
        input: ProcessingFrame<'_>,
    ) -> Result<ResourceOutputFrame, Status> {
        let cpu = unsafe { &*input.descriptor().resource.data.cast::<CpuFrame>() };
        assert_eq!(cpu.data, self.pixels);
        let retained = input.retain()?;
        // This independent test implementation exports a vendor handle while
        // keeping the original capture allocation pinned through every reader.
        let frame = CaptureResourceFrame {
            offer_generation: 1,
            offer_id: 1,
            resource: FrameDescriptor {
                kind: KIND,
                version: 1,
                size: 8,
                reserved: 0,
                namespace: [0; 16],
                data: ptr::from_ref(&HANDLE).cast(),
            },
            ..*retained.descriptor()
        };
        Ok(unsafe { ResourceOutputFrame::from_owned(frame) })
    }
    fn hdr_metadata(&self, _: u64) -> Option<HdrMetadata> {
        Some(HdrMetadata::default())
    }
    fn cancel(&mut self, _: u64) {
        self.events.borrow_mut().push("processor_cancel");
    }
    fn shutdown(&mut self) {}
}
struct Encoder {
    held: Option<CaptureResourceFrame>,
    events: Rc<RefCell<Vec<&'static str>>>,
}
unsafe extern "C" fn enumerate(
    _: *mut c_void,
    _: u64,
    sink: CaptureResourceOfferSink,
    generation: *mut u64,
) -> Status {
    unsafe {
        generation.write(1);
        (sink.write)(sink.context, &offer(1, KIND, ptr::from_ref(&HANDLE).cast()))
    }
}
unsafe extern "C" fn configure(
    _: *mut c_void,
    _: u64,
    _: u64,
    _: u64,
    _: u64,
    source: *const CaptureResourceOffer,
) -> Status {
    if unsafe { (*source).resource.kind } == KIND {
        STATUS_OK
    } else {
        MEDIA_UNSUPPORTED
    }
}
unsafe extern "C" fn submit(
    context: *mut c_void,
    _: u64,
    frame: *const CaptureResourceFrame,
    flags: u32,
) -> Status {
    let encoder = unsafe { &mut *context.cast::<Encoder>() };
    let frame = unsafe { &*frame };
    assert_eq!(frame.sequence, 9);
    assert_eq!(frame.timestamp_us, 1234);
    assert_eq!(frame.capture_pacing_timestamp_us, 111);
    assert_eq!(flags, FRAME_RESOURCE_SUBMIT_FORCE_IDR);
    assert_eq!(frame.resource.data, ptr::from_ref(&HANDLE).cast());
    assert!(encoder.held.is_none());
    let status = unsafe { (frame.retain)(frame.context) };
    if status == STATUS_OK {
        encoder.held = Some(*frame);
    }
    status
}
unsafe extern "C" fn cancel(context: *mut c_void, _: u64) {
    let e = unsafe { &mut *context.cast::<Encoder>() };
    e.events.borrow_mut().push("encoder_cancel");
    if let Some(frame) = e.held.take() {
        unsafe { (frame.release)(frame.context, &NONE) }
    }
}
unsafe extern "C" fn cpu(_: *mut c_void, _: u64, _: *const CpuFrame, _: u32) -> Status {
    panic!("unexpected CPU fallback")
}
unsafe extern "C" fn dma(_: *mut c_void, _: u64, _: *const DmaBufFrame, _: u32) -> Status {
    panic!("unexpected DMA-BUF fallback")
}
unsafe extern "C" fn lease(_: *mut c_void, _: u64, _: *const DmaBufFrameLease, _: u32) -> Status {
    panic!("unexpected lease fallback")
}
unsafe extern "C" fn receive(_: *mut c_void, _: u64, _: PacketSink) -> Status {
    MEDIA_PENDING
}
unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
    STATUS_OK
}
unsafe extern "C" fn ready(_: *mut c_void, _: u64) -> Status {
    STATUS_OK
}
unsafe extern "C" fn timing(_: *mut c_void, _: u64, _: *mut EncodeTimingSnapshotV1) -> Status {
    MEDIA_UNSUPPORTED
}
fn encoder(e: &mut Encoder) -> EncoderApiV1_5 {
    EncoderApiV1_5 {
        base: EncoderApiV1_4 {
            base: EncoderApiV1_3 {
                base: EncoderApiV1_2 {
                    base: NativeInterface {
                        header: AbiHeader {
                            major: 1,
                            minor: 5,
                            size: ENCODER_API_V1_5_SIZE,
                        },
                        context: ptr::from_mut(e).cast(),
                        release: None,
                    },
                    submit: cpu,
                    submit_dmabuf: dma,
                    submit_dmabuf_leased: lease,
                    receive,
                    drain,
                    cancel_dmabuf_leases: cancel,
                },
                encode_timing: timing,
            },
            gpu_input_ready: ready,
        },
        enumerate_resources: enumerate,
        configure_resource: configure,
        submit_resource: submit,
    }
}
struct Runtime {
    capture: usize,
    processor: usize,
    encoder: usize,
    /// A plugin encoder supplied as a frame resource consumer, if any.
    consumer: usize,
    events: Rc<RefCell<Vec<&'static str>>>,
}
unsafe extern "C" fn invoke(context: *mut c_void, bytes: BorrowedBytes, sink: ReplySink) -> Status {
    let r = unsafe { &*context.cast::<Runtime>() };
    let command: Command = serde_json::from_slice(unsafe { bytes.read() }.unwrap()).unwrap();
    let response = match command {
        Command::NativeBindTyped {
            service, contract, ..
        } => {
            let (binding, address, expected) = match service.as_str() {
                ENCODE => (1, r.encoder, native_contracts::ENCODER),
                // The submitted provider's control table is unused until a
                // source session starts; only its consumer receives frames.
                "fixture.plugin-provider/1" => (4, r.consumer, native_contracts::ENCODER_PROVIDER),
                "fixture.plugin-consumer/1" => {
                    (5, r.consumer, native_contracts::FRAME_RESOURCE_CONSUMER)
                }
                "fixture.effect/1" => (2, r.processor, native_contracts::FRAME_PROCESSOR),
                "amaterasu.capture-pipewire/1" | "amaterasu.capture-x11/1" => {
                    (3, r.capture, native_contracts::CAPTURE)
                }
                _ => panic!("unexpected service {service}"),
            };
            assert_eq!(contract, expected);
            Response::NativeBound { binding, address }
        }
        Command::NativeRelease { binding } => {
            r.events.borrow_mut().push(match binding {
                1 => "encoder_release",
                2 => "processor_release",
                3 => "capture_release",
                4 => "provider_release",
                5 => "consumer_release",
                _ => panic!("binding"),
            });
            Response::Ok
        }
        _ => panic!("unexpected runtime command"),
    };
    unsafe { sink.send(&serde_json::to_vec(&response).unwrap()) };
    STATUS_OK
}
#[test]
fn unchanged_video_consumer_uses_sdk_for_cpu_capture_and_retires_readers_before_bindings() {
    for (provider, managed) in [
        (CaptureProvider::Pipewire, true),
        (CaptureProvider::X11, true),
        (CaptureProvider::Pipewire, false),
        (CaptureProvider::X11, false),
    ] {
        let events = Rc::new(RefCell::new(Vec::new()));
        let pixels = Arc::new(vec![37; 16 * 16 * 4]);
        let generation = Arc::new(AtomicU64::new(1));
        let source = CpuSourceFactory::new(Arc::new(Source {
            pixels: pixels.clone(),
            generation: generation.clone(),
        }));
        let mut capture = ptr::null();
        let query = NativeInterfaceQuery {
            header: AbiHeader::of::<NativeInterfaceQuery>(),
            interface_major: 1,
            min_minor: 8,
            min_size: CAPTURE_API_V1_8_SIZE,
        };
        assert_eq!(
            unsafe {
                source.api.bind.unwrap()(
                    source.api.context,
                    BorrowedBytes::new(b"amaterasu.video"),
                    &query,
                    &mut capture,
                )
            },
            STATUS_OK
        );
        let effect = into_frame_processor_binding(Effect {
            caps: Box::new(caps()),
            pixels: pixels.as_ptr(),
            events: events.clone(),
        });
        let mut state = Encoder {
            held: None,
            events: events.clone(),
        };
        let encoder = encoder(&mut state);
        let endpoint = Runtime {
            capture: capture as usize,
            processor: effect as usize,
            encoder: ptr::from_ref(&encoder) as usize,
            consumer: 0,
            events: events.clone(),
        };
        let runtime = unsafe {
            RuntimeClient::from_endpoint(HostApi {
                header: AbiHeader::of::<HostApi>(),
                context: ptr::from_ref(&endpoint).cast_mut().cast(),
                invoke,
            })
        };
        let mut plane = NativePlane::bind(
            &runtime,
            Arc::new(Mutex::new(Queues::default())),
            &[provider].into(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
            Some("fixture.effect/1"),
        )
        .unwrap()
        .with_processing_frame_settings((!managed).then(|| serde_json::json!({})));
        let mut configuration: Configuration = serde_json::from_value(
            serde_json::json!({"width":16,"height":16,"fps":60,"bitrate":100_000}),
        )
        .unwrap();
        if managed {
            configuration.gpu_processing=Some(serde_json::from_value(serde_json::json!({"service":"fixture.effect/1","enabled":true,"source_hdr_required":false,"settings":{}})).unwrap());
        }
        plane
            .configure_gpu(1, configuration.gpu_processing.as_ref())
            .unwrap();
        let progress = plane
            .pump(Pump {
                video: 1,
                capture: 2,
                encoder: 3,
                provider,
                configuration: &configuration,
                frame_path: FramePath::Cpu,
                capture_due: true,
                constant_rate: false,
                repeat_due: false,
                repeat_frame: None,
                timestamp_us: 1234,
                sequence: 9,
                force_idr: true,
            })
            .unwrap();
        assert!(progress.submitted);
        assert_eq!(source.retirement_ready(), Ok(false));
        assert!(state.held.is_some());
        // Desktop unlock can replace capture storage/format without ending the
        // selected desktop session. Keep the processor and its HDR policy alive.
        generation.store(2, Ordering::Relaxed);
        let pump = || Pump {
            video: 1,
            capture: 2,
            encoder: 3,
            provider,
            configuration: &configuration,
            frame_path: FramePath::Cpu,
            capture_due: true,
            constant_rate: false,
            repeat_due: false,
            repeat_frame: None,
            timestamp_us: 1234,
            sequence: 9,
            force_idr: true,
        };
        assert!(plane.pump(pump()).unwrap().capture_deferred);
        assert!(
            state.held.is_some(),
            "previous reader survives renegotiation"
        );
        assert!(events.borrow().is_empty(), "no cancellation during unlock");
        assert!(plane.hdr_metadata(1, provider, 2).unwrap().is_some());
        let previous = state.held.take().unwrap();
        unsafe { (previous.release)(previous.context, &NONE) };
        assert!(plane.pump(pump()).unwrap().submitted);
        drop(plane);
        assert!(state.held.is_none());
        assert_eq!(source.retirement_ready(), Ok(true));
        let order = events.borrow();
        let at = |name| order.iter().position(|event| *event == name).unwrap();
        assert!(at("encoder_cancel") < at("processor_cancel"));
        assert!(at("processor_cancel") < at("capture_release"));
        drop(order);
        // The fake broker observes release ordering; this fixture owns the tables.
        unsafe {
            (*effect).base.base.base.release.unwrap()((*effect).base.base.base.context);
            (*capture).release.unwrap()((*capture).context);
        }
    }
}

/// A processor whose only output is a standard DMA-BUF, like RTX HDR's offer
/// for consumers that cannot read CUDA memory.
struct DmaBufEffect {
    caps: Box<CpuResourceCapabilitiesV2>,
    output: Box<DmaBufResourceCapabilities>,
    pixels: Box<DmaBufFrame>,
}
impl FrameProcessor for DmaBufEffect {
    fn configure_stream(&mut self, _: u64, _: &[u8]) -> Result<(), Status> {
        Ok(())
    }
    fn update_stream(&mut self, _: u64, _: &[u8]) -> Result<(), Status> {
        Ok(())
    }
    fn input_offers(&mut self, _: u64) -> Result<(u64, Vec<CaptureResourceOffer>), Status> {
        Ok((
            1,
            vec![offer(
                1,
                FRAME_RESOURCE_CPU,
                ptr::from_ref(&*self.caps).cast(),
            )],
        ))
    }
    fn configure_input(
        &mut self,
        _: u64,
        _: u64,
        _: u64,
        _: u64,
        _: &CaptureResourceOffer,
    ) -> Result<(), Status> {
        Ok(())
    }
    fn output_offers(&mut self, _: u64) -> Result<(u64, Vec<CaptureResourceOffer>), Status> {
        let mut output = offer(
            1,
            FRAME_RESOURCE_DMABUF,
            ptr::from_ref(&*self.output).cast(),
        );
        output.accessibility = FRAME_ACCESS_GPU;
        output.resource.size = size_of::<DmaBufResourceCapabilities>() as u32;
        Ok((1, vec![output]))
    }
    fn select_output(&mut self, _: u64, _: u64, _: u64) -> Result<(), Status> {
        Ok(())
    }
    fn process(
        &mut self,
        _: u64,
        input: ProcessingFrame<'_>,
    ) -> Result<ResourceOutputFrame, Status> {
        let retained = input.retain()?;
        let source = *retained.descriptor();
        self.pixels.timestamp_us = source.timestamp_us;
        self.pixels.sequence = source.sequence;
        let frame = CaptureResourceFrame {
            offer_generation: 1,
            offer_id: 1,
            resource: FrameDescriptor {
                kind: FRAME_RESOURCE_DMABUF,
                version: 1,
                size: size_of::<DmaBufFrame>() as u32,
                reserved: 0,
                namespace: [0; 16],
                data: ptr::from_ref(&*self.pixels).cast(),
            },
            ..source
        };
        Ok(unsafe { ResourceOutputFrame::from_owned(frame) })
    }
    fn hdr_metadata(&self, _: u64) -> Option<HdrMetadata> {
        None
    }
    fn cancel(&mut self, _: u64) {}
    fn shutdown(&mut self) {}
}

/// A plugin encoder's frame consumer: retains frames asynchronously and
/// records the presentation it was given.
struct PluginConsumer {
    caps: DmaBufResourceCapabilities,
    held: Vec<CaptureResourceFrame>,
}
unsafe extern "C" fn plugin_enumerate(
    context: *mut c_void,
    _: u64,
    sink: CaptureResourceOfferSink,
    generation: *mut u64,
) -> Status {
    let consumer = unsafe { &*context.cast::<PluginConsumer>() };
    let mut accepted = offer(
        100,
        FRAME_RESOURCE_DMABUF,
        ptr::from_ref(&consumer.caps).cast(),
    );
    accepted.accessibility = FRAME_ACCESS_GPU;
    accepted.resource.size = size_of::<DmaBufResourceCapabilities>() as u32;
    unsafe {
        generation.write(1);
        (sink.write)(sink.context, &accepted)
    }
}
unsafe extern "C" fn plugin_configure(
    _: *mut c_void,
    _: u64,
    _: u64,
    _: u64,
    _: u64,
    producer: *const CaptureResourceOffer,
) -> Status {
    if unsafe { (*producer).resource.kind } == FRAME_RESOURCE_DMABUF {
        STATUS_OK
    } else {
        MEDIA_UNSUPPORTED
    }
}
unsafe extern "C" fn plugin_submit(
    context: *mut c_void,
    _: u64,
    frame: *const CaptureResourceFrame,
    _: u32,
) -> Status {
    let consumer = unsafe { &mut *context.cast::<PluginConsumer>() };
    let frame = unsafe { *frame };
    let status = unsafe { (frame.retain)(frame.context) };
    if status == STATUS_OK {
        consumer.held.push(frame);
    }
    status
}
unsafe extern "C" fn plugin_cancel(context: *mut c_void, _: u64) {
    let consumer = unsafe { &mut *context.cast::<PluginConsumer>() };
    for frame in consumer.held.drain(..) {
        unsafe { (frame.release)(frame.context, &NONE) };
    }
}

#[test]
fn processing_stacks_with_an_encoder_supplied_as_a_resource_consumer() {
    let pixels = Arc::new(vec![37; 16 * 16 * 4]);
    let source = CpuSourceFactory::new(Arc::new(Source {
        pixels,
        generation: Arc::new(AtomicU64::new(1)),
    }));
    let mut capture = ptr::null();
    let query = NativeInterfaceQuery {
        header: AbiHeader::of::<NativeInterfaceQuery>(),
        interface_major: 1,
        min_minor: 8,
        min_size: CAPTURE_API_V1_8_SIZE,
    };
    assert_eq!(
        unsafe {
            source.api.bind.unwrap()(
                source.api.context,
                BorrowedBytes::new(b"amaterasu.video"),
                &query,
                &mut capture,
            )
        },
        STATUS_OK
    );
    let output = DmaBufResourceCapabilities {
        fourcc: u32::from_le_bytes(*b"XB30"),
        color: DMABUF_COLOR_BT2020_PQ,
        modifier: 7,
        min_width: 16,
        max_width: 16,
        min_height: 16,
        max_height: 16,
        device_id: [3; 16],
    };
    let mut planes = [DmaBufPlane::UNUSED; DMABUF_MAX_PLANES];
    planes[0] = DmaBufPlane {
        fd: 9,
        acquire_fence_fd: -1,
        offset: 0,
        pitch: 64,
    };
    let effect = into_frame_processor_binding(DmaBufEffect {
        caps: Box::new(caps()),
        output: Box::new(output),
        pixels: Box::new(DmaBufFrame {
            width: 16,
            height: 16,
            fourcc: output.fourcc,
            plane_count: 1,
            modifier: 7,
            planes,
            color: DMABUF_COLOR_BT2020_PQ,
            reserved: 0,
            timestamp_us: 0,
            sequence: 0,
        }),
    });
    let mut plugin = PluginConsumer {
        caps: output,
        held: Vec::new(),
    };
    let consumer = FrameResourceConsumerApi {
        base: NativeInterface {
            header: AbiHeader::of::<FrameResourceConsumerApi>(),
            context: ptr::from_mut(&mut plugin).cast(),
            release: None,
        },
        enumerate_resources: plugin_enumerate,
        configure: plugin_configure,
        submit: plugin_submit,
        cancel: plugin_cancel,
    };
    let mut state = Encoder {
        held: None,
        events: Rc::default(),
    };
    let native = encoder(&mut state);
    let endpoint = Runtime {
        capture: capture as usize,
        processor: effect as usize,
        encoder: ptr::from_ref(&native) as usize,
        consumer: ptr::from_ref(&consumer) as usize,
        events: Rc::default(),
    };
    let runtime = unsafe {
        RuntimeClient::from_endpoint(HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: ptr::from_ref(&endpoint).cast_mut().cast(),
            invoke,
        })
    };
    let registration: crate::model::SourceEncoderRegistration =
        serde_json::from_value(serde_json::json!({
            "control_service": "fixture.plugin/1",
            "native_service": "fixture.plugin-provider/1",
            "source_ids": [1],
            "submitted": true,
        }))
        .unwrap();
    let mut plane = NativePlane::bind(
        &runtime,
        Arc::new(Mutex::new(Queues::default())),
        &[CaptureProvider::Pipewire].into(),
        &BTreeMap::new(),
        &[("plugin".to_owned(), registration)].into(),
        &[("plugin".to_owned(), "fixture.plugin-consumer/1".to_owned())].into(),
        None,
        Some("fixture.effect/1"),
    )
    .unwrap();
    let mut configuration: Configuration = serde_json::from_value(
        serde_json::json!({"width":16,"height":16,"fps":60,"bitrate":100_000}),
    )
    .unwrap();
    configuration.gpu_processing = Some(
        serde_json::from_value(
            serde_json::json!({"service":"fixture.effect/1","enabled":true,
            "source_hdr_required":false,"settings":{}}),
        )
        .unwrap(),
    );
    plane.select_encoder("fixture.plugin/1");
    plane.set_stream_clock(1, 10_000, [6; 16]);
    plane
        .configure_gpu(1, configuration.gpu_processing.as_ref())
        .unwrap();
    let result = plane.pump(Pump {
        video: 1,
        capture: 2,
        encoder: 3,
        provider: CaptureProvider::Pipewire,
        configuration: &configuration,
        frame_path: FramePath::Generic,
        capture_due: true,
        constant_rate: false,
        repeat_due: false,
        repeat_frame: None,
        timestamp_us: 1234,
        sequence: 9,
        force_idr: false,
    });
    // Units are collected through the plugin's own source session, which
    // this fixture does not start; the processed frame was already accepted.
    assert_eq!(result.err(), Some("encoder_source_unavailable"));
    assert_eq!(plugin.held.len(), 1);
    let received = plugin.held[0];
    assert_eq!(received.resource.kind, FRAME_RESOURCE_DMABUF);
    assert_eq!(received.timestamp_us, 11_234);
    assert_eq!(received.sequence, 9);
    assert_eq!(received.clock.epoch_id, [6; 16]);
    let dmabuf = unsafe { *received.resource.data.cast::<DmaBufFrame>() };
    assert_eq!((dmabuf.modifier, dmabuf.planes[0].fd), (7, 9));
    assert_eq!((dmabuf.timestamp_us, dmabuf.sequence), (11_234, 9));
    assert!(state.held.is_none(), "the native encoder is not involved");
    drop(plane);
    assert!(plugin.held.is_empty(), "cancellation released the consumer");
    assert_eq!(source.retirement_ready(), Ok(true));
    unsafe {
        (*effect).base.base.base.release.unwrap()((*effect).base.base.base.context);
        (*capture).release.unwrap()((*capture).context);
    }
}

use super::*;
use crate::pyrowave::gpu::fixture::Image;

unsafe extern "C" fn collect_capability(
    context: *mut c_void,
    offer: *const CaptureResourceOffer,
) -> Status {
    // SAFETY: producer lends a standard capability during enumeration.
    let offers = unsafe { &mut *context.cast::<Vec<(u64, DmaBufResourceCapabilities)>>() };
    let offer = unsafe { &*offer };
    if offer.resource.kind != FRAME_RESOURCE_DMABUF {
        return STATUS_INVALID_ARGUMENT;
    }
    let caps = unsafe {
        std::ptr::read_unaligned(offer.resource.data.cast::<DmaBufResourceCapabilities>())
    };
    offers.push((offer.id, caps));
    STATUS_OK
}
#[test]
#[ignore = "requires native PyroWave, a Vulkan GPU and matching render-node access"]
fn gpu_rendered_dmabufs_encode_through_the_registered_sdk_without_cpu_frames() {
    let path = PathBuf::from(
        std::env::var_os("AMATERASU_PYROWAVE_LIBRARY").expect("pinned native library"),
    );
    let decoder = library::Runtime::load_gpu(&path).unwrap();
    let provider_path = if cfg!(feature = "pyrowave-embedded") {
        PathBuf::from("/embedded/pyrowave")
    } else {
        path.clone()
    };
    let state = Box::new(Mutex::new(Endpoint {
        settings: json!({"enabled":true,"pyrowave":{"library_path":provider_path,"gpu_only":true}}),
        ..Default::default()
    }));
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (&*state as *const Mutex<Endpoint>).cast_mut().cast(),
        invoke: dispatch,
    };
    // SAFETY: endpoint remains boxed until all bindings and module retire.
    let mut module =
        Box::new(EncodeModule::start(unsafe { RuntimeClient::from_endpoint(endpoint) }).unwrap());
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
    // SAFETY: exact typed tables returned by the registered factories.
    let (api, input) = unsafe {
        (
            &*provider.0.cast::<EncoderProviderApi>(),
            &*consumer.0.cast::<FrameResourceConsumerApi>(),
        )
    };
    for (iteration, hdr, sdr10, capture_rgb10, intensity) in [
        (0, false, false, false, 0.25f32),
        (1, true, false, true, 767.0f32 / 1023.0),
        (2, true, false, true, 768.0f32 / 1023.0),
        (3, true, false, true, 940.0f32 / 1023.0),
        (4, false, true, true, 767.0f32 / 1023.0),
        (5, false, true, true, 768.0f32 / 1023.0),
        (6, false, false, true, 0.25f32),
    ] {
        let ten_bit = hdr || sdr10;
        let mut fixture = Image::create(
            decoder.gpu_context().unwrap(),
            64,
            48,
            capture_rgb10,
            intensity,
        )
        .unwrap();
        if !hdr && capture_rgb10 {
            fixture.capability.color = DMABUF_COLOR_SRGB;
            fixture.frame.color = DMABUF_COLOR_SRGB;
        }
        let mut config = configuration();
        config.profile = if hdr {
            session::Profile::Hdr10
        } else if sdr10 {
            session::Profile::Sdr10
        } else {
            session::Profile::Sdr8
        };
        let probe = request(
            &state,
            &mut module,
            "video",
            json!({"action":"probe","configuration":config}),
        );
        assert_eq!(probe["usable"], true, "GPU probe: {probe}");
        assert_eq!(probe["probe_scope"], "gpu_encode");
        assert_eq!(probe["capture_import_verified"], false);
        let open = request(
            &state,
            &mut module,
            "video",
            json!({"action":"open","operation_id":format!("gpu-{iteration}"),"configuration":config}),
        );
        let id = open["session_id"].as_u64().unwrap();
        let common = EncoderCommonConfigV1 {
            version: 1,
            size: size_of::<EncoderCommonConfigV1>() as u32,
            flags: ENCODER_CONFIG_HAS_COLOR,
            reserved: 0,
            coded_width: config.width,
            coded_height: config.height,
            crop_x: 0,
            crop_y: 0,
            visible_width: config.width,
            visible_height: config.height,
            fps_numerator: config.fps,
            fps_denominator: 1,
            bitrate_bps: config.bitrate,
            profile: 0,
            level: 0,
            chroma: ENCODER_CHROMA_420,
            bit_depth: if ten_bit { 10 } else { 8 },
            color: FrameColorMetadata {
                transfer: if hdr {
                    FRAME_TRANSFER_PQ
                } else {
                    FRAME_TRANSFER_SRGB
                },
                ..Default::default()
            },
            output_clock: FrameClockMetadata {
                version: 1,
                size: size_of::<FrameClockMetadata>() as u32,
                domain: FRAME_CLOCK_STREAM_RELATIVE,
                reserved: 0,
                epoch_id: config.clock_epoch,
            },
            output_clock_origin_us: config.clock_origin_us,
        };
        // SAFETY: the pointer-free common descriptor stays live through configure.
        let options = unsafe {
            std::slice::from_raw_parts(
                (&common as *const EncoderCommonConfigV1).cast::<u8>(),
                size_of::<EncoderCommonConfigV1>(),
            )
        };
        let native = EncoderSessionConfig {
            size: size_of::<EncoderSessionConfig>() as u32,
            mode: ENCODER_SESSION_SUBMITTED,
            codec: native_id(if ten_bit {
                session::CODEC_HDR10
            } else {
                session::CODEC
            }),
            framing: native_id(session::RECORDS),
            source_id: 6,
            config_generation: 1,
            options_schema: EncoderFormatId {
                namespace_id: ENCODER_FORMAT_NAMESPACE_STANDARD,
                value: ENCODER_SCHEMA_COMMON_CONFIG,
                schema: ENCODER_FORMAT_SCHEMA_V1,
            },
            options: BorrowedBytes::new(options),
        };
        // SAFETY: input options remain readable through configure.
        assert_eq!(
            unsafe { (api.configure_session)(api.base.context, id, &native) },
            STATUS_OK
        );
        let mut capabilities = Vec::<(u64, DmaBufResourceCapabilities)>::new();
        let mut generation = 0;
        // SAFETY: local vector and generation are lent through this enumeration.
        assert_eq!(
            unsafe {
                (input.enumerate_resources)(
                    input.base.context,
                    id,
                    CaptureResourceOfferSink {
                        context: (&mut capabilities as *mut Vec<_>).cast(),
                        write: collect_capability,
                    },
                    &mut generation,
                )
            },
            STATUS_OK
        );
        let (offer_id, _) = capabilities
            .iter()
            .find(|(_, caps)| {
                caps.fourcc == fixture.capability.fourcc
                    && caps.modifier == fixture.capability.modifier
                    && caps.color == fixture.capability.color
            })
            .expect("same actual GPU import offer");
        let offer = CaptureResourceOffer {
            id: 10,
            ownership: FRAME_OWNERSHIP_RETAINABLE,
            accessibility: FRAME_ACCESS_GPU,
            reserved: 0,
            resource: FrameDescriptor {
                kind: FRAME_RESOURCE_DMABUF,
                version: 1,
                size: size_of::<DmaBufResourceCapabilities>() as u32,
                reserved: 0,
                namespace: [0; 16],
                data: (&fixture.capability as *const DmaBufResourceCapabilities).cast(),
            },
            acquire_sync: none(),
            completion_sync: none(),
        };
        // SAFETY: exact standard producer capability remains readable through configure.
        assert_eq!(
            unsafe { (input.configure)(input.base.context, id, generation, *offer_id, 9, &offer) },
            STATUS_OK
        );
        // Explicit GPU selection cannot be replaced by a CPU offer.
        assert_eq!(
            unsafe { (input.configure)(input.base.context, id, generation, 1, 9, &offer) },
            MEDIA_UNSUPPORTED
        );
        let frame = CaptureResourceFrame {
            offer_generation: 9,
            offer_id: 10,
            timestamp_us: 1100,
            capture_pacing_timestamp_us: 1100,
            sequence: 1,
            width: 64,
            height: 48,
            freshness: 1,
            reserved: 0,
            color: FrameColorMetadata {
                version: 1,
                size: size_of::<FrameColorMetadata>() as u32,
                primaries: if hdr {
                    FRAME_PRIMARIES_BT2020
                } else {
                    FRAME_PRIMARIES_BT709
                },
                transfer: if hdr {
                    FRAME_TRANSFER_PQ
                } else {
                    FRAME_TRANSFER_SRGB
                },
                matrix: FRAME_MATRIX_RGB,
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
                kind: FRAME_RESOURCE_DMABUF,
                version: 1,
                size: size_of::<DmaBufFrame>() as u32,
                reserved: 0,
                namespace: [0; 16],
                data: (&fixture.frame as *const DmaBufFrame).cast(),
            },
            acquire_sync: none(),
            context: std::ptr::null_mut(),
            retain,
            release,
        };
        // Use the same already captured resource as the standard encoder path:
        // its producer-relative clock is independent of the video timeline.
        let captured_payload = DmaBufFrame {
            timestamp_us: 37,
            ..fixture.frame
        };
        let captured = CaptureResourceFrame {
            timestamp_us: 37,
            clock: FrameClockMetadata {
                version: 1,
                size: size_of::<FrameClockMetadata>() as u32,
                domain: FRAME_CLOCK_STREAM_RELATIVE,
                ..Default::default()
            },
            resource: FrameDescriptor {
                data: (&captured_payload as *const DmaBufFrame).cast(),
                ..frame.resource
            },
            ..frame
        };
        let presented = unsafe {
            amaterasu_module_sdk::encode::PresentedDmaBufResource::new(
                &captured,
                amaterasu_module_sdk::encode::HostVideoPresentation {
                    timestamp_us: 1100,
                    epoch_id: [7; 16],
                    sequence: 1,
                },
            )
        }
        .unwrap();
        // SAFETY: immutable GPU source and acquire fences live through submission.
        assert_eq!(
            unsafe {
                (input.submit)(
                    input.base.context,
                    id,
                    presented.descriptor(),
                    FRAME_RESOURCE_SUBMIT_FORCE_IDR,
                )
            },
            STATUS_OK
        );
        drop(presented);
        drop(fixture);
        let mut received = Received {
            bytes: Vec::new(),
            flags: 0,
            sequence: 0,
            timestamp: 0,
            reject: false,
        };
        assert_eq!(
            receive(api, id, &mut received, library::MAX_FRAME_BYTES),
            STATUS_OK
        );
        // The independently decoded bytes must also survive the selected wire framer.
        use amaterasu_module_sdk::video_framing::FramingProvider;
        let wire_unit = NegotiatedVideoUnit {
            size: size_of::<NegotiatedVideoUnit>() as u32,
            version: 1,
            stream: id,
            generation: 1,
            offer_id: 3,
            codec: native.codec,
            framing: native.framing,
            access_unit_id: 1,
            submission_id: 1,
            source_id: 6,
            config_generation: 1,
            timestamp_us: 100,
            clock: FrameClockMetadata {
                version: 1,
                size: size_of::<FrameClockMetadata>() as u32,
                domain: FRAME_CLOCK_STREAM_RELATIVE,
                reserved: 0,
                epoch_id: [7; 16],
            },
            flags: VIDEO_UNIT_KEYFRAME,
            reserved: 0,
            data: BorrowedBytes::new(&received.bytes),
        };
        let limits = VideoFramingLimits {
            size: size_of::<VideoFramingLimits>() as u32,
            version: VIDEO_FRAMING_LIMITS_VERSION,
            packet_size: 1024,
            max_datagrams: 64,
            max_queue_bytes: 65536,
            max_wire_bytes: 65536,
            max_fec_blocks: 4,
            max_fec_percentage: 255,
            ..Default::default()
        };
        let framed = crate::pyrowave::framing::Framer
            .plan("video", &wire_unit, &received.bytes, &limits)
            .unwrap();
        assert_eq!(framed.framed_bytes, received.bytes);
        let sequence_color = u32::from_le_bytes(received.bytes[12..16].try_into().unwrap());
        assert_eq!(
            sequence_color & (7 << 27),
            if hdr { 7 << 27 } else { 0 },
            "encoded sequence must identify the negotiated HDR color"
        );
        let decoded = decoder.decode(&received.bytes, 64, 48).unwrap();
        let expected = (intensity * 255.0).round() as u8;
        let decoded16 = decoder.decode16(&received.bytes, 64, 48).unwrap();
        if ten_bit {
            let expected16 = ((intensity * 1023.0).round() / 1023.0 * 65535.0).round() as u16;
            assert!(
                decoded16[..64 * 48]
                    .iter()
                    .all(|value| value.abs_diff(expected16) <= 32),
                "HDR precision or PQ range was lost"
            );
        }
        eprintln!(
            "decode16 Y {}..{}",
            decoded16[..64 * 48].iter().min().unwrap(),
            decoded16[..64 * 48].iter().max().unwrap()
        );
        eprintln!(
            "iteration {iteration} hdr={hdr} expected={expected} luma {}..{} chroma {}..{} bytes={}",
            decoded[..64 * 48].iter().min().unwrap(),
            decoded[..64 * 48].iter().max().unwrap(),
            decoded[64 * 48..].iter().min().unwrap(),
            decoded[64 * 48..].iter().max().unwrap(),
            received.bytes.len()
        );
        assert!(
            decoded[..64 * 48]
                .iter()
                .all(|value| value.abs_diff(expected) <= 2),
            "GPU source pixels were not preserved"
        );
        assert_eq!(received.sequence, 1);
        assert_eq!(received.timestamp, 100);
        assert_eq!(
            request(
                &state,
                &mut module,
                "video",
                json!({"action":"close","session_id":id})
            )["ok"],
            true
        );
    }
    drop(consumer);
    drop(provider);
    drop(module);
}

#[test]
#[ignore = "requires native PyroWave, matching GBM/Vulkan GPU and render-node access"]
fn every_renderable_advertised_modifier_preserves_pixels_and_hdr_color() {
    let path = PathBuf::from(
        std::env::var_os("AMATERASU_PYROWAVE_LIBRARY").expect("pinned native library"),
    );
    let runtime = library::Runtime::load_gpu(&path).unwrap();
    let gpu = runtime.gpu_context().unwrap();
    let mut checked = 0;
    for hdr in [false, true] {
        for caps in gpu.offers(64, 48, hdr) {
            let fixture = match Image::create_capability(gpu, 64, 48, hdr, caps, [0.25; 3]) {
                Ok(fixture) => fixture,
                Err(reason) => {
                    eprintln!(
                        "fixture unavailable {:x}/{:x}: {reason}",
                        caps.fourcc, caps.modifier
                    );
                    continue;
                }
            };
            let mut encoder = runtime.encoder(64, 48).unwrap();
            // SAFETY: fixture owns immutable rendered storage and all fences
            // through the complete native GPU import and retirement operation.
            let bytes = unsafe { encoder.encode_gpu(&fixture.frame, hdr, hdr, 200_000) }.unwrap();
            drop(fixture);
            drop(encoder);
            let decoded = runtime.decode(&bytes, 64, 48).unwrap();
            assert!(
                decoded[..64 * 48]
                    .iter()
                    .all(|value| value.abs_diff(64) <= 2),
                "pixel corruption on {:x}/{:x}",
                caps.fourcc,
                caps.modifier
            );
            checked += 1;
        }
    }
    assert!(checked >= 2);
    let color = [0.75, 0.10, 0.0];
    let fixture = gpu
        .offers(64, 48, true)
        .into_iter()
        .find_map(|caps| Image::create_capability(gpu, 64, 48, true, caps, color).ok())
        .expect("renderable HDR color fixture");
    let mut encoder = runtime.encoder(64, 48).unwrap();
    // SAFETY: colored fixture storage and its acquire fences stay borrowed.
    let bytes = unsafe { encoder.encode_gpu(&fixture.frame, true, true, 200_000) }.unwrap();
    drop(fixture);
    drop(encoder);
    let decoded = runtime.decode16(&bytes, 64, 48).unwrap();
    let [r, g, b] = color.map(|component| (component as f64 * 1023.0).round() / 1023.0);
    let midpoint = 512.0 / 1023.0;
    let expected = [
        0.2627 * r + 0.678 * g + 0.0593 * b,
        -0.13963 * r - 0.36037 * g + 0.5 * b + midpoint,
        0.5 * r - 0.459786 * g - 0.0402143 * b + midpoint,
    ];
    let luma = 64 * 48;
    for (plane, range) in [
        0..luma,
        luma..luma + luma / 4,
        luma + luma / 4..luma * 3 / 2,
    ]
    .into_iter()
    .enumerate()
    {
        let target = (expected[plane].clamp(0.0, 1.0) * 65535.0).round() as u16;
        assert!(
            decoded[range]
                .iter()
                .all(|value| value.abs_diff(target) <= 64),
            "HDR BT.2020 plane {plane} target {target}"
        );
    }
    eprintln!("validated {checked} actual rendered format/modifier imports and HDR BT.2020 color");
}

#[test]
#[ignore = "requires pinned PyroWave and GPU memory for 1080p/4K HDR import checks"]
fn gpu_import_preserves_hdr_at_stream_dimensions_and_rejects_non_fence_acquires() {
    use std::{fs::File, os::fd::AsRawFd};
    let path = PathBuf::from(
        std::env::var_os("AMATERASU_PYROWAVE_LIBRARY").expect("pinned native library"),
    );
    let runtime = library::Runtime::load_gpu(&path).unwrap();
    let gpu = runtime.gpu_context().unwrap();
    for (width, height) in [(1920, 1080), (3840, 2160)] {
        let fixture = Image::create(gpu, width, height, true, 940.0 / 1023.0).unwrap();
        let mut encoder = runtime.encoder(width, height).unwrap();
        let not_a_fence = File::open("/dev/null").unwrap();
        let mut malformed = fixture.frame;
        malformed.planes[0].acquire_fence_fd = not_a_fence.as_raw_fd();
        // SAFETY: the source is an immutable GPU allocation. The invalid sync
        // file must be rejected by validation before it can be sampled.
        assert!(unsafe { encoder.encode_gpu(&malformed, true, true, 1_000_000) }.is_err());
        assert!(
            not_a_fence.metadata().is_ok(),
            "borrowed fence fd was consumed"
        );
        // SAFETY: fixture and every real acquire fence remain live until all
        // import reads have retired; only encoded test output is read back.
        let bytes = unsafe { encoder.encode_gpu(&fixture.frame, true, true, 1_000_000) }.unwrap();
        drop(fixture);
        drop(encoder);
        let decoded = runtime.decode16(&bytes, width, height).unwrap();
        let target = (940.0f64 / 1023.0 * 65535.0).round() as u16;
        let pixels = (width * height) as usize;
        assert!(
            decoded[..pixels]
                .iter()
                .all(|value| value.abs_diff(target) <= 64),
            "HDR precision lost at {width}x{height}"
        );
        eprintln!(
            "qualified direct HDR GPU import and invalid-acquire rejection at {width}x{height}"
        );
    }
}

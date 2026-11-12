use super::*;
use amaterasu_module_sdk::abi::{AbiHeader, NativeInterface};
use std::time::Duration;

fn configuration(width: u32, height: u32, color_mode: ColorMode) -> Configuration {
    serde_json::from_value(serde_json::json!({"width":width,"height":height,"fps":60,
        "bitrate":1000000,"color_mode":color_mode}))
    .unwrap()
}
fn solid_job(width: u32, height: u32, color: [u8; 4], target: Configuration) -> Job {
    Job {
        video: 1,
        configuration: target,
        source: OwnedFrame {
            width,
            height,
            stride: width * 4,
            format: PIXEL_BGRA8,
            bytes: color.repeat(width as usize * height as usize),
        },
    }
}

#[test]
fn native_conversion_preserves_limited_and_full_range_levels_and_matrix() {
    let mut converter = Converter::new().unwrap();
    for (mode, black, white, red) in [
        (ColorMode::Bt709Limited, 16, 235, 63),
        (ColorMode::Bt709Full, 0, 255, 54),
        (ColorMode::Bt601Limited, 16, 235, 81),
        (ColorMode::Bt601Full, 0, 255, 76),
    ] {
        for (color, luma) in [
            ([0, 0, 0, 255], black),
            ([255; 4], white),
            ([0, 0, 255, 255], red),
        ] {
            let frame = converter
                .convert(&solid_job(32, 32, color, configuration(16, 16, mode)))
                .unwrap();
            assert_eq!(
                (frame.width, frame.height, frame.stride, frame.format),
                (16, 16, 16, PIXEL_NV12)
            );
            assert!(
                frame.bytes[..256]
                    .iter()
                    .all(|value| value.abs_diff(luma) <= 1)
            );
        }
    }
}

#[test]
fn wide_desktop_is_fitted_inside_neutral_black_bars() {
    let mut converter = Converter::new().unwrap();
    let frame = converter
        .convert(&solid_job(
            32,
            16,
            [255; 4],
            configuration(16, 16, ColorMode::Bt709Limited),
        ))
        .unwrap();
    for y in 0..16 {
        let expected = if (4..12).contains(&y) { 235 } else { 16 };
        assert!(
            frame.bytes[y * 16..(y + 1) * 16]
                .iter()
                .all(|value| value.abs_diff(expected) <= 1)
        );
    }
    assert!(
        frame.bytes[256..]
            .iter()
            .all(|value| value.abs_diff(128) <= 1)
    );
}

struct PausedConverter {
    release: Receiver<()>,
    entered: SyncSender<()>,
}
impl FrameConverter for PausedConverter {
    fn convert(&mut self, job: &Job) -> Result<OwnedFrame, &'static str> {
        self.entered.send(()).unwrap();
        self.release.recv().unwrap();
        Ok(OwnedFrame {
            width: job.configuration.width,
            height: job.configuration.height,
            format: PIXEL_NV12,
            stride: job.configuration.width,
            bytes: vec![128; 16 * 16 * 3 / 2],
        })
    }
}

unsafe extern "C" fn capture(context: *mut c_void, _: u64, sink: FrameSink) -> Status {
    // SAFETY: the test supplies a live counter and the sink borrows these pixels
    // only for the synchronous callback.
    unsafe {
        *context.cast::<usize>() += 1;
    }
    let pixels = [255; 32 * 32 * 4];
    let frame = CpuFrame {
        width: 32,
        height: 32,
        stride: 128,
        format: PIXEL_BGRA8,
        data: pixels.as_ptr(),
        len: pixels.len(),
        timestamp_us: 1,
        sequence: 1,
    };
    unsafe { sink.send(&frame) }
}
unsafe extern "C" fn accept(
    context: *mut c_void,
    _: u64,
    frame: *const CpuFrame,
    _: u32,
) -> Status {
    // SAFETY: this test retains the frame and a live writable observation vector
    // throughout synchronous encoder submission.
    let frame = unsafe { &*frame };
    unsafe { &mut *context.cast::<Vec<(u64, u64, usize)>>() }.push((
        frame.sequence,
        frame.timestamp_us,
        frame.len,
    ));
    STATUS_OK
}
unsafe extern "C" fn receive(_: *mut c_void, _: u64, _: amaterasu_media::PacketSink) -> Status {
    MEDIA_PENDING
}
unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
    STATUS_OK
}

#[test]
fn paused_conversion_keeps_capture_pump_responsive_and_discards_previous_stream() {
    let (release, wait) = mpsc::sync_channel(1);
    let (entered, ready) = mpsc::sync_channel(1);
    let mut processor = Processor::with_converter(move || {
        Ok(PausedConverter {
            release: wait,
            entered,
        })
    })
    .unwrap();
    let mut captures = 0usize;
    let mut observed = Vec::<(u64, u64, usize)>::new();
    let capture_api = CaptureApiPrefix {
        base: NativeInterface {
            header: AbiHeader::of::<CaptureApiPrefix>(),
            context: (&mut captures as *mut usize).cast(),
            release: None,
        },
        read_frame: capture,
    };
    let encoder_api = EncoderApi {
        base: NativeInterface {
            header: AbiHeader::of::<EncoderApi>(),
            context: (&mut observed as *mut Vec<(u64, u64, usize)>).cast(),
            release: None,
        },
        submit: accept,
        receive,
        drain,
    };
    let config = configuration(16, 16, ColorMode::Bt709Limited);
    let mut input = Pump {
        video: 1,
        provider: crate::model::CaptureProvider::Pipewire,
        frame_path: crate::plane::FramePath::Cpu,
        capture: 1,
        encoder: 1,
        configuration: &config,
        capture_due: true,
        constant_rate: false,
        repeat_due: false,
        repeat_frame: None,
        timestamp_us: 3,
        sequence: 2,
        force_idr: false,
    };
    assert!(
        !processor
            .pump(&capture_api, &encoder_api, &input)
            .unwrap()
            .submitted
    );
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    let started = Instant::now();
    assert!(
        !processor
            .pump(&capture_api, &encoder_api, &input)
            .unwrap()
            .submitted
    );
    assert!(started.elapsed() < Duration::from_millis(50));
    assert!(
        processor
            .pump(&capture_api, &encoder_api, &input)
            .unwrap()
            .capture_deferred
    );
    assert_eq!(
        captures, 1,
        "a second frame was queued behind blocked conversion"
    );
    input.video = 2;
    input.capture_due = false;
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while processor.outstanding {
        assert!(
            !processor
                .pump(&capture_api, &encoder_api, &input)
                .unwrap()
                .submitted
        );
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    assert!(
        observed.is_empty(),
        "pixels crossed a stream ownership boundary"
    );
    input.capture_due = true;
    processor.pump(&capture_api, &encoder_api, &input).unwrap();
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    input.capture_due = false;
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let result = processor.pump(&capture_api, &encoder_api, &input).unwrap();
        if result.submitted {
            assert!(result.owned_frame.is_some());
            break;
        }
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    assert_eq!(observed, [(2, 3, 16 * 16 * 3 / 2)]);
}

#[test]
fn concurrent_streams_keep_independent_workers_and_release_only_their_own_pixels() {
    let mut processors = Processors::default();
    let mut captures = 0usize;
    let mut observed = Vec::<(u64, u64, usize)>::new();
    let capture_api = CaptureApiPrefix {
        base: NativeInterface {
            header: AbiHeader::of::<CaptureApiPrefix>(),
            context: (&mut captures as *mut usize).cast(),
            release: None,
        },
        read_frame: capture,
    };
    let encoder_api = EncoderApi {
        base: NativeInterface {
            header: AbiHeader::of::<EncoderApi>(),
            context: (&mut observed as *mut Vec<(u64, u64, usize)>).cast(),
            release: None,
        },
        submit: accept,
        receive,
        drain,
    };
    let config = configuration(16, 16, ColorMode::Bt709Limited);
    let mut input = Pump {
        video: 1,
        provider: crate::model::CaptureProvider::Pipewire,
        frame_path: crate::plane::FramePath::Cpu,
        capture: 1,
        encoder: 1,
        configuration: &config,
        capture_due: true,
        constant_rate: false,
        repeat_due: false,
        repeat_frame: None,
        timestamp_us: 1,
        sequence: 1,
        force_idr: false,
    };
    for video in [1, 2] {
        input.video = video;
        input.encoder = video;
        input.sequence = video;
        processors.pump(&capture_api, &encoder_api, &input).unwrap();
    }
    input.capture_due = false;
    let deadline = Instant::now() + Duration::from_secs(1);
    while observed.len() < 2 {
        for video in [1, 2] {
            input.video = video;
            input.encoder = video;
            input.sequence = video;
            processors.pump(&capture_api, &encoder_api, &input).unwrap();
        }
        assert!(Instant::now() < deadline, "one stream starved the other");
        thread::yield_now();
    }
    observed.sort_unstable();
    assert_eq!(observed, [(1, 1, 384), (2, 1, 384)]);
    assert_eq!(captures, 2);
    processors.remove(1);
    assert_eq!(processors.streams.keys().copied().collect::<Vec<_>>(), [2]);
    input.video = 2;
    input.encoder = 3;
    processors.pump(&capture_api, &encoder_api, &input).unwrap();
    assert_eq!(processors.streams[&2].0, 3);
    assert_eq!(observed.len(), 2, "old encoder pixels were resubmitted");
}

#[test]
#[ignore = "CPU throughput benchmark for real 4K-to-1080p conversion"]
fn benchmark_real_four_k_desktop_conversion() {
    let mut converter = Converter::new().unwrap();
    let job = solid_job(
        3840,
        2160,
        [32, 64, 128, 255],
        configuration(1920, 1080, ColorMode::Bt709Limited),
    );
    converter.convert(&job).unwrap();
    let started = Instant::now();
    for _ in 0..120 {
        assert_eq!(
            converter.convert(&job).unwrap().bytes.len(),
            1920 * 1080 * 3 / 2
        );
    }
    eprintln!(
        "120 real 4K-to-1080p NV12 conversions: {:?}",
        started.elapsed()
    );
}

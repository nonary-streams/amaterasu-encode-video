//! Hardware qualification for the NVENC/CUDA backend. Every test needs an
//! NVIDIA GPU with NVENC and is ignored by default. Decoding uses the system
//! `ffmpeg`/`ffprobe` with software decoders. Run them serially:
//!
//! ```text
//! cargo test -p amaterasu-encode --features linux-nvenc-cuda nvenc_cuda_gpu -- --ignored --test-threads=1
//! ```
//!
//! The DMA-BUF test allocates on `AMATERASU_TEST_RENDER_NODE` (default
//! `/dev/dri/renderD128`), which must be the NVIDIA GPU's render node.
use super::{
    codec::Codec,
    runtime::{
        Encoder, TestBuffer, amaterasu_nvcuda_test_allocate_gbm, amaterasu_nvcuda_test_release_gbm,
    },
};
use crate::{
    input::GpuInput,
    layout,
    model::{Configuration, EncoderDescription, Packet},
    sessions::Sessions,
};
use amaterasu_abi::{STATUS_OK, Status};
use amaterasu_media::*;
use serde_json::Value;
use std::{
    ffi::{CString, c_void},
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const FRAMES: u64 = 30;
const SECOND_IDR: u64 = 15;
/// Top and bottom halves of every synthetic frame, as 0x00RRGGBB.
const TOP_XRGB: u32 = 0x00c0_4020;
const BOTTOM_XRGB: u32 = 0x0020_80e0;

fn published() -> Vec<EncoderDescription> {
    let published = super::descriptions();
    assert!(!published.is_empty(), "no NVENC/CUDA encoder was published");
    published
}

fn configuration(codec: Codec, color_mode: ColorMode) -> Configuration {
    let codec = match codec {
        Codec::H264 => VideoCodec::H264,
        Codec::Hevc => VideoCodec::Hevc,
        Codec::Av1 => VideoCodec::Av1,
    };
    let mut configuration: Configuration = serde_json::from_value(serde_json::json!({
        "width": WIDTH, "height": HEIGHT, "fps": 30, "bitrate": 4_000_000,
        "max_reference_frames": 1
    }))
    .unwrap();
    configuration.codec = codec;
    configuration.color_mode = color_mode;
    configuration
}

fn bgra_frame() -> Vec<u8> {
    let mut bytes = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for row in 0..HEIGHT {
        let xrgb = if row < HEIGHT / 2 {
            TOP_XRGB
        } else {
            BOTTOM_XRGB
        };
        for _ in 0..WIDTH {
            bytes.extend_from_slice(&xrgb.to_le_bytes());
        }
    }
    bytes
}

/// CPU reference Y, Cb, Cr for one solid colour.
fn expected_yuv(xrgb: u32, mode: ColorMode) -> [u8; 3] {
    let block: Vec<u8> = (0..4).flat_map(|_| xrgb.to_le_bytes()).collect();
    let nv12 = layout::bgra(&block, 2, 2, 8, mode).unwrap();
    [nv12[0], nv12[4], nv12[5]]
}

fn encode_cpu_frames(codec: Codec, mode: ColorMode, nv12_input: bool) -> Vec<Packet> {
    let description = published()
        .into_iter()
        .find(|entry| entry.codec == codec.token())
        .unwrap_or_else(|| panic!("{} is not published", codec.token()));
    let mut encoder = Encoder::open(&description.identity, &configuration(codec, mode))
        .unwrap_or_else(|error| panic!("{error}: {:?}", super::last_open_failure()));
    let bgra = bgra_frame();
    let nv12 = layout::bgra(
        &bgra,
        WIDTH as usize,
        HEIGHT as usize,
        (WIDTH * 4) as usize,
        mode,
    )
    .unwrap();
    // Collect output between submissions, as the session worker does, so the
    // bounded packet queue never fills.
    let mut packets = Vec::new();
    for frame in 0..FRAMES {
        let timestamp = (frame * 333_333) as i64;
        let force = frame == 0 || frame == SECOND_IDR;
        if nv12_input {
            encoder.submit(nv12.clone(), timestamp, force).unwrap();
        } else {
            encoder
                .submit_gpu(GpuInput::Bgra(bgra.clone()), timestamp, force)
                .unwrap();
        }
        packets.extend(std::iter::from_fn(|| encoder.receive().unwrap()));
    }
    encoder.drain().unwrap();
    packets.extend(std::iter::from_fn(|| encoder.receive().unwrap()));
    packets
}

fn assert_packet_sequence(packets: &[Packet]) {
    assert_eq!(packets.len() as u64, FRAMES);
    for (index, packet) in packets.iter().enumerate() {
        let expected_key = index == 0 || index as u64 == SECOND_IDR;
        assert_eq!(packet.keyframe, expected_key, "packet {index}");
        assert_eq!(packet.timestamp_100ns, index as i64 * 333_333);
    }
}

fn stream_file(codec: Codec, label: &str, packets: impl IntoIterator<Item = Vec<u8>>) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "amaterasu-nvenc-cuda-{}-{label}-{}.bin",
        codec.token(),
        std::process::id()
    ));
    let bytes: Vec<u8> = packets.into_iter().flatten().collect();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn demuxer(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Av1 => "obu",
    }
}

fn probe(codec: Codec, path: &PathBuf) -> Value {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_streams",
            "-of",
            "json",
            "-f",
            demuxer(codec),
        ])
        .arg(path)
        .output()
        .expect("ffprobe must be installed");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    report["streams"][0].clone()
}

fn assert_stream_signal(codec: Codec, mode: ColorMode, stream: &Value) {
    let (space, range) = match mode {
        ColorMode::Bt601Limited => ("smpte170m", "tv"),
        ColorMode::Bt601Full => ("smpte170m", "pc"),
        ColorMode::Bt709Limited => ("bt709", "tv"),
        ColorMode::Bt709Full => ("bt709", "pc"),
    };
    assert_eq!(stream["width"], WIDTH, "{stream}");
    assert_eq!(stream["height"], HEIGHT, "{stream}");
    // FFmpeg may report full-range 4:2:0 as the legacy yuvj420p format.
    assert!(
        matches!(stream["pix_fmt"].as_str(), Some("yuv420p" | "yuvj420p")),
        "{stream}"
    );
    assert_eq!(stream["color_space"], space, "{stream}");
    assert_eq!(stream["color_primaries"], space, "{stream}");
    assert_eq!(
        stream["color_transfer"],
        if space == "bt709" {
            "bt709"
        } else {
            "smpte170m"
        },
        "{stream}"
    );
    assert_eq!(stream["color_range"], range, "{stream}");
    let profile = stream["profile"].as_str().unwrap_or_default();
    match codec {
        Codec::H264 => {
            assert!(profile.contains("Baseline"), "{stream}");
            // FFprobe 9 no longer reports `refs`; older versions still do.
            if !stream["refs"].is_null() {
                assert_eq!(stream["refs"], 1, "{stream}");
            }
        }
        Codec::Hevc | Codec::Av1 => assert_eq!(profile, "Main", "{stream}"),
    }
    assert_eq!(stream["has_b_frames"].as_u64().unwrap_or(0), 0, "{stream}");
}

/// Reads `max_num_ref_frames` from every H.264 SPS with FFmpeg's
/// trace_headers bitstream filter, independent of ffprobe's stream fields.
fn h264_reference_frame_limits(path: &PathBuf) -> Vec<u64> {
    let output = Command::new("ffmpeg")
        .args(["-v", "trace", "-f", "h264", "-i"])
        .arg(path)
        .args(["-c", "copy", "-bsf:v", "trace_headers", "-f", "null", "-"])
        .output()
        .expect("ffmpeg must be installed");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| line.contains(" max_num_ref_frames "))
        .filter_map(|line| line.rsplit("= ").next()?.trim().parse().ok())
        .collect()
}

/// Decodes with a software decoder and returns raw I420 frames.
fn decode(codec: Codec, path: &PathBuf) -> Vec<Vec<u8>> {
    let mut command = Command::new("ffmpeg");
    command.args(["-v", "error", "-f", demuxer(codec)]);
    if codec == Codec::Av1 {
        command.args(["-c:v", "libdav1d"]);
    }
    let output = command
        .arg("-i")
        .arg(path)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
        .output()
        .expect("ffmpeg must be installed");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let frame_bytes = (WIDTH * HEIGHT * 3 / 2) as usize;
    assert_eq!(output.stdout.len() % frame_bytes, 0);
    output
        .stdout
        .chunks(frame_bytes)
        .map(<[u8]>::to_vec)
        .collect()
}

/// Mean Y, U, V over rows [first, last) of a decoded I420 frame.
fn region_mean(frame: &[u8], first_row: u32, last_row: u32) -> [f64; 3] {
    let (width, height) = (WIDTH as usize, HEIGHT as usize);
    let luma: Vec<u8> = (first_row as usize..last_row as usize)
        .flat_map(|row| frame[row * width..][..width].iter().copied())
        .collect();
    let chroma_rows = first_row as usize / 2..last_row as usize / 2;
    let plane = |offset: usize| -> Vec<u8> {
        chroma_rows
            .clone()
            .flat_map(|row| {
                frame[offset + row * width / 2..][..width / 2]
                    .iter()
                    .copied()
            })
            .collect()
    };
    let mean = |values: Vec<u8>| {
        values.iter().map(|value| f64::from(*value)).sum::<f64>() / values.len() as f64
    };
    [
        mean(luma),
        mean(plane(width * height)),
        mean(plane(width * height + width * height / 4)),
    ]
}

/// Compares the halves away from the boundary with the CPU reference.
fn assert_decoded_colours(frames: &[Vec<u8>], mode: ColorMode) {
    assert_eq!(frames.len() as u64, FRAMES);
    let last = frames.last().unwrap();
    for (rows, xrgb) in [
        ((16, HEIGHT / 2 - 16), TOP_XRGB),
        ((HEIGHT / 2 + 16, HEIGHT - 16), BOTTOM_XRGB),
    ] {
        let measured = region_mean(last, rows.0, rows.1);
        let expected = expected_yuv(xrgb, mode);
        for (plane, (measured, expected)) in measured.iter().zip(expected).enumerate() {
            assert!(
                (measured - f64::from(expected)).abs() <= 4.0,
                "{mode:?} rows {rows:?} plane {plane}: decoded {measured:.1}, expected {expected}"
            );
        }
    }
}

#[test]
#[ignore = "requires an NVIDIA GPU with NVENC"]
fn nvenc_cuda_gpu_publishes_codecs_with_cpu_input() {
    for description in published() {
        eprintln!(
            "{} {} {:?} {:?}",
            description.identity.clsid,
            description
                .identity
                .hardware_url
                .as_deref()
                .unwrap_or_default(),
            description.capabilities.inputs,
            (
                description.capabilities.max_width,
                description.capabilities.max_height
            )
        );
        assert!(description.capabilities.inputs.contains(&"cpu"));
    }
}

#[test]
#[ignore = "requires an NVIDIA GPU with NVENC"]
fn nvenc_cuda_gpu_encodes_synthetic_frames_that_ffmpeg_decodes() {
    let published = published();
    for codec in Codec::ALL {
        if !published.iter().any(|entry| entry.codec == codec.token()) {
            eprintln!("skipping {}: not supported by this GPU", codec.token());
            continue;
        }
        let cases = [
            (ColorMode::Bt709Limited, false),
            (ColorMode::Bt709Full, false),
            (ColorMode::Bt601Limited, false),
            (ColorMode::Bt601Full, false),
            (ColorMode::Bt709Limited, true),
        ];
        for (mode, nv12_input) in cases {
            let label = format!("{mode:?}-{}", if nv12_input { "nv12" } else { "bgra" });
            let packets = encode_cpu_frames(codec, mode, nv12_input);
            assert_packet_sequence(&packets);
            let path = stream_file(
                codec,
                &label,
                packets.into_iter().map(|packet| packet.bytes),
            );
            assert_stream_signal(codec, mode, &probe(codec, &path));
            if codec == Codec::H264 {
                let limits = h264_reference_frame_limits(&path);
                assert!(!limits.is_empty() && limits.iter().all(|limit| *limit == 1));
            }
            assert_decoded_colours(&decode(codec, &path), mode);
            std::fs::remove_file(path).unwrap();
        }
    }
}

struct GbmBuffer(TestBuffer);

impl GbmBuffer {
    fn allocate() -> Self {
        let node = std::env::var("AMATERASU_TEST_RENDER_NODE")
            .unwrap_or_else(|_| "/dev/dri/renderD128".into());
        let node = CString::new(node).unwrap();
        let mut buffer = TestBuffer {
            device: std::ptr::null_mut(),
            buffer: std::ptr::null_mut(),
            render_fd: -1,
            dmabuf_fd: -1,
            pitch: 0,
            offset: 0,
            modifier: 0,
        };
        // SAFETY: test-only allocation into a live record released by Drop.
        let result = unsafe {
            amaterasu_nvcuda_test_allocate_gbm(
                node.as_ptr(),
                WIDTH,
                HEIGHT,
                TOP_XRGB,
                BOTTOM_XRGB,
                &mut buffer,
            )
        };
        assert_eq!(result, 0, "GBM allocation failed");
        Self(buffer)
    }

    fn frame(&self, sequence: u64) -> DmaBufFrame {
        let mut planes = [DmaBufPlane::UNUSED; DMABUF_MAX_PLANES];
        planes[0] = DmaBufPlane {
            fd: self.0.dmabuf_fd,
            acquire_fence_fd: -1,
            offset: self.0.offset,
            pitch: self.0.pitch,
        };
        DmaBufFrame {
            width: WIDTH,
            height: HEIGHT,
            fourcc: u32::from_le_bytes(*b"XR24"),
            plane_count: 1,
            modifier: self.0.modifier,
            planes,
            color: DMABUF_COLOR_SRGB,
            reserved: 0,
            timestamp_us: sequence * 33_333,
            sequence,
        }
    }
}

impl Drop for GbmBuffer {
    fn drop(&mut self) {
        // SAFETY: releases the record allocated in `allocate` exactly once.
        unsafe { amaterasu_nvcuda_test_release_gbm(&mut self.0) };
    }
}

unsafe extern "C" fn collect_packet(context: *mut c_void, packet: *const EncodedPacket) -> Status {
    // SAFETY: the test passes a live Vec as context and the session passes a
    // packet valid for this call; the bytes are copied before returning.
    let (packets, packet) = unsafe { (&mut *context.cast::<Vec<(u32, Vec<u8>)>>(), &*packet) };
    let bytes = if packet.len == 0 {
        Vec::new()
    } else {
        // SAFETY: data/len describe the borrowed packet for this call.
        unsafe { std::slice::from_raw_parts(packet.data, packet.len) }.to_vec()
    };
    packets.push((packet.flags, bytes));
    STATUS_OK
}

#[test]
#[ignore = "requires an NVIDIA GPU with NVENC and a GBM render node on it"]
fn nvenc_cuda_gpu_imports_a_gbm_dmabuf_through_the_native_session() {
    let description = published()
        .into_iter()
        .find(|entry| entry.capabilities.inputs.contains(&"dmabuf"))
        .expect("no NVENC/CUDA encoder advertises DMA-BUF input");
    let codec = Codec::from_clsid(&description.identity.clsid).unwrap();
    let mode = ColorMode::Bt709Limited;
    let mut sessions = Sessions::new(true);
    let reply = sessions
        .open(
            "gpu-test",
            description.identity.clone(),
            configuration(codec, mode),
            None,
            false,
        )
        .unwrap();
    let id = reply["session_id"].as_u64().unwrap();
    let buffer = GbmBuffer::allocate();
    let mut packets: Vec<(u32, Vec<u8>)> = Vec::new();
    let sink = PacketSink {
        context: (&mut packets as *mut Vec<(u32, Vec<u8>)>).cast(),
        write: collect_packet,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    for sequence in 1..=FRAMES {
        let frame = buffer.frame(sequence);
        let force = u32::from(sequence == 1);
        loop {
            // SAFETY: the GBM descriptor stays open for the call.
            let status = unsafe { sessions.submit_dmabuf("gpu-test", id, &frame, force) };
            if status == STATUS_OK {
                break;
            }
            assert_eq!(status, MEDIA_BUSY, "submit_dmabuf failed");
            // SAFETY: the sink context outlives the call.
            let _ = unsafe { sessions.receive("gpu-test", id, sink) };
            assert!(Instant::now() < deadline);
        }
    }
    while sessions.drain("gpu-test", id) == MEDIA_BUSY {
        assert!(Instant::now() < deadline);
    }
    while !packets
        .iter()
        .any(|(flags, _)| flags & FLAG_END_OF_STREAM != 0)
    {
        // SAFETY: the sink context outlives the call.
        let status = unsafe { sessions.receive("gpu-test", id, sink) };
        assert!(
            matches!(status, STATUS_OK | MEDIA_PENDING),
            "receive {status}"
        );
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let encoded: Vec<Vec<u8>> = packets
        .into_iter()
        .filter(|(flags, _)| flags & FLAG_END_OF_STREAM == 0)
        .map(|(_, bytes)| bytes)
        .collect();
    assert_eq!(encoded.len() as u64, FRAMES);
    let path = stream_file(codec, "dmabuf", encoded);
    assert_stream_signal(codec, mode, &probe(codec, &path));
    assert_decoded_colours(&decode(codec, &path), mode);
    std::fs::remove_file(path).unwrap();
}

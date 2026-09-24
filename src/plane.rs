//! Native bindings stay on the module tick thread. Only copied packet queues cross threads.
use crate::{
    model::{CaptureProvider, Configuration},
    queue::Queues,
};
use amaterasu_media::*;
use amaterasu_module_sdk::{ModuleResult, NativeBinding, RuntimeClient, abi::*};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::sync::{Arc, Mutex};
pub const ENCODE: &str = "amaterasu.encode/1";
pub struct Pump<'a> {
    pub video: u64,
    pub provider: CaptureProvider,
    pub capture: u64,
    pub encoder: u64,
    pub configuration: &'a Configuration,
    pub capture_due: bool,
    pub force_idr: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GeometryProgress {
    Ready,
    Busy,
}
#[derive(Default)]
pub struct Progress {
    pub dropped: bool,
    pub submitted: bool,
    pub frame: Option<FrameMetadata>,
}

/// Non-secret metadata from a frame accepted by the encoder. The frame bytes
/// remain borrowed only during the native callback and are never retained.
#[derive(Clone, Copy)]
pub struct FrameMetadata {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub timestamp_us: u64,
    pub sequence: u64,
}
pub trait Plane {
    fn verify_geometry(
        &mut self,
        provider: CaptureProvider,
        capture: u64,
        expected: Option<CaptureGeometry>,
    ) -> Result<GeometryProgress, &'static str>;
    fn pump(&mut self, input: Pump<'_>) -> Result<Progress, &'static str>;
    fn drain(&mut self, encoder: u64) -> Result<bool, &'static str>;
}

pub struct NativePlane {
    captures: BTreeMap<CaptureProvider, NativeBinding>,
    encoder: NativeBinding,
    queues: Arc<Mutex<Queues>>,
}

impl NativePlane {
    pub fn bind(
        runtime: &RuntimeClient,
        queues: Arc<Mutex<Queues>>,
        providers: &BTreeSet<CaptureProvider>,
    ) -> ModuleResult<Self> {
        let mut captures = BTreeMap::new();
        // SAFETY: signed dependencies retain configured providers through binding
        // destruction. Calls stay on the module tick thread.
        unsafe {
            for provider in providers {
                captures.insert(
                    *provider,
                    runtime.bind_native(provider.service(), 1, 0, CAPTURE_API_V1_0_SIZE)?,
                );
            }
            Ok(Self {
                captures,
                encoder: runtime.bind_native(ENCODE, 1, 0, size_of::<EncoderApi>() as u32)?,
                queues,
            })
        }
    }
}

struct Submission<'a> {
    api: &'a EncoderApi,
    encoder: u64,
    configuration: &'a Configuration,
    status: Status,
    called: bool,
    force_idr: bool,
    frame: Option<FrameMetadata>,
}

unsafe extern "C" fn submit(context: *mut c_void, frame: *const CpuFrame) -> Status {
    if frame.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: stack context and capture frame live through synchronous read_frame.
    let submission = unsafe { &mut *context.cast::<Submission<'_>>() };
    let frame = unsafe { &*frame };
    submission.called = true;
    submission.status = if frame.validate().is_err()
        || frame.width != submission.configuration.width
        || frame.height != submission.configuration.height
    {
        STATUS_INVALID_ARGUMENT
    } else {
        submission.frame = Some(FrameMetadata {
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            timestamp_us: frame.timestamp_us,
            sequence: frame.sequence,
        });
        // SAFETY: validated borrowed capture frame remains live; encoder copies on acceptance.
        unsafe {
            (submission.api.submit)(
                submission.api.base.context,
                submission.encoder,
                frame,
                u32::from(submission.force_idr),
            )
        }
    };
    // Capture ownership ends here even on encoder backpressure: deliberately drop frame.
    STATUS_OK
}

struct Destination<'a> {
    queues: &'a Arc<Mutex<Queues>>,
    video: u64,
}

unsafe extern "C" fn packet(context: *mut c_void, packet: *const EncodedPacket) -> Status {
    if packet.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: encoder borrows packet; destination lives through synchronous receive.
    let destination = unsafe { &*context.cast::<Destination<'_>>() };
    let Ok(mut queues) = destination.queues.try_lock() else {
        return MEDIA_BUSY;
    };
    unsafe { queues.push(destination.video, &*packet) }
}

impl Plane for NativePlane {
    fn verify_geometry(
        &mut self,
        provider: CaptureProvider,
        capture: u64,
        expected: Option<CaptureGeometry>,
    ) -> Result<GeometryProgress, &'static str> {
        let binding = self
            .captures
            .get(&provider)
            .ok_or("capture_provider_unavailable")?;
        verify_capture_geometry(binding, capture, expected)
    }

    fn pump(&mut self, input: Pump<'_>) -> Result<Progress, &'static str> {
        // SAFETY: configured services promise these layouts; bindings remain live.
        let encoder_api =
            unsafe { self.encoder.table::<EncoderApi>() }.map_err(|_| "encoder_interface")?;
        let progress = if input.capture_due {
            let binding = self
                .captures
                .get(&input.provider)
                .ok_or("capture_provider_unavailable")?;
            // SAFETY: the v1.0 prefix was validated at bind time and read_frame is
            // part of that prefix, so old providers do not expose an optional tail.
            let capture_api =
                unsafe { binding.table::<CaptureApiPrefix>() }.map_err(|_| "capture_interface")?;
            capture(capture_api, encoder_api, &input)?
        } else {
            Progress::default()
        };
        receive(encoder_api, input.encoder, input.video, &self.queues)?;
        Ok(progress)
    }

    fn drain(&mut self, encoder: u64) -> Result<bool, &'static str> {
        // SAFETY: exact encoder v1 binding stays live through the synchronous call.
        let api = unsafe { self.encoder.table::<EncoderApi>() }.map_err(|_| "encoder_interface")?;
        match unsafe { (api.drain)(api.base.context, encoder) } {
            STATUS_OK => Ok(true),
            MEDIA_BUSY | MEDIA_PENDING => Ok(false),
            _ => Err("encoder_drain_failed"),
        }
    }
}

fn capture(
    capture: &CaptureApiPrefix,
    encoder: &EncoderApi,
    input: &Pump<'_>,
) -> Result<Progress, &'static str> {
    let mut submission = Submission {
        api: encoder,
        encoder: input.encoder,
        configuration: input.configuration,
        status: MEDIA_PENDING,
        called: false,
        force_idr: input.force_idr,
        frame: None,
    };
    let sink = FrameSink {
        context: (&mut submission as *mut Submission<'_>).cast(),
        write: submit,
    };
    // SAFETY: stack sink borrows one frame and encoder copies accepted input.
    let result = unsafe { (capture.read_frame)(capture.base.context, input.capture, sink) };
    if !matches!(result, STATUS_OK | MEDIA_PENDING | MEDIA_BUSY) {
        return Err("capture_failed");
    }
    if submission.called && !matches!(submission.status, STATUS_OK | MEDIA_BUSY) {
        return Err("frame_rejected");
    }
    Ok(Progress {
        dropped: submission.called && submission.status == MEDIA_BUSY,
        submitted: submission.called && submission.status == STATUS_OK,
        frame: (submission.called && submission.status == STATUS_OK)
            .then_some(submission.frame)
            .flatten(),
    })
}

fn verify_capture_geometry(
    binding: &NativeBinding,
    capture: u64,
    expected: Option<CaptureGeometry>,
) -> Result<GeometryProgress, &'static str> {
    // SAFETY: capture bindings were requested with the complete v1.0 prefix.
    let prefix = unsafe { binding.table::<CaptureApiPrefix>() }.map_err(|_| "capture_interface")?;
    let header = prefix.base.header;
    if !geometry_tail_available(header)? {
        return if expected.is_none() {
            Ok(GeometryProgress::Ready)
        } else {
            Err("capture_geometry_unavailable")
        };
    }
    // SAFETY: the header advertises the geometry minor and full table size.
    // NativeBinding additionally validates the advertised size and alignment.
    let api = unsafe { binding.table::<CaptureApi>() }.map_err(|_| "capture_interface")?;
    verify_geometry_callback(api, capture, expected)
}

fn geometry_tail_available(header: AbiHeader) -> Result<bool, &'static str> {
    if header.minor < CAPTURE_INTERFACE_MINOR {
        return Ok(false);
    }
    if header.size < size_of::<CaptureApi>() as u32 {
        return Err("capture_interface");
    }
    Ok(true)
}

fn verify_geometry_callback(
    api: &CaptureApi,
    capture: u64,
    expected: Option<CaptureGeometry>,
) -> Result<GeometryProgress, &'static str> {
    let mut actual = std::mem::MaybeUninit::<CaptureGeometry>::uninit();
    // SAFETY: the provider initializes `actual` exactly when returning STATUS_OK.
    let status = unsafe { (api.geometry)(api.base.context, capture, actual.as_mut_ptr()) };
    match status {
        STATUS_OK => {
            // SAFETY: STATUS_OK is the ABI promise that the output was initialized.
            let actual = unsafe { actual.assume_init() };
            if actual.validate().is_err() || expected != Some(actual) {
                Err("capture_geometry_changed")
            } else {
                Ok(GeometryProgress::Ready)
            }
        }
        MEDIA_BUSY => Ok(GeometryProgress::Busy),
        MEDIA_UNSUPPORTED if expected.is_none() => Ok(GeometryProgress::Ready),
        MEDIA_UNSUPPORTED => Err("capture_geometry_unavailable"),
        MEDIA_CLOSED => Err("capture_closed"),
        _ => Err("capture_geometry_failed"),
    }
}

fn receive(
    api: &EncoderApi,
    encoder: u64,
    video: u64,
    queues: &Arc<Mutex<Queues>>,
) -> Result<(), &'static str> {
    let mut destination = Destination { queues, video };
    let sink = PacketSink {
        context: (&mut destination as *mut Destination<'_>).cast(),
        write: packet,
    };
    // SAFETY: stack sink copies at most one bounded borrowed packet.
    match unsafe { (api.receive)(api.base.context, encoder, sink) } {
        STATUS_OK | MEDIA_PENDING | MEDIA_BUSY => Ok(()),
        _ => Err("encoder_failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_module_sdk::abi::AbiHeader;
    unsafe extern "C" fn busy(_: *mut c_void, _: u64, _: *const CpuFrame, _: u32) -> Status {
        MEDIA_BUSY
    }
    unsafe extern "C" fn receive(_: *mut c_void, _: u64, _: PacketSink) -> Status {
        MEDIA_PENDING
    }
    unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
        STATUS_OK
    }
    #[test]
    fn capture_callback_drops_busy_frame_and_rejects_size_mismatch() {
        let api = EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context: std::ptr::null_mut(),
                release: None,
            },
            submit: busy,
            receive,
            drain,
        };
        let configuration = Configuration {
            width: 16,
            height: 16,
            fps: 60,
            bitrate: 100_000,
            max_reference_frames: 0,
        };
        let mut submission = Submission {
            api: &api,
            encoder: 1,
            configuration: &configuration,
            status: MEDIA_PENDING,
            called: false,
            force_idr: false,
            frame: None,
        };
        let bytes = [0; 16 * 16 * 4];
        let mut frame = CpuFrame {
            width: 16,
            height: 16,
            format: PIXEL_BGRA8,
            stride: 64,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: 1,
            sequence: 1,
        };
        let context = (&mut submission as *mut Submission<'_>).cast();
        assert_eq!(unsafe { submit(context, &frame) }, STATUS_OK);
        assert_eq!(submission.status, MEDIA_BUSY);
        frame.width = 8;
        assert_eq!(unsafe { submit(context, &frame) }, STATUS_OK);
        assert_eq!(submission.status, STATUS_INVALID_ARGUMENT);
    }
    #[test]
    fn native_submission_passes_force_idr_as_exact_c_flag() {
        unsafe extern "C" fn flag(
            context: *mut c_void,
            _: u64,
            _: *const CpuFrame,
            force: u32,
        ) -> Status {
            // SAFETY: test encoder context points to a live writable flag.
            unsafe {
                *context.cast::<u32>() = force;
            }
            STATUS_OK
        }
        let mut observed = 0u32;
        let api = EncoderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderApi>(),
                context: (&mut observed as *mut u32).cast(),
                release: None,
            },
            submit: flag,
            receive,
            drain,
        };
        let configuration = Configuration {
            width: 16,
            height: 16,
            fps: 60,
            bitrate: 100000,
            max_reference_frames: 0,
        };
        let bytes = [0; 1024];
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
        let mut submission = Submission {
            api: &api,
            encoder: 1,
            configuration: &configuration,
            status: MEDIA_PENDING,
            called: false,
            force_idr: true,
            frame: None,
        };
        assert_eq!(
            unsafe { submit((&mut submission as *mut Submission<'_>).cast(), &frame) },
            STATUS_OK
        );
        assert_eq!(observed, 1);
        assert_eq!(submission.status, STATUS_OK);
    }

    unsafe extern "C" fn no_frame(_: *mut c_void, _: u64, _: FrameSink) -> Status {
        MEDIA_PENDING
    }

    #[test]
    fn old_short_capture_table_does_not_expose_geometry_tail() {
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
            read_frame: no_frame,
        };

        assert_eq!(geometry_tail_available(prefix.base.header), Ok(false));
        let expected = CaptureGeometry::new(
            DesktopRect::new(0, 0, 1920, 1080).unwrap(),
            DesktopRect::new(0, 0, 1920, 1080).unwrap(),
        )
        .unwrap();
        assert_eq!(expected.validate(), Ok(()));

        let truncated_current = AbiHeader {
            size: CAPTURE_API_V1_0_SIZE,
            major: 1,
            minor: CAPTURE_INTERFACE_MINOR,
        };
        assert_eq!(
            geometry_tail_available(truncated_current),
            Err("capture_interface")
        );
    }

    unsafe extern "C" fn report_geometry(
        context: *mut c_void,
        _: u64,
        output: *mut CaptureGeometry,
    ) -> Status {
        // SAFETY: the test context and output point to live CaptureGeometry storage.
        unsafe { output.write(*context.cast::<CaptureGeometry>()) };
        STATUS_OK
    }

    fn capture_api(geometry: &mut CaptureGeometry) -> CaptureApi {
        CaptureApi {
            base: NativeInterface {
                header: AbiHeader {
                    size: size_of::<CaptureApi>() as u32,
                    major: 1,
                    minor: CAPTURE_INTERFACE_MINOR,
                },
                context: (geometry as *mut CaptureGeometry).cast(),
                release: None,
            },
            read_frame: no_frame,
            geometry: report_geometry,
        }
    }

    #[test]
    fn geometry_tail_requires_valid_exact_snapshot() {
        let expected = CaptureGeometry::new(
            DesktopRect::new(-1920, 0, 1920, 1080).unwrap(),
            DesktopRect::new(-1920, -200, 3840, 1440).unwrap(),
        )
        .unwrap();
        let mut actual = expected;
        let api = capture_api(&mut actual);
        assert_eq!(
            verify_geometry_callback(&api, 1, Some(expected)),
            Ok(GeometryProgress::Ready)
        );

        let mut changed = expected;
        changed.output.left += 1;
        let changed_api = capture_api(&mut changed);
        assert_eq!(
            verify_geometry_callback(&changed_api, 1, Some(expected)),
            Err("capture_geometry_changed")
        );
    }

    unsafe extern "C" fn busy_geometry(_: *mut c_void, _: u64, _: *mut CaptureGeometry) -> Status {
        MEDIA_BUSY
    }

    #[test]
    fn busy_geometry_defers_without_reclassifying_topology() {
        let api = CaptureApi {
            base: NativeInterface {
                header: AbiHeader {
                    size: size_of::<CaptureApi>() as u32,
                    major: 1,
                    minor: CAPTURE_INTERFACE_MINOR,
                },
                context: std::ptr::null_mut(),
                release: None,
            },
            read_frame: no_frame,
            geometry: busy_geometry,
        };
        assert_eq!(
            verify_geometry_callback(&api, 1, None),
            Ok(GeometryProgress::Busy)
        );
    }
}

//! COM/MF objects stay on their creating worker thread. All pointers passed to
//! Windows reference live COM objects, stack outputs, or bounded owned buffers.
use crate::model::{Configuration, EncoderDescription, EncoderIdentity, Packet};
use std::{
    collections::VecDeque,
    mem::ManuallyDrop,
    sync::atomic::{AtomicI32, AtomicU8, AtomicU64, Ordering},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Media::MediaFoundation::*,
        System::{Com::*, Variant::VARIANT},
    },
    core::{Interface, PWSTR},
};

type NativeResult<T> = windows::core::Result<T>;
const STAGE_NONE: u8 = 0;
const STAGE_PROCESS_INPUT: u8 = 1;
const STAGE_PROCESS_OUTPUT: u8 = 2;
const STAGE_STREAM_CHANGE: u8 = 3;
const STAGE_DRAIN: u8 = 4;
const STAGE_EVENT: u8 = 5;
const STAGE_OPEN_RUNTIME: u8 = 6;
const STAGE_OPEN_ENUMERATE: u8 = 7;
const STAGE_OPEN_ACTIVATE: u8 = 8;
const STAGE_OPEN_ASYNC: u8 = 9;
const STAGE_OPEN_CODEC: u8 = 10;
const STAGE_OPEN_STREAMS: u8 = 11;
const STAGE_OPEN_INPUT_TYPE: u8 = 12;
const STAGE_OPEN_OUTPUT_TYPE: u8 = 13;
const STAGE_OPEN_START: u8 = 14;

static LAST_STAGE: AtomicU8 = AtomicU8::new(STAGE_NONE);
static LAST_OPEN_STAGE: AtomicU8 = AtomicU8::new(STAGE_OPEN_RUNTIME);
static LAST_HRESULT: AtomicI32 = AtomicI32::new(0);
static NEED_INPUT_EVENTS: AtomicU64 = AtomicU64::new(0);
static HAVE_OUTPUT_EVENTS: AtomicU64 = AtomicU64::new(0);
static OTHER_EVENTS: AtomicU64 = AtomicU64::new(0);
static PROCESS_INPUT_CALLS: AtomicU64 = AtomicU64::new(0);
static PROCESS_OUTPUT_CALLS: AtomicU64 = AtomicU64::new(0);

/// Non-secret Media Foundation failure detail for an owned diagnostics query.
/// It intentionally retains only an operation stage and HRESULT, never frame,
/// bitstream, endpoint, or display content.
#[derive(Clone, Copy, Debug)]
pub struct Diagnostic {
    pub stage: &'static str,
    pub hresult: u32,
}

/// Bounded event/call counters for the most recently opened encoder worker.
/// They contain no frames, media bytes, endpoints, or user identifiers.
#[derive(Clone, Copy)]
pub struct Metrics {
    pub need_input_events: u64,
    pub have_output_events: u64,
    pub other_events: u64,
    pub process_input_calls: u64,
    pub process_output_calls: u64,
}

pub fn metrics() -> Metrics {
    Metrics {
        need_input_events: NEED_INPUT_EVENTS.load(Ordering::Acquire),
        have_output_events: HAVE_OUTPUT_EVENTS.load(Ordering::Acquire),
        other_events: OTHER_EVENTS.load(Ordering::Acquire),
        process_input_calls: PROCESS_INPUT_CALLS.load(Ordering::Acquire),
        process_output_calls: PROCESS_OUTPUT_CALLS.load(Ordering::Acquire),
    }
}

pub fn last_failure() -> Option<Diagnostic> {
    let stage = LAST_STAGE.load(Ordering::Acquire);
    (stage != STAGE_NONE).then(|| Diagnostic {
        stage: stage_name(stage),
        hresult: LAST_HRESULT.load(Ordering::Acquire) as u32,
    })
}

fn clear_failure() {
    LAST_HRESULT.store(0, Ordering::Release);
    LAST_STAGE.store(STAGE_NONE, Ordering::Release);
    LAST_OPEN_STAGE.store(STAGE_OPEN_RUNTIME, Ordering::Release);
    NEED_INPUT_EVENTS.store(0, Ordering::Release);
    HAVE_OUTPUT_EVENTS.store(0, Ordering::Release);
    OTHER_EVENTS.store(0, Ordering::Release);
    PROCESS_INPUT_CALLS.store(0, Ordering::Release);
    PROCESS_OUTPUT_CALLS.store(0, Ordering::Release);
}

fn record_failure(stage: u8, error: &windows::core::Error) {
    LAST_HRESULT.store(error.code().0, Ordering::Release);
    LAST_STAGE.store(stage, Ordering::Release);
}

fn stage_name(stage: u8) -> &'static str {
    match stage {
        STAGE_PROCESS_INPUT => "process_input",
        STAGE_PROCESS_OUTPUT => "process_output",
        STAGE_STREAM_CHANGE => "stream_change",
        STAGE_DRAIN => "drain",
        STAGE_EVENT => "event",
        STAGE_OPEN_RUNTIME => "open_runtime",
        STAGE_OPEN_ENUMERATE => "open_enumerate",
        STAGE_OPEN_ACTIVATE => "open_activate",
        STAGE_OPEN_ASYNC => "open_async",
        STAGE_OPEN_CODEC => "open_codec",
        STAGE_OPEN_STREAMS => "open_streams",
        STAGE_OPEN_INPUT_TYPE => "open_input_type",
        STAGE_OPEN_OUTPUT_TYPE => "open_output_type",
        STAGE_OPEN_START => "open_start",
        _ => "unknown",
    }
}
struct Activation(IMFActivate);
impl Drop for Activation {
    fn drop(&mut self) {
        // SAFETY: activation is owned by this worker, including on setup failure.
        unsafe {
            let _ = self.0.ShutdownObject();
        }
    }
}

struct Runtime;
impl Runtime {
    fn start() -> NativeResult<Self> {
        // SAFETY: called on a dedicated thread and balanced on that thread.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                CoUninitialize();
                return Err(error);
            }
        }
        Ok(Self)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: all transforms are dropped before their runtime owner.
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

fn activations() -> NativeResult<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut pointer = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: the returned COM references are moved into Vec, then the task
    // allocation (only its array storage) is freed exactly once.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut pointer,
            &mut count,
        )?;
        let mut values = Vec::new();
        if !pointer.is_null() {
            for index in 0..count as usize {
                if let Some(value) = pointer.add(index).read() {
                    values.push(value);
                }
            }
            CoTaskMemFree(Some(pointer.cast()));
        }
        Ok(values)
    }
}

fn allocated_string(
    activation: &IMFActivate,
    key: &windows::core::GUID,
) -> NativeResult<Option<String>> {
    let mut value = PWSTR::null();
    let mut length = 0;
    // SAFETY: Media Foundation allocates the UTF-16 string. It is copied with
    // a bounded length and the task allocation is freed exactly once.
    unsafe {
        match activation.GetAllocatedString(key, &mut value, &mut length) {
            Ok(()) => {
                let text = (!value.is_null() && length <= 1024).then(|| {
                    String::from_utf16_lossy(std::slice::from_raw_parts(value.0, length as usize))
                });
                CoTaskMemFree(Some(value.0.cast()));
                Ok(text)
            }
            Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn description(activation: &IMFActivate) -> NativeResult<EncoderDescription> {
    let name = allocated_string(activation, &MFT_FRIENDLY_NAME_Attribute)?
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Hardware H.264 encoder".into());
    let clsid = unsafe { activation.GetGUID(&MFT_TRANSFORM_CLSID_Attribute)? };
    Ok(EncoderDescription {
        identity: EncoderIdentity {
            clsid: format!("{clsid:?}"),
            hardware_url: allocated_string(activation, &MFT_ENUM_HARDWARE_URL_Attribute)?,
            friendly_name: name.clone(),
        },
        name,
        codec: "h264",
        hardware: true,
        vendor_id: unsafe {
            activation
                .GetUINT32(&MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)
                .ok()
        },
        capabilities: Default::default(),
    })
}

fn matching_activation(
    values: Vec<IMFActivate>,
    identity: &EncoderIdentity,
) -> NativeResult<IMFActivate> {
    values
        .into_iter()
        .find_map(|activation| {
            description(&activation)
                .ok()
                .filter(|description| description.identity == *identity)
                .map(|_| activation)
        })
        .ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG))
}

#[cfg(test)]
fn matching_description<'a>(
    values: &'a [EncoderDescription],
    identity: &EncoderIdentity,
) -> Option<&'a EncoderDescription> {
    values
        .iter()
        .find(|description| description.identity == *identity)
}

pub fn enumerate() -> Result<Vec<EncoderDescription>, &'static str> {
    let _runtime = Runtime::start().map_err(|_| "media_foundation_unavailable")?;
    let values = activations().map_err(|_| "encoder_enumeration_failed")?;
    let mut result = Vec::new();
    for value in values {
        result.push(description(&value).map_err(|_| "encoder_enumeration_failed")?);
    }
    Ok(result)
}

pub struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    input: u32,
    output: u32,
    credits: u32,
    packets: VecDeque<Packet>,
    duration: i64,
    draining: bool,
    drained: bool,
    configuration: Configuration,
    normalizer: crate::bitstream::Normalizer,
    _activation: Activation,
    _runtime: Runtime,
}

impl Encoder {
    pub fn open(identity: &EncoderIdentity, config: &Configuration) -> Result<Self, &'static str> {
        clear_failure();
        if config.nvenc_h264_cavlc {
            return Err("nvenc_h264_cavlc_unsupported");
        }
        if config.nvenc_spatial_aq {
            return Err("nvenc_spatial_aq_unsupported");
        }
        if config.amd_av1_latency_mode != crate::model::AmdAv1LatencyMode::Auto {
            return Err("amd_av1_latency_mode_unsupported");
        }
        if config.nvenc_preset.is_some() {
            return Err("nvenc_preset_unsupported");
        }
        Self::create(identity, config).map_err(|error| {
            record_failure(LAST_OPEN_STAGE.load(Ordering::Acquire), &error);
            "encoder_open_failed"
        })
    }
    fn create(identity: &EncoderIdentity, config: &Configuration) -> NativeResult<Self> {
        LAST_OPEN_STAGE.store(STAGE_OPEN_RUNTIME, Ordering::Release);
        let runtime = Runtime::start()?;
        LAST_OPEN_STAGE.store(STAGE_OPEN_ENUMERATE, Ordering::Release);
        let values = activations()?;
        LAST_OPEN_STAGE.store(STAGE_OPEN_ACTIVATE, Ordering::Release);
        let activation = Activation(matching_activation(values, identity)?);
        // SAFETY: generated bindings own all COM references; sample buffers remain
        // owned until ProcessInput takes its own reference. No raw vtable casts.
        unsafe {
            let transform: IMFTransform = activation.0.ActivateObject()?;
            LAST_OPEN_STAGE.store(STAGE_OPEN_ASYNC, Ordering::Release);
            let attributes = transform.GetAttributes()?;
            if attributes.GetUINT32(&MF_TRANSFORM_ASYNC)? != 1 {
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_NOTIMPL,
                ));
            }
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let events = transform.cast()?;
            LAST_OPEN_STAGE.store(STAGE_OPEN_CODEC, Ordering::Release);
            let codec = transform.cast::<ICodecAPI>().ok();
            if let Some(codec) = &codec {
                // These are optional encoder hints. MFTs are permitted not to
                // expose them, and rejecting a hint must not make a valid
                // system-memory NV12/H.264 path unavailable.
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                let _ =
                    codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
                if config.max_reference_frames != 0 {
                    configure_reference_frames(codec, config.max_reference_frames)?;
                }
            } else if config.max_reference_frames != 0 {
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_NOTIMPL,
                ));
            }
            LAST_OPEN_STAGE.store(STAGE_OPEN_STREAMS, Ordering::Release);
            let mut inputs = [0];
            let mut outputs = [0];
            if let Err(error) = transform.GetStreamIDs(&mut inputs, &mut outputs)
                && error.code() != windows::Win32::Foundation::E_NOTIMPL
            {
                return Err(error);
            }
            let input_type = media_type(config, MFVideoFormat_NV12)?;
            input_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, config.width)?;
            let output_type = media_type(config, MFVideoFormat_H264)?;
            output_type.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate)?;
            output_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            negotiate_media_types(&transform, inputs[0], outputs[0], &input_type, &output_type)?;
            if config.max_reference_frames != 0 {
                verify_reference_frames(
                    codec
                        .as_ref()
                        .expect("reference-frame policy requires ICodecAPI"),
                    config.max_reference_frames,
                )?;
            }
            LAST_OPEN_STAGE.store(STAGE_OPEN_START, Ordering::Release);
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self {
                transform,
                events,
                codec,
                input: inputs[0],
                output: outputs[0],
                credits: 0,
                packets: VecDeque::new(),
                duration: 10_000_000 / i64::from(config.fps),
                draining: false,
                drained: false,
                configuration: config.clone(),
                normalizer: crate::bitstream::Normalizer::default(),
                _activation: activation,
                _runtime: runtime,
            })
        }
    }
    pub fn submit(
        &mut self,
        data: Vec<u8>,
        timestamp: i64,
        force: bool,
    ) -> Result<(), &'static str> {
        if self.draining {
            return Err("encoder_draining");
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.credits == 0 {
            self.pump().map_err(|_| "encoder_event_failed")?;
            if Instant::now() >= deadline {
                return Err("encoder_timeout");
            }
            if self.credits == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        // SAFETY: buffer length equals the owned NV12 vector; Lock/Unlock are
        // balanced, and sample retains the COM buffer after this function.
        unsafe {
            if force {
                let codec = self.codec.as_ref().ok_or("force_keyframe_unsupported")?;
                codec
                    .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(true))
                    .map_err(|_| "force_keyframe_failed")?;
            }
            let mut input_info = MFT_INPUT_STREAM_INFO::default();
            self.transform
                .GetInputStreamInfo(self.input, &mut input_info)
                .map_err(|_| "encoder_input_failed")?;
            let buffer = aligned_buffer(data.len() as u32, input_info.cbAlignment)
                .map_err(|_| "sample_allocation_failed")?;
            let mut pointer = std::ptr::null_mut();
            buffer
                .Lock(&mut pointer, None, None)
                .map_err(|_| "sample_lock_failed")?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), pointer, data.len());
            buffer.Unlock().map_err(|_| "sample_unlock_failed")?;
            buffer
                .SetCurrentLength(data.len() as u32)
                .map_err(|_| "sample_length_failed")?;
            let sample = MFCreateSample().map_err(|_| "sample_allocation_failed")?;
            sample
                .AddBuffer(&buffer)
                .map_err(|_| "sample_buffer_failed")?;
            sample
                .SetSampleTime(timestamp)
                .map_err(|_| "sample_time_failed")?;
            sample
                .SetSampleDuration(self.duration)
                .map_err(|_| "sample_time_failed")?;
            if let Err(error) = self.transform.ProcessInput(self.input, &sample, 0) {
                record_failure(STAGE_PROCESS_INPUT, &error);
                return Err("encoder_input_failed");
            }
            PROCESS_INPUT_CALLS.fetch_add(1, Ordering::Release);
            self.credits -= 1;
        }
        Ok(())
    }
    pub fn receive(&mut self) -> Result<Option<Packet>, &'static str> {
        self.pump().map_err(|_| "encoder_output_failed")?;
        Ok(self.packets.pop_front())
    }
    pub fn drain(&mut self) -> Result<(), &'static str> {
        if !self.draining {
            // SAFETY: live transform, sequential worker access.
            unsafe {
                if let Err(error) = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                {
                    record_failure(STAGE_DRAIN, &error);
                    return Err("encoder_drain_failed");
                }
                if let Err(error) = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) {
                    record_failure(STAGE_DRAIN, &error);
                    return Err("encoder_drain_failed");
                }
            }
            self.draining = true;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.drained {
            self.pump().map_err(|_| "encoder_drain_failed")?;
            if Instant::now() >= deadline {
                return Err("encoder_timeout");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
    fn pump(&mut self) -> NativeResult<()> {
        // SAFETY: nonblocking event reads and output processing are serialized.
        unsafe {
            for _ in 0..64 {
                let event = match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                    Ok(event) => event,
                    Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                    Err(error) => {
                        record_failure(STAGE_EVENT, &error);
                        return Err(error);
                    }
                };
                event.GetStatus()?.ok()?;
                match event.GetType()? {
                    value if value == METransformNeedInput.0 as u32 => {
                        NEED_INPUT_EVENTS.fetch_add(1, Ordering::Release);
                        self.credits = self.credits.saturating_add(1)
                    }
                    value if value == METransformHaveOutput.0 as u32 => {
                        HAVE_OUTPUT_EVENTS.fetch_add(1, Ordering::Release);
                        self.read_output()?
                    }
                    value if value == METransformDrainComplete.0 as u32 => self.drained = true,
                    _ => {
                        OTHER_EVENTS.fetch_add(1, Ordering::Release);
                    }
                }
            }
        }
        Ok(())
    }
    fn read_output(&mut self) -> NativeResult<()> {
        match self.read_output_sample() {
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                if let Err(error) = self.renegotiate_output() {
                    record_failure(STAGE_STREAM_CHANGE, &error);
                    return Err(error);
                }
                // An asynchronous MFT permits one ProcessOutput call for each
                // METransformHaveOutput event. Format renegotiation consumes
                // this event, so wait for the next event instead of issuing a
                // second call that the MFT must reject as E_UNEXPECTED.
                Ok(())
            }
            result => {
                if let Err(error) = &result {
                    record_failure(STAGE_PROCESS_OUTPUT, error);
                }
                result
            }
        }
    }
    fn renegotiate_output(&self) -> NativeResult<()> {
        // SAFETY: inspect bounded offered types and preserve the configured
        // codec and dimensions; unsupported dynamic changes fail closed.
        unsafe {
            let expected_size =
                (u64::from(self.configuration.width) << 32) | u64::from(self.configuration.height);
            for index in 0..32 {
                let media = self.transform.GetOutputAvailableType(self.output, index)?;
                if media.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_H264 {
                    continue;
                }
                if let Ok(size) = media.GetUINT64(&MF_MT_FRAME_SIZE)
                    && size != expected_size
                {
                    continue;
                }
                media.SetUINT64(&MF_MT_FRAME_SIZE, expected_size)?;
                media.SetUINT64(
                    &MF_MT_FRAME_RATE,
                    (u64::from(self.configuration.fps) << 32) | 1,
                )?;
                media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                media.SetUINT32(&MF_MT_AVG_BITRATE, self.configuration.bitrate)?;
                media.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
                if self.transform.SetOutputType(self.output, &media, 0).is_ok() {
                    if self.configuration.max_reference_frames != 0 {
                        verify_reference_frames(
                            self.codec
                                .as_ref()
                                .expect("reference-frame policy requires ICodecAPI"),
                            self.configuration.max_reference_frames,
                        )?;
                    }
                    return Ok(());
                }
            }
        }
        Err(windows::core::Error::from_hresult(MF_E_INVALIDMEDIATYPE))
    }
    fn read_output_sample(&mut self) -> NativeResult<()> {
        // SAFETY: ManuallyDrop members transfer one COM reference from MF. Take
        // both even on errors, so driver-allocated samples/events cannot leak.
        unsafe {
            let info = self.transform.GetOutputStreamInfo(self.output)?;
            let sample = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
                let sample = MFCreateSample()?;
                if info.cbSize > 16 * 1024 * 1024 {
                    return Err(windows::core::Error::from_hresult(
                        windows::Win32::Foundation::E_OUTOFMEMORY,
                    ));
                }
                sample.AddBuffer(&aligned_buffer(info.cbSize, info.cbAlignment)?)?;
                Some(sample)
            } else {
                None
            };
            let mut output = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            PROCESS_OUTPUT_CALLS.fetch_add(1, Ordering::Release);
            let result = self.transform.ProcessOutput(0, &mut output, &mut status);
            let sample = ManuallyDrop::take(&mut output[0].pSample);
            drop(ManuallyDrop::take(&mut output[0].pEvents));
            if let Err(error) = result {
                record_failure(STAGE_PROCESS_OUTPUT, &error);
                return Err(error);
            }
            if let Some(sample) = sample {
                let buffer = sample.ConvertToContiguousBuffer()?;
                let length = buffer.GetCurrentLength()? as usize;
                if length == 0 {
                    return Ok(());
                }
                if length > 16 * 1024 * 1024 || self.packets.len() >= 16 {
                    return Err(windows::core::Error::from_hresult(
                        windows::Win32::Foundation::E_OUTOFMEMORY,
                    ));
                }
                let mut pointer = std::ptr::null_mut();
                buffer.Lock(&mut pointer, None, None)?;
                let bytes = std::slice::from_raw_parts(pointer, length).to_vec();
                buffer.Unlock()?;
                let output_type = self.transform.GetOutputCurrentType(self.output)?;
                // MFVideoFormat_H264 specifies one complete coded picture per
                // sample; H264_ES explicitly does not. Progressive negotiation
                // makes that picture a frame rather than an individual field.
                if output_type.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_H264
                    || output_type.GetUINT32(&MF_MT_INTERLACE_MODE)?
                        != MFVideoInterlace_Progressive.0 as u32
                {
                    return Err(windows::core::Error::from_hresult(MF_E_INVALIDMEDIATYPE));
                }
                if let Ok(length) = output_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                    if length > 1024 * 1024 {
                        return Err(windows::core::Error::from_hresult(
                            windows::Win32::Foundation::E_INVALIDARG,
                        ));
                    }
                    let mut header = vec![0; length as usize];
                    output_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None)?;
                    self.normalizer.header(&header).map_err(|_| {
                        windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG)
                    })?;
                }
                let (bytes, keyframe) = self.normalizer.sample(&bytes).map_err(|_| {
                    windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG)
                })?;
                self.packets.push_back(Packet {
                    bytes,
                    timestamp_100ns: sample.GetSampleTime()?,
                    keyframe,
                });
            }
        }
        Ok(())
    }
}

fn negotiate_media_types(
    transform: &IMFTransform,
    input: u32,
    output: u32,
    input_type: &IMFMediaType,
    output_type: &IMFMediaType,
) -> NativeResult<()> {
    // Media Foundation permits an MFT's input and output type availability to
    // depend on the other stream. Prefer input-first for standard encoders,
    // then make one narrowly defined retry for MFTs that explicitly report the
    // other type is not set (as the NVIDIA H.264 MFT does on this host).
    unsafe {
        LAST_OPEN_STAGE.store(STAGE_OPEN_INPUT_TYPE, Ordering::Release);
        match transform.SetInputType(input, input_type, 0) {
            Ok(()) => {
                LAST_OPEN_STAGE.store(STAGE_OPEN_OUTPUT_TYPE, Ordering::Release);
                transform.SetOutputType(output, output_type, 0)?;
            }
            Err(error) if error.code() == MF_E_TRANSFORM_TYPE_NOT_SET => {
                LAST_OPEN_STAGE.store(STAGE_OPEN_OUTPUT_TYPE, Ordering::Release);
                transform.SetOutputType(output, output_type, 0)?;
                LAST_OPEN_STAGE.store(STAGE_OPEN_INPUT_TYPE, Ordering::Release);
                transform.SetInputType(input, input_type, 0)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Configures the static H.264 SPS reference-frame limit before media types are
/// committed. Reading the value back prevents a driver from silently accepting
/// but ignoring a strict one-reference-frame request.
fn configure_reference_frames(codec: &ICodecAPI, maximum: u32) -> NativeResult<()> {
    if maximum == 0 {
        return Ok(());
    }
    // SAFETY: the live codec is owned by this thread. Microsoft specifies this
    // property as VT_UI4 and requires SetValue/GetValue before encoding starts.
    unsafe {
        codec.SetValue(&CODECAPI_AVEncVideoMaxNumRefFrame, &VARIANT::from(maximum))?;
    }
    verify_reference_frames(codec, maximum)
}

fn verify_reference_frames(codec: &ICodecAPI, maximum: u32) -> NativeResult<()> {
    if maximum == 0 {
        return Ok(());
    }
    // SAFETY: the live codec is owned by this thread. The property is VT_UI4;
    // conversion also rejects a driver returning the wrong variant type.
    let configured = unsafe { codec.GetValue(&CODECAPI_AVEncVideoMaxNumRefFrame)? };
    if u32::try_from(&configured)? != maximum {
        return Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_NOTIMPL,
        ));
    }
    Ok(())
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: no other thread owns or uses this transform. Shutdown stops the
        // async event queue; COM references drop before the runtime guard.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

fn media_type(config: &Configuration, subtype: windows::core::GUID) -> NativeResult<IMFMediaType> {
    // SAFETY: locally owned attribute store; sizes/rates were bounded by control validation.
    unsafe {
        let media = MFCreateMediaType()?;
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        media.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(config.width) << 32) | u64::from(config.height),
        )?;
        media.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(config.fps) << 32) | 1)?;
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        media.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        Ok(media)
    }
}

fn aligned_buffer(length: u32, alignment: u32) -> NativeResult<IMFMediaBuffer> {
    let alignment = alignment.max(1);
    if !alignment.is_power_of_two() || alignment > 65_536 {
        return Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_INVALIDARG,
        ));
    }
    // SAFETY: documented alignment mask is alignment minus one. MF owns memory.
    unsafe { MFCreateAlignedMemoryBuffer(length, alignment - 1) }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn diagnostic_stage_mapping_is_stable_and_never_contains_media_data() {
        assert_eq!(stage_name(STAGE_PROCESS_INPUT), "process_input");
        assert_eq!(stage_name(STAGE_PROCESS_OUTPUT), "process_output");
        assert_eq!(stage_name(STAGE_STREAM_CHANGE), "stream_change");
        assert_eq!(stage_name(STAGE_DRAIN), "drain");
        assert_eq!(stage_name(STAGE_OPEN_RUNTIME), "open_runtime");
        assert_eq!(stage_name(STAGE_OPEN_INPUT_TYPE), "open_input_type");
        assert_eq!(stage_name(STAGE_OPEN_OUTPUT_TYPE), "open_output_type");
        assert_eq!(stage_name(STAGE_EVENT), "event");
        assert_eq!(stage_name(255), "unknown");
    }

    fn description(clsid: &str, name: &str) -> EncoderDescription {
        EncoderDescription {
            identity: EncoderIdentity {
                clsid: clsid.into(),
                hardware_url: Some(format!("mft://{clsid}")),
                friendly_name: name.into(),
            },
            name: name.into(),
            codec: "h264",
            hardware: true,
            vendor_id: None,
            capabilities: Default::default(),
        }
    }

    #[test]
    fn exact_identity_survives_reordered_enumeration_and_rejects_mismatch() {
        let nvidia = description("nvidia", "NVIDIA H.264");
        let intel = description("intel", "Intel H.264");
        let reordered = [intel.clone(), nvidia.clone()];
        assert_eq!(
            matching_description(&reordered, &nvidia.identity)
                .expect("stable identity must select NVIDIA")
                .name,
            "NVIDIA H.264"
        );
        let mut changed = nvidia.identity.clone();
        changed.hardware_url = Some("mft://different".into());
        assert!(matching_description(&reordered, &changed).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nvenc_h264_cavlc_is_rejected_before_media_foundation_startup() {
        let configuration = Configuration {
            video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
            width: 16,
            height: 16,
            fps: 60,
            bitrate: 1_000_000,
            min_threads: 2,
            slices_per_frame: 1,
            qp: 28,
            nvenc_settings: Default::default(),
            codec: amaterasu_media::VideoCodec::H264,
            color_mode: amaterasu_media::ColorMode::default(),
            max_reference_frames: 0,
            nvenc_preset: None,
            nvenc_h264_cavlc: true,
            nvenc_spatial_aq: false,
            amd_av1_latency_mode: crate::model::AmdAv1LatencyMode::Auto,
            sw_preset: None,
            sw_tune: None,
        };
        let identity = EncoderIdentity {
            clsid: "test-clsid".into(),
            hardware_url: Some("test://hardware-mft".into()),
            friendly_name: "Test H.264".into(),
        };

        assert!(matches!(
            Encoder::open(&identity, &configuration),
            Err("nvenc_h264_cavlc_unsupported")
        ));
    }

    #[test]
    fn nvenc_spatial_aq_is_rejected_before_media_foundation_startup() {
        let configuration = Configuration {
            video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
            width: 16,
            height: 16,
            fps: 60,
            bitrate: 1_000_000,
            min_threads: 2,
            slices_per_frame: 1,
            qp: 28,
            nvenc_settings: Default::default(),
            codec: amaterasu_media::VideoCodec::H264,
            color_mode: amaterasu_media::ColorMode::default(),
            max_reference_frames: 0,
            nvenc_preset: None,
            nvenc_h264_cavlc: false,
            nvenc_spatial_aq: true,
            amd_av1_latency_mode: crate::model::AmdAv1LatencyMode::Auto,
            sw_preset: None,
            sw_tune: None,
        };
        let identity = EncoderIdentity {
            clsid: "test-clsid".into(),
            hardware_url: Some("test://hardware-mft".into()),
            friendly_name: "Test H.264".into(),
        };

        assert!(matches!(
            Encoder::open(&identity, &configuration),
            Err("nvenc_spatial_aq_unsupported")
        ));
    }

    #[test]
    fn amd_av1_latency_mode_is_rejected_before_media_foundation_startup() {
        let configuration = Configuration {
            video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
            width: 16,
            height: 16,
            fps: 60,
            bitrate: 1_000_000,
            min_threads: 2,
            slices_per_frame: 1,
            qp: 28,
            nvenc_settings: Default::default(),
            codec: amaterasu_media::VideoCodec::H264,
            color_mode: amaterasu_media::ColorMode::default(),
            max_reference_frames: 0,
            nvenc_preset: None,
            nvenc_h264_cavlc: false,
            nvenc_spatial_aq: false,
            amd_av1_latency_mode: crate::model::AmdAv1LatencyMode::Realtime,
            sw_preset: None,
            sw_tune: None,
        };
        let identity = EncoderIdentity {
            clsid: "test-clsid".into(),
            hardware_url: Some("test://hardware-mft".into()),
            friendly_name: "Test H.264".into(),
        };

        assert!(matches!(
            Encoder::open(&identity, &configuration),
            Err("amd_av1_latency_mode_unsupported")
        ));
    }

    #[test]
    #[ignore = "requires a Windows host with a hardware H.264 MFT; synthetic frames only"]
    fn hardware_encodes_synthetic_nv12_with_idr_and_drains() {
        let devices = enumerate().unwrap();
        assert!(!devices.is_empty(), "no hardware NV12/H264 MFT available");
        let config = Configuration {
            video_stream_format: amaterasu_module_sdk::VideoStreamFormat::H264_SDR,
            width: 640,
            height: 480,
            fps: 30,
            bitrate: 2_000_000,
            min_threads: 2,
            slices_per_frame: 1,
            qp: 28,
            nvenc_settings: Default::default(),
            codec: amaterasu_media::VideoCodec::H264,
            color_mode: amaterasu_media::ColorMode::default(),
            max_reference_frames: 0,
            nvenc_preset: None,
            nvenc_h264_cavlc: false,
            nvenc_spatial_aq: false,
            amd_av1_latency_mode: crate::model::AmdAv1LatencyMode::Auto,
            sw_preset: None,
            sw_tune: None,
        };
        assert!(config.validate().is_ok());
        let selected = devices
            .iter()
            .find(|device| device.name.to_ascii_lowercase().contains("nvidia"))
            .or_else(|| devices.first())
            .expect("enumeration was checked non-empty");
        if devices
            .iter()
            .any(|device| device.name.to_ascii_lowercase().contains("nvidia"))
        {
            assert!(selected.name.to_ascii_lowercase().contains("nvidia"));
        }
        let mut encoder = Encoder::open(&selected.identity, &config).unwrap_or_else(|error| {
            panic!("{error}: {:?}", last_failure());
        });
        for sequence in 0..4 {
            let mut pixels = vec![128; 640 * 480 * 3 / 2];
            pixels[..640 * 480].fill(16 + sequence as u8);
            encoder
                .submit(pixels, sequence * 333_333, sequence == 0)
                .unwrap();
        }
        encoder.drain().unwrap();
        let mut count = 0;
        let mut idr = false;
        while let Some(packet) = encoder.receive().unwrap() {
            assert!(packet.bytes.starts_with(&[0, 0, 0, 1]));
            idr |= packet.keyframe;
            count += 1;
        }
        assert!(count >= 4);
        assert!(idr);
    }
}

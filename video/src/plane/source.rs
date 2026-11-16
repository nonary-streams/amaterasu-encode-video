//! Source-driven encoder session lifecycle on the video module thread.
use super::provider_output::ProviderOutput;
use crate::{model::Configuration, queue::Queues};
use amaterasu_media::*;
use amaterasu_module_sdk::abi::{BorrowedBytes, STATUS_INVALID_ARGUMENT, STATUS_OK, Status};
use std::{
    ffi::c_void,
    mem::size_of,
    sync::atomic::{AtomicU32, Ordering},
};

fn frame_cadence(
    configuration: &Configuration,
) -> Result<amaterasu_module_sdk::encode::FrameCadence, &'static str> {
    match configuration.fps_milli {
        Some(fps_milli) => amaterasu_module_sdk::encode::FrameCadence::from_millihz(fps_milli),
        None => amaterasu_module_sdk::encode::FrameCadence::from_fps(configuration.fps),
    }
    .map_err(|_| "invalid_video_cadence")
}

fn id(value: u64) -> EncoderFormatId {
    EncoderFormatId {
        namespace_id: ENCODER_FORMAT_NAMESPACE_STANDARD,
        value,
        schema: ENCODER_FORMAT_SCHEMA_V1,
    }
}

struct Reply {
    calls: u32,
    request: u64,
    outcome: Option<u32>,
}
unsafe extern "C" fn reply(
    context: *mut c_void,
    response: *const EncoderControlResponse,
) -> Status {
    if context.is_null() || response.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the caller supplies a synchronous callback context and response.
    let (state, response) = unsafe { (&mut *context.cast::<Reply>(), &*response) };
    state.calls += 1;
    if state.calls != 1
        || response.size < size_of::<EncoderControlResponse>() as u32
        || response.request_id != state.request
        || response.reserved != 0
    {
        return STATUS_INVALID_ARGUMENT;
    }
    state.outcome = Some(response.outcome);
    STATUS_OK
}
fn sink(state: &mut Reply) -> EncoderControlSink {
    EncoderControlSink {
        context: (state as *mut Reply).cast(),
        write: reply,
    }
}
unsafe extern "C" fn notify(context: *mut c_void, _: u64, event: u32) {
    if !context.is_null() && (1..=3).contains(&event) {
        // SAFETY: the boxed signal stays alive until unsubscribe returns.
        unsafe { &*context.cast::<AtomicU32>() }.fetch_or(1 << event, Ordering::Release);
    }
}

pub(super) struct Source {
    session: u64,
    submitted: bool,
    output: ProviderOutput,
    signal: Box<AtomicU32>,
    subscribed: bool,
    stopped: bool,
    next_control: u64,
    pending_idr: Option<u64>,
}
fn with_source_config<T>(
    source: u64,
    configuration: &Configuration,
    clock_origin_us: u64,
    epoch_id: [u8; 16],
    operation: impl FnOnce(&EncoderSessionConfig) -> Result<T, &'static str>,
) -> Result<T, &'static str> {
    if !configuration.color_mode.is_default() {
        return Err("encoder_source_format_unsupported");
    }
    if epoch_id == [0; 16] || clock_origin_us == 0 {
        return Err("encoder_clock_unavailable");
    }
    let cadence = frame_cadence(configuration)?;
    let common = EncoderCommonConfigV1 {
        version: 1,
        size: size_of::<EncoderCommonConfigV1>() as u32,
        flags: 0,
        reserved: 0,
        coded_width: configuration.width,
        coded_height: configuration.height,
        crop_x: 0,
        crop_y: 0,
        visible_width: configuration.width,
        visible_height: configuration.height,
        fps_numerator: cadence.numerator(),
        fps_denominator: cadence.denominator(),
        bitrate_bps: u64::from(configuration.bitrate),
        profile: 0,
        level: 0,
        chroma: ENCODER_CHROMA_420,
        bit_depth: configuration
            .extension_profile
            .as_ref()
            .and_then(|p| p.media_format)
            .map_or(8, |f| u32::from(f.bit_depth)),
        color: FrameColorMetadata::default(),
        output_clock: FrameClockMetadata {
            version: 1,
            size: size_of::<FrameClockMetadata>() as u32,
            domain: FRAME_CLOCK_STREAM_RELATIVE,
            reserved: 0,
            epoch_id,
        },
        output_clock_origin_us: clock_origin_us,
    };
    // SAFETY: common is plain C data, borrowed only during the synchronous calls.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&common as *const EncoderCommonConfigV1).cast::<u8>(),
            size_of::<EncoderCommonConfigV1>(),
        )
    };
    let (codec, framing) = if let Some(profile) = &configuration.extension_profile {
        (
            EncoderFormatId {
                namespace_id: profile.codec.namespace_id,
                value: profile.codec.value,
                schema: profile.codec.schema,
            },
            EncoderFormatId {
                namespace_id: profile.framing.namespace_id,
                value: profile.framing.value,
                schema: profile.framing.schema,
            },
        )
    } else {
        (
            id(u64::from(configuration.codec.abi_id())),
            id(if configuration.codec == VideoCodec::Av1 {
                ENCODER_FRAMING_AV1_OBU
            } else {
                ENCODER_FRAMING_ANNEX_B
            }),
        )
    };
    let config = EncoderSessionConfig {
        size: size_of::<EncoderSessionConfig>() as u32,
        mode: ENCODER_SESSION_SOURCE,
        codec,
        framing,
        source_id: source,
        config_generation: 1,
        options_schema: id(ENCODER_SCHEMA_COMMON_CONFIG),
        options: BorrowedBytes::new(bytes),
    };
    operation(&config)
}

unsafe fn probe_config(
    api: &EncoderProviderApi,
    config: &EncoderSessionConfig,
) -> Result<(), &'static str> {
    let mut probe = Reply {
        calls: 0,
        request: 0,
        outcome: None,
    };
    // SAFETY: config and callback context remain live for the synchronous call.
    let status = unsafe { (api.probe)(api.base.context, config, sink(&mut probe)) };
    if status != STATUS_OK || probe.calls != 1 || probe.outcome != Some(ENCODER_OUTCOME_APPLIED) {
        return Err("encoder_probe_failed");
    }
    Ok(())
}

impl Source {
    pub(super) fn session(&self) -> u64 {
        self.session
    }
    /// # Safety
    /// The authenticated provider binding and control session must outlive this object.
    pub(super) unsafe fn open(
        api: &EncoderProviderApi,
        video: u64,
        session: u64,
        source: u64,
        configuration: &Configuration,
        clock_origin_us: u64,
        epoch_id: [u8; 16],
    ) -> Result<Self, &'static str> {
        with_source_config(source, configuration, clock_origin_us, epoch_id, |config| {
            let output = if configuration.extension_profile.is_some() {
                ProviderOutput::new_negotiated(session, video, config)
            } else {
                ProviderOutput::new(session, video, config)
            }
            .map_err(|_| "encoder_format_unsupported")?;
            // SAFETY: caller retains the selected provider binding.
            unsafe { probe_config(api, config) }?;
            // SAFETY: authorized session is created by the matching control provider.
            if unsafe { (api.configure_session)(api.base.context, session, config) } != STATUS_OK {
                // SAFETY: conservatively retire any provider state from failed configure.
                unsafe { (api.destroy_session)(api.base.context, session) };
                return Err("encoder_configure_failed");
            }
            let signal = Box::new(AtomicU32::new(0));
            let subscribed = match (api.subscribe, api.unsubscribe) {
                (Some(subscribe), Some(_)) => {
                    let event = EncoderEventSink {
                        context: (&*signal as *const AtomicU32).cast_mut().cast(),
                        notify,
                    };
                    // SAFETY: signal is boxed and kept alive until the unsubscribe barrier.
                    if unsafe { subscribe(api.base.context, session, event) } != STATUS_OK {
                        unsafe { (api.destroy_session)(api.base.context, session) };
                        return Err("encoder_subscribe_failed");
                    }
                    true
                }
                (None, None) => false,
                _ => {
                    unsafe { (api.destroy_session)(api.base.context, session) };
                    return Err("encoder_notification_interface");
                }
            };
            Ok(Self {
                session,
                submitted: false,
                output,
                signal,
                subscribed,
                stopped: false,
                next_control: 1,
                pending_idr: None,
            })
        })
    }

    pub(super) unsafe fn open_submitted(
        api: &EncoderProviderApi,
        video: u64,
        session: u64,
        source: u64,
        configuration: &Configuration,
        clock_origin_us: u64,
        epoch_id: [u8; 16],
    ) -> Result<Self, &'static str> {
        with_source_config(source, configuration, clock_origin_us, epoch_id, |config| {
            let mut submitted = *config;
            submitted.mode = ENCODER_SESSION_SUBMITTED;
            let config = &submitted;
            let output = if configuration.extension_profile.is_some() {
                ProviderOutput::new_negotiated(session, video, config)
            } else {
                ProviderOutput::new(session, video, config)
            }
            .map_err(|_| "encoder_format_unsupported")?;
            // SAFETY: caller retains the selected provider binding.
            // The correlated broker probe already verified GPU support; do not encode again here.

            // SAFETY: authorized session is created by the matching control provider.
            if unsafe { (api.configure_session)(api.base.context, session, config) } != STATUS_OK {
                // SAFETY: conservatively retire any provider state from failed configure.
                unsafe { (api.destroy_session)(api.base.context, session) };
                return Err("encoder_configure_failed");
            }
            let signal = Box::new(AtomicU32::new(0));
            let subscribed = match (api.subscribe, api.unsubscribe) {
                (Some(subscribe), Some(_)) => {
                    let event = EncoderEventSink {
                        context: (&*signal as *const AtomicU32).cast_mut().cast(),
                        notify,
                    };
                    // SAFETY: signal is boxed and kept alive until the unsubscribe barrier.
                    if unsafe { subscribe(api.base.context, session, event) } != STATUS_OK {
                        unsafe { (api.destroy_session)(api.base.context, session) };
                        return Err("encoder_subscribe_failed");
                    }
                    true
                }
                (None, None) => false,
                _ => {
                    unsafe { (api.destroy_session)(api.base.context, session) };
                    return Err("encoder_notification_interface");
                }
            };
            Ok(Self {
                session,
                submitted: true,
                output,
                signal,
                subscribed,
                stopped: false,
                next_control: 1,
                pending_idr: None,
            })
        })
    }

    /// # Safety
    /// The authenticated provider binding remains live during the probe.
    pub(super) unsafe fn probe(
        api: &EncoderProviderApi,
        source: u64,
        configuration: &Configuration,
    ) -> Result<(), &'static str> {
        with_source_config(
            source,
            configuration,
            host_monotonic_micros().max(1),
            [1; 16],
            |config| unsafe { probe_config(api, config) },
        )
    }
    /// # Safety
    /// api is the same live binding used to open the session.
    pub(super) unsafe fn pump(
        &mut self,
        api: &EncoderProviderApi,
        queues: &mut Queues,
        force_idr: bool,
    ) -> Result<bool, &'static str> {
        let events = self.signal.swap(0, Ordering::AcqRel);
        self.stopped |= events & (1 << ENCODER_EVENT_STOPPED) != 0;
        let mut completed = !force_idr || self.submitted;
        if force_idr && !self.submitted {
            let request_id = *self.pending_idr.get_or_insert(self.next_control);
            let polling = request_id < self.next_control;
            let request = EncoderControlRequest {
                size: size_of::<EncoderControlRequest>() as u32,
                kind: if polling {
                    ENCODER_CONTROL_POLL
                } else {
                    ENCODER_CONTROL_REQUEST_KEYFRAME
                },
                request_id,
                expected_generation: 1,
                schema: id(0),
                data: BorrowedBytes::new(&[]),
            };
            let mut result = Reply {
                calls: 0,
                request: request_id,
                outcome: None,
            };
            // SAFETY: request and callback context live for the synchronous call.
            let status = unsafe {
                (api.control)(api.base.context, self.session, &request, sink(&mut result))
            };
            if matches!(status, MEDIA_BUSY | MEDIA_PENDING) && result.calls == 0 {
                // No admission: retry the same control ID, while still receiving output.
            } else if status != STATUS_OK || result.calls != 1 {
                return Err("encoder_idr_failed");
            } else {
                self.next_control = request_id
                    .checked_add(1)
                    .ok_or("encoder_control_exhausted")?;
                match result.outcome {
                    Some(ENCODER_OUTCOME_APPLIED | ENCODER_OUTCOME_CLAMPED) => {
                        self.pending_idr = None;
                        completed = true;
                    }
                    Some(ENCODER_OUTCOME_ACCEPTED | ENCODER_OUTCOME_PENDING) => {}
                    _ => return Err("encoder_idr_unsupported"),
                }
            }
        }
        // SAFETY: api and queue are live, output callbacks are synchronous.
        match unsafe { self.output.receive(api, queues) } {
            STATUS_OK | MEDIA_BUSY => Ok(completed),
            MEDIA_PENDING if !self.stopped => Ok(completed),
            MEDIA_PENDING => Err("encoder_source_stopped"),
            _ => Err("encoder_receive_failed"),
        }
    }
    /// # Safety
    /// api is the same live provider binding.
    pub(super) unsafe fn drain(&mut self, api: &EncoderProviderApi) -> Result<bool, &'static str> {
        match unsafe { self.output.drain(api) } {
            STATUS_OK => Ok(true),
            MEDIA_BUSY | MEDIA_PENDING => Ok(false),
            _ => Err("encoder_drain_failed"),
        }
    }
    /// # Safety
    /// Must run before closing the control session or dropping its binding.
    pub(super) unsafe fn finish(self, api: &EncoderProviderApi) {
        if self.subscribed {
            // SAFETY: matching pair was checked at subscription; this is a callback barrier.
            unsafe { (api.unsubscribe.unwrap())(api.base.context, self.session) };
        }
        // SAFETY: all delivery has stopped and binding/session remain live.
        unsafe { (api.destroy_session)(api.base.context, self.session) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_cadence_is_preserved_as_exact_millihertz() {
        let configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "fps_milli": 59_940,
            "bitrate": 5_000_000
        }))
        .unwrap();
        let cadence = frame_cadence(&configuration).unwrap();
        assert_eq!(cadence.numerator(), 59_940);
        assert_eq!(cadence.denominator(), 1000);
    }

    #[test]
    fn legacy_integer_cadence_keeps_its_original_ratio() {
        let configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "bitrate": 5_000_000
        }))
        .unwrap();
        let cadence = frame_cadence(&configuration).unwrap();
        assert_eq!(cadence.numerator(), 60);
        assert_eq!(cadence.denominator(), 1);
        assert!(
            serde_json::to_value(configuration)
                .unwrap()
                .get("fps_milli")
                .is_none()
        );
    }
    use amaterasu_module_sdk::abi::{AbiHeader, NativeInterface};

    unsafe extern "C" fn probe(
        _: *mut c_void,
        _: *const EncoderSessionConfig,
        sink: EncoderControlSink,
    ) -> Status {
        let response = EncoderControlResponse {
            size: size_of::<EncoderControlResponse>() as u32,
            kind: 0,
            request_id: 0,
            effective_generation: 1,
            effective_submission_id: 0,
            outcome: ENCODER_OUTCOME_APPLIED,
            reserved: 0,
            schema: id(0),
            data: BorrowedBytes::new(&[]),
        };
        unsafe { (sink.write)(sink.context, &response) }
    }
    unsafe extern "C" fn configure(
        context: *mut c_void,
        _: u64,
        _: *const EncoderSessionConfig,
    ) -> Status {
        unsafe {
            *context.cast::<u32>() += 1;
        }
        MEDIA_UNSUPPORTED
    }
    unsafe extern "C" fn destroy(context: *mut c_void, _: u64) {
        unsafe {
            *context.cast::<u32>() += 10;
        }
    }
    unsafe extern "C" fn receive(_: *mut c_void, _: u64, _: usize, _: EncoderOutputSink) -> Status {
        MEDIA_PENDING
    }
    unsafe extern "C" fn control(
        _: *mut c_void,
        _: u64,
        _: *const EncoderControlRequest,
        _: EncoderControlSink,
    ) -> Status {
        MEDIA_UNSUPPORTED
    }
    unsafe extern "C" fn drain(_: *mut c_void, _: u64) -> Status {
        STATUS_OK
    }

    #[test]
    fn failed_configure_retires_partial_provider_state_before_control_close() {
        let mut calls = 0u32;
        let api = EncoderProviderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderProviderApi>(),
                context: (&mut calls as *mut u32).cast(),
                release: None,
            },
            probe,
            configure_session: configure,
            receive,
            control,
            subscribe: None,
            unsubscribe: None,
            drain,
            destroy_session: destroy,
        };
        let configuration: Configuration = serde_json::from_value(
            serde_json::json!({"width":16,"height":16,"fps":60,"bitrate":1_000_000}),
        )
        .unwrap();
        let result = unsafe { Source::open(&api, 1, 2, 42, &configuration, 1000, [1; 16]) };
        assert!(matches!(result, Err("encoder_configure_failed")));
        assert_eq!(calls, 11);
    }

    #[test]
    fn source_preflight_probes_the_custom_codec_without_configuring_a_session() {
        unsafe extern "C" fn check(
            context: *mut c_void,
            config: *const EncoderSessionConfig,
            sink: EncoderControlSink,
        ) -> Status {
            let config = unsafe { &*config };
            assert_eq!(
                (
                    config.codec.namespace_id,
                    config.codec.value,
                    config.framing.value,
                    config.source_id
                ),
                (7, 8, 9, 42)
            );
            let count = unsafe { &mut *context.cast::<u32>() };
            *count += 1;
            unsafe { probe(context, config, sink) }
        }
        let mut calls = 0u32;
        let api = EncoderProviderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderProviderApi>(),
                context: (&mut calls as *mut u32).cast(),
                release: None,
            },
            probe: check,
            configure_session: configure,
            receive,
            control,
            subscribe: None,
            unsubscribe: None,
            drain,
            destroy_session: destroy,
        };
        let configuration: Configuration = serde_json::from_value(serde_json::json!({
            "width":16,"height":16,"fps":60,"bitrate":1_000_000,
            "extension_profile":{"codec":{"namespace_id":7,"value":8,"schema":1},
                "framing":{"namespace_id":7,"value":9,"schema":1},"wire_selector":4,"payload_type":100,"encoding_name":"EXAMPLE"}
        })).unwrap();
        assert!(unsafe { Source::probe(&api, 42, &configuration) }.is_ok());
        assert_eq!(calls, 1);
    }
    #[derive(Default)]
    struct BusyState {
        controls: Vec<u64>,
        receives: usize,
    }
    unsafe extern "C" fn busy_control(
        context: *mut c_void,
        _: u64,
        request: *const EncoderControlRequest,
        sink: EncoderControlSink,
    ) -> Status {
        let state = unsafe { &mut *context.cast::<BusyState>() };
        let request = unsafe { &*request };
        state.controls.push(request.request_id);
        if state.controls.len() == 1 {
            return MEDIA_BUSY;
        }
        let response = EncoderControlResponse {
            size: size_of::<EncoderControlResponse>() as u32,
            kind: request.kind,
            request_id: request.request_id,
            effective_generation: 1,
            effective_submission_id: 0,
            outcome: ENCODER_OUTCOME_APPLIED,
            reserved: 0,
            schema: id(0),
            data: BorrowedBytes::new(&[]),
        };
        unsafe { (sink.write)(sink.context, &response) }
    }
    unsafe extern "C" fn count_receive(
        context: *mut c_void,
        _: u64,
        _: usize,
        _: EncoderOutputSink,
    ) -> Status {
        unsafe {
            (*context.cast::<BusyState>()).receives += 1;
        }
        MEDIA_PENDING
    }
    #[test]
    fn busy_idr_retries_same_request_and_continues_receiving() {
        let mut state = BusyState::default();
        let api = EncoderProviderApi {
            base: NativeInterface {
                header: AbiHeader::of::<EncoderProviderApi>(),
                context: (&mut state as *mut BusyState).cast(),
                release: None,
            },
            probe,
            configure_session: configure,
            receive: count_receive,
            control: busy_control,
            subscribe: None,
            unsubscribe: None,
            drain,
            destroy_session: destroy,
        };
        let config = EncoderSessionConfig {
            size: size_of::<EncoderSessionConfig>() as u32,
            mode: ENCODER_SESSION_SOURCE,
            codec: id(u64::from(CODEC_H264)),
            framing: id(ENCODER_FRAMING_ANNEX_B),
            source_id: 42,
            config_generation: 1,
            options_schema: id(0),
            options: BorrowedBytes::new(&[]),
        };
        let mut source = Source {
            submitted: false,
            session: 2,
            output: ProviderOutput::new(2, 1, &config).unwrap(),
            signal: Box::new(AtomicU32::new(0)),
            subscribed: false,
            stopped: false,
            next_control: 1,
            pending_idr: None,
        };
        let mut queues = Queues::default();
        assert_eq!(unsafe { source.pump(&api, &mut queues, true) }, Ok(false));
        assert_eq!(unsafe { source.pump(&api, &mut queues, true) }, Ok(true));
        assert_eq!(state.controls, vec![1, 1]);
        assert_eq!(state.receives, 2);
    }
}

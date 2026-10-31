//! Synchronous, schema-agnostic capture-to-consumer forwarding.
use amaterasu_media::{
    CaptureApiV1_8, CaptureResourceFrame, CaptureResourceFrameSink, CaptureResourceOffer,
    CaptureResourceOfferSink, FRAME_ACCESS_CPU, FRAME_ACCESS_GPU, FrameResourceConsumerApi,
    MEDIA_UNSUPPORTED,
};
use amaterasu_module_sdk::abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status};
use std::{ffi::c_void, marker::PhantomData, rc::Rc};

const MAX_OFFERS: usize = 64;

/// A protocol or native operation failure. A native status is preserved so the
/// caller can distinguish backpressure from a terminal provider failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionError {
    Protocol,
    Native(Status),
    NoCompatibleOffer,
    Stale,
}

struct Ids {
    ids: Vec<u64>,
    invalid: bool,
    eligible: Vec<u64>,
    required: u32,
}

impl Ids {
    fn new(required: u32) -> Self {
        Self {
            ids: Vec::new(),
            invalid: false,
            eligible: Vec::new(),
            required,
        }
    }
    fn add(&mut self, offer: &CaptureResourceOffer) -> bool {
        if offer.id == 0
            || offer.reserved != 0
            || offer.accessibility & !(FRAME_ACCESS_CPU | FRAME_ACCESS_GPU) != 0
            || self.ids.len() >= MAX_OFFERS
            || self.ids.contains(&offer.id)
        {
            self.invalid = true;
            return false;
        }
        self.ids.push(offer.id);
        if offer.accessibility & self.required == self.required {
            self.eligible.push(offer.id);
        }
        true
    }
}

unsafe extern "C" fn collect(context: *mut c_void, offer: *const CaptureResourceOffer) -> Status {
    if context.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: enumeration supplies a live context.
    let ids = unsafe { &mut *context.cast::<Ids>() };
    if offer.is_null() {
        ids.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: enumeration supplies a live offer.
    let offer = unsafe { &*offer };
    if ids.add(offer) {
        STATUS_OK
    } else {
        STATUS_INVALID_ARGUMENT
    }
}

struct Configure<'a> {
    ids: Ids,
    consumer_ids: &'a [u64],
    consumer: &'a FrameResourceConsumerApi,
    stream: u64,
    generation: u64,
    producer_generation: u64,
    required: u32,
    selected: Option<u64>,
    failure: Option<Status>,
}
unsafe extern "C" fn configure(context: *mut c_void, offer: *const CaptureResourceOffer) -> Status {
    if context.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: enumeration supplies a live context.
    let state = unsafe { &mut *context.cast::<Configure<'_>>() };
    if offer.is_null() {
        state.ids.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: enumeration supplies a live offer.
    let offer = unsafe { &*offer };
    if !state.ids.add(offer) {
        return STATUS_INVALID_ARGUMENT;
    }
    if state.selected.is_some() || offer.accessibility & state.required != state.required {
        return STATUS_OK;
    }
    for id in state.consumer_ids {
        // SAFETY: both bindings and the borrowed offer remain live for this call.
        let status = unsafe {
            (state.consumer.configure)(
                state.consumer.base.context,
                state.stream,
                state.generation,
                *id,
                state.producer_generation,
                offer,
            )
        };
        if status == STATUS_OK {
            state.selected = Some(offer.id);
            break;
        }
        if status != MEDIA_UNSUPPORTED {
            state.failure = Some(status);
            return status;
        }
    }
    STATUS_OK
}

fn valid_color_metadata(color: &amaterasu_media::FrameColorMetadata) -> bool {
    match color.version {
        0 => {
            color.size == 0
                && color.primaries == 0
                && color.transfer == 0
                && color.matrix == 0
                && color.range == 0
        }
        1 => color.size as usize == std::mem::size_of::<amaterasu_media::FrameColorMetadata>(),
        _ => true,
    }
}

fn valid_clock_metadata(clock: &amaterasu_media::FrameClockMetadata) -> bool {
    match clock.version {
        0 => {
            clock.size == 0 && clock.domain == 0 && clock.reserved == 0 && clock.epoch_id == [0; 16]
        }
        1 => {
            clock.size as usize == std::mem::size_of::<amaterasu_media::FrameClockMetadata>()
                && clock.reserved == 0
        }
        _ => true,
    }
}

struct Delivery<'a> {
    consumer: &'a FrameResourceConsumerApi,
    stream: u64,
    generation: u64,
    offer: u64,
    flags: u32,
    expected_epoch: [u8; 16],
    last_sequence: u64,
    accepted_sequence: Option<u64>,
    calls: u32,
    result: Status,
    invalid: bool,
}
unsafe extern "C" fn deliver(context: *mut c_void, frame: *const CaptureResourceFrame) -> Status {
    if context.is_null() {
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: read supplies this stack context throughout the callback.
    let state = unsafe { &mut *context.cast::<Delivery<'_>>() };
    state.calls += 1;
    if frame.is_null() || state.calls != 1 {
        state.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: producer supplies the frame for the synchronous callback.
    let frame = unsafe { &*frame };
    if frame.offer_generation != state.generation
        || frame.offer_id != state.offer
        || frame.reserved != 0
        || !valid_color_metadata(&frame.color)
        || !valid_clock_metadata(&frame.clock)
        || frame.clock.version != 1
        || frame.clock.domain != amaterasu_media::FRAME_CLOCK_HOST_MONOTONIC
        || frame.clock.epoch_id != state.expected_epoch
        || frame.sequence <= state.last_sequence
        || !valid_clock_metadata(&frame.pacing_clock)
    {
        state.invalid = true;
        return STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the consumer binding and borrowed frame remain live during submission.
    state.result = unsafe {
        (state.consumer.submit)(
            state.consumer.base.context,
            state.stream,
            frame,
            state.flags,
        )
    };
    if state.result == STATUS_OK {
        state.accepted_sequence = Some(frame.sequence);
    }
    state.result
}

/// Per-stream negotiation state. The caller must cancel before releasing either
/// binding or closing its stream. No native table or frame pointer is retained.
/// Keep this object on the providers' owning thread.
pub(super) struct ResourceBridge {
    capture_stream: u64,
    consumer_stream: u64,
    selected: Option<(u64, u64)>,
    active: bool,
    retired: bool,
    required: u32,
    expected_epoch: [u8; 16],
    last_sequence: u64,
    thread: PhantomData<Rc<()>>,
}

impl ResourceBridge {
    pub(super) fn new(
        capture_stream: u64,
        consumer_stream: u64,
        required: u32,
        expected_host_epoch: [u8; 16],
    ) -> Result<Self, ConnectionError> {
        if required & !(FRAME_ACCESS_CPU | FRAME_ACCESS_GPU) != 0 || expected_host_epoch == [0; 16]
        {
            return Err(ConnectionError::Protocol);
        }
        Ok(Self {
            capture_stream,
            consumer_stream,
            selected: None,
            active: false,
            retired: false,
            required,
            expected_epoch: expected_host_epoch,
            last_sequence: 0,
            thread: PhantomData,
        })
    }

    pub(super) fn is_retired(&self) -> bool {
        self.retired
    }

    /// # Safety
    /// The authenticated consumer binding and stream must still be live, and
    /// this must run on the provider's owning thread before either is released.
    pub(super) unsafe fn cancel(&mut self, consumer: &FrameResourceConsumerApi) {
        if self.active {
            // SAFETY: caller guarantees the binding and stream are live.
            unsafe { (consumer.cancel)(consumer.base.context, self.consumer_stream) };
            self.active = false;
            self.retired = true;
        }
        self.selected = None;
    }

    unsafe fn enumerate_capture(
        &self,
        capture: &CaptureApiV1_8,
        sink: CaptureResourceOfferSink,
    ) -> Result<u64, ConnectionError> {
        let mut generation = 0;
        // SAFETY: caller guarantees the binding and stream; callback is synchronous.
        let status = unsafe {
            (capture.enumerate_resources)(
                capture.base.base.base.context,
                self.capture_stream,
                sink,
                &mut generation,
            )
        };
        if status != STATUS_OK {
            return Err(ConnectionError::Native(status));
        }
        if generation == 0 {
            return Err(ConnectionError::Protocol);
        }
        Ok(generation)
    }

    /// # Safety
    /// Both independently authenticated bindings and their streams must be live,
    /// belong to the intended principal/session, and be called on their owning
    /// thread. The caller must cancel before releasing either binding.
    pub(super) unsafe fn negotiate(
        &mut self,
        capture: &CaptureApiV1_8,
        consumer: &FrameResourceConsumerApi,
    ) -> Result<(), ConnectionError> {
        if self.retired {
            return Err(ConnectionError::Stale);
        }
        if self.selected.is_some() {
            return Ok(());
        }
        let mut consumer_ids = Ids::new(self.required);
        let mut consumer_generation = 0;
        let sink = CaptureResourceOfferSink {
            context: (&mut consumer_ids as *mut Ids).cast(),
            write: collect,
        };
        // SAFETY: the caller guarantees a live binding and callbacks are synchronous.
        let status = unsafe {
            (consumer.enumerate_resources)(
                consumer.base.context,
                self.consumer_stream,
                sink,
                &mut consumer_generation,
            )
        };
        if consumer_ids.invalid {
            return Err(ConnectionError::Protocol);
        }
        if status != STATUS_OK {
            return Err(ConnectionError::Native(status));
        }
        if consumer_generation == 0 {
            return Err(ConnectionError::Protocol);
        }
        let mut first = Ids::new(0);
        let sink = CaptureResourceOfferSink {
            context: (&mut first as *mut Ids).cast(),
            write: collect,
        };
        // SAFETY: the caller guarantees a live capture binding.
        let first_result = unsafe { self.enumerate_capture(capture, sink) };
        if first.invalid {
            return Err(ConnectionError::Protocol);
        }
        let producer_generation = first_result?;
        let mut state = Configure {
            ids: Ids::new(0),
            consumer_ids: &consumer_ids.eligible,
            consumer,
            stream: self.consumer_stream,
            generation: consumer_generation,
            producer_generation,
            required: self.required,
            selected: None,
            failure: None,
        };
        let sink = CaptureResourceOfferSink {
            context: (&mut state as *mut Configure<'_>).cast(),
            write: configure,
        };
        // SAFETY: the caller guarantees both live bindings.
        let second = unsafe { self.enumerate_capture(capture, sink) };
        self.active = state.selected.is_some();
        let error = if state.ids.invalid {
            Some(ConnectionError::Protocol)
        } else {
            state.failure.map(ConnectionError::Native)
        };
        if let Some(error) = error {
            // SAFETY: the caller guarantees the live consumer binding.
            unsafe { self.cancel(consumer) };
            return Err(error);
        }
        let second_generation = match second {
            Ok(generation) => generation,
            Err(error) => {
                // SAFETY: the caller guarantees the live consumer binding.
                unsafe { self.cancel(consumer) };
                return Err(error);
            }
        };
        first.ids.sort_unstable();
        state.ids.ids.sort_unstable();
        if producer_generation != second_generation || first.ids != state.ids.ids {
            // SAFETY: the caller guarantees the live consumer binding.
            unsafe { self.cancel(consumer) };
            return Err(ConnectionError::Stale);
        }
        let Some(offer) = state.selected else {
            return Err(ConnectionError::NoCompatibleOffer);
        };
        // SAFETY: the caller guarantees the live capture binding and stream.
        let status = unsafe {
            (capture.select_resource)(
                capture.base.base.base.context,
                self.capture_stream,
                producer_generation,
                offer,
            )
        };
        if status != STATUS_OK {
            // SAFETY: the caller guarantees the live consumer binding.
            unsafe { self.cancel(consumer) };
            return Err(if status == MEDIA_UNSUPPORTED {
                ConnectionError::Stale
            } else {
                ConnectionError::Native(status)
            });
        }
        self.selected = Some((producer_generation, offer));
        Ok(())
    }

    /// # Safety
    /// The requirements of `negotiate` apply throughout this synchronous call.
    pub(super) unsafe fn forward(
        &mut self,
        capture: &CaptureApiV1_8,
        consumer: &FrameResourceConsumerApi,
        flags: u32,
    ) -> Result<Status, ConnectionError> {
        if self.selected.is_none() {
            // SAFETY: caller supplies both live bindings.
            unsafe { self.negotiate(capture, consumer) }?;
        }
        let (generation, offer) = self.selected.expect("negotiated selection");
        let mut delivery = Delivery {
            consumer,
            stream: self.consumer_stream,
            generation,
            offer,
            flags,
            expected_epoch: self.expected_epoch,
            last_sequence: self.last_sequence,
            accepted_sequence: None,
            calls: 0,
            result: STATUS_OK,
            invalid: false,
        };
        let sink = CaptureResourceFrameSink {
            context: (&mut delivery as *mut Delivery<'_>).cast(),
            write: deliver,
        };
        // SAFETY: bindings and callback context remain live through this synchronous read.
        let status = unsafe {
            (capture.read_resource_frame)(capture.base.base.base.context, self.capture_stream, sink)
        };
        if delivery.invalid
            || delivery.calls > 1
            || (delivery.calls == 1 && status != delivery.result)
            || (delivery.calls == 0 && status == STATUS_OK)
        {
            // SAFETY: caller supplies a live consumer binding.
            unsafe { self.cancel(consumer) };
            return Err(ConnectionError::Protocol);
        }
        if delivery.calls == 0 && status == MEDIA_UNSUPPORTED {
            // SAFETY: caller supplies a live consumer binding.
            unsafe { self.cancel(consumer) };
            return Err(ConnectionError::Stale);
        }
        if let Some(sequence) = delivery.accepted_sequence {
            self.last_sequence = sequence;
        }
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_media::{
        FRAME_OWNERSHIP_RETAINABLE, FRAME_RESOURCE_VENDOR_BASE, FRAME_SYNC_NONE, FrameDescriptor,
    };
    use amaterasu_module_sdk::abi::{AbiHeader, NativeInterface};
    use std::ptr;

    #[derive(Default)]
    struct Consumer {
        configured: u32,
        submitted: u32,
        transfer: u32,
        epoch: [u8; 16],
        cancelled: u32,
        reject: bool,
        busy: bool,
        seen_stream: u64,
        seen_generation: u64,
        seen_producer_generation: u64,
        namespace: Option<[u8; 16]>,
    }
    unsafe extern "C" fn enumerate(
        _: *mut c_void,
        _: u64,
        _: CaptureResourceOfferSink,
        _: *mut u64,
    ) -> Status {
        STATUS_OK
    }
    unsafe extern "C" fn config(
        context: *mut c_void,
        stream: u64,
        generation: u64,
        id: u64,
        producer_generation: u64,
        offer: *const CaptureResourceOffer,
    ) -> Status {
        // SAFETY: fixture supplies both pointers for the call.
        let (consumer, offer) = unsafe { (&mut *context.cast::<Consumer>(), &*offer) };
        consumer.seen_stream = stream;
        consumer.seen_generation = generation;
        consumer.seen_producer_generation = producer_generation;
        if consumer.reject
            || consumer
                .namespace
                .is_some_and(|ns| ns != offer.resource.namespace)
            || id != 8
            || offer.resource.kind != FRAME_RESOURCE_VENDOR_BASE
        {
            return MEDIA_UNSUPPORTED;
        }
        consumer.configured += 1;
        STATUS_OK
    }
    unsafe extern "C" fn submit(
        context: *mut c_void,
        _: u64,
        frame: *const CaptureResourceFrame,
        _: u32,
    ) -> Status {
        // SAFETY: fixture supplies the consumer context and borrowed frame.
        unsafe {
            let consumer = &mut *context.cast::<Consumer>();
            if consumer.busy {
                return amaterasu_media::MEDIA_BUSY;
            }
            consumer.submitted += 1;
            consumer.transfer = (*frame).color.transfer;
            consumer.epoch = (*frame).pacing_clock.epoch_id;
        }
        STATUS_OK
    }
    unsafe extern "C" fn cancel(context: *mut c_void, _: u64) {
        // SAFETY: fixture supplies the consumer context.
        unsafe {
            (*context.cast::<Consumer>()).cancelled += 1;
        }
    }
    fn table(consumer: &mut Consumer) -> FrameResourceConsumerApi {
        FrameResourceConsumerApi {
            base: NativeInterface {
                header: AbiHeader::of::<FrameResourceConsumerApi>(),
                context: (consumer as *mut Consumer).cast(),
                release: None,
            },
            enumerate_resources: enumerate,
            configure: config,
            submit,
            cancel,
        }
    }
    fn descriptor(kind: u32) -> FrameDescriptor {
        FrameDescriptor {
            kind,
            version: 0,
            size: 0,
            reserved: 0,
            namespace: [0; 16],
            data: ptr::null(),
        }
    }
    fn offer() -> CaptureResourceOffer {
        CaptureResourceOffer {
            id: 5,
            ownership: FRAME_OWNERSHIP_RETAINABLE,
            accessibility: 0,
            reserved: 0,
            resource: descriptor(FRAME_RESOURCE_VENDOR_BASE),
            acquire_sync: descriptor(FRAME_SYNC_NONE),
            completion_sync: descriptor(FRAME_SYNC_NONE),
        }
    }
    unsafe extern "C" fn retain(_: *mut c_void) -> Status {
        STATUS_OK
    }
    unsafe extern "C" fn release(_: *mut c_void, _: *const FrameDescriptor) {}
    #[test]
    fn custom_offer_configures_and_frame_is_submitted_unchanged() {
        let mut consumer = Consumer::default();
        let api = table(&mut consumer);
        let mut state = Configure {
            ids: Ids::new(0),
            consumer_ids: &[7, 8],
            consumer: &api,
            stream: 3,
            generation: 4,
            producer_generation: 9,
            required: 0,
            selected: None,
            failure: None,
        };
        let offer = offer();
        // SAFETY: fixture objects outlive each synchronous callback.
        assert_eq!(
            unsafe { configure((&mut state as *mut Configure<'_>).cast(), &offer) },
            STATUS_OK
        );
        assert_eq!(state.selected, Some(5));
        let frame = CaptureResourceFrame {
            offer_generation: 9,
            offer_id: 5,
            timestamp_us: 1,
            capture_pacing_timestamp_us: 1,
            sequence: 1,
            width: 1,
            height: 1,
            freshness: 1,
            reserved: 0,
            color: amaterasu_media::FrameColorMetadata {
                version: 1,
                size: std::mem::size_of::<amaterasu_media::FrameColorMetadata>() as u32,
                transfer: amaterasu_media::FRAME_TRANSFER_PQ,
                ..Default::default()
            },
            clock: amaterasu_media::FrameClockMetadata {
                version: 1,
                size: std::mem::size_of::<amaterasu_media::FrameClockMetadata>() as u32,
                domain: amaterasu_media::FRAME_CLOCK_HOST_MONOTONIC,
                epoch_id: [1; 16],
                ..Default::default()
            },
            pacing_clock: amaterasu_media::FrameClockMetadata {
                version: 1,
                size: std::mem::size_of::<amaterasu_media::FrameClockMetadata>() as u32,
                domain: amaterasu_media::FRAME_CLOCK_HOST_MONOTONIC,
                epoch_id: [42; 16],
                ..Default::default()
            },
            resource: offer.resource,
            acquire_sync: offer.acquire_sync,
            context: ptr::null_mut(),
            retain,
            release,
        };
        let mut delivery = Delivery {
            consumer: &api,
            stream: 3,
            generation: 9,
            offer: 5,
            flags: 0,
            expected_epoch: [1; 16],
            last_sequence: 0,
            accepted_sequence: None,
            calls: 0,
            result: STATUS_OK,
            invalid: false,
        };
        assert_eq!(
            unsafe { deliver((&mut delivery as *mut Delivery<'_>).cast(), &frame) },
            STATUS_OK
        );
        assert_eq!(consumer.configured, 1);
        assert_eq!(consumer.submitted, 1);
        assert_eq!(consumer.transfer, amaterasu_media::FRAME_TRANSFER_PQ);
        assert_eq!(consumer.epoch, [42; 16]);
        assert_eq!(
            unsafe { deliver((&mut delivery as *mut Delivery<'_>).cast(), &frame) },
            STATUS_INVALID_ARGUMENT
        );
        assert!(delivery.invalid);
    }
    #[test]
    fn rejects_duplicate_offers_and_incompatible_consumer() {
        let mut consumer = Consumer {
            reject: true,
            ..Consumer::default()
        };
        let api = table(&mut consumer);
        let mut state = Configure {
            ids: Ids::new(0),
            consumer_ids: &[8],
            consumer: &api,
            stream: 3,
            generation: 4,
            producer_generation: 9,
            required: 0,
            selected: None,
            failure: None,
        };
        let offer = offer();
        // SAFETY: fixture objects outlive each synchronous callback.
        assert_eq!(
            unsafe { configure((&mut state as *mut Configure<'_>).cast(), &offer) },
            STATUS_OK
        );
        assert!(state.selected.is_none());
        assert_eq!(
            unsafe { configure((&mut state as *mut Configure<'_>).cast(), &offer) },
            STATUS_INVALID_ARGUMENT
        );
        assert!(state.ids.invalid);
    }
    #[test]
    fn gpu_requirement_rejects_unspecified_and_cpu_access() {
        let mut ids = Ids::new(FRAME_ACCESS_GPU);
        let mut candidate = offer();
        candidate.accessibility = 0;
        assert!(ids.add(&candidate));
        candidate.id += 1;
        candidate.accessibility = FRAME_ACCESS_CPU;
        assert!(ids.add(&candidate));
        assert!(ids.eligible.is_empty());
        candidate.id += 1;
        candidate.accessibility = FRAME_ACCESS_GPU;
        assert!(ids.add(&candidate));
        assert_eq!(ids.eligible, vec![candidate.id]);
        candidate.id += 1;
        candidate.accessibility = 0x8000_0000;
        assert!(!ids.add(&candidate));
    }
    #[test]
    fn null_offer_poisoning_survives_a_swallowed_callback_error() {
        let mut ids = Ids::new(0);
        // SAFETY: the callback context remains live.
        assert_eq!(
            unsafe { collect((&mut ids as *mut Ids).cast(), ptr::null()) },
            STATUS_INVALID_ARGUMENT
        );
        assert_eq!(
            unsafe { collect((&mut ids as *mut Ids).cast(), &offer()) },
            STATUS_OK
        );
        assert!(ids.invalid);
        let mut consumer = Consumer::default();
        let api = table(&mut consumer);
        let mut state = Configure {
            ids: Ids::new(0),
            consumer_ids: &[8],
            consumer: &api,
            stream: 3,
            generation: 4,
            producer_generation: 9,
            required: 0,
            selected: None,
            failure: None,
        };
        assert_eq!(
            unsafe { configure((&mut state as *mut Configure<'_>).cast(), ptr::null()) },
            STATUS_INVALID_ARGUMENT
        );
        assert_eq!(
            unsafe { configure((&mut state as *mut Configure<'_>).cast(), &offer()) },
            STATUS_OK
        );
        assert!(state.ids.invalid);
    }

    #[derive(Default)]
    struct Producer {
        selected: bool,
        stale: bool,
        selection_fails: bool,
        enumerate_calls: u32,
        change_generation: bool,
        wrong_frame: bool,
        no_frame_busy: bool,
        frame_sequence: u64,
        wrong_epoch: bool,
    }
    unsafe extern "C" fn producer_enumerate(
        context: *mut c_void,
        _: u64,
        sink: CaptureResourceOfferSink,
        generation: *mut u64,
    ) -> Status {
        // SAFETY: the fixture owns the context and output pointer.
        let producer = unsafe { &mut *context.cast::<Producer>() };
        producer.enumerate_calls += 1;
        let mut candidate = offer();
        candidate.accessibility = FRAME_ACCESS_GPU;
        let status = unsafe { (sink.write)(sink.context, &candidate) };
        if status != STATUS_OK {
            return status;
        }
        unsafe {
            *generation =
                if producer.change_generation && producer.enumerate_calls.is_multiple_of(2) {
                    10
                } else {
                    9
                };
        }
        STATUS_OK
    }
    unsafe extern "C" fn producer_select(
        context: *mut c_void,
        _: u64,
        generation: u64,
        id: u64,
    ) -> Status {
        let producer = unsafe { &mut *context.cast::<Producer>() };
        if producer.selection_fails || generation != 9 || id != 5 {
            return MEDIA_UNSUPPORTED;
        }
        producer.selected = true;
        STATUS_OK
    }
    unsafe extern "C" fn producer_read(
        context: *mut c_void,
        _: u64,
        sink: CaptureResourceFrameSink,
    ) -> Status {
        let producer = unsafe { &mut *context.cast::<Producer>() };
        if producer.stale || !producer.selected {
            return MEDIA_UNSUPPORTED;
        }
        if producer.no_frame_busy {
            return amaterasu_media::MEDIA_BUSY;
        }
        producer.frame_sequence += 1;
        let descriptor = descriptor(FRAME_RESOURCE_VENDOR_BASE);
        let frame = CaptureResourceFrame {
            offer_generation: if producer.wrong_frame { 10 } else { 9 },
            offer_id: 5,
            timestamp_us: 1,
            capture_pacing_timestamp_us: 1,
            sequence: producer.frame_sequence,
            width: 1,
            height: 1,
            freshness: 1,
            reserved: 0,
            color: Default::default(),
            clock: amaterasu_media::FrameClockMetadata {
                version: 1,
                size: std::mem::size_of::<amaterasu_media::FrameClockMetadata>() as u32,
                domain: amaterasu_media::FRAME_CLOCK_HOST_MONOTONIC,
                epoch_id: if producer.wrong_epoch {
                    [2; 16]
                } else {
                    [1; 16]
                },
                ..Default::default()
            },
            pacing_clock: Default::default(),
            resource: descriptor,
            acquire_sync: self::descriptor(FRAME_SYNC_NONE),
            context: ptr::null_mut(),
            retain,
            release,
        };
        unsafe { (sink.write)(sink.context, &frame) }
    }
    unsafe extern "C" fn consumer_enumerate(
        _: *mut c_void,
        _: u64,
        sink: CaptureResourceOfferSink,
        generation: *mut u64,
    ) -> Status {
        let mut candidate = offer();
        candidate.id = 8;
        candidate.accessibility = FRAME_ACCESS_GPU;
        let status = unsafe { (sink.write)(sink.context, &candidate) };
        unsafe {
            *generation = 4;
        }
        status
    }
    unsafe extern "C" fn unused_frame(
        _: *mut c_void,
        _: u64,
        _: amaterasu_media::FrameSink,
    ) -> Status {
        MEDIA_UNSUPPORTED
    }
    unsafe extern "C" fn unused_geometry(
        _: *mut c_void,
        _: u64,
        _: *mut amaterasu_media::CaptureGeometry,
    ) -> Status {
        MEDIA_UNSUPPORTED
    }
    unsafe extern "C" fn unused_fresh(
        _: *mut c_void,
        _: u64,
        _: amaterasu_media::FreshFrameSinkV1_2,
    ) -> Status {
        MEDIA_UNSUPPORTED
    }
    unsafe extern "C" fn unused_timestamp(
        _: *mut c_void,
        _: u64,
        _: amaterasu_media::FreshFrameSinkV1_3,
    ) -> Status {
        MEDIA_UNSUPPORTED
    }
    unsafe extern "C" fn unused_dmabuf(
        _: *mut c_void,
        _: u64,
        _: amaterasu_media::DmaBufFrameSink,
    ) -> Status {
        MEDIA_UNSUPPORTED
    }
    unsafe extern "C" fn unused_presentation(
        _: *mut c_void,
        _: u64,
        _: *mut amaterasu_media::PresentationStatus,
    ) -> Status {
        MEDIA_UNSUPPORTED
    }
    unsafe extern "C" fn unused_lease(
        _: *mut c_void,
        _: u64,
        _: amaterasu_media::DmaBufLeaseSink,
    ) -> Status {
        MEDIA_UNSUPPORTED
    }
    unsafe extern "C" fn unused_subscribe(
        _: *mut c_void,
        _: u64,
        _: amaterasu_media::CaptureEventSink,
    ) -> Status {
        MEDIA_UNSUPPORTED
    }
    unsafe extern "C" fn unused_unsubscribe(_: *mut c_void, _: u64) {}
    unsafe extern "C" fn unused_swap(
        _: *mut c_void,
        _: u64,
        _: u64,
        _: u32,
        _: amaterasu_module_sdk::abi::BorrowedBytes,
    ) -> Status {
        MEDIA_UNSUPPORTED
    }
    fn capture_table(producer: &mut Producer) -> CaptureApiV1_8 {
        CaptureApiV1_8 {
            base: amaterasu_media::CaptureApiV1_7 {
                base: amaterasu_media::CaptureApiV1_6 {
                    base: NativeInterface {
                        header: AbiHeader::of::<CaptureApiV1_8>(),
                        context: (producer as *mut Producer).cast(),
                        release: None,
                    },
                    read_frame: unused_frame,
                    geometry: unused_geometry,
                    read_frame_with_freshness: unused_fresh,
                    read_frame_with_capture_timestamp: unused_timestamp,
                    read_dmabuf_frame: unused_dmabuf,
                    latest_presentation: unused_presentation,
                    read_dmabuf_frame_leased: unused_lease,
                },
                subscribe_events: unused_subscribe,
                unsubscribe_events: unused_unsubscribe,
                resolve_swap: unused_swap,
            },
            enumerate_resources: producer_enumerate,
            select_resource: producer_select,
            read_resource_frame: producer_read,
        }
    }
    #[test]
    fn connection_forwards_then_stale_cancels_once_without_reactivation() {
        let mut producer = Producer::default();
        let capture = capture_table(&mut producer);
        let mut consumer = Consumer::default();
        let mut api = table(&mut consumer);
        api.enumerate_resources = consumer_enumerate;
        // SAFETY: fixture tables and contexts outlive the connection.
        let mut connection = ResourceBridge::new(1, 3, FRAME_ACCESS_GPU, [1; 16]).unwrap();
        assert_eq!(consumer.cancelled, 0);
        assert_eq!(
            unsafe { connection.forward(&capture, &api, 0) },
            Ok(STATUS_OK)
        );
        assert_eq!(consumer.submitted, 1);
        assert_eq!(consumer.cancelled, 0);
        producer.stale = true;
        std::hint::black_box(&producer);
        assert_eq!(
            unsafe { connection.forward(&capture, &api, 0) },
            Err(ConnectionError::Stale)
        );
        assert_eq!(consumer.cancelled, 1);
        assert_eq!(
            unsafe { connection.forward(&capture, &api, 0) },
            Err(ConnectionError::Stale)
        );
        // SAFETY: both fixture bindings remain live.
        unsafe { connection.cancel(&api) };
        assert_eq!(consumer.cancelled, 1);
    }
    #[test]
    fn failed_selection_and_changed_generation_cancel_configured_consumer_once() {
        for generation_changes in [false, true] {
            let mut producer = Producer {
                selection_fails: !generation_changes,
                change_generation: generation_changes,
                ..Producer::default()
            };
            let capture = capture_table(&mut producer);
            let mut consumer = Consumer::default();
            let mut api = table(&mut consumer);
            api.enumerate_resources = consumer_enumerate;
            // SAFETY: fixture tables and contexts outlive the connection.
            let mut connection = ResourceBridge::new(1, 3, FRAME_ACCESS_GPU, [1; 16]).unwrap();
            assert_eq!(
                unsafe { connection.negotiate(&capture, &api) },
                Err(ConnectionError::Stale)
            );
            assert_eq!(consumer.configured, 1);
            assert_eq!(consumer.cancelled, 1);
            // SAFETY: both fixture bindings remain live.
            unsafe { connection.cancel(&api) };
            assert_eq!(consumer.cancelled, 1);
        }
        let mut producer = Producer::default();
        let capture = capture_table(&mut producer);
        let mut consumer = Consumer {
            reject: true,
            ..Consumer::default()
        };
        let mut api = table(&mut consumer);
        api.enumerate_resources = consumer_enumerate;
        let mut connection = ResourceBridge::new(1, 3, FRAME_ACCESS_GPU, [1; 16]).unwrap();
        assert_eq!(
            unsafe { connection.negotiate(&capture, &api) },
            Err(ConnectionError::NoCompatibleOffer)
        );
        // SAFETY: both fixture bindings remain live.
        unsafe { connection.cancel(&api) };
        assert_eq!(consumer.cancelled, 0);
    }
    #[test]
    fn malformed_known_metadata_is_rejected_and_unknown_versions_remain_opaque() {
        let mut color = amaterasu_media::FrameColorMetadata {
            version: 1,
            size: 1,
            ..Default::default()
        };
        assert!(!valid_color_metadata(&color));
        color.version = 42;
        assert!(valid_color_metadata(&color));
        let mut clock = amaterasu_media::FrameClockMetadata {
            version: 0,
            epoch_id: [1; 16],
            ..Default::default()
        };
        assert!(!valid_clock_metadata(&clock));
        clock.version = 1;
        clock.size = std::mem::size_of::<amaterasu_media::FrameClockMetadata>() as u32;
        clock.reserved = 1;
        assert!(!valid_clock_metadata(&clock));
        clock.version = 42;
        assert!(valid_clock_metadata(&clock));
    }
    #[test]
    fn opaque_namespace_and_stream_generations_are_consumer_decisions() {
        let mut producer = Producer::default();
        let capture = capture_table(&mut producer);
        let mut consumer = Consumer {
            namespace: Some([1; 16]),
            ..Consumer::default()
        };
        let mut api = table(&mut consumer);
        api.enumerate_resources = consumer_enumerate;
        let mut bridge = ResourceBridge::new(91, 37, FRAME_ACCESS_GPU, [1; 16]).unwrap();
        // SAFETY: fixture bindings and stream contexts live throughout the operation.
        assert_eq!(
            unsafe { bridge.negotiate(&capture, &api) },
            Err(ConnectionError::NoCompatibleOffer)
        );
        assert!(!producer.selected);
        assert_eq!(
            (
                consumer.seen_stream,
                consumer.seen_generation,
                consumer.seen_producer_generation
            ),
            (37, 4, 9)
        );
        consumer.namespace = Some([0; 16]);
        assert_eq!(unsafe { bridge.negotiate(&capture, &api) }, Ok(()));
        assert!(producer.selected);
        unsafe { bridge.cancel(&api) };
        assert_eq!(consumer.cancelled, 1);
    }

    #[test]
    fn busy_does_not_submit_and_wrong_frame_retires_the_consumer() {
        let mut producer = Producer::default();
        let capture = capture_table(&mut producer);
        let mut consumer = Consumer {
            busy: true,
            ..Consumer::default()
        };
        let mut api = table(&mut consumer);
        api.enumerate_resources = consumer_enumerate;
        let mut bridge = ResourceBridge::new(1, 3, FRAME_ACCESS_GPU, [1; 16]).unwrap();
        // SAFETY: fixture bindings and stream contexts live throughout these calls.
        assert_eq!(
            unsafe { bridge.forward(&capture, &api, 0) },
            Ok(amaterasu_media::MEDIA_BUSY)
        );
        assert_eq!(consumer.submitted, 0);
        producer.no_frame_busy = true;
        std::hint::black_box(&producer);
        assert_eq!(
            unsafe { bridge.forward(&capture, &api, 0) },
            Ok(amaterasu_media::MEDIA_BUSY)
        );
        producer.no_frame_busy = false;
        producer.wrong_frame = true;
        std::hint::black_box(&producer);
        assert_eq!(
            unsafe { bridge.forward(&capture, &api, 0) },
            Err(ConnectionError::Protocol)
        );
        assert_eq!(consumer.submitted, 0);
        assert_eq!(consumer.cancelled, 1);
        unsafe { bridge.cancel(&api) };
        assert_eq!(consumer.cancelled, 1);
    }
    #[test]
    fn equal_offer_ids_on_distinct_streams_do_not_share_bridge_state() {
        let mut producer_a = Producer::default();
        let mut producer_b = Producer::default();
        let capture_a = capture_table(&mut producer_a);
        let capture_b = capture_table(&mut producer_b);
        let mut consumer_a = Consumer::default();
        let mut consumer_b = Consumer::default();
        let mut api_a = table(&mut consumer_a);
        let mut api_b = table(&mut consumer_b);
        api_a.enumerate_resources = consumer_enumerate;
        api_b.enumerate_resources = consumer_enumerate;
        let mut bridge_a = ResourceBridge::new(1, 31, FRAME_ACCESS_GPU, [1; 16]).unwrap();
        let mut bridge_b = ResourceBridge::new(2, 32, FRAME_ACCESS_GPU, [1; 16]).unwrap();
        // SAFETY: each independently bound fixture and stream stays live through cancellation.
        assert_eq!(
            unsafe { bridge_a.forward(&capture_a, &api_a, 0) },
            Ok(STATUS_OK)
        );
        assert_eq!(
            unsafe { bridge_b.forward(&capture_b, &api_b, 0) },
            Ok(STATUS_OK)
        );
        assert_eq!((consumer_a.seen_stream, consumer_b.seen_stream), (31, 32));
        unsafe { bridge_a.cancel(&api_a) };
        assert_eq!((consumer_a.cancelled, consumer_b.cancelled), (1, 0));
        assert_eq!(
            unsafe { bridge_b.forward(&capture_b, &api_b, 0) },
            Ok(STATUS_OK)
        );
        unsafe { bridge_b.cancel(&api_b) };
        assert_eq!((consumer_a.submitted, consumer_b.submitted), (1, 2));
    }
    #[test]
    fn epoch_and_successful_sequence_are_enforced_before_submission() {
        assert!(ResourceBridge::new(1, 2, 0, [0; 16]).is_err());
        let mut producer = Producer::default();
        let capture = capture_table(&mut producer);
        let mut consumer = Consumer {
            busy: true,
            ..Consumer::default()
        };
        let mut api = table(&mut consumer);
        api.enumerate_resources = consumer_enumerate;
        let mut bridge = ResourceBridge::new(1, 2, FRAME_ACCESS_GPU, [1; 16]).unwrap();
        // SAFETY: fixture bindings and contexts remain live until cancellation.
        assert_eq!(
            unsafe { bridge.forward(&capture, &api, 0) },
            Ok(amaterasu_media::MEDIA_BUSY)
        );
        producer.frame_sequence = 0;
        consumer.busy = false;
        std::hint::black_box((&producer, &consumer));
        assert_eq!(unsafe { bridge.forward(&capture, &api, 0) }, Ok(STATUS_OK));
        producer.frame_sequence = 0;
        std::hint::black_box(&producer);
        assert_eq!(
            unsafe { bridge.forward(&capture, &api, 0) },
            Err(ConnectionError::Protocol)
        );
        assert_eq!(consumer.submitted, 1);
        assert_eq!(consumer.cancelled, 1);

        let mut producer = Producer {
            wrong_epoch: true,
            ..Producer::default()
        };
        let capture = capture_table(&mut producer);
        let mut consumer = Consumer::default();
        let mut api = table(&mut consumer);
        api.enumerate_resources = consumer_enumerate;
        let mut bridge = ResourceBridge::new(1, 2, FRAME_ACCESS_GPU, [1; 16]).unwrap();
        assert_eq!(
            unsafe { bridge.forward(&capture, &api, 0) },
            Err(ConnectionError::Protocol)
        );
        assert_eq!((consumer.submitted, consumer.cancelled), (0, 1));
    }
}

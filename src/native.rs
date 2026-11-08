use crate::sessions::Sessions;
use amaterasu_abi::*;
use amaterasu_media::*;
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
};

pub type Shared = Arc<Mutex<Sessions>>;
pub struct Registration {
    pub factory: NativeServiceFactory,
    _context: Box<Shared>,
}

impl Registration {
    pub fn new(shared: Shared) -> Self {
        let mut context = Box::new(shared);
        let factory = NativeServiceFactory {
            header: AbiHeader::of::<NativeServiceFactory>(),
            context: (&mut *context as *mut Shared).cast(),
            bind: Some(bind),
        };
        Self {
            factory,
            _context: context,
        }
    }
}

/// The v1.5 table preserves earlier prefixes. Provider references remain
/// on the module thread until encoded output or joined encoder shutdown.
struct Binding {
    api: EncoderApiV1_5,
    owner: String,
    shared: Shared,
}

unsafe extern "C" fn bind(
    context: *mut c_void,
    principal: BorrowedBytes,
    query: *const NativeInterfaceQuery,
    output: *mut *const NativeInterface,
) -> Status {
    guarded(|| {
        if context.is_null() || query.is_null() || output.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: runtime provides live factory/query/output and principal bytes.
        unsafe {
            *output = std::ptr::null();
            let query = &*query;
            if !query.header.supports::<NativeInterfaceQuery>()
                || query.interface_major != INTERFACE_MAJOR
                || query.min_minor > ENCODER_RESOURCE_INTERFACE_MINOR
                || query.min_size > ENCODER_API_V1_5_SIZE
            {
                return MEDIA_UNSUPPORTED;
            }
            let owner = match principal
                .read()
                .ok()
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
            {
                Some(value) if !value.is_empty() && value.len() <= 128 => value.to_owned(),
                _ => return STATUS_INVALID_ARGUMENT,
            };
            let shared = (&*context.cast::<Shared>()).clone();
            let mut binding = Box::new(Binding {
                api: EncoderApiV1_5 {
                    base: EncoderApiV1_4 {
                        base: EncoderApiV1_3 {
                            base: EncoderApiV1_2 {
                                base: NativeInterface {
                                    header: AbiHeader {
                                        size: ENCODER_API_V1_5_SIZE,
                                        major: INTERFACE_MAJOR,
                                        minor: ENCODER_RESOURCE_INTERFACE_MINOR,
                                    },
                                    context: std::ptr::null_mut(),
                                    release: Some(release),
                                },
                                submit,
                                receive,
                                drain,
                                submit_dmabuf,
                                submit_dmabuf_leased,
                                cancel_dmabuf_leases,
                            },
                            encode_timing,
                        },
                        gpu_input_ready,
                    },
                    enumerate_resources,
                    configure_resource,
                    submit_resource,
                },
                owner,
                shared,
            });
            binding.api.base.base.base.base.context = (&mut *binding as *mut Binding).cast();
            let pointer = Box::into_raw(binding);
            *output = &(*pointer).api.base.base.base.base;
        }
        STATUS_OK
    })
}

unsafe extern "C" fn release(context: *mut c_void) {
    let _ = guarded(|| {
        if !context.is_null() {
            // SAFETY: release is called exactly once after binding calls finish.
            drop(unsafe { Box::from_raw(context.cast::<Binding>()) });
        }
        STATUS_OK
    });
}

unsafe extern "C" fn submit(
    context: *mut c_void,
    id: u64,
    frame: *const CpuFrame,
    force: u32,
) -> Status {
    guarded(|| {
        if context.is_null() || frame.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: live binding context and frame are guaranteed by native ABI.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut sessions) = binding.shared.try_lock() else {
            return MEDIA_BUSY;
        };
        unsafe { sessions.submit(&binding.owner, id, &*frame, force) }
    })
}

/// STATUS_OK means every descriptor the encoder needs was duplicated before
/// returning; the producer may close or reuse its own descriptors at once.
unsafe extern "C" fn submit_dmabuf(
    context: *mut c_void,
    id: u64,
    frame: *const DmaBufFrame,
    force: u32,
) -> Status {
    guarded(|| {
        if context.is_null() || frame.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: live binding context and frame are guaranteed by native ABI.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut sessions) = binding.shared.try_lock() else {
            return MEDIA_BUSY;
        };
        // SAFETY: the producer keeps the frame's descriptors open for this call.
        unsafe { sessions.submit_dmabuf(&binding.owner, id, &*frame, force) }
    })
}

/// The encoder retains immutable storage before queueing GPU work. References
/// are retired on the module thread after output or during joined cancellation.
unsafe extern "C" fn submit_dmabuf_leased(
    context: *mut c_void,
    id: u64,
    lease: *const DmaBufFrameLease,
    force: u32,
) -> Status {
    guarded(|| {
        if context.is_null() || lease.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: runtime retains the binding and provider lends a live lease.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut sessions) = binding.shared.try_lock() else {
            return MEDIA_BUSY;
        };
        // SAFETY: the caller holds immutable source storage until return.
        unsafe { sessions.submit_dmabuf_leased(&binding.owner, id, &*lease, force) }
    })
}

unsafe extern "C" fn enumerate_resources(
    context: *mut c_void,
    id: u64,
    sink: CaptureResourceOfferSink,
    generation: *mut u64,
) -> Status {
    guarded(|| {
        if context.is_null() || generation.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: runtime retains the binding; the caller's outputs are live.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut sessions) = binding.shared.try_lock() else {
            return MEDIA_BUSY;
        };
        // SAFETY: the sink and generation output are live for this call.
        unsafe { sessions.enumerate_resources(&binding.owner, id, sink, &mut *generation) }
    })
}

unsafe extern "C" fn configure_resource(
    context: *mut c_void,
    id: u64,
    consumer_generation: u64,
    consumer_offer: u64,
    producer_generation: u64,
    producer: *const CaptureResourceOffer,
) -> Status {
    guarded(|| {
        if context.is_null() || producer.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: runtime retains the binding; the producer offer is borrowed live.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut sessions) = binding.shared.try_lock() else {
            return MEDIA_BUSY;
        };
        unsafe {
            sessions.configure_resource(
                &binding.owner,
                id,
                consumer_generation,
                consumer_offer,
                producer_generation,
                &*producer,
            )
        }
    })
}

unsafe extern "C" fn submit_resource(
    context: *mut c_void,
    id: u64,
    frame: *const CaptureResourceFrame,
    flags: u32,
) -> Status {
    guarded(|| {
        if context.is_null() || frame.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: runtime retains the binding; the producer lends a live frame.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut sessions) = binding.shared.try_lock() else {
            return MEDIA_BUSY;
        };
        // SAFETY: the frame, payloads and callbacks are live for this call.
        unsafe { sessions.submit_resource(&binding.owner, id, &*frame, flags) }
    })
}

unsafe extern "C" fn gpu_input_ready(context: *mut c_void, id: u64) -> Status {
    guarded(|| {
        if context.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: the runtime keeps this authenticated binding live during the query.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut sessions) = binding.shared.try_lock() else {
            return MEDIA_BUSY;
        };
        sessions.gpu_input_ready(&binding.owner, id)
    })
}

/// Quiesces the stream while the caller still owns its capture and processor
/// bindings. Closing joins native worker activity before ownership is returned.
unsafe extern "C" fn cancel_dmabuf_leases(context: *mut c_void, id: u64) {
    let _ = guarded(|| {
        if context.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: the consumer owns this live native binding through cancellation.
        let binding = unsafe { &*context.cast::<Binding>() };
        // Cancellation is a mandatory lifetime barrier, so it must wait rather
        // than report busy while a concurrent source read is still in flight.
        let mut sessions = binding
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Missing sessions were already retired. Ownership remains caller-scoped.
        let _ = sessions.close(&binding.owner, id);
        STATUS_OK
    });
}

unsafe extern "C" fn receive(context: *mut c_void, id: u64, sink: PacketSink) -> Status {
    guarded(|| {
        if context.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: binding remains live through call and sink obeys ABI contract.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut sessions) = binding.shared.try_lock() else {
            return MEDIA_BUSY;
        };
        unsafe { sessions.receive(&binding.owner, id, sink) }
    })
}

unsafe extern "C" fn encode_timing(
    context: *mut c_void,
    id: u64,
    output: *mut EncodeTimingSnapshotV1,
) -> Status {
    guarded(|| {
        if context.is_null() || output.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: binding and caller-owned writable output outlive this call.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut sessions) = binding.shared.try_lock() else {
            return MEDIA_BUSY;
        };
        match sessions.encode_timing(&binding.owner, id) {
            Ok(snapshot) => {
                unsafe { output.write(snapshot) };
                STATUS_OK
            }
            Err(status) => status,
        }
    })
}

unsafe extern "C" fn drain(context: *mut c_void, id: u64) -> Status {
    guarded(|| {
        if context.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: binding remains live through call.
        let binding = unsafe { &*context.cast::<Binding>() };
        let Ok(mut sessions) = binding.shared.try_lock() else {
            return MEDIA_BUSY;
        };
        sessions.drain(&binding.owner, id)
    })
}

fn guarded(operation: impl FnOnce() -> Status) -> Status {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or(STATUS_INTERNAL_ERROR)
}

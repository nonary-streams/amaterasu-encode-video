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

/// The v1.1 table is a strict extension of v1.0, so one binding serves both:
/// v1.0 consumers read only the unchanged prefix.
struct Binding {
    api: EncoderApiV1_1,
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
                || query.min_minor > ENCODER_DMABUF_INTERFACE_MINOR
                || query.min_size > ENCODER_API_V1_1_SIZE
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
                api: EncoderApiV1_1 {
                    base: NativeInterface {
                        header: AbiHeader {
                            size: ENCODER_API_V1_1_SIZE,
                            major: INTERFACE_MAJOR,
                            minor: ENCODER_DMABUF_INTERFACE_MINOR,
                        },
                        context: std::ptr::null_mut(),
                        release: Some(release),
                    },
                    submit,
                    receive,
                    drain,
                    submit_dmabuf,
                },
                owner,
                shared,
            });
            binding.api.base.context = (&mut *binding as *mut Binding).cast();
            let pointer = Box::into_raw(binding);
            *output = &(*pointer).api.base;
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

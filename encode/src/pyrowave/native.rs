//! C ABI boundary for the standard submitted-resource input contract.
use super::session::{Provider, Selection, Shared};
use amaterasu_media::*;
use amaterasu_module_sdk::{abi::*, encode::ProviderAdapter, media::BorrowedResourceFrame};
use std::{
    collections::BTreeSet,
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::Mutex,
};

pub(super) struct Registration {
    pub provider: NativeServiceFactory,
    pub consumer: NativeServiceFactory,
    _shared: Box<Shared>,
}
impl Registration {
    pub fn new(shared: Shared) -> Self {
        let mut shared = Box::new(shared);
        let context = (&mut *shared as *mut Shared).cast();
        Self {
            provider: NativeServiceFactory {
                header: AbiHeader::of::<NativeServiceFactory>(),
                context,
                bind: Some(bind_provider),
            },
            consumer: NativeServiceFactory {
                header: AbiHeader::of::<NativeServiceFactory>(),
                context,
                bind: Some(bind_consumer),
            },
            _shared: shared,
        }
    }
}
fn guard(operation: impl FnOnce() -> Status) -> Status {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or(STATUS_INTERNAL_ERROR)
}
fn barrier(operation: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(operation)).is_err() {
        std::process::abort();
    }
}
// SAFETY: runtime lends factory context, query, principal and output for the call.
unsafe fn arguments(
    context: *mut c_void,
    principal: BorrowedBytes,
    query: *const NativeInterfaceQuery,
    output: *mut *const NativeInterface,
    size: u32,
) -> Result<(Shared, String), Status> {
    if context.is_null() || query.is_null() || output.is_null() {
        return Err(STATUS_INVALID_ARGUMENT);
    }
    unsafe {
        *output = ptr::null();
    }
    let query = unsafe { &*query };
    if !query.header.supports::<NativeInterfaceQuery>()
        || query.interface_major != 1
        || query.min_minor > 0
        || query.min_size > size
    {
        return Err(MEDIA_UNSUPPORTED);
    }
    let bytes = unsafe { principal.read()? };
    let owner = std::str::from_utf8(bytes)
        .ok()
        .filter(|owner| !owner.is_empty() && owner.len() <= 128)
        .ok_or(STATUS_INVALID_ARGUMENT)?;
    Ok((
        unsafe { (&*context.cast::<Shared>()).clone() },
        owner.into(),
    ))
}
unsafe extern "C" fn bind_provider(
    context: *mut c_void,
    principal: BorrowedBytes,
    query: *const NativeInterfaceQuery,
    output: *mut *const NativeInterface,
) -> Status {
    guard(|| {
        // SAFETY: factory/context and runtime query borrows live through bind.
        let (shared, owner) = match unsafe {
            arguments(
                context,
                principal,
                query,
                output,
                ENCODER_PROVIDER_API_V1_0_SIZE,
            )
        } {
            Ok(value) => value,
            Err(status) => return status,
        };
        let table = ProviderAdapter::new(Provider { owner, shared }).into_binding();
        // SAFETY: output is writable, ownership transfers to the runtime once.
        unsafe {
            *output = table.cast();
        }
        STATUS_OK
    })
}
#[repr(C)]
struct Consumer {
    api: FrameResourceConsumerApi,
    owner: String,
    shared: Shared,
    sessions: Mutex<BTreeSet<u64>>,
}
unsafe extern "C" fn bind_consumer(
    context: *mut c_void,
    principal: BorrowedBytes,
    query: *const NativeInterfaceQuery,
    output: *mut *const NativeInterface,
) -> Status {
    guard(|| {
        // SAFETY: factory/context and runtime query borrows live through bind.
        let (shared, owner) = match unsafe {
            arguments(
                context,
                principal,
                query,
                output,
                FRAME_RESOURCE_CONSUMER_API_V1_0_SIZE,
            )
        } {
            Ok(value) => value,
            Err(status) => return status,
        };
        let mut binding = Box::new(Consumer {
            api: FrameResourceConsumerApi {
                base: NativeInterface {
                    header: AbiHeader::of::<FrameResourceConsumerApi>(),
                    context: ptr::null_mut(),
                    release: Some(release),
                },
                enumerate_resources: enumerate,
                configure,
                submit,
                cancel,
            },
            owner,
            shared,
            sessions: Mutex::new(BTreeSet::new()),
        });
        binding.api.base.context = (&mut *binding as *mut Consumer).cast();
        // SAFETY: Box pins the table until the one release callback.
        unsafe {
            *output = (&*Box::into_raw(binding) as *const Consumer).cast();
        }
        STATUS_OK
    })
}
unsafe extern "C" fn release(context: *mut c_void) {
    barrier(|| {
        if context.is_null() {
            std::process::abort();
        }
        // SAFETY: runtime calls once after all binding operations retire.
        let binding = unsafe { Box::from_raw(context.cast::<Consumer>()) };
        let ids = binding
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = binding
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in ids.iter() {
            state.close(&binding.owner, *id);
        }
    });
}
fn none() -> FrameDescriptor {
    FrameDescriptor {
        kind: FRAME_SYNC_NONE,
        version: 0,
        size: 0,
        reserved: 0,
        namespace: [0; 16],
        data: ptr::null(),
    }
}
pub(super) fn is_none(value: &FrameDescriptor) -> bool {
    value.kind == FRAME_SYNC_NONE
        && value.version == 0
        && value.size == 0
        && value.reserved == 0
        && value.namespace == [0; 16]
        && value.data.is_null()
}
unsafe extern "C" fn enumerate(
    context: *mut c_void,
    id: u64,
    sink: CaptureResourceOfferSink,
    generation: *mut u64,
) -> Status {
    guard(|| {
        if context.is_null() || generation.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: live binding context from this table.
        let binding = unsafe { &*context.cast::<Consumer>() };
        let state = match binding.shared.try_lock() {
            Ok(state) => state,
            Err(_) => return MEDIA_BUSY,
        };
        let session = match state.session(&binding.owner, id) {
            Ok(session) => session,
            Err(status) => return status,
        };
        if !session.configured() {
            return MEDIA_CLOSED;
        }
        let (width, height) = session.dimensions();
        if session.gpu_only() {
            #[cfg(feature = "pyrowave-gpu")]
            {
                let offers = session.gpu_offers();
                drop(state);
                unsafe {
                    *generation = 1;
                }
                for (index, caps) in offers.iter().enumerate() {
                    let offer = CaptureResourceOffer {
                        id: 100 + index as u64,
                        ownership: FRAME_OWNERSHIP_BORROWED,
                        accessibility: FRAME_ACCESS_GPU,
                        reserved: 0,
                        resource: FrameDescriptor {
                            kind: FRAME_RESOURCE_DMABUF,
                            version: 1,
                            size: size_of::<DmaBufResourceCapabilities>() as u32,
                            reserved: 0,
                            namespace: [0; 16],
                            data: (caps as *const DmaBufResourceCapabilities).cast(),
                        },
                        acquire_sync: none(),
                        completion_sync: none(),
                    };
                    // SAFETY: C-compatible offer/capability storage lives through this callback.
                    let status = unsafe { (sink.write)(sink.context, &offer) };
                    if status != STATUS_OK {
                        return status;
                    }
                }
                return STATUS_OK;
            }
        }
        drop(state);
        // SAFETY: caller provides writable generation pointer.
        unsafe {
            *generation = 1;
        }
        for (id, format) in [(1, PIXEL_BGRA8), (2, PIXEL_NV12)] {
            let caps = CpuResourceCapabilities {
                format,
                min_width: width,
                max_width: width,
                min_height: height,
                max_height: height,
            };
            let offer = CaptureResourceOffer {
                id,
                ownership: FRAME_OWNERSHIP_BORROWED,
                accessibility: FRAME_ACCESS_CPU,
                reserved: 0,
                resource: FrameDescriptor {
                    kind: FRAME_RESOURCE_CPU,
                    version: 1,
                    size: size_of::<CpuResourceCapabilities>() as u32,
                    reserved: 0,
                    namespace: [0; 16],
                    data: (&caps as *const CpuResourceCapabilities).cast(),
                },
                acquire_sync: none(),
                completion_sync: none(),
            };
            // SAFETY: capabilities and offer are borrowed for this callback only.
            let status = unsafe { (sink.write)(sink.context, &offer) };
            if status != STATUS_OK {
                return status;
            }
        }
        STATUS_OK
    })
}
unsafe extern "C" fn configure(
    context: *mut c_void,
    id: u64,
    generation: u64,
    offer_id: u64,
    producer_generation: u64,
    offer: *const CaptureResourceOffer,
) -> Status {
    guard(|| {
        if context.is_null() || offer.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: runtime and producer lend binding/offer for the call.
        let (binding, offer) = unsafe { (&*context.cast::<Consumer>(), &*offer) };
        if offer_id >= 100 {
            #[cfg(feature = "pyrowave-gpu")]
            return configure_gpu(
                binding,
                id,
                generation,
                offer_id,
                producer_generation,
                offer,
            );
            #[cfg(not(feature = "pyrowave-gpu"))]
            return MEDIA_UNSUPPORTED;
        }
        let format = match offer_id {
            1 => PIXEL_BGRA8,
            2 => PIXEL_NV12,
            _ => return MEDIA_UNSUPPORTED,
        };
        let descriptor = &offer.resource;
        if generation != 1
            || producer_generation == 0
            || offer.id == 0
            || offer.reserved != 0
            || !matches!(
                offer.ownership,
                FRAME_OWNERSHIP_BORROWED | FRAME_OWNERSHIP_RETAINABLE
            )
            || offer.accessibility & FRAME_ACCESS_CPU == 0
            || offer.accessibility & !(FRAME_ACCESS_CPU | FRAME_ACCESS_GPU) != 0
            || descriptor.kind != FRAME_RESOURCE_CPU
            || descriptor.version != 1
            || descriptor.size as usize != size_of::<CpuResourceCapabilities>()
            || descriptor.reserved != 0
            || descriptor.namespace != [0; 16]
            || descriptor.data.is_null()
            || !is_none(&offer.acquire_sync)
            || !is_none(&offer.completion_sync)
        {
            return MEDIA_UNSUPPORTED;
        }
        // SAFETY: exact standard capability schema checked; unaligned reads are allowed.
        let caps =
            unsafe { ptr::read_unaligned(descriptor.data.cast::<CpuResourceCapabilities>()) };
        let mut ids = binding
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state = match binding.shared.try_lock() {
            Ok(state) => state,
            Err(_) => return MEDIA_BUSY,
        };
        let session = match state.session_mut(&binding.owner, id) {
            Ok(session) => session,
            Err(status) => return status,
        };
        let (width, height) = session.dimensions();
        if !session.configured()
            || session.gpu_only()
            || caps.format != format
            || width < caps.min_width
            || width > caps.max_width
            || height < caps.min_height
            || height > caps.max_height
        {
            return MEDIA_UNSUPPORTED;
        }
        session.selection = Some(Selection {
            generation: producer_generation,
            id: offer.id,
            format,
            dmabuf: None,
        });
        ids.insert(id);
        STATUS_OK
    })
}
#[cfg(feature = "pyrowave-gpu")]
fn configure_gpu(
    binding: &Consumer,
    id: u64,
    generation: u64,
    offer_id: u64,
    producer_generation: u64,
    offer: &CaptureResourceOffer,
) -> Status {
    let descriptor = &offer.resource;
    let completion = &offer.completion_sync;
    let supported_completion = is_none(completion)
        || (completion.kind == FRAME_SYNC_SYNC_FILE
            && completion.version == 1
            && completion.size == 0
            && completion.reserved == 0
            && completion.namespace == [0; 16]
            && completion.data.is_null());
    if generation != 1
        || producer_generation == 0
        || offer.id == 0
        || offer.reserved != 0
        || offer.ownership != FRAME_OWNERSHIP_RETAINABLE
        || offer.accessibility & FRAME_ACCESS_GPU == 0
        || offer.accessibility & !(FRAME_ACCESS_CPU | FRAME_ACCESS_GPU) != 0
        || descriptor.kind != FRAME_RESOURCE_DMABUF
        || descriptor.version != 1
        || descriptor.size as usize != size_of::<DmaBufResourceCapabilities>()
        || descriptor.reserved != 0
        || descriptor.namespace != [0; 16]
        || descriptor.data.is_null()
        || !is_none(&offer.acquire_sync)
        || !supported_completion
    {
        return MEDIA_UNSUPPORTED;
    }
    // SAFETY: the exact negotiated standard capability is readable for the call.
    let producer =
        unsafe { ptr::read_unaligned(descriptor.data.cast::<DmaBufResourceCapabilities>()) };
    let mut ids = binding
        .sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut state = match binding.shared.try_lock() {
        Ok(state) => state,
        Err(_) => return MEDIA_BUSY,
    };
    let session = match state.session_mut(&binding.owner, id) {
        Ok(session) => session,
        Err(status) => return status,
    };
    let offers = session.gpu_offers();
    let Some(consumer) = offers.get((offer_id - 100) as usize) else {
        return MEDIA_UNSUPPORTED;
    };
    let (width, height) = session.dimensions();
    if !session.configured()
        || !session.gpu_only()
        || producer.fourcc != consumer.fourcc
        || producer.modifier != consumer.modifier
        || producer.color != consumer.color
        || producer.min_width > width
        || producer.max_width < width
        || producer.min_height > height
        || producer.max_height < height
        || (producer.device_id != [0; 16] && producer.device_id != consumer.device_id)
    {
        return MEDIA_UNSUPPORTED;
    }
    // An absent producer identity is unknown. It does not certify compatibility:
    // submit must import that actual allocation successfully before encoding it.
    // No CPU or cross-device staging fallback is available on this route.
    session.selection = Some(Selection {
        generation: producer_generation,
        id: offer.id,
        format: 0,
        dmabuf: Some(producer),
    });
    ids.insert(id);
    STATUS_OK
}
unsafe extern "C" fn submit(
    context: *mut c_void,
    id: u64,
    frame: *const CaptureResourceFrame,
    flags: u32,
) -> Status {
    guard(|| {
        if context.is_null() || frame.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: capture lends immutable storage through this call. GPU input
        // retires every read before return; diagnostic CPU input is copied.
        let binding = unsafe { &*context.cast::<Consumer>() };
        let borrowed = unsafe { BorrowedResourceFrame::from_descriptor(&*frame) };
        let mut state = match binding.shared.try_lock() {
            Ok(state) => state,
            Err(_) => return MEDIA_BUSY,
        };
        let session = match state.session_mut(&binding.owner, id) {
            Ok(session) => session,
            Err(status) => return status,
        };
        session
            .submit(id, borrowed, flags)
            .err()
            .unwrap_or(STATUS_OK)
    })
}
unsafe extern "C" fn cancel(context: *mut c_void, id: u64) {
    barrier(|| {
        if context.is_null() {
            std::process::abort();
        }
        // SAFETY: binding remains valid through this mandatory lifetime barrier.
        let binding = unsafe { &*context.cast::<Consumer>() };
        binding
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .close(&binding.owner, id);
    });
}

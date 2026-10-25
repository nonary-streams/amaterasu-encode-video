use super::*;
use amaterasu_abi::*;
use amaterasu_media::*;
use std::ffi::c_void;
#[derive(Default)]
struct Runtime {
    registered_factory: usize,
}

unsafe extern "C" fn invoke(
    context: *mut c_void,
    bytes: BorrowedBytes,
    reply: ReplySink,
) -> Status {
    // SAFETY: test owns endpoint state and request/reply borrow for this call.
    let state = unsafe { &mut *context.cast::<Runtime>() };
    let command: Command = serde_json::from_slice(unsafe { bytes.read().unwrap() }).unwrap();
    let response = match command {
        Command::GetSettings {} => Response::Result { payload: json!({}) },
        Command::Register { .. } => Response::Ok,
        Command::NativeRegister { factory, .. } => {
            state.registered_factory = factory;
            Response::Ok
        }
        _ => panic!("unexpected startup request"),
    };
    unsafe {
        reply.send(&serde_json::to_vec(&response).unwrap());
    }
    STATUS_OK
}

#[test]
fn start_move_and_native_bind_preserve_the_registered_factory_address() {
    let mut state = Runtime::default();
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (&mut state as *mut Runtime).cast(),
        invoke,
    };
    // SAFETY: endpoint state outlives the module and all calls below.
    let client = unsafe { RuntimeClient::from_endpoint(endpoint) };
    let module = EncodeModule::start(client).unwrap();
    let moved = Box::new(module);
    assert_eq!(
        state.registered_factory,
        &moved._registration.factory as *const NativeServiceFactory as usize
    );
    let query = NativeInterfaceQuery {
        header: AbiHeader::of::<NativeInterfaceQuery>(),
        interface_major: 1,
        min_minor: 0,
        min_size: size_of::<EncoderApi>() as u32,
    };
    let mut output = std::ptr::null();
    // SAFETY: registered table is pinned by the moved module; output transfers
    // one binding, released before module destruction.
    unsafe {
        let factory = &*(state.registered_factory as *const NativeServiceFactory);
        assert_eq!(
            factory.bind.unwrap()(
                factory.context,
                BorrowedBytes::new(b"consumer"),
                &query,
                &mut output
            ),
            STATUS_OK
        );
        let api = &*output.cast::<EncoderApi>();
        assert_eq!((api.drain)(api.base.context, 1), MEDIA_CLOSED);
        api.base.release.unwrap()(api.base.context);
    }
}

mod probe_support;
mod probing;
#[cfg(amaterasu_x264)]
mod software;

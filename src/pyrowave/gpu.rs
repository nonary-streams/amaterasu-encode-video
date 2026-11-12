//! Private Linux/Vulkan ABI. Capture resources stay borrowed until GPU retirement.
use amaterasu_media::DmaBufFrame;
use libloading::Library;
use std::{ffi::c_void, marker::PhantomData, ptr};

#[repr(C)]
struct Functions {
    get_handles: unsafe extern "C" fn(),
    image_create: unsafe extern "C" fn(),
    image_view: unsafe extern "C" fn(),
    image_destroy: unsafe extern "C" fn(),
    wait_idle: unsafe extern "C" fn(),
    encode_scaled: unsafe extern "C" fn(),
}
#[repr(C)]
struct Frame {
    width: u32,
    height: u32,
    fourcc: u32,
    plane_count: u32,
    modifier: u64,
    fds: [i32; 4],
    fences: [i32; 4],
    offsets: [u32; 4],
    pitches: [u32; 4],
    hdr: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Identity {
    pub device_uuid: [u8; 16],
    pub render_major: u32,
    pub render_minor: u32,
}
unsafe extern "C" {
    fn am_pyrowave_gpu_create(functions: *const Functions, device: *mut c_void) -> *mut c_void;
    fn am_pyrowave_gpu_destroy(context: *mut c_void);
    fn am_pyrowave_gpu_identity(context: *mut c_void, output: *mut Identity) -> i32;
    fn am_pyrowave_gpu_modifiers(
        context: *mut c_void,
        fourcc: u32,
        modifiers: *mut u64,
        capacity: u32,
        count: *mut u32,
    ) -> i32;
    fn am_pyrowave_gpu_supports(
        context: *mut c_void,
        fourcc: u32,
        modifier: u64,
        width: u32,
        height: u32,
        planes: *mut u32,
    ) -> i32;
    fn am_pyrowave_gpu_import(
        context: *mut c_void,
        frame: *const Frame,
        image: *mut *mut c_void,
    ) -> i32;
    fn am_pyrowave_gpu_encode(image: *mut c_void, encoder: *mut c_void, budget: usize) -> i32;
    fn am_pyrowave_gpu_retire(image: *mut c_void);
}
pub(super) struct Context {
    native: *mut c_void,
    pub identity: Identity,
}
// SAFETY: device calls and image lifetimes are serialized by Runtime's device
// mutex. Capability queries only read immutable physical-device properties.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}
impl Context {
    /// # Safety
    /// Library and borrowed PyroWave device outlive this context and all imports.
    pub unsafe fn new(library: &Library, device: *mut c_void) -> Result<Self, &'static str> {
        // SAFETY: runtime pins the exact C API library through context teardown.
        let ownership = unsafe {
            library
                .get::<unsafe extern "C" fn() -> u32>(b"pyrowave_amaterasu_import_fd_contract_v1\0")
        }
        .map_err(|_| "pyrowave_import_contract_unavailable")?;
        if unsafe { ownership() } != 1 {
            return Err("pyrowave_import_contract_unavailable");
        }
        macro_rules! function {
            ($name:literal) => {
                unsafe {
                    *library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|_| "pyrowave_gpu_api_unavailable")?
                }
            };
        }
        let functions = Functions {
            get_handles: function!("pyrowave_device_get_vk_device_handles"),
            image_create: function!("pyrowave_image_create"),
            image_view: function!("pyrowave_image_get_image_view"),
            image_destroy: function!("pyrowave_image_destroy"),
            wait_idle: function!("pyrowave_amaterasu_wait_idle_v1"),
            encode_scaled: function!("pyrowave_encoder_encode_gpu_scaled_synchronous"),
        };
        // SAFETY: C copies the function table and only borrows the pinned device.
        let native = unsafe { am_pyrowave_gpu_create(&functions, device) };
        if native.is_null() {
            return Err("pyrowave_gpu_unavailable");
        }
        let mut context = Self {
            native,
            identity: Default::default(),
        };
        if unsafe { am_pyrowave_gpu_identity(native, &mut context.identity) } != 0 {
            return Err("pyrowave_gpu_identity_unavailable");
        }
        Ok(context)
    }
    pub fn offers(
        &self,
        width: u32,
        height: u32,
        hdr: bool,
    ) -> Vec<amaterasu_media::DmaBufResourceCapabilities> {
        let formats = if hdr {
            [*b"AR30", *b"XR30", *b"AB30", *b"XB30"]
        } else {
            [*b"AR24", *b"XR24", *b"AB24", *b"XB24"]
        };
        let mut offers = Vec::new();
        for fourcc in formats.map(u32::from_le_bytes) {
            let mut modifiers = [0; 16];
            let mut count = 0;
            // SAFETY: C writes at most the explicitly supplied bounded capacity.
            if unsafe {
                am_pyrowave_gpu_modifiers(
                    self.native,
                    fourcc,
                    modifiers.as_mut_ptr(),
                    modifiers.len() as u32,
                    &mut count,
                )
            } != 0
            {
                continue;
            }
            for modifier in modifiers.into_iter().take(count as usize) {
                if self.supports(fourcc, modifier, width, height).is_some() {
                    offers.push(amaterasu_media::DmaBufResourceCapabilities {
                        fourcc,
                        color: if hdr {
                            amaterasu_media::DMABUF_COLOR_BT2020_PQ
                        } else {
                            amaterasu_media::DMABUF_COLOR_SRGB
                        },
                        modifier,
                        min_width: width,
                        max_width: width,
                        min_height: height,
                        max_height: height,
                        device_id: self.identity.device_uuid,
                    });
                }
            }
        }
        offers
    }
    pub fn supports(&self, fourcc: u32, modifier: u64, width: u32, height: u32) -> Option<u32> {
        let mut planes = 0;
        // SAFETY: live immutable context; query writes only this stack output.
        (unsafe {
            am_pyrowave_gpu_supports(self.native, fourcc, modifier, width, height, &mut planes)
        } == 0)
            .then_some(planes)
    }
    /// # Safety
    /// Frame descriptors refer to one immutable allocation, lent through the
    /// returned import's destruction. The runtime device mutex remains locked.
    pub unsafe fn import<'a>(
        &'a self,
        frame: &'a DmaBufFrame,
        hdr: bool,
    ) -> Result<Import<'a>, &'static str> {
        let input = Frame {
            width: frame.width,
            height: frame.height,
            fourcc: frame.fourcc,
            plane_count: frame.plane_count,
            modifier: frame.modifier,
            fds: frame.planes.map(|plane| plane.fd),
            fences: frame.planes.map(|plane| plane.acquire_fence_fd),
            offsets: frame.planes.map(|plane| plane.offset),
            pitches: frame.planes.map(|plane| plane.pitch),
            hdr: u32::from(hdr),
        };
        let mut image = ptr::null_mut();
        // SAFETY: C duplicates handles and finishes failure-path access before
        // returning. Successful imports are retired synchronously by Import.
        if unsafe { am_pyrowave_gpu_import(self.native, &input, &mut image) } != 0 {
            return Err("pyrowave_dmabuf_import_failed");
        }
        Ok(Import {
            native: image,
            _borrow: PhantomData,
        })
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: all imports retire before their borrowed context can drop.
        unsafe { am_pyrowave_gpu_destroy(self.native) };
    }
}
pub(super) struct Import<'a> {
    native: *mut c_void,
    _borrow: PhantomData<&'a DmaBufFrame>,
}
impl Import<'_> {
    /// # Safety
    /// Encoder belongs to this import's runtime device, which stays serialized.
    pub unsafe fn encode(&self, encoder: *mut c_void, budget: usize) -> Result<(), &'static str> {
        // SAFETY: caller pins the encoder and device; self pins imported storage.
        if unsafe { am_pyrowave_gpu_encode(self.native, encoder, budget) } == 0 {
            Ok(())
        } else {
            Err("pyrowave_gpu_encode_failed")
        }
    }
}
impl Drop for Import<'_> {
    fn drop(&mut self) {
        // SAFETY: this owns exactly one import. C waits for all possible GPU
        // accesses, including after native error, before retiring it.
        unsafe { am_pyrowave_gpu_retire(self.native) };
    }
}

#[cfg(all(test, feature = "pyrowave-gpu-tests"))]
pub(super) mod fixture {
    use super::*;
    unsafe extern "C" {
        fn am_pyrowave_test_image_create(
            identity: *const Identity,
            width: u32,
            height: u32,
            fourcc: u32,
            modifier: u64,
            color: *const f32,
            hdr: u32,
            frame: *mut Frame,
        ) -> *mut c_void;
        fn am_pyrowave_test_image_destroy(image: *mut c_void);
    }
    pub struct Image {
        native: *mut c_void,
        pub frame: DmaBufFrame,
        pub capability: amaterasu_media::DmaBufResourceCapabilities,
    }
    impl Image {
        pub fn create(
            context: &Context,
            width: u32,
            height: u32,
            hdr: bool,
            intensity: f32,
        ) -> Result<Self, &'static str> {
            let mut offers = context.offers(width, height, hdr);
            offers.sort_by_key(|capability| capability.modifier);
            for capability in offers {
                if let Ok(image) =
                    Self::create_capability(context, width, height, hdr, capability, [intensity; 3])
                {
                    return Ok(image);
                }
            }
            Err("no mutually importable render-node fixture format")
        }
        pub fn create_capability(
            context: &Context,
            width: u32,
            height: u32,
            hdr: bool,
            capability: amaterasu_media::DmaBufResourceCapabilities,
            color: [f32; 3],
        ) -> Result<Self, &'static str> {
            let mut frame = std::mem::MaybeUninit::<Frame>::uninit();
            // SAFETY: C writes the complete output only on successful GPU
            // allocation/render. It owns every returned fd until destruction.
            let native = unsafe {
                am_pyrowave_test_image_create(
                    &context.identity,
                    width,
                    height,
                    capability.fourcc,
                    capability.modifier,
                    color.as_ptr(),
                    u32::from(hdr),
                    frame.as_mut_ptr(),
                )
            };
            if native.is_null() {
                return Err("GPU fixture allocation failed");
            }
            let input = unsafe { frame.assume_init() };
            let planes = std::array::from_fn(|index| {
                if index < input.plane_count as usize {
                    amaterasu_media::DmaBufPlane {
                        fd: input.fds[index],
                        acquire_fence_fd: input.fences[index],
                        offset: input.offsets[index],
                        pitch: input.pitches[index],
                    }
                } else {
                    amaterasu_media::DmaBufPlane::UNUSED
                }
            });
            Ok(Self {
                native,
                capability,
                frame: DmaBufFrame {
                    width,
                    height,
                    fourcc: input.fourcc,
                    plane_count: input.plane_count,
                    modifier: input.modifier,
                    planes,
                    color: capability.color,
                    reserved: 0,
                    timestamp_us: 1100,
                    sequence: 1,
                },
            })
        }
    }
    impl Drop for Image {
        fn drop(&mut self) {
            // SAFETY: tests drop the producer only after submitted imports retire.
            unsafe { am_pyrowave_test_image_destroy(self.native) };
        }
    }
}

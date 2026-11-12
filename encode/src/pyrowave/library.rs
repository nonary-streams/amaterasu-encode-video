//! The pinned PyroWave 0.6 C ABI. Vulkan objects never cross the plugin boundary.
use libloading::Library;
use std::{
    ffi::c_void,
    path::Path,
    ptr,
    sync::{Arc, Mutex},
};

pub(super) const RECORD_BYTES: usize = 1024;
pub(super) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
type Handle = *mut c_void;
#[repr(C)]
struct EncoderInfo {
    device: Handle,
    width: i32,
    height: i32,
    chroma: i32,
}
#[repr(C)]
#[derive(Default)]
pub(super) struct CpuBuffer {
    pub data: [*mut c_void; 3],
    pub stride: [usize; 3],
    pub size: [usize; 3],
    pub width: i32,
    pub height: i32,
    pub format: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Packet {
    offset: usize,
    size: usize,
}
#[repr(C)]
struct RateControl {
    maximum_bitstream_size: usize,
}
struct Api {
    create_device: unsafe extern "C" fn(*mut Handle) -> i32,
    destroy_device: unsafe extern "C" fn(Handle),
    create_encoder: unsafe extern "C" fn(*const EncoderInfo, *mut Handle) -> i32,
    destroy_encoder: unsafe extern "C" fn(Handle),
    encode: unsafe extern "C" fn(Handle, *const CpuBuffer, *const RateControl) -> i32,
    count: unsafe extern "C" fn(Handle, usize, *mut usize) -> i32,
    raw_bitstream: unsafe extern "C" fn(
        Handle,
        *mut *const c_void,
        *mut usize,
        *mut *const c_void,
        *mut usize,
    ) -> i32,
    packetize:
        unsafe extern "C" fn(Handle, *mut Packet, usize, *mut usize, *mut c_void, usize) -> i32,
    _library: Library,
}

pub(super) struct Runtime {
    api: Api,
    device: Mutex<Handle>,
    probe: Mutex<()>,
    #[cfg(feature = "pyrowave-gpu")]
    gpu: Option<super::gpu::Context>,
}
// SAFETY: the mutex serializes every device/encoder operation. Vulkan objects
// have no OS-thread affinity; no foreign borrowed pointer escapes a call.
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}

impl Runtime {
    pub fn load(path: &Path) -> Result<Arc<Self>, &'static str> {
        if !cfg!(target_endian = "little") {
            return Err("pyrowave_endianness_unsupported");
        }
        // SAFETY: trusted startup selects the native library. Its code stays
        // loaded through all function pointers, device and encoder destructors.
        let library = unsafe { Library::new(path) }.map_err(|_| "pyrowave_library_unavailable")?;
        unsafe {
            let version: libloading::Symbol<unsafe extern "C" fn(*mut u32, *mut u32, *mut u32)> =
                library
                    .get(b"pyrowave_get_api_version\0")
                    .map_err(|_| "pyrowave_api_unavailable")?;
            let (mut major, mut minor, mut patch) = (0, 0, 0);
            version(&mut major, &mut minor, &mut patch);
            if (major, minor, patch) != (0, 6, 0) {
                return Err("pyrowave_api_mismatch");
            }
            macro_rules! symbol {
                ($name:literal) => {
                    *library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|_| "pyrowave_api_unavailable")?
                };
            }
            let api = Api {
                create_device: symbol!("pyrowave_create_default_device"),
                destroy_device: symbol!("pyrowave_device_destroy"),
                create_encoder: symbol!("pyrowave_encoder_create"),
                destroy_encoder: symbol!("pyrowave_encoder_destroy"),
                encode: symbol!("pyrowave_encoder_encode_cpu_synchronous"),
                count: symbol!("pyrowave_encoder_compute_num_packets"),
                raw_bitstream: symbol!("pyrowave_encoder_get_mapped_raw_bitstream"),
                packetize: symbol!("pyrowave_encoder_packetize"),
                _library: library,
            };
            let mut device = ptr::null_mut();
            if (api.create_device)(&mut device) != 0 || device.is_null() {
                return Err("pyrowave_device_unavailable");
            }
            Ok(Arc::new(Self {
                api,
                device: Mutex::new(device),
                probe: Mutex::new(()),
                #[cfg(feature = "pyrowave-gpu")]
                gpu: None,
            }))
        }
    }
    #[cfg(feature = "pyrowave-gpu")]
    pub fn load_gpu(path: &Path) -> Result<Arc<Self>, &'static str> {
        let mut runtime = Self::load(path)?;
        let unique = Arc::get_mut(&mut runtime).expect("new unique runtime");
        let device = *unique
            .device
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: this context retires before its owned native device and library.
        unique.gpu = Some(unsafe { super::gpu::Context::new(&unique.api._library, device)? });
        Ok(runtime)
    }
    pub fn gpu_only(&self) -> bool {
        #[cfg(feature = "pyrowave-gpu")]
        {
            self.gpu.is_some()
        }
        #[cfg(not(feature = "pyrowave-gpu"))]
        {
            false
        }
    }
    #[cfg(feature = "pyrowave-gpu")]
    pub fn gpu_context(&self) -> Option<&super::gpu::Context> {
        self.gpu.as_ref()
    }
    pub fn begin_probe(&self) -> Result<std::sync::MutexGuard<'_, ()>, &'static str> {
        self.probe.try_lock().map_err(|_| "pyrowave_probe_busy")
    }
    pub fn encoder(self: &Arc<Self>, width: u32, height: u32) -> Result<Encoder, &'static str> {
        let device = self
            .device
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let info = EncoderInfo {
            device: *device,
            width: width as i32,
            height: height as i32,
            chroma: 0,
        };
        let mut handle = ptr::null_mut();
        // SAFETY: device and info are valid through this synchronous call. The
        // returned encoder is owned here and destroyed before its device.
        if unsafe { (self.api.create_encoder)(&info, &mut handle) } != 0 || handle.is_null() {
            return Err("pyrowave_encoder_unavailable");
        }
        Ok(Encoder {
            runtime: self.clone(),
            handle,
        })
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        #[cfg(feature = "pyrowave-gpu")]
        self.gpu.take();
        let device = self
            .device
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: all encoder Arcs retired before the final runtime owner drops.
        unsafe { (self.api.destroy_device)(*device) };
    }
}
pub(super) struct Encoder {
    runtime: Arc<Runtime>,
    handle: Handle,
}
// SAFETY: all uses are serialized by the runtime device mutex, including drop.
unsafe impl Send for Encoder {}
impl Encoder {
    #[cfg(feature = "pyrowave-gpu")]
    pub fn probe_gpu(&mut self, hdr: bool, budget: usize) -> Result<(), &'static str> {
        let _device = self
            .runtime
            .device
            .lock()
            .map_err(|_| "pyrowave_gpu_unavailable")?;
        // SAFETY: the pinned private API uses this encoder's serialized device,
        // renders its own image and completes every GPU access before returning.
        let probe = unsafe {
            self.runtime
                .api
                ._library
                .get::<unsafe extern "C" fn(Handle, bool, usize) -> i32>(
                    b"pyrowave_amaterasu_probe_v1\0",
                )
        }
        .map_err(|_| "pyrowave_gpu_probe_unavailable")?;
        if unsafe { probe(self.handle, hdr, budget) } != 0 {
            return Err("pyrowave_gpu_probe_failed");
        }
        self.packetize(hdr).map(|_| ())
    }
    /// CPU allocations must remain readable through this call. Packetization
    /// waits for GPU completion before the borrow returns, also on error: drop
    /// synchronously retires an encoder after any native failure.
    pub unsafe fn encode(
        &mut self,
        input: &CpuBuffer,
        budget: usize,
    ) -> Result<Vec<u8>, &'static str> {
        let _device = self
            .runtime
            .device
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let api = &self.runtime.api;
        let rate = RateControl {
            maximum_bitstream_size: budget,
        };
        // SAFETY: caller lends validated planes; native code owns no CPU pointer
        // after encode returns. Device operations are serialized under this guard.
        if unsafe { (api.encode)(self.handle, input, &rate) } != 0 {
            return Err("pyrowave_encode_failed");
        }
        self.packetize(false)
    }
    #[cfg(feature = "pyrowave-gpu")]
    pub unsafe fn encode_gpu(
        &mut self,
        frame: &amaterasu_media::DmaBufFrame,
        hdr: bool,
        budget: usize,
    ) -> Result<Vec<u8>, &'static str> {
        let _device = self
            .runtime
            .device
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let context = self
            .runtime
            .gpu
            .as_ref()
            .ok_or("pyrowave_gpu_unavailable")?;
        // SAFETY: caller keeps the immutable capture presentation borrowed until
        // the import's synchronous retirement, including every failure path.
        let imported = unsafe { context.import(frame, hdr)? };
        unsafe { imported.encode(self.handle, budget)? };
        self.packetize(hdr)
    }
    fn packetize(&self, hdr: bool) -> Result<Vec<u8>, &'static str> {
        let api = &self.runtime.api;
        let mut raw = ptr::null();
        let mut metadata = ptr::null();
        let (mut raw_size, mut metadata_size) = (0, 0);
        // SAFETY: device calls are serialized and the encoder owns both mapped
        // compressed buffers until the next encode. No source pixels are mapped.
        if unsafe {
            (api.raw_bitstream)(
                self.handle,
                &mut raw,
                &mut raw_size,
                &mut metadata,
                &mut metadata_size,
            )
        } != 0
            || raw.is_null()
            || metadata.is_null()
            || metadata_size > MAX_FRAME_BYTES
        {
            return Err("pyrowave_packetize_failed");
        }
        let metadata = unsafe { std::slice::from_raw_parts(metadata.cast::<u8>(), metadata_size) };
        let packed_size = packetized_size(metadata, raw_size)?;
        let mut count = 0;
        if unsafe { (api.count)(self.handle, RECORD_BYTES, &mut count) } != 0 {
            return Err("pyrowave_packetize_failed");
        }
        let capacity = count
            .checked_mul(4)
            .and_then(|size| size.checked_add(4))
            .and_then(|size| size.checked_add(packed_size))
            .filter(|size| *size <= MAX_FRAME_BYTES)
            .ok_or("pyrowave_frame_too_large")?;
        if count == 0 || count > metadata.len() / 8 + 1 {
            return Err("pyrowave_empty_frame");
        }
        let mut packets = vec![Packet::default(); count];
        // The boundary is a packing target: one indivisible codec block can
        // exceed it. Upstream packetization only asserts its buffer bound, so
        // establish the exact encoded size from the pinned metadata first.
        let mut bitstream = vec![0u8; packed_size];
        let mut actual = count;
        if unsafe {
            (api.packetize)(
                self.handle,
                packets.as_mut_ptr(),
                RECORD_BYTES,
                &mut actual,
                bitstream.as_mut_ptr().cast(),
                bitstream.len(),
            )
        } != 0
            || actual != count
        {
            return Err("pyrowave_packetize_failed");
        }
        set_sequence_color(&mut bitstream, hdr)?;
        let mut records = Vec::with_capacity(capacity);
        records.extend_from_slice(&(count as u32).to_le_bytes());
        let mut cursor = 0;
        for packet in packets {
            if packet.size == 0 || packet.offset != cursor {
                return Err("pyrowave_invalid_record");
            }
            let end = packet
                .offset
                .checked_add(packet.size)
                .ok_or("pyrowave_invalid_record")?;
            let bytes = bitstream
                .get(packet.offset..end)
                .ok_or("pyrowave_invalid_record")?;
            records.extend_from_slice(&(packet.size as u32).to_le_bytes());
            records.extend_from_slice(bytes);
            cursor = end;
        }
        if cursor != bitstream.len() {
            return Err("pyrowave_invalid_record");
        }
        Ok(records)
    }
}

/// The pinned packer leaves the documented sequence color fields at SDR defaults.
/// Set those fields from the negotiated profile without changing codec syntax.
fn set_sequence_color(bitstream: &mut [u8], hdr: bool) -> Result<(), &'static str> {
    if bitstream.len() < 8 {
        return Err("pyrowave_invalid_record");
    }
    let sequence = u32::from_le_bytes(bitstream[..4].try_into().unwrap());
    let mut color = u32::from_le_bytes(bitstream[4..8].try_into().unwrap());
    if sequence & (1 << 31) == 0 || color & (3 << 24) != 0 {
        return Err("pyrowave_invalid_record");
    }
    // Bits 27..29 are BT.2020 primaries, PQ transfer and BT.2020 transform.
    // Bit 30 remains zero for full range; bit 31 is centered chroma.
    color = (color & !(7 << 27)) | if hdr { 7 << 27 } else { 0 };
    bitstream[4..8].copy_from_slice(&color.to_le_bytes());
    Ok(())
}

/// Pinned 186f0393 metadata is a native-endian {offset_u32, num_words} array.
/// Packetization adds one eight-byte sequence header and copies nonempty blocks
/// whole. Validate every source span before the native packer's unchecked copy.
fn packetized_size(metadata: &[u8], raw_size: usize) -> Result<usize, &'static str> {
    let (entries, remainder) = metadata.as_chunks::<8>();
    if entries.is_empty() || !remainder.is_empty() {
        return Err("pyrowave_invalid_metadata");
    }
    let mut total = 8usize;
    for (index, entry) in entries.iter().enumerate() {
        let words = u32::from_ne_bytes(entry[4..].try_into().unwrap()) as usize;
        let offset = (u32::from_ne_bytes(entry[..4].try_into().unwrap()) as usize)
            .checked_mul(4)
            .ok_or("pyrowave_invalid_metadata")?;
        // Upstream reads the first block's sequence even when it is empty.
        if index == 0 && offset.checked_add(8).is_none_or(|end| end > raw_size) {
            return Err("pyrowave_invalid_metadata");
        }
        if words == 0 {
            continue;
        }
        let bytes = words.checked_mul(4).ok_or("pyrowave_invalid_metadata")?;
        if bytes < 8 || offset.checked_add(bytes).is_none_or(|end| end > raw_size) {
            return Err("pyrowave_invalid_metadata");
        }
        total = total
            .checked_add(bytes)
            .filter(|size| *size <= MAX_FRAME_BYTES)
            .ok_or("pyrowave_frame_too_large")?;
    }
    Ok(total)
}
impl Drop for Encoder {
    fn drop(&mut self) {
        let _device = self
            .runtime
            .device
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: this encoder is unique and C API destroy waits for GPU idle.
        unsafe { (self.runtime.api.destroy_encoder)(self.handle) };
    }
}

#[cfg(test)]
impl Runtime {
    /// Independent decoder entry points validate that plugin output is real
    /// codec data, rather than checking an encoder's own packet bookkeeping.
    pub(super) fn decode(
        &self,
        records: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, String> {
        #[repr(C)]
        struct DecoderInfo {
            device: Handle,
            width: i32,
            height: i32,
            chroma: i32,
            fragment_path: bool,
        }
        let device = self.device.lock().unwrap();
        // SAFETY: the library is live and exact 0.6 ABI is checked on load.
        unsafe {
            let create: libloading::Symbol<
                unsafe extern "C" fn(*const DecoderInfo, *mut Handle) -> i32,
            > = self.api._library.get(b"pyrowave_decoder_create\0").unwrap();
            let destroy: libloading::Symbol<unsafe extern "C" fn(Handle)> = self
                .api
                ._library
                .get(b"pyrowave_decoder_destroy\0")
                .unwrap();
            let push: libloading::Symbol<
                unsafe extern "C" fn(Handle, *const c_void, usize) -> i32,
            > = self
                .api
                ._library
                .get(b"pyrowave_decoder_push_packet\0")
                .unwrap();
            let ready: libloading::Symbol<unsafe extern "C" fn(Handle, bool) -> bool> = self
                .api
                ._library
                .get(b"pyrowave_decoder_decode_is_ready\0")
                .unwrap();
            let decode: libloading::Symbol<unsafe extern "C" fn(Handle, *const CpuBuffer) -> i32> =
                self.api
                    ._library
                    .get(b"pyrowave_decoder_decode_cpu_buffer_synchronous\0")
                    .unwrap();
            let mut handle = ptr::null_mut();
            if create(
                &DecoderInfo {
                    device: *device,
                    width: width as i32,
                    height: height as i32,
                    chroma: 0,
                    fragment_path: false,
                },
                &mut handle,
            ) != 0
            {
                return Err("decoder create failed".into());
            }
            let result = (|| {
                let mut offset = 4;
                let count = u32::from_le_bytes(
                    records.get(..4).ok_or("missing count")?.try_into().unwrap(),
                );
                for _ in 0..count {
                    let size = u32::from_le_bytes(
                        records
                            .get(offset..offset + 4)
                            .ok_or("missing length")?
                            .try_into()
                            .unwrap(),
                    ) as usize;
                    offset += 4;
                    let packet = records.get(offset..offset + size).ok_or("short record")?;
                    if push(handle, packet.as_ptr().cast(), packet.len()) != 0 {
                        return Err("decoder rejected record".into());
                    }
                    offset += size;
                }
                if offset != records.len() || !ready(handle, false) {
                    return Err("incomplete frame".into());
                }
                let luma = (width * height) as usize;
                let mut pixels = vec![0u8; luma * 3 / 2];
                let buffer = CpuBuffer {
                    data: [
                        pixels.as_mut_ptr().cast(),
                        pixels[luma..].as_mut_ptr().cast(),
                        pixels[luma + luma / 4..].as_mut_ptr().cast(),
                    ],
                    stride: [width as usize, width as usize / 2, width as usize / 2],
                    size: [luma, luma / 4, luma / 4],
                    width: width as i32,
                    height: height as i32,
                    format: 1,
                };
                if decode(handle, &buffer) != 0 {
                    return Err("decode failed".into());
                }
                Ok(pixels)
            })();
            destroy(handle);
            result
        }
    }
}

#[cfg(all(test, feature = "pyrowave-gpu-tests"))]
impl Runtime {
    pub(super) fn decode16(
        &self,
        records: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u16>, &'static str> {
        #[repr(C)]
        struct DecoderApi {
            get_handles: unsafe extern "C" fn(),
            create: unsafe extern "C" fn(),
            push: unsafe extern "C" fn(),
            ready: unsafe extern "C" fn(),
            decode: unsafe extern "C" fn(),
            destroy: unsafe extern "C" fn(),
        }
        unsafe extern "C" {
            fn am_pyrowave_test_decode16(
                api: *const DecoderApi,
                device: Handle,
                records: *const u8,
                size: usize,
                width: u32,
                height: u32,
                output: *mut u16,
                output_len: usize,
            ) -> i32;
        }
        let device = self.device.lock().unwrap();
        macro_rules! symbol {
            ($name:literal) => {
                unsafe {
                    *self
                        .api
                        ._library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|_| "decoder API unavailable")?
                }
            };
        }
        let api = DecoderApi {
            get_handles: symbol!("pyrowave_device_get_vk_device_handles"),
            create: symbol!("pyrowave_decoder_create"),
            push: symbol!("pyrowave_decoder_push_packet"),
            ready: symbol!("pyrowave_decoder_decode_is_ready"),
            decode: symbol!("pyrowave_decoder_decode_gpu_buffer"),
            destroy: symbol!("pyrowave_decoder_destroy"),
        };
        let mut output = vec![0u16; (width * height * 3 / 2) as usize];
        // SAFETY: C uses the pinned device/API and writes the bounded output.
        // GPU readback is confined to this independently decoded test output.
        if unsafe {
            am_pyrowave_test_decode16(
                &api,
                *device,
                records.as_ptr(),
                records.len(),
                width,
                height,
                output.as_mut_ptr(),
                output.len(),
            )
        } == 0
        {
            Ok(output)
        } else {
            Err("GPU decoder readback failed")
        }
    }
}

#[cfg(test)]
mod bounds_tests {
    use super::*;

    fn metadata(offset: u32, words: u32) -> Vec<u8> {
        [offset.to_ne_bytes(), words.to_ne_bytes()].concat()
    }

    #[test]
    fn native_packing_bounds_include_indivisible_blocks_larger_than_the_target() {
        assert_eq!(packetized_size(&metadata(0, 300), 1200), Ok(1208));
        assert!(packetized_size(&metadata(1, 300), 1200).is_err());
        assert!(packetized_size(&metadata(u32::MAX, 2), 1200).is_err());
        assert!(packetized_size(&metadata(u32::MAX, 0), 1200).is_err());
        assert!(packetized_size(&metadata(0, u32::MAX), usize::MAX).is_err());
        assert!(packetized_size(&metadata(0, 1), 1200).is_err());
        assert!(packetized_size(&[0; 7], 1200).is_err());
        assert!(packetized_size(&[], 1200).is_err());
    }

    #[test]
    fn sequence_color_marks_hdr_without_changing_geometry_sequence_or_block_count() {
        let first = (1u32 << 31) | 1919 | (1079 << 14) | (3 << 28);
        let mut bytes = [first.to_le_bytes(), 23u32.to_le_bytes()].concat();
        set_sequence_color(&mut bytes, true).unwrap();
        assert_eq!(&bytes[..4], &first.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            23 | (7 << 27)
        );
        set_sequence_color(&mut bytes, false).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 23);
        bytes[3] &= 0x7f;
        assert!(set_sequence_color(&mut bytes, true).is_err());
    }
}

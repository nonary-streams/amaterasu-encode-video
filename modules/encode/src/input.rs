//! Encoder inputs that bypass CPU colour conversion and the capabilities a
//! backend reports for them.
//!
//! Packed NV12 keeps its original path (`Backend::submit`). A backend that
//! converts on the GPU additionally accepts packed BGRA and, on Linux,
//! DMA-BUF frames whose descriptors were duplicated before the native submit
//! call returned. Dropping a `DmaBufInput` closes every duplicate it owns.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// What a backend accepts besides packed NV12.
#[derive(Clone, Default)]
pub struct InputSupport {
    /// BGRA CPU frames are converted to NV12 on the GPU, not by `layout`.
    pub gpu_bgra_conversion: bool,
    /// `None` when the backend cannot import DMA-BUFs at all.
    pub dmabuf: Option<Arc<DmaBufImportSupport>>,
}

/// DRM fourcc/modifier pairs a backend can import, fixed when it opens.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DmaBufImportSupport {
    formats: BTreeMap<u32, FormatSupport>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct FormatSupport {
    implicit_modifier: bool,
    modifiers: BTreeSet<u64>,
}

impl DmaBufImportSupport {
    /// Records one fourcc. `implicit_modifier` admits producers that
    /// allocated without an explicit modifier (`DRM_FORMAT_MOD_INVALID`).
    pub fn add_format(
        &mut self,
        fourcc: u32,
        implicit_modifier: bool,
        modifiers: impl IntoIterator<Item = u64>,
    ) {
        let modifiers: BTreeSet<u64> = modifiers
            .into_iter()
            .filter(|modifier| *modifier != amaterasu_media::DRM_FORMAT_MOD_INVALID)
            .collect();
        if implicit_modifier || !modifiers.is_empty() {
            self.formats.insert(
                fourcc,
                FormatSupport {
                    implicit_modifier,
                    modifiers,
                },
            );
        }
    }

    pub fn is_empty(&self) -> bool {
        self.formats.is_empty()
    }

    pub fn supports(&self, fourcc: u32, modifier: u64) -> bool {
        self.formats.get(&fourcc).is_some_and(|format| {
            if modifier == amaterasu_media::DRM_FORMAT_MOD_INVALID {
                format.implicit_modifier
            } else {
                format.modifiers.contains(&modifier)
            }
        })
    }
}

/// Input for a backend with GPU conversion.
pub enum GpuInput {
    /// Tightly packed BGRA rows (`width * 4` bytes each).
    Bgra(Vec<u8>),
    #[cfg(unix)]
    DmaBuf(DmaBufInput),
}

#[cfg(unix)]
pub use dmabuf::DmaBufInput;
#[cfg(all(unix, test, amaterasu_nvenc_cuda))]
pub use dmabuf::DmaBufPlaneInput;

#[cfg(unix)]
mod dmabuf {
    use amaterasu_abi::{STATUS_INVALID_ARGUMENT, Status};
    use amaterasu_media::DmaBufFrame;
    use std::os::fd::{BorrowedFd, OwnedFd};

    /// One plane whose descriptors this process owns.
    pub struct DmaBufPlaneInput {
        pub fd: OwnedFd,
        /// Taken (set to `None`) when a native consumer assumes ownership.
        pub acquire_fence: Option<OwnedFd>,
        pub offset: u32,
        pub pitch: u32,
    }

    /// A DMA-BUF frame with duplicated descriptors, safe to move to the
    /// encoder worker after the producer's borrow ends.
    pub struct DmaBufInput {
        pub width: u32,
        pub height: u32,
        pub fourcc: u32,
        pub modifier: u64,
        pub planes: Vec<DmaBufPlaneInput>,
    }

    impl DmaBufInput {
        /// Duplicates every plane and fence descriptor with
        /// `F_DUPFD_CLOEXEC`. If any duplication fails, the duplicates made
        /// so far are closed and nothing is retained.
        ///
        /// # Safety
        /// `frame` passed `DmaBufFrame::validate`, and each of its
        /// non-negative descriptors is open for the duration of this call.
        pub unsafe fn duplicate(frame: &DmaBufFrame) -> Result<Self, Status> {
            let mut planes = Vec::with_capacity(frame.used_planes().len());
            for plane in frame.used_planes() {
                // SAFETY: the caller guarantees validated, open descriptors.
                let fd = unsafe { duplicate_descriptor(plane.fd) }?;
                let acquire_fence = if plane.acquire_fence_fd >= 0 {
                    // SAFETY: as above; -1 means no fence and is skipped.
                    Some(unsafe { duplicate_descriptor(plane.acquire_fence_fd) }?)
                } else {
                    None
                };
                planes.push(DmaBufPlaneInput {
                    fd,
                    acquire_fence,
                    offset: plane.offset,
                    pitch: plane.pitch,
                });
            }
            Ok(Self {
                width: frame.width,
                height: frame.height,
                fourcc: frame.fourcc,
                modifier: frame.modifier,
                planes,
            })
        }
    }

    /// # Safety
    /// `descriptor` is non-negative and open for the duration of this call.
    unsafe fn duplicate_descriptor(descriptor: i32) -> Result<OwnedFd, Status> {
        if descriptor < 0 {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        // SAFETY: the borrow ends before this function returns; the caller
        // guarantees the descriptor stays open until then.
        let borrowed = unsafe { BorrowedFd::borrow_raw(descriptor) };
        // std duplicates with fcntl(F_DUPFD_CLOEXEC); EBADF reports a
        // descriptor that was not actually open.
        borrowed
            .try_clone_to_owned()
            .map_err(|_| STATUS_INVALID_ARGUMENT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amaterasu_media::DRM_FORMAT_MOD_INVALID;

    #[test]
    fn import_support_distinguishes_explicit_and_implicit_modifiers() {
        let mut support = DmaBufImportSupport::default();
        support.add_format(1, false, [0, 7]);
        support.add_format(2, true, []);
        support.add_format(3, false, [DRM_FORMAT_MOD_INVALID]);

        assert!(support.supports(1, 0));
        assert!(support.supports(1, 7));
        assert!(!support.supports(1, 8));
        assert!(!support.supports(1, DRM_FORMAT_MOD_INVALID));
        assert!(support.supports(2, DRM_FORMAT_MOD_INVALID));
        assert!(!support.supports(2, 0));
        assert!(!support.supports(3, DRM_FORMAT_MOD_INVALID));
        assert!(!support.supports(4, 0));
        assert!(!support.is_empty());
        assert!(DmaBufImportSupport::default().is_empty());
    }
}

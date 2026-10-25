//! NVENC API version negotiation.
//!
//! The adapter is compiled against one nvEncodeAPI.h. Its struct layouts are
//! offered to the driver at that header's API version and then at each
//! earlier minor version of the same major, newest first, filtered by the
//! driver's maximum. Older API versions reuse the header's struct revision
//! numbers re-stamped with the older API version: a driver that expects a
//! different revision answers NV_ENC_ERR_INVALID_VERSION and the next
//! candidate is tried, so a mismatch is an open failure, never a
//! misinterpreted struct. Other majors are not offered because their
//! layouts differ from the compiled header.

/// Mirrors `struct am_nvcuda_struct_versions` in native/adapter.h.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StructVersions {
    pub api: u32,
    pub function_list: u32,
    pub open_session: u32,
    pub caps_param: u32,
    pub preset_config: u32,
    pub config: u32,
    pub rc_params: u32,
    pub initialize: u32,
    pub create_bitstream: u32,
    pub register_resource: u32,
    pub map_input: u32,
    pub pic_params: u32,
    pub lock_bitstream: u32,
}

/// `NVENCAPI_VERSION` packs the major in bits 0-7 and the minor in 24-27.
const API_VERSION_BITS: u32 = 0x0f00_00ff;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ApiVersion {
    pub major: u32,
    pub minor: u32,
}

impl ApiVersion {
    /// Decodes `NVENCAPI_VERSION` (major | minor << 24).
    pub fn from_nvenc(version: u32) -> Self {
        Self {
            major: version & 0xff,
            minor: (version >> 24) & 0x0f,
        }
    }

    /// Decodes NvEncodeAPIGetMaxSupportedVersion's (major << 4 | minor).
    pub fn from_driver(packed: u32) -> Self {
        Self {
            major: packed >> 4,
            minor: packed & 0x0f,
        }
    }

    pub fn nvenc(self) -> u32 {
        self.major | (self.minor << 24)
    }
}

/// Replaces the API version embedded in an `NVENCAPI_STRUCT_VERSION` value,
/// keeping its struct revision, the 0x7 tag and the extended-layout bit.
pub fn restamp(struct_version: u32, api: ApiVersion) -> u32 {
    (struct_version & !API_VERSION_BITS) | api.nvenc()
}

impl StructVersions {
    pub fn for_api(&self, api: ApiVersion) -> Self {
        Self {
            api: api.nvenc(),
            function_list: restamp(self.function_list, api),
            open_session: restamp(self.open_session, api),
            caps_param: restamp(self.caps_param, api),
            preset_config: restamp(self.preset_config, api),
            config: restamp(self.config, api),
            rc_params: restamp(self.rc_params, api),
            initialize: restamp(self.initialize, api),
            create_bitstream: restamp(self.create_bitstream, api),
            register_resource: restamp(self.register_resource, api),
            map_input: restamp(self.map_input, api),
            pic_params: restamp(self.pic_params, api),
            lock_bitstream: restamp(self.lock_bitstream, api),
        }
    }
}

/// Candidate version tables, newest first. Empty when the driver predates
/// the compiled header's major version.
pub fn candidates(header: &StructVersions, driver_packed: u32) -> Vec<StructVersions> {
    let compiled = ApiVersion::from_nvenc(header.api);
    let driver = ApiVersion::from_driver(driver_packed);
    (0..=compiled.minor)
        .rev()
        .map(|minor| ApiVersion {
            major: compiled.major,
            minor,
        })
        .filter(|candidate| *candidate <= driver)
        .map(|candidate| header.for_api(candidate))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NVENCAPI_STRUCT_VERSION(revision), optionally with the extended bit.
    fn struct_version(api: ApiVersion, revision: u32, extended: bool) -> u32 {
        api.nvenc() | (revision << 16) | (0x7 << 28) | if extended { 1 << 31 } else { 0 }
    }

    /// The SDK 13.1 header values (nvEncodeAPI.h *_VER macros).
    fn sdk_13_1() -> StructVersions {
        let api = ApiVersion {
            major: 13,
            minor: 1,
        };
        StructVersions {
            api: api.nvenc(),
            function_list: struct_version(api, 2, false),
            open_session: struct_version(api, 1, false),
            caps_param: struct_version(api, 1, false),
            preset_config: struct_version(api, 5, true),
            config: struct_version(api, 9, true),
            rc_params: struct_version(api, 1, false),
            initialize: struct_version(api, 7, true),
            create_bitstream: struct_version(api, 1, false),
            register_resource: struct_version(api, 5, false),
            map_input: struct_version(api, 4, false),
            pic_params: struct_version(api, 7, true),
            lock_bitstream: struct_version(api, 2, true),
        }
    }

    #[test]
    fn api_versions_decode_both_packings() {
        assert_eq!(
            ApiVersion::from_nvenc(13 | (1 << 24)),
            ApiVersion {
                major: 13,
                minor: 1
            }
        );
        assert_eq!(
            ApiVersion::from_driver((13 << 4) | 1),
            ApiVersion {
                major: 13,
                minor: 1
            }
        );
        assert_eq!(
            ApiVersion {
                major: 12,
                minor: 2
            }
            .nvenc(),
            12 | (2 << 24)
        );
    }

    #[test]
    fn restamping_keeps_revision_tag_and_extended_bit() {
        let header = sdk_13_1();
        let older = ApiVersion {
            major: 13,
            minor: 0,
        };
        assert_eq!(
            restamp(header.config, older),
            struct_version(older, 9, true)
        );
        assert_eq!(
            restamp(header.map_input, older),
            struct_version(older, 4, false)
        );
        assert_eq!(header.for_api(ApiVersion::from_nvenc(header.api)), header);
    }

    #[test]
    fn candidates_are_newest_first_and_bounded_by_the_driver() {
        let header = sdk_13_1();
        let apis = |driver: u32| -> Vec<u32> {
            candidates(&header, driver)
                .iter()
                .map(|candidate| candidate.api)
                .collect()
        };
        assert_eq!(apis((13 << 4) | 1), [13 | (1 << 24), 13]);
        assert_eq!(apis(13 << 4), [13]);
        assert_eq!(apis((14 << 4) | 2), [13 | (1 << 24), 13]);
        assert!(apis((12 << 4) | 2).is_empty());
        let fallback = &candidates(&header, 13 << 4)[0];
        assert_eq!(
            fallback.pic_params,
            struct_version(
                ApiVersion {
                    major: 13,
                    minor: 0
                },
                7,
                true
            )
        );
    }
}

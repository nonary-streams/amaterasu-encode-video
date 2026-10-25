/*
 * Amaterasu-owned ABI between the Rust NVENC/CUDA backend and its C adapter.
 * Rust mirrors every record below with #[repr(C)] in runtime.rs. Only these
 * records cross the boundary; NVIDIA, CUDA, EGL and GL objects stay in C.
 *
 * Threading: an encoder handle and every call on it belong to the encoder
 * worker thread that opened it. The CUDA context and the EGL/GL context are
 * made current on that thread at open and stay current until close.
 */
#ifndef AMATERASU_NVENC_CUDA_ADAPTER_H
#define AMATERASU_NVENC_CUDA_ADAPTER_H

#include <stddef.h>
#include <stdint.h>

#define AM_NVCUDA_PCI_BUS_ID_LENGTH 32
#define AM_NVCUDA_DEVICE_NAME_LENGTH 256
#define AM_NVCUDA_MAX_DEVICES 16
#define AM_NVCUDA_MAX_PLANES 4
#define AM_NVCUDA_CODEC_COUNT 3
#define AM_NVCUDA_MAX_PACKET_BYTES (16u * 1024u * 1024u)

enum am_nvcuda_result {
    AM_NVCUDA_OK = 0,
    AM_NVCUDA_LIBRARY_UNAVAILABLE = 1,
    AM_NVCUDA_DRIVER_UNAVAILABLE = 2,
    AM_NVCUDA_API_UNSUPPORTED = 3,
    AM_NVCUDA_NO_DEVICE = 4,
    AM_NVCUDA_UNSUPPORTED = 5,
    AM_NVCUDA_INVALID_ARGUMENT = 6,
    AM_NVCUDA_ALLOCATION_FAILED = 7,
    AM_NVCUDA_NATIVE_FAILURE = 8,
    AM_NVCUDA_INVALID_OUTPUT = 9,
    AM_NVCUDA_CONVERTER_UNAVAILABLE = 10,
    AM_NVCUDA_IMPORT_FAILED = 11,
    AM_NVCUDA_FENCE_TIMEOUT = 12,
};

/* Codec identifiers match amaterasu_media::CODEC_* values. */
enum am_nvcuda_codec {
    AM_NVCUDA_CODEC_H264 = 1,
    AM_NVCUDA_CODEC_HEVC = 2,
    AM_NVCUDA_CODEC_AV1 = 3,
};

/* Last failing operation and its raw NVENC, CUDA, EGL or GL code. */
struct am_nvcuda_status {
    uint32_t stage;
    int32_t native_code;
};

/* One NVENC API version and the struct versions used with it. The Rust
 * negotiation policy produces these; the adapter only applies them. */
struct am_nvcuda_struct_versions {
    uint32_t api;
    uint32_t function_list;
    uint32_t open_session;
    uint32_t caps_param;
    uint32_t preset_config;
    uint32_t config;
    uint32_t rc_params;
    uint32_t initialize;
    uint32_t create_bitstream;
    uint32_t register_resource;
    uint32_t map_input;
    uint32_t pic_params;
    uint32_t lock_bitstream;
};

struct am_nvcuda_codec_caps {
    uint32_t supported;
    uint32_t max_width;
    uint32_t max_height;
};

struct am_nvcuda_device {
    char pci_bus_id[AM_NVCUDA_PCI_BUS_ID_LENGTH];
    char name[AM_NVCUDA_DEVICE_NAME_LENGTH];
    /* Indexed by am_nvcuda_codec - 1. */
    struct am_nvcuda_codec_caps codecs[AM_NVCUDA_CODEC_COUNT];
    /* An EGL display on this GPU supports the GL converter. */
    uint32_t gpu_conversion;
    /* That display can also import DMA-BUFs. */
    uint32_t dmabuf_import;
};

struct am_nvcuda_settings {
    uint32_t codec;
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t bitrate;
    /* Zero leaves the DPB size to the driver. */
    uint32_t reference_frames;
    uint32_t full_range;
    uint32_t color_primaries;
    uint32_t transfer_characteristics;
    uint32_t matrix_coefficients;
    /* H.264/HEVC chroma_sample_loc_type, or AV1 chroma_sample_position. */
    uint32_t chroma_sample_location;
    /* Normalized RGB -> code value / 255 rows: {r, g, b, offset}. */
    float luma_row[4];
    float cb_row[4];
    float cr_row[4];
};

/* A DMA-BUF frame whose descriptors the Rust caller owns. Plane fds are only
 * borrowed. A fence fd whose ownership moved to EGL is replaced with -1 on
 * return; the caller closes every descriptor still present. */
struct am_nvcuda_dmabuf {
    uint32_t width;
    uint32_t height;
    uint32_t fourcc;
    uint32_t plane_count;
    uint64_t modifier;
    int32_t plane_fds[AM_NVCUDA_MAX_PLANES];
    int32_t fence_fds[AM_NVCUDA_MAX_PLANES];
    uint32_t offsets[AM_NVCUDA_MAX_PLANES];
    uint32_t pitches[AM_NVCUDA_MAX_PLANES];
};

struct am_nvcuda_encoder;

/* GPU-free: record sizes, so Rust tests can check their mirrored layouts. */
struct am_nvcuda_abi_sizes {
    uint32_t status;
    uint32_t struct_versions;
    uint32_t device;
    uint32_t settings;
    uint32_t dmabuf;
    uint32_t test_buffer;
};
void amaterasu_nvcuda_abi_sizes(struct am_nvcuda_abi_sizes *sizes);

/* GPU-free: struct versions declared by the compiled nvEncodeAPI.h. */
int amaterasu_nvcuda_header_versions(struct am_nvcuda_struct_versions *versions);

/* Loads the driver libraries once per process and reports
 * NvEncodeAPIGetMaxSupportedVersion's packed (major << 4 | minor) value. */
int amaterasu_nvcuda_driver_api(uint32_t *packed_version, struct am_nvcuda_status *status);

int amaterasu_nvcuda_enumerate(const struct am_nvcuda_struct_versions *candidates,
                               uint32_t candidate_count, struct am_nvcuda_device *devices,
                               uint32_t capacity, uint32_t *device_count,
                               struct am_nvcuda_status *status);

struct am_nvcuda_encoder *amaterasu_nvcuda_open(
    const char *pci_bus_id, const struct am_nvcuda_struct_versions *candidates,
    uint32_t candidate_count, const struct am_nvcuda_settings *settings,
    struct am_nvcuda_status *status, int *result);

/* Modifiers this encoder's EGL display imports into a GL_TEXTURE_2D for the
 * fourcc; implicit is nonzero when DRM_FORMAT_MOD_INVALID imports work. */
int amaterasu_nvcuda_import_modifiers(struct am_nvcuda_encoder *encoder, uint32_t fourcc,
                                      uint64_t *modifiers, uint32_t capacity,
                                      uint32_t *modifier_count, uint32_t *implicit);

int amaterasu_nvcuda_upload_nv12(struct am_nvcuda_encoder *encoder, const uint8_t *nv12,
                                 size_t length, struct am_nvcuda_status *status);
int amaterasu_nvcuda_upload_bgra(struct am_nvcuda_encoder *encoder, const uint8_t *bgra,
                                 size_t length, struct am_nvcuda_status *status);
int amaterasu_nvcuda_import_dmabuf(struct am_nvcuda_encoder *encoder,
                                   struct am_nvcuda_dmabuf *frame,
                                   struct am_nvcuda_status *status);

/* Encodes the current input buffer. On success *data stays valid, and the
 * bitstream stays locked, until amaterasu_nvcuda_release_output or close. */
int amaterasu_nvcuda_encode(struct am_nvcuda_encoder *encoder, int64_t timestamp_100ns,
                            int force_idr, const uint8_t **data, size_t *length,
                            uint32_t *picture_type, struct am_nvcuda_status *status);
int amaterasu_nvcuda_release_output(struct am_nvcuda_encoder *encoder,
                                    struct am_nvcuda_status *status);
int amaterasu_nvcuda_drain(struct am_nvcuda_encoder *encoder, struct am_nvcuda_status *status);
void amaterasu_nvcuda_close(struct am_nvcuda_encoder *encoder);

/* Test support for the ignored GPU tests: allocate an XRGB8888 GBM buffer
 * whose top half is top_xrgb and bottom half bottom_xrgb. */
struct am_nvcuda_test_buffer {
    void *device;
    void *buffer;
    int32_t render_fd;
    int32_t dmabuf_fd;
    uint32_t pitch;
    uint32_t offset;
    uint64_t modifier;
};
int amaterasu_nvcuda_test_allocate_gbm(const char *render_node, uint32_t width,
                                       uint32_t height, uint32_t top_xrgb,
                                       uint32_t bottom_xrgb,
                                       struct am_nvcuda_test_buffer *buffer);
void amaterasu_nvcuda_test_release_gbm(struct am_nvcuda_test_buffer *buffer);

#endif

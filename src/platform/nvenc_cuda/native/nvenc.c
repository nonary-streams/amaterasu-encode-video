/*
 * NVENC session management on a CUDA context. The encoder input is one
 * pitched NV12 CUDA allocation registered with NVENC as a CUDA device
 * pointer. CPU NV12 frames are copied into it with cuMemcpy2D; RGB sources
 * are converted into it on the GPU by convert.c.
 *
 * Ownership: am_nvcuda_encoder owns, in creation order, the CUDA context,
 * the NVENC session, the bitstream buffer, the CUDA input allocation and
 * its NVENC registration, and the GL converter. Close releases them in
 * reverse order on the worker thread that opened them.
 */
#include "internal.h"

#include <stdlib.h>
#include <string.h>

#define AM_MAX_CODEC_GUIDS 64
#define AM_MAX_INPUT_FORMATS 64

struct am_nvcuda_encoder {
    const struct am_driver *driver;
    NV_ENCODE_API_FUNCTION_LIST functions;
    struct am_nvcuda_struct_versions versions;
    CUcontext context;
    void *session;
    NV_ENC_OUTPUT_PTR bitstream;
    CUdeviceptr input;
    size_t input_pitch;
    NV_ENC_REGISTERED_PTR registered_input;
    /* Non-NULL from a successful encode until its output is released. */
    NV_ENC_INPUT_PTR mapped_input;
    int bitstream_locked;
    struct am_converter converter;
    uint32_t codec;
    uint32_t width;
    uint32_t height;
    int draining;
};

void amaterasu_nvcuda_abi_sizes(struct am_nvcuda_abi_sizes *sizes) {
    sizes->status = (uint32_t)sizeof(struct am_nvcuda_status);
    sizes->struct_versions = (uint32_t)sizeof(struct am_nvcuda_struct_versions);
    sizes->device = (uint32_t)sizeof(struct am_nvcuda_device);
    sizes->settings = (uint32_t)sizeof(struct am_nvcuda_settings);
    sizes->dmabuf = (uint32_t)sizeof(struct am_nvcuda_dmabuf);
    sizes->test_buffer = (uint32_t)sizeof(struct am_nvcuda_test_buffer);
}

int amaterasu_nvcuda_header_versions(struct am_nvcuda_struct_versions *versions) {
    if (versions == NULL) {
        return AM_NVCUDA_INVALID_ARGUMENT;
    }
    versions->api = NVENCAPI_VERSION;
    versions->function_list = NV_ENCODE_API_FUNCTION_LIST_VER;
    versions->open_session = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    versions->caps_param = NV_ENC_CAPS_PARAM_VER;
    versions->preset_config = NV_ENC_PRESET_CONFIG_VER;
    versions->config = NV_ENC_CONFIG_VER;
    versions->rc_params = NV_ENC_RC_PARAMS_VER;
    versions->initialize = NV_ENC_INITIALIZE_PARAMS_VER;
    versions->create_bitstream = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    versions->register_resource = NV_ENC_REGISTER_RESOURCE_VER;
    versions->map_input = NV_ENC_MAP_INPUT_RESOURCE_VER;
    versions->pic_params = NV_ENC_PIC_PARAMS_VER;
    versions->lock_bitstream = NV_ENC_LOCK_BITSTREAM_VER;
    return AM_NVCUDA_OK;
}

static const GUID *codec_guid(uint32_t codec) {
    switch (codec) {
    case AM_NVCUDA_CODEC_H264: return &NV_ENC_CODEC_H264_GUID;
    case AM_NVCUDA_CODEC_HEVC: return &NV_ENC_CODEC_HEVC_GUID;
    case AM_NVCUDA_CODEC_AV1: return &NV_ENC_CODEC_AV1_GUID;
    default: return NULL;
    }
}

/* Moonlight's constrained H.264 decoders get Baseline (no CABAC, no B);
 * HEVC and AV1 use their 8-bit 4:2:0 Main profiles. */
static const GUID *profile_guid(uint32_t codec) {
    switch (codec) {
    case AM_NVCUDA_CODEC_H264: return &NV_ENC_H264_PROFILE_BASELINE_GUID;
    case AM_NVCUDA_CODEC_HEVC: return &NV_ENC_HEVC_PROFILE_MAIN_GUID;
    case AM_NVCUDA_CODEC_AV1: return &NV_ENC_AV1_PROFILE_MAIN_GUID;
    default: return NULL;
    }
}

static int same_guid(const GUID *left, const GUID *right) {
    return memcmp(left, right, sizeof(GUID)) == 0;
}

/* Tries each Rust-selected API version until the driver accepts both the
 * function list and the session. On success the session uses `versions`. */
static int open_session(const struct am_driver *driver, CUcontext context,
                        const struct am_nvcuda_struct_versions *candidates,
                        uint32_t candidate_count, NV_ENCODE_API_FUNCTION_LIST *functions,
                        struct am_nvcuda_struct_versions *versions, void **session,
                        struct am_nvcuda_status *status) {
    am_set_status(status, AM_STAGE_API, (int32_t)NV_ENC_ERR_INVALID_VERSION);
    for (uint32_t index = 0; index < candidate_count; ++index) {
        const struct am_nvcuda_struct_versions *candidate = &candidates[index];
        memset(functions, 0, sizeof(*functions));
        functions->version = candidate->function_list;
        NVENCSTATUS native_status = driver->nvenc_create_instance(functions);
        if (native_status == NV_ENC_ERR_INVALID_VERSION) {
            continue;
        }
        if (native_status != NV_ENC_SUCCESS) {
            am_set_status(status, AM_STAGE_API, (int32_t)native_status);
            return AM_NVCUDA_API_UNSUPPORTED;
        }
        NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS parameters;
        memset(&parameters, 0, sizeof(parameters));
        parameters.version = candidate->open_session;
        parameters.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
        parameters.device = context;
        parameters.apiVersion = candidate->api;
        *session = NULL;
        native_status = functions->nvEncOpenEncodeSessionEx(&parameters, session);
        if (native_status == NV_ENC_SUCCESS && *session != NULL) {
            *versions = *candidate;
            return AM_NVCUDA_OK;
        }
        if (native_status != NV_ENC_ERR_INVALID_VERSION) {
            am_set_status(status, AM_STAGE_SESSION, (int32_t)native_status);
            return AM_NVCUDA_NATIVE_FAILURE;
        }
    }
    return AM_NVCUDA_API_UNSUPPORTED;
}

static int query_cap(const NV_ENCODE_API_FUNCTION_LIST *functions,
                     const struct am_nvcuda_struct_versions *versions, void *session,
                     const GUID *codec, NV_ENC_CAPS capability, int *value) {
    NV_ENC_CAPS_PARAM parameters;
    memset(&parameters, 0, sizeof(parameters));
    parameters.version = versions->caps_param;
    parameters.capsToQuery = capability;
    *value = 0;
    return functions->nvEncGetEncodeCaps(session, *codec, &parameters, value) == NV_ENC_SUCCESS;
}

static int codec_listed(const NV_ENCODE_API_FUNCTION_LIST *functions, void *session,
                        const GUID *codec) {
    GUID codecs[AM_MAX_CODEC_GUIDS];
    uint32_t count = 0;
    if (functions->nvEncGetEncodeGUIDs(session, codecs, AM_MAX_CODEC_GUIDS, &count) !=
        NV_ENC_SUCCESS) {
        return 0;
    }
    for (uint32_t index = 0; index < count && index < AM_MAX_CODEC_GUIDS; ++index) {
        if (same_guid(&codecs[index], codec)) {
            return 1;
        }
    }
    return 0;
}

static int accepts_nv12(const NV_ENCODE_API_FUNCTION_LIST *functions, void *session,
                        const GUID *codec) {
    NV_ENC_BUFFER_FORMAT formats[AM_MAX_INPUT_FORMATS];
    uint32_t count = 0;
    if (functions->nvEncGetInputFormats(session, *codec, formats, AM_MAX_INPUT_FORMATS,
                                        &count) != NV_ENC_SUCCESS) {
        return 0;
    }
    for (uint32_t index = 0; index < count && index < AM_MAX_INPUT_FORMATS; ++index) {
        if (formats[index] == NV_ENC_BUFFER_FORMAT_NV12) {
            return 1;
        }
    }
    return 0;
}

/* A codec is usable when it is listed, takes NV12 and offers CBR. */
static void probe_codec(const NV_ENCODE_API_FUNCTION_LIST *functions,
                        const struct am_nvcuda_struct_versions *versions, void *session,
                        uint32_t codec, struct am_nvcuda_codec_caps *caps) {
    memset(caps, 0, sizeof(*caps));
    const GUID *guid = codec_guid(codec);
    int max_width = 0;
    int max_height = 0;
    int rate_control = 0;
    if (!codec_listed(functions, session, guid) || !accepts_nv12(functions, session, guid) ||
        !query_cap(functions, versions, session, guid, NV_ENC_CAPS_WIDTH_MAX, &max_width) ||
        !query_cap(functions, versions, session, guid, NV_ENC_CAPS_HEIGHT_MAX, &max_height) ||
        !query_cap(functions, versions, session, guid, NV_ENC_CAPS_SUPPORTED_RATECONTROL_MODES,
                   &rate_control) ||
        (rate_control & NV_ENC_PARAMS_RC_CBR) == 0 || max_width <= 0 || max_height <= 0) {
        return;
    }
    caps->supported = 1;
    caps->max_width = (uint32_t)max_width;
    caps->max_height = (uint32_t)max_height;
}

static int find_device(const struct am_driver *driver, const char *pci_bus_id, CUdevice *device) {
    int count = 0;
    if (driver->cuda.device_get_count(&count) != CUDA_SUCCESS) {
        return 0;
    }
    for (int ordinal = 0; ordinal < count && ordinal < AM_NVCUDA_MAX_DEVICES; ++ordinal) {
        char bus_id[AM_NVCUDA_PCI_BUS_ID_LENGTH] = {0};
        if (driver->cuda.device_get(device, ordinal) == CUDA_SUCCESS &&
            driver->cuda.device_get_pci_bus_id(bus_id, (int)sizeof(bus_id), *device) ==
                CUDA_SUCCESS &&
            strcmp(bus_id, pci_bus_id) == 0) {
            return 1;
        }
    }
    return 0;
}

static void describe_device(const struct am_driver *driver,
                            const struct am_nvcuda_struct_versions *candidates,
                            uint32_t candidate_count, CUdevice device,
                            struct am_nvcuda_device *description) {
    CUcontext context = NULL;
    if (driver->cuda.ctx_create(&context, CU_CTX_SCHED_BLOCKING_SYNC, device) != CUDA_SUCCESS) {
        return;
    }
    NV_ENCODE_API_FUNCTION_LIST functions;
    struct am_nvcuda_struct_versions versions;
    void *session = NULL;
    struct am_nvcuda_status ignored;
    if (open_session(driver, context, candidates, candidate_count, &functions, &versions,
                     &session, &ignored) == AM_NVCUDA_OK) {
        for (uint32_t codec = 1; codec <= AM_NVCUDA_CODEC_COUNT; ++codec) {
            probe_codec(&functions, &versions, session, codec, &description->codecs[codec - 1]);
        }
        functions.nvEncDestroyEncoder(session);
    }
    driver->cuda.ctx_destroy(context);
    const struct am_egl_display *display = NULL;
    if (am_egl_display_for_device(driver, device, &display, &ignored) == AM_NVCUDA_OK) {
        description->gpu_conversion = 1;
        description->dmabuf_import = (uint32_t)display->dmabuf_import;
    }
}

int amaterasu_nvcuda_enumerate(const struct am_nvcuda_struct_versions *candidates,
                               uint32_t candidate_count, struct am_nvcuda_device *devices,
                               uint32_t capacity, uint32_t *device_count,
                               struct am_nvcuda_status *status) {
    if (candidates == NULL || candidate_count == 0 || devices == NULL || device_count == NULL ||
        capacity == 0) {
        return AM_NVCUDA_INVALID_ARGUMENT;
    }
    *device_count = 0;
    memset(devices, 0, sizeof(*devices) * capacity);
    int result = AM_NVCUDA_OK;
    const struct am_driver *driver = am_driver_load(status, &result);
    if (driver == NULL) {
        return result;
    }
    int count = 0;
    CUresult cuda_status = driver->cuda.device_get_count(&count);
    if (cuda_status != CUDA_SUCCESS) {
        am_set_status(status, AM_STAGE_DRIVER, (int32_t)cuda_status);
        return AM_NVCUDA_DRIVER_UNAVAILABLE;
    }
    for (int ordinal = 0; ordinal < count && *device_count < capacity; ++ordinal) {
        CUdevice device = 0;
        struct am_nvcuda_device *description = &devices[*device_count];
        if (driver->cuda.device_get(&device, ordinal) != CUDA_SUCCESS ||
            driver->cuda.device_get_pci_bus_id(description->pci_bus_id,
                                               (int)sizeof(description->pci_bus_id),
                                               device) != CUDA_SUCCESS ||
            driver->cuda.device_get_name(description->name, (int)sizeof(description->name),
                                         device) != CUDA_SUCCESS) {
            memset(description, 0, sizeof(*description));
            continue;
        }
        description->pci_bus_id[sizeof(description->pci_bus_id) - 1] = '\0';
        description->name[sizeof(description->name) - 1] = '\0';
        describe_device(driver, candidates, candidate_count, device, description);
        ++*device_count;
    }
    return AM_NVCUDA_OK;
}

static int validate_settings(const struct am_nvcuda_settings *settings) {
    return codec_guid(settings->codec) != NULL && settings->width >= 16 &&
           settings->height >= 16 && settings->width % 2 == 0 && settings->height % 2 == 0 &&
           settings->fps >= 1 && settings->fps <= 240 && settings->bitrate >= 100000 &&
           settings->reference_frames <= 16 && settings->full_range <= 1;
}

static void configure_vui(NV_ENC_CONFIG_H264_VUI_PARAMETERS *vui,
                          const struct am_nvcuda_settings *settings) {
    vui->videoSignalTypePresentFlag = 1;
    vui->videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    vui->videoFullRangeFlag = settings->full_range;
    vui->colourDescriptionPresentFlag = 1;
    vui->colourPrimaries = (NV_ENC_VUI_COLOR_PRIMARIES)settings->color_primaries;
    vui->transferCharacteristics =
        (NV_ENC_VUI_TRANSFER_CHARACTERISTIC)settings->transfer_characteristics;
    vui->colourMatrix = (NV_ENC_VUI_MATRIX_COEFFS)settings->matrix_coefficients;
    vui->chromaSampleLocationFlag = 1;
    vui->chromaSampleLocationTop = settings->chroma_sample_location;
    vui->chromaSampleLocationBot = settings->chroma_sample_location;
    /* Lets decoders output each picture without waiting for reordering. */
    vui->bitstreamRestrictionFlag = 1;
}

static void configure_h264(NV_ENC_CONFIG_H264 *h264, const struct am_nvcuda_settings *settings) {
    h264->repeatSPSPPS = 1;
    h264->idrPeriod = NVENC_INFINITE_GOPLENGTH;
    h264->sliceMode = 3;
    h264->sliceModeData = 1;
    h264->chromaFormatIDC = 1;
    h264->entropyCodingMode = NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC;
    h264->inputBitDepth = NV_ENC_BIT_DEPTH_8;
    h264->outputBitDepth = NV_ENC_BIT_DEPTH_8;
    h264->maxNumRefFrames = settings->reference_frames;
    h264->numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
    configure_vui(&h264->h264VUIParameters, settings);
}

static void configure_hevc(NV_ENC_CONFIG_HEVC *hevc, const struct am_nvcuda_settings *settings) {
    hevc->repeatSPSPPS = 1;
    hevc->idrPeriod = NVENC_INFINITE_GOPLENGTH;
    hevc->sliceMode = 3;
    hevc->sliceModeData = 1;
    hevc->chromaFormatIDC = 1;
    hevc->inputBitDepth = NV_ENC_BIT_DEPTH_8;
    hevc->outputBitDepth = NV_ENC_BIT_DEPTH_8;
    hevc->maxNumRefFramesInDPB = settings->reference_frames;
    hevc->numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
    configure_vui(&hevc->hevcVUIParameters, settings);
}

static void configure_av1(NV_ENC_CONFIG_AV1 *av1, const struct am_nvcuda_settings *settings) {
    av1->level = NV_ENC_LEVEL_AV1_AUTOSELECT;
    av1->tier = NV_ENC_TIER_AV1_0;
    av1->outputAnnexBFormat = 0;
    av1->repeatSeqHdr = 1;
    av1->idrPeriod = NVENC_INFINITE_GOPLENGTH;
    av1->chromaFormatIDC = 1;
    av1->inputBitDepth = NV_ENC_BIT_DEPTH_8;
    av1->outputBitDepth = NV_ENC_BIT_DEPTH_8;
    av1->colorPrimaries = (NV_ENC_VUI_COLOR_PRIMARIES)settings->color_primaries;
    av1->transferCharacteristics =
        (NV_ENC_VUI_TRANSFER_CHARACTERISTIC)settings->transfer_characteristics;
    av1->matrixCoefficients = (NV_ENC_VUI_MATRIX_COEFFS)settings->matrix_coefficients;
    av1->colorRange = settings->full_range;
    av1->chromaSamplePosition = settings->chroma_sample_location;
    av1->maxNumRefFramesInDPB = settings->reference_frames;
    av1->numFwdRefs = NV_ENC_NUM_REF_FRAMES_1;
}

/* Low latency: P1 with ultra-low-latency tuning, CBR with a one-frame VBV,
 * no B pictures, no lookahead and an infinite GOP so IDR only happens when
 * requested. */
static int configure_encoder(struct am_nvcuda_encoder *encoder,
                             const struct am_nvcuda_settings *settings,
                             struct am_nvcuda_status *status) {
    const GUID *codec = codec_guid(settings->codec);
    struct am_nvcuda_codec_caps caps;
    probe_codec(&encoder->functions, &encoder->versions, encoder->session, settings->codec, &caps);
    if (!caps.supported || settings->width > caps.max_width ||
        settings->height > caps.max_height) {
        am_set_status(status, AM_STAGE_CAPS, 0);
        return AM_NVCUDA_UNSUPPORTED;
    }
    int custom_vbv = 0;
    int multiple_references = 0;
    query_cap(&encoder->functions, &encoder->versions, encoder->session, codec,
              NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE, &custom_vbv);
    query_cap(&encoder->functions, &encoder->versions, encoder->session, codec,
              NV_ENC_CAPS_SUPPORT_MULTIPLE_REF_FRAMES, &multiple_references);

    NV_ENC_PRESET_CONFIG preset;
    memset(&preset, 0, sizeof(preset));
    preset.version = encoder->versions.preset_config;
    preset.presetCfg.version = encoder->versions.config;
    NVENCSTATUS native_status = encoder->functions.nvEncGetEncodePresetConfigEx(
        encoder->session, *codec, NV_ENC_PRESET_P1_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
        &preset);
    if (native_status != NV_ENC_SUCCESS) {
        am_set_status(status, AM_STAGE_PRESET, (int32_t)native_status);
        return AM_NVCUDA_NATIVE_FAILURE;
    }

    NV_ENC_CONFIG config = preset.presetCfg;
    config.version = encoder->versions.config;
    config.profileGUID = *profile_guid(settings->codec);
    config.gopLength = NVENC_INFINITE_GOPLENGTH;
    config.frameIntervalP = 1;
    config.frameFieldMode = NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
    config.rcParams.version = encoder->versions.rc_params;
    config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    config.rcParams.averageBitRate = settings->bitrate;
    config.rcParams.maxBitRate = settings->bitrate;
    config.rcParams.enableLookahead = 0;
    config.rcParams.lookaheadDepth = 0;
    config.rcParams.enableAQ = 0;
    config.rcParams.enableTemporalAQ = 0;
    config.rcParams.zeroReorderDelay = 1;
    config.rcParams.lowDelayKeyFrameScale = 1;
    config.rcParams.multiPass = NV_ENC_MULTI_PASS_DISABLED;
    if (custom_vbv) {
        uint32_t frame_bits = settings->bitrate / settings->fps;
        config.rcParams.vbvBufferSize = frame_bits > 0 ? frame_bits : 1;
        config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
    }

    struct am_nvcuda_settings effective = *settings;
    if (effective.reference_frames > 1 && !multiple_references) {
        effective.reference_frames = 1;
    }
    switch (settings->codec) {
    case AM_NVCUDA_CODEC_H264:
        configure_h264(&config.encodeCodecConfig.h264Config, &effective);
        break;
    case AM_NVCUDA_CODEC_HEVC:
        configure_hevc(&config.encodeCodecConfig.hevcConfig, &effective);
        break;
    default:
        configure_av1(&config.encodeCodecConfig.av1Config, &effective);
        break;
    }

    NV_ENC_INITIALIZE_PARAMS initialize;
    memset(&initialize, 0, sizeof(initialize));
    initialize.version = encoder->versions.initialize;
    initialize.encodeGUID = *codec;
    initialize.presetGUID = NV_ENC_PRESET_P1_GUID;
    initialize.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    initialize.encodeWidth = settings->width;
    initialize.encodeHeight = settings->height;
    initialize.darWidth = settings->width;
    initialize.darHeight = settings->height;
    initialize.frameRateNum = settings->fps;
    initialize.frameRateDen = 1;
    initialize.enableEncodeAsync = 0;
    initialize.enablePTD = 1;
    initialize.encodeConfig = &config;
    native_status = encoder->functions.nvEncInitializeEncoder(encoder->session, &initialize);
    if (native_status != NV_ENC_SUCCESS) {
        am_set_status(status, AM_STAGE_INITIALIZE, (int32_t)native_status);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    return AM_NVCUDA_OK;
}

static int create_buffers(struct am_nvcuda_encoder *encoder, struct am_nvcuda_status *status) {
    NV_ENC_CREATE_BITSTREAM_BUFFER bitstream;
    memset(&bitstream, 0, sizeof(bitstream));
    bitstream.version = encoder->versions.create_bitstream;
    NVENCSTATUS native_status =
        encoder->functions.nvEncCreateBitstreamBuffer(encoder->session, &bitstream);
    if (native_status != NV_ENC_SUCCESS || bitstream.bitstreamBuffer == NULL) {
        am_set_status(status, AM_STAGE_BITSTREAM, (int32_t)native_status);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    encoder->bitstream = bitstream.bitstreamBuffer;

    /* One allocation: luma rows followed by interleaved chroma rows, both at
     * the same pitch, which is the layout NVENC expects for NV12. */
    CUresult cuda_status = encoder->driver->cuda.mem_alloc_pitch(
        &encoder->input, &encoder->input_pitch, encoder->width,
        (size_t)encoder->height + encoder->height / 2, 16);
    if (cuda_status != CUDA_SUCCESS || encoder->input_pitch > UINT32_MAX) {
        am_set_status(status, AM_STAGE_INPUT, (int32_t)cuda_status);
        return AM_NVCUDA_ALLOCATION_FAILED;
    }
    NV_ENC_REGISTER_RESOURCE resource;
    memset(&resource, 0, sizeof(resource));
    resource.version = encoder->versions.register_resource;
    resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
    resource.width = encoder->width;
    resource.height = encoder->height;
    resource.pitch = (uint32_t)encoder->input_pitch;
    resource.resourceToRegister = (void *)(uintptr_t)encoder->input;
    resource.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
    resource.bufferUsage = NV_ENC_INPUT_IMAGE;
    native_status = encoder->functions.nvEncRegisterResource(encoder->session, &resource);
    if (native_status != NV_ENC_SUCCESS || resource.registeredResource == NULL) {
        am_set_status(status, AM_STAGE_INPUT, (int32_t)native_status);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    encoder->registered_input = resource.registeredResource;
    return AM_NVCUDA_OK;
}

static int open_encoder(struct am_nvcuda_encoder *encoder, const char *pci_bus_id,
                        const struct am_nvcuda_struct_versions *candidates,
                        uint32_t candidate_count, const struct am_nvcuda_settings *settings,
                        struct am_nvcuda_status *status) {
    CUdevice device = 0;
    if (!find_device(encoder->driver, pci_bus_id, &device)) {
        am_set_status(status, AM_STAGE_DEVICE, 0);
        return AM_NVCUDA_NO_DEVICE;
    }
    /* Created on, and left current for, the encoder worker thread. */
    CUresult cuda_status =
        encoder->driver->cuda.ctx_create(&encoder->context, CU_CTX_SCHED_BLOCKING_SYNC, device);
    if (cuda_status != CUDA_SUCCESS) {
        encoder->context = NULL;
        am_set_status(status, AM_STAGE_DEVICE, (int32_t)cuda_status);
        return AM_NVCUDA_DRIVER_UNAVAILABLE;
    }
    int result = open_session(encoder->driver, encoder->context, candidates, candidate_count,
                              &encoder->functions, &encoder->versions, &encoder->session, status);
    if (result == AM_NVCUDA_OK) {
        result = configure_encoder(encoder, settings, status);
    }
    if (result == AM_NVCUDA_OK) {
        result = create_buffers(encoder, status);
    }
    if (result == AM_NVCUDA_OK) {
        result = am_converter_open(&encoder->converter, encoder->driver, device, settings,
                                   status);
    }
    return result;
}

struct am_nvcuda_encoder *amaterasu_nvcuda_open(
    const char *pci_bus_id, const struct am_nvcuda_struct_versions *candidates,
    uint32_t candidate_count, const struct am_nvcuda_settings *settings,
    struct am_nvcuda_status *status, int *result) {
    if (result == NULL) {
        return NULL;
    }
    if (pci_bus_id == NULL || candidates == NULL || candidate_count == 0 || settings == NULL ||
        !validate_settings(settings)) {
        am_set_status(status, AM_STAGE_NONE, 0);
        *result = AM_NVCUDA_INVALID_ARGUMENT;
        return NULL;
    }
    const struct am_driver *driver = am_driver_load(status, result);
    if (driver == NULL) {
        return NULL;
    }
    struct am_nvcuda_encoder *encoder = calloc(1, sizeof(*encoder));
    if (encoder == NULL) {
        *result = AM_NVCUDA_ALLOCATION_FAILED;
        return NULL;
    }
    encoder->driver = driver;
    encoder->codec = settings->codec;
    encoder->width = settings->width;
    encoder->height = settings->height;
    *result = open_encoder(encoder, pci_bus_id, candidates, candidate_count, settings, status);
    if (*result != AM_NVCUDA_OK) {
        amaterasu_nvcuda_close(encoder);
        return NULL;
    }
    am_set_status(status, AM_STAGE_NONE, 0);
    return encoder;
}

int amaterasu_nvcuda_import_modifiers(struct am_nvcuda_encoder *encoder, uint32_t fourcc,
                                      uint64_t *modifiers, uint32_t capacity,
                                      uint32_t *modifier_count, uint32_t *implicit) {
    if (encoder == NULL || modifiers == NULL || modifier_count == NULL || implicit == NULL) {
        return AM_NVCUDA_INVALID_ARGUMENT;
    }
    return am_converter_import_modifiers(&encoder->converter, fourcc, modifiers, capacity,
                                         modifier_count, implicit);
}

static int input_writable(const struct am_nvcuda_encoder *encoder) {
    return encoder != NULL && !encoder->draining && encoder->mapped_input == NULL;
}

int amaterasu_nvcuda_upload_nv12(struct am_nvcuda_encoder *encoder, const uint8_t *nv12,
                                 size_t length, struct am_nvcuda_status *status) {
    size_t luma_bytes = encoder != NULL ? (size_t)encoder->width * encoder->height : 0;
    if (!input_writable(encoder) || nv12 == NULL || length != luma_bytes + luma_bytes / 2) {
        am_set_status(status, AM_STAGE_UPLOAD, 0);
        return AM_NVCUDA_INVALID_ARGUMENT;
    }
    CUDA_MEMCPY2D copy;
    memset(&copy, 0, sizeof(copy));
    copy.srcMemoryType = CU_MEMORYTYPE_HOST;
    copy.srcHost = nv12;
    copy.srcPitch = encoder->width;
    copy.dstMemoryType = CU_MEMORYTYPE_DEVICE;
    copy.dstDevice = encoder->input;
    copy.dstPitch = encoder->input_pitch;
    copy.WidthInBytes = encoder->width;
    /* The packed chroma rows directly follow the luma rows in both layouts. */
    copy.Height = (size_t)encoder->height + encoder->height / 2;
    CUresult cuda_status = encoder->driver->cuda.memcpy_2d(&copy);
    if (cuda_status != CUDA_SUCCESS) {
        am_set_status(status, AM_STAGE_UPLOAD, (int32_t)cuda_status);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    return AM_NVCUDA_OK;
}

int amaterasu_nvcuda_upload_bgra(struct am_nvcuda_encoder *encoder, const uint8_t *bgra,
                                 size_t length, struct am_nvcuda_status *status) {
    if (!input_writable(encoder) || bgra == NULL ||
        length != (size_t)encoder->width * encoder->height * 4u) {
        am_set_status(status, AM_STAGE_UPLOAD, 0);
        return AM_NVCUDA_INVALID_ARGUMENT;
    }
    int result = am_converter_upload_bgra(&encoder->converter, bgra, status);
    if (result != AM_NVCUDA_OK) {
        return result;
    }
    return am_converter_copy_nv12(&encoder->converter, encoder->input, encoder->input_pitch,
                                  status);
}

int amaterasu_nvcuda_import_dmabuf(struct am_nvcuda_encoder *encoder,
                                   struct am_nvcuda_dmabuf *frame,
                                   struct am_nvcuda_status *status) {
    if (!input_writable(encoder) || frame == NULL) {
        am_set_status(status, AM_STAGE_IMPORT, 0);
        return AM_NVCUDA_INVALID_ARGUMENT;
    }
    int result = am_converter_import_dmabuf(&encoder->converter, frame, status);
    if (result != AM_NVCUDA_OK) {
        return result;
    }
    return am_converter_copy_nv12(&encoder->converter, encoder->input, encoder->input_pitch,
                                  status);
}

static void unmap_input(struct am_nvcuda_encoder *encoder) {
    if (encoder->mapped_input != NULL) {
        encoder->functions.nvEncUnmapInputResource(encoder->session, encoder->mapped_input);
        encoder->mapped_input = NULL;
    }
}

static int map_input(struct am_nvcuda_encoder *encoder, NV_ENC_BUFFER_FORMAT *format,
                     struct am_nvcuda_status *status) {
    NV_ENC_MAP_INPUT_RESOURCE map;
    memset(&map, 0, sizeof(map));
    map.version = encoder->versions.map_input;
    map.registeredResource = encoder->registered_input;
    NVENCSTATUS native_status = encoder->functions.nvEncMapInputResource(encoder->session, &map);
    if (native_status != NV_ENC_SUCCESS || map.mappedResource == NULL) {
        am_set_status(status, AM_STAGE_ENCODE, (int32_t)native_status);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    encoder->mapped_input = map.mappedResource;
    *format = map.mappedBufferFmt;
    return AM_NVCUDA_OK;
}

static int submit_picture(struct am_nvcuda_encoder *encoder, NV_ENC_BUFFER_FORMAT format,
                          int64_t timestamp_100ns, int force_idr,
                          struct am_nvcuda_status *status) {
    NV_ENC_PIC_PARAMS picture;
    memset(&picture, 0, sizeof(picture));
    picture.version = encoder->versions.pic_params;
    picture.inputWidth = encoder->width;
    picture.inputHeight = encoder->height;
    picture.inputPitch = (uint32_t)encoder->input_pitch;
    picture.inputBuffer = encoder->mapped_input;
    picture.outputBitstream = encoder->bitstream;
    picture.bufferFmt = format;
    picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    picture.inputTimeStamp = (uint64_t)timestamp_100ns;
    picture.encodePicFlags =
        force_idr ? (uint32_t)(NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS) : 0u;
    NVENCSTATUS native_status = encoder->functions.nvEncEncodePicture(encoder->session, &picture);
    if (native_status == NV_ENC_ERR_NEED_MORE_INPUT) {
        /* Impossible without B frames or lookahead; one input buffer cannot
         * back a delayed picture, so fail closed instead of reusing it. */
        am_set_status(status, AM_STAGE_ENCODE, (int32_t)native_status);
        return AM_NVCUDA_INVALID_OUTPUT;
    }
    if (native_status != NV_ENC_SUCCESS) {
        am_set_status(status, AM_STAGE_ENCODE, (int32_t)native_status);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    return AM_NVCUDA_OK;
}

/* Synchronous mode: the blocking lock returns the picture just submitted. */
static int lock_output(struct am_nvcuda_encoder *encoder, int64_t timestamp_100ns,
                       const uint8_t **data, size_t *length, uint32_t *picture_type,
                       struct am_nvcuda_status *status) {
    NV_ENC_LOCK_BITSTREAM lock;
    memset(&lock, 0, sizeof(lock));
    lock.version = encoder->versions.lock_bitstream;
    lock.outputBitstream = encoder->bitstream;
    lock.doNotWait = 0;
    NVENCSTATUS native_status = encoder->functions.nvEncLockBitstream(encoder->session, &lock);
    if (native_status != NV_ENC_SUCCESS) {
        am_set_status(status, AM_STAGE_ENCODE, (int32_t)native_status);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    encoder->bitstream_locked = 1;
    if (lock.bitstreamBufferPtr == NULL || lock.bitstreamSizeInBytes == 0 ||
        lock.bitstreamSizeInBytes > AM_NVCUDA_MAX_PACKET_BYTES ||
        lock.outputTimeStamp != (uint64_t)timestamp_100ns) {
        am_set_status(status, AM_STAGE_ENCODE, 0);
        return AM_NVCUDA_INVALID_OUTPUT;
    }
    *data = (const uint8_t *)lock.bitstreamBufferPtr;
    *length = lock.bitstreamSizeInBytes;
    *picture_type = (uint32_t)lock.pictureType;
    return AM_NVCUDA_OK;
}

int amaterasu_nvcuda_release_output(struct am_nvcuda_encoder *encoder,
                                    struct am_nvcuda_status *status) {
    if (encoder == NULL) {
        return AM_NVCUDA_INVALID_ARGUMENT;
    }
    int result = AM_NVCUDA_OK;
    if (encoder->bitstream_locked) {
        NVENCSTATUS native_status =
            encoder->functions.nvEncUnlockBitstream(encoder->session, encoder->bitstream);
        encoder->bitstream_locked = 0;
        if (native_status != NV_ENC_SUCCESS) {
            am_set_status(status, AM_STAGE_ENCODE, (int32_t)native_status);
            result = AM_NVCUDA_NATIVE_FAILURE;
        }
    }
    unmap_input(encoder);
    return result;
}

int amaterasu_nvcuda_encode(struct am_nvcuda_encoder *encoder, int64_t timestamp_100ns,
                            int force_idr, const uint8_t **data, size_t *length,
                            uint32_t *picture_type, struct am_nvcuda_status *status) {
    if (!input_writable(encoder) || timestamp_100ns < 0 || data == NULL || length == NULL ||
        picture_type == NULL) {
        am_set_status(status, AM_STAGE_ENCODE, 0);
        return AM_NVCUDA_INVALID_ARGUMENT;
    }
    *data = NULL;
    *length = 0;
    NV_ENC_BUFFER_FORMAT format = NV_ENC_BUFFER_FORMAT_UNDEFINED;
    int result = map_input(encoder, &format, status);
    if (result == AM_NVCUDA_OK) {
        result = submit_picture(encoder, format, timestamp_100ns, force_idr, status);
    }
    if (result == AM_NVCUDA_OK) {
        result = lock_output(encoder, timestamp_100ns, data, length, picture_type, status);
    }
    if (result != AM_NVCUDA_OK) {
        amaterasu_nvcuda_release_output(encoder, NULL);
        *data = NULL;
        *length = 0;
    }
    return result;
}

/* Every submitted picture was already returned synchronously, so EOS only
 * tells NVENC no further input will arrive; no bitstream follows it. */
int amaterasu_nvcuda_drain(struct am_nvcuda_encoder *encoder, struct am_nvcuda_status *status) {
    if (encoder == NULL || encoder->mapped_input != NULL) {
        return AM_NVCUDA_INVALID_ARGUMENT;
    }
    if (encoder->draining) {
        return AM_NVCUDA_OK;
    }
    NV_ENC_PIC_PARAMS picture;
    memset(&picture, 0, sizeof(picture));
    picture.version = encoder->versions.pic_params;
    picture.encodePicFlags = NV_ENC_PIC_FLAG_EOS;
    NVENCSTATUS native_status = encoder->functions.nvEncEncodePicture(encoder->session, &picture);
    if (native_status != NV_ENC_SUCCESS) {
        am_set_status(status, AM_STAGE_DRAIN, (int32_t)native_status);
        return AM_NVCUDA_NATIVE_FAILURE;
    }
    encoder->draining = 1;
    return AM_NVCUDA_OK;
}

void amaterasu_nvcuda_close(struct am_nvcuda_encoder *encoder) {
    if (encoder == NULL) {
        return;
    }
    if (encoder->session != NULL) {
        amaterasu_nvcuda_release_output(encoder, NULL);
    }
    am_converter_close(&encoder->converter);
    if (encoder->registered_input != NULL) {
        encoder->functions.nvEncUnregisterResource(encoder->session, encoder->registered_input);
    }
    if (encoder->bitstream != NULL) {
        encoder->functions.nvEncDestroyBitstreamBuffer(encoder->session, encoder->bitstream);
    }
    if (encoder->session != NULL) {
        encoder->functions.nvEncDestroyEncoder(encoder->session);
    }
    if (encoder->input != 0) {
        encoder->driver->cuda.mem_free(encoder->input);
    }
    if (encoder->context != NULL) {
        encoder->driver->cuda.ctx_destroy(encoder->context);
    }
    free(encoder);
}

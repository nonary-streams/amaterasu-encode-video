# Video encoder

`amaterasu-encode` provides `amaterasu.encode/1` control and the matching native
`amaterasu_media::EncoderApi` interface. Settings default to `{"enabled":false}`.
Disabled enumeration returns an empty list and opening returns `disabled`.
The default build remains hardware-only on Windows and has no Linux encoder.
Explicit build features can add real Linux hardware NVENC, software x264, or
software x265 HEVC encoding; no generated frames are used. Automatic video selection chooses x264
when the requested color or reference mode is unsupported by Linux NVENC. If a
probed NVENC open later fails with a confirmed availability/configuration error,
automatic selection retries x264 once using a distinct operation ID. Explicit
encoder selections and uncertain opens do not fall back. Linux NVENC is advertised
only after FFmpeg opens a real `h264_nvenc`
session with the supported SDR H.264 settings. Opening an unregistered Linux
hardware identity returns `hardware_encoder_unavailable`.

Control requests are strict JSON; frames and encoded bytes never use JSON:

```json
{"action":"enumerate"}
{"action":"probe","encoder":{"clsid":"amaterasu.software.x264","hardware_url":null,"friendly_name":"x264 software H.264"},"configuration":{"width":1920,"height":1080,"fps":60,"bitrate":15000000,"max_reference_frames":1}}
{"action":"open","encoder":{"clsid":"amaterasu.software.x264","hardware_url":null,"friendly_name":"x264 software H.264"},"operation_id":"video-1","configuration":{"width":1920,"height":1080,"fps":60,"bitrate":15000000,"max_reference_frames":1}}
{"action":"lookup","operation_id":"video-1"}
{"action":"status","session_id":1}
{"action":"close","session_id":1}
```

Enumeration returns `{ok:true,enabled,encoders:[{index,name,codec:"h264"|"hevc"|"av1",hardware:true}]}`.
Backends that report more capability add optional `inputs` (`"cpu"`, `"dmabuf"`),
`max_width` and `max_height` fields; descriptions without them are unchanged.
An index identifies the current encoder enumeration, not a stable GPU ID.
Open/lookup return `{ok:true,session_id,configuration,codec:"h264"|"hevc",max_reference_frames}`. Status adds
`state:"open"|"draining"|"failed"`; close returns `{ok:true,session_id,state:"closed"}`.
Errors contain only `{ok:false,code}`; native driver messages are not exposed.
Enumeration confirms a registered hardware NV12/H.264 transform, not that every
configuration can open. Open negotiates the actual requested configuration.

`probe` validates codec viability with the exact identity and configuration. It
opens a transient backend, submits one owned limited-range NV12 image at the
requested geometry, requires codec-validated H.264/HEVC IDR bytes with parameter sets or an AV1 key temporal unit and
the submitted timestamp, drains, then closes and joins before responding.
It never opens capture, allocates a session ID, retains an operation record, or
returns coded bytes through JSON. Activation and probe calls run outside the
shared session mutex so existing native bindings remain available.

A completed probe returns
`{ok:true,codec:"h264"|"hevc"|"av1",encoder,configuration,usable,settled:true,code,reason}`.
Success has null `code` and `reason`; a safely unusable backend has
`code:"encoder_unusable"` and a bounded static `reason`. Validation and ambiguous
cleanup failures return ordinary `{ok:false,code}` errors, which do not authorize
Auto fallback. One bounded NV12 input and at most sixteen output packets limit
work; output collection has a two-second budget. Vendor calls and synchronous
worker teardown remain non-preemptible. A driver that does not return must keep
the module loaded, as with normal Open/close. Probe success qualifies that
transient configuration, while a later real Open may still fail after hotplug.

The optional operation ID is 1–128 ASCII graphic bytes. Repeating it with the
same caller, encoder identity and configuration returns the original session;
changed inputs conflict. A closed operation returns `session_closed` on open
and `session_not_found` on lookup; it never respawns. Up to 128 operation records
are retained per provider instance. Lookup is immediate-caller scoped and does
not accept an owner field. Cancellation before dispatch prevents opening; failed
delivery of a newly opened session closes it. Cancellation of lookup or a retry
does not close an existing session. Callers reconciling uncertain opens can use
lookup after settling their original runtime request.

At most eight active sessions and two per caller are allowed. Width is even,
16–3840; height is even, 16–2160; frame rate is 1–120 FPS; bitrate is
100,000–100,000,000 bits/second. Hardware may impose narrower limits. The H.264
backend uses Baseline with no B pictures and requested low latency. The optional
HEVC backend is constrained to Main profile, 8-bit 4:2:0, BT.709 limited SDR,
and closed-GOP IDR recovery.
`configuration.max_reference_frames` accepts `0` through `16` and defaults to
`0`. Zero preserves legacy behavior and leaves the encoder's H.264 reference-frame
selection unconstrained; it is omitted when serializing the configuration so its
wire shape remains compatible with older strict consumers. One requires the selected encoder to accept and report
an exact single-reference-frame limit; failure to set or verify it rejects open.
The CUDA H.264 and FFmpeg NVENC backends reject values above one; CUDA HEVC/AV1
and software x264 can honor larger requested reference limits.
The Windows backend verifies the value before media-type commitment, after the
initial input/output types are committed, and after output stream-change
renegotiation. Open and lookup metadata report the selected constraint. This is
an encoder setting, not proof that unqualified hardware emitted a compliant stream.

Native bindings capture the authenticated runtime endpoint principal. Submit,
receive, and drain reject another caller's session IDs, even when known. Provide
and call grants for `amaterasu.encode/1` are required for the respective runtime
endpoints. All native modules remain trusted code; this is not a native sandbox.
The factory table is heap-stable before registration and stays alive until
runtime consumers release their bindings. Binding calls are serialized through
session state; concurrent or reentrant calls return `MEDIA_BUSY`.

Submit copies a validated CPU frame before returning. BGRA8 is converted to
8-bit NV12 using the session's SDR color_mode (bt601_limited, bt601_full,
bt709_limited, or bt709_full); alpha is ignored. The default is bt709_limited
and is omitted from serialized configuration. NV12 input is assumed to use the
session color mode; because its color cannot be verified, non-default modes
reject raw NV12 input. NV12 uses contiguous Y/UV planes with the same stride.
Padding is removed, strides are limited to 16,384 bytes, and dimensions must
match the session. Timestamps and sequence numbers must strictly increase and
fit signed Media Foundation 100ns time after conversion from microseconds.
Sixteen retained input metadata entries bound queued input. Windows MFT submissions
remain synchronous. Software x264 runs on the encoder worker and accepts at most
three outstanding operations (one running and two queued); `MEDIA_BUSY` accepts
nothing, and dropped input retains neither sequence nor timestamp. Close cancels
queued operations after the current native call returns, then joins the worker so
module-owned code cannot be unloaded while native code is running.
`MEDIA_BACKEND_ERROR` makes the session terminal until close, since a hardware
call may already have accepted its sample before failing or timing out.

Owner-scoped `status` reports numeric `timing` counters for BGRA-to-NV12 conversion
and `encoder_timing` counters for worker queue wait, submit/receive/drain duration,
busy submissions, and pending operations. Counters update when the caller polls
status or the media API; no per-frame timing logs or frame content are emitted.

Receive delivers at most one borrowed packet. A rejected sink retains the exact
packet for retry. Samples are normalized to Annex B; IDR is determined from NAL
unit type 5, with SPS/PPS cached and prepended when missing from an IDR packet.
Annex B and four-byte length-prefixed samples are supported. Other AVCC length
widths, malformed samples, and unsupported parameter records fail closed.
Timestamps map packets back to submitted sequence numbers; no B-frame reordering
is supported. Buffers are limited to 16 MiB per packet and sixteen queued packets.

Drain stops new input, requests the selected backend drain, and receives pending
output through the normal callback API before exactly one accepted empty EOS
marker. Rejected EOS is retried. Close discards any undelivered output and shuts
the transform down; callers wanting all output must drain and receive first.
Session resources are released on close or module drop.

## Software x264 selection and distribution

Build with `cargo build -p amaterasu-encode --features software-x264` to opt in.
This uses a small C adapter compiled against system `x264.h`/`x264_config.h` and
loads the matching shared library at runtime. The adapter accesses versioned
structures; Rust never duplicates their layouts. The loader requires the
header's exact `x264_encoder_open_<build>` symbol and all used public functions,
and keeps that library live until the encoder is closed. No x264 library or
FFmpeg library is bundled or linked by the build. Existing locked `cc` and
`libloading` crates provide compilation and native library loading.

Native builds discover headers with `pkg-config --variable=includedir x264`.
If discovery fails, the module still builds and reports headers unavailable.
`AMATERASU_X264_INCLUDE_DIR` explicitly selects target development headers,
including paths containing spaces; an invalid explicit directory fails the
build. Cross builds never use host headers automatically and require that
variable. A C compiler is required when headers are selected. At runtime the
system loader searches versioned platform x264 names and then the usual
unversioned name, always checking the versioned open symbol. Missing or mismatched
libraries are unavailable, with no subprocess encoder fallback.

The installed x264 header identifies its license as GPL-2.0-or-later; upstream
also offers [commercial licensing](https://x264.org/licensing/). Shipping an
x264-enabled integration requires a deliberate compatible licensing and source
distribution policy or a suitable commercial license, required notices, and
platform packaging qualification. Dynamic loading does not remove those
licensing requirements. This change does not choose a repository-wide license,
bundle codec binaries, or authorize an x264-enabled release. Default builds omit
the x264 adapter. See [VideoLAN's x264 project](https://images.videolan.org/developers/x264.html)
and the [public API header](https://github.com/mirror/x264/blob/master/x264.h).

`configuration.sw_preset` and `configuration.sw_tune` are optional exact strings.
Omission resolves to `superfast` and `zerolatency`, respectively, and preserves
the prior wire shape. Presets are bounded to 128 bytes and tunes to 256 bytes,
with no control characters. x264 validates the strings at software Open;
unsupported names return `invalid_software_configuration`. Native numeric
presets and combined tunes accepted by x264 remain valid. Empty tune means no
additional tuning; empty preset is passed unchanged for x264 validation.
The current upstream x264 presets are `ultrafast`, `superfast`, `veryfast`,
`faster`, `fast`, `medium`, `slow`, `slower`, `veryslow`, and `placebo`; its numeric
indices are also accepted. Tunes include `film`, `animation`, `grain`,
`stillimage`, `psnr`, `ssim`, `fastdecode`, and `zerolatency`. Compatible combined
tunes such as `film,zerolatency` are accepted; incompatible combinations fail at
Open. The product editor retains raw strings instead of narrowing them to this
list, since installed codec vocabulary is authoritative.
Hardware sessions preserve these fields as requested metadata without applying
software-only policy, matching the reference's backend-specific behavior.

`configuration.slices_per_frame` is the positive slice count negotiated from
Moonlight's `videoEncoderSlicesPerFrame`; `configuration.min_threads` preserves
the reference signed 32-bit request (default 2). The adapter passes
`max(slices_per_frame, min_threads)` to x264's `i_threads`, enables sliced
threading, and sets `i_slice_count` to the same requested value, matching FFmpeg's
`thread_count`, `FF_THREAD_SLICE`, and `slices` mapping. An opt-in native test
checks the effective parameters and counts H.264 slice NAL units in actual
encoded output. x264 can cap effective thread or slice counts based on frame
dimensions and backend limits. QP defaults to 28 but x264 ABR ignores it; it remains metadata
until Amaterasu has a VAAPI constant-QP consumer.

The adapter calls x264's preset/tune initializer before applying stream limits.
It emits progressive 8-bit limited-range BT.709 Baseline H.264, no B pictures,
Annex B with repeated SPS/PPS and AUD, and a very long IDR interval. Explicit
forced IDR uses x264's IDR picture type and output NAL type is verified by the
shared normalizer. ABR/VBV follows the requested bitrate (whole kbps), with a
one-frame VBV target and the effective negotiated slice-thread count.
Required no-B, zero frame/sync
lookahead, Baseline, color, bitrate and optional reference-frame constraints
take precedence over preset/tune. Their other motion-search, analysis, and
psychovisual choices affect real encoded pictures. Open checks effective
parameters and rejects a maximum delay of sixteen frames or more, keeping the
sixteen-entry native metadata queue live. Frame threading may retain a small
bounded delay even with zerolatency.

Each x264 encode call returns one complete access unit. Output storage is copied
before another native encode can invalidate it. Native 100ns input timestamps
return unchanged with the coded picture; the session retains sequence mapping.
Drain flushes delayed pictures before returning no packet, so existing exact
packet retry, owner checks, packet-boundary flags, and retried EOS remain intact.

Real opt-in tests on Linux have encoded synthetic NV12, independently decoded
every picture with FFmpeg, observed decoded picture differences for changed
presets and tunes, checked forced IDR/timestamps, filled native backpressure,
retried refused packet/EOS, and drained delayed frame-thread output:

```text
cargo test -p amaterasu-encode --features software-x264 software_ -- --ignored
```

These tests require a matching installed x264 runtime; independent decode tests
also require `ffmpeg`. Windows/macOS x264 packaging and runtime builds, live
desktop capture/client playback and sustained performance remain separate
qualification gates. Product v19 now persists and snapshots these tuning fields
through signed host/application/certificate-bound client policy; see the
NVHTTP policy in the `amaterasu-network` repository
(`modules/nvhttp/README.md`, software encoder policy section). Neither
`global:sw_preset` nor `global:sw_tune` is fully verified by this backend alone.

The Windows backend uses official `windows` crate COM bindings. A dedicated
worker thread owns each COM apartment, Media Foundation runtime guard, hardware
activation, transform, event generator and codec interface. Hardware-only
`MFTEnumEx` enumeration is followed by asynchronous unlock, output/input media
type negotiation and streaming notifications. Input waits for NeedInput events;
output is pulled only for HaveOutput events; drain waits for DrainComplete.
A bounded stream-change renegotiation preserves the configured H.264 dimensions.
CodecAPI requests low latency, no B pictures and explicit forced keyframes.
Strict single-reference-frame sessions use the static
`CODECAPI_AVEncVideoMaxNumRefFrame` `VT_UI4` property, which maps to H.264 SPS
`max_num_ref_frames`.

Vibeshine documents `nvenc_preset` with numeric default `1` and choices `1` through
`7` (P1 through P7). The request field is optional and omitted when absent to keep
older wire payloads compatible. This NVENC-only setting applies only when NVENC is
selected. The initial FFmpeg NVENC backend uses a fixed P1 low-latency preset;
supplying this field fails validation with `nvenc_preset_unsupported`, and no
caller-selected preset is applied.

Vibeshine documents `nvenc_h264_cavlc` with a `disabled` default. When H.264 and
NVENC are selected, `enabled` requests CAVLC instead of CABAC; the reference notes
that CAVLC can decode slightly faster in software at roughly 10% more bitrate for
equal quality. Product configuration accepts the `enabled` and `disabled` tokens
through its `bool_f` normalization. This module accepts only a strict JSON boolean
at `configuration.nvenc_h264_cavlc`; the product tokens are not valid at this typed
module boundary. The omitted field defaults to `false` and serializes omitted for
backward compatibility. Explicit `true` is parsed and preserved as data, then
rejected with `nvenc_h264_cavlc_unsupported` during configuration and session
validation and before Windows Media Foundation startup. This module has hardware H.264 MFTs and optional x264,
without NVENC/CAVLC effect, so `false` preserves current MFT
behavior and enabling the option fails closed; NVENC CAVLC support remains pending.
The source semantics are documented in the pinned [Vibeshine configuration
reference](https://github.com/Nonary/vibeshine/blob/457237db7e57f9c4be21c35bb07e28d1c100eb7d/docs/configuration.md#nvenc_h264_cavlc)
and its [`bool_f` configuration mapping](https://github.com/Nonary/vibeshine/blob/457237db7e57f9c4be21c35bb07e28d1c100eb7d/src/config.cpp#L1580).

Vibeshine's stable setting ID `global:nvenc_spatial_aq` defaults to disabled.
Its `bool_f` parser reads the product setting, and NVENC maps it to
`adaptive_quantization`. This module accepts only a strict JSON boolean at
`configuration.nvenc_spatial_aq`; product tokens are not valid at this typed
module boundary. The omitted field defaults to `false` and serializes omitted
for backward compatibility. Explicit `true` is parsed and retained, then
rejected with `nvenc_spatial_aq_unsupported` during configuration and session
validation and before Media Foundation startup. This module has hardware
H.264 MFTs and optional x264, without NVENC spatial AQ effect, so `false` preserves current MFT
behavior and enabling the option fails closed; NVENC spatial AQ support remains
pending. The source behavior is documented in the pinned [Vibeshine
configuration reference](https://github.com/Nonary/vibeshine/blob/457237db7e57f9c4be21c35bb07e28d1c100eb7d/docs/configuration.md#nvenc_spatial_aq)
and [`bool_f` mapping](https://github.com/Nonary/vibeshine/blob/457237db7e57f9c4be21c35bb07e28d1c100eb7d/src/config.cpp).

`configuration.amd_av1_latency_mode` accepts the exact tokens `auto`, `none`,
`power_saving`, `realtime`, and `lowest`. It defaults to `auto`, which is omitted
from serialized configuration for compatibility with older strict consumers.
Every explicit non-`auto` value fails validation with
`amd_av1_latency_mode_unsupported`; the Windows open guard rejects it before
Media Foundation startup. This provider exposes H.264 MFTs and optional x264 and
has no native AMD AMF AV1 backend, so these values are never applied to H.264 or
silently dropped. AMD AMF/AV1 support and the policy effects remain pending.

Buffer alignment comes from transform stream information. Sample, event and
activation references are released on failure paths as well as normal teardown.

Event waits have a two-second deadline and worker replies a three-second wait.
These are not hard deadlines for vendor COM calls. Initialization, enumeration,
and worker join can block if a driver never returns. Detaching code executing
inside an unloadable module would be unsafe, so shutdown waits for its worker.
The host's external supervision is the recovery boundary for a hung driver.
No crash recovery or durable encoder sessions are claimed.

## Linux hardware NVENC

The opt-in `linux-ffmpeg-nvenc` feature uses the system FFmpeg `libavcodec`
`h264_nvenc` encoder and `libavutil`. The build script compiles
`src/platform/ffmpeg_nvenc.c` against target FFmpeg development headers and
links the matching libraries through pkg-config. The encoder artifact embeds the
compiled avcodec/avutil ABI majors; LinuxSession packaging and user-service
preflight require those exact loader sonames and API symbols when this feature
is present. Runtime packaging must provide compatible FFmpeg
libraries and NVIDIA's NVENC runtime/driver. Enumeration opens and closes
throwaway H.264 contexts for both supported BT.709 range modes; if either
fails its SPS/VUI check, NVENC stays out of discovery and software x264 remains
available. This capability probe does not guarantee that every requested
resolution or bitrate will open.

The first backend supports 8-bit NV12 input, H.264 Baseline, no B frames,
BT.709 limited-range and full-range SDR, and a fixed low-latency CBR
configuration. Baseline is explicitly selected through the encoder's private
profile option as well as the codec context; the active reference lists and
decoded-picture buffer are limited to one frame. Both default and one-reference
Moonlight requests are supported, and SPS validation rejects larger reference
requirements. It sets FFmpeg's full-range color metadata for full-range input
and parses the opened encoder's actual SPS VUI to require the requested range
flag plus BT.709 primaries, transfer, and matrix identifiers. Any mismatched or
missing VUI rejects Open, and repeated SPS units in output packets are checked
again before delivery. Other color modes and explicit NVENC
controls fail closed. Frames are copied into FFmpeg-owned NV12 storage; packets
are copied and normalized to Annex B before the borrowed FFmpeg packet is
released. Packet PTS remains in 100-nanosecond units. The worker owns the
FFmpeg context, submit queue is bounded by the existing encoder worker, and
drain flushes delayed packets before session EOS. No CUDA-frame import, HDR,
HEVC, AV1, or dynamic device selection is claimed.

## Linux native NVENC on CUDA

The opt-in `linux-nvenc-cuda` feature adds a native NVENC SDK backend on a CUDA
context for H.264, HEVC and AV1, 8-bit SDR 4:2:0. It is preferred over the
FFmpeg path: enumeration lists it first. FFmpeg is not involved.

Build: `build.rs` compiles `src/platform/nvenc_cuda/native/*.c` against
nv-codec-headers (`nvEncodeAPI.h` plus `dynlink_cuda.h`, which declares the CUDA
driver API, so no CUDA toolkit is needed) and EGL, desktop GL core
(`GL/glcorearb.h`) and GBM headers. Native builds search `/usr/include/ffnvcodec`
and `/usr/include`; `AMATERASU_NVENC_INCLUDE_DIR` and `AMATERASU_EGL_INCLUDE_DIR`
override them and are required for cross builds. An override never falls back to
host headers. Without the headers the feature builds an unavailable stub: nothing
is published and opening one of its identities returns
`nvenc_cuda_headers_unavailable` (`nvenc_cuda_feature_disabled` without the feature).
The adapter links only libc and libdl. At run time it loads `libcuda.so.1`,
`libnvidia-encode.so.1`, `libEGL.so.1` and optionally `libgbm.so.1` with dlopen,
once per process, and never unloads them; a machine without NVIDIA still loads the
module and publishes nothing. Displays are likewise initialized once per GPU and
never terminated, because `eglTerminate` would destroy other sessions' contexts.

Identities: clsid `amaterasu.hardware.nvenc-cuda.h264`, `.hevc` or `.av1`, with
`hardware_url` `pci:<CUDA PCI bus id>`. Enumeration opens a throwaway session on
each CUDA device and publishes one description per codec whose GUID is listed,
accepts NV12 and offers CBR (AV1 normally needs Ada or newer). A GPU is published
only when an EGL display on it can host the converter. Descriptions carry
`vendor_id` 0x10de, `inputs` (`["cpu"]`, plus `"dmabuf"` when that display imports
DMA-BUFs) and `max_width`/`max_height` (the device limit clamped to 3840x2160).
Open locates the bus id again and fails with distinct `nvenc_cuda_*` codes.

NVENC API negotiation offers the compiled header's version, then earlier minor
versions of the same major that the driver supports, with the header's struct
revisions re-stamped; a driver that disagrees answers `NV_ENC_ERR_INVALID_VERSION`
and the next candidate is tried. Drivers below the header's major report
`nvenc_cuda_driver_too_old`. Configuration: P1 preset, ultra-low-latency tuning,
CBR with a one-frame VBV, no B pictures, no lookahead, infinite GOP with IDR only
on request (`FORCEIDR` plus repeated headers), synchronous encode with picture
type decision enabled. H.264 uses Baseline (CAVLC) with a one-frame DPB for
Moonlight's constrained decoders, matching the FFmpeg backend; HEVC and AV1 use
Main and honour `max_reference_frames` (zero leaves the driver default). H.264
and HEVC use one slice and repeat SPS/PPS (and VPS) on IDR; AV1 repeats the
sequence header on key frames with the default tiling, since the configuration
has no slice field. All four `color_mode` values are supported: VUI (or the AV1
colour config) signals BT.709 or SMPTE 170M primaries, transfer and matrix with
the matching range, and centre chroma siting (AV1: unknown, which it cannot express).

Inputs, all converted on the GPU, never by `layout`:

- CPU NV12: packed by the session, copied with `cuMemcpy2D` into one pitched
  CUDA NV12 allocation registered with NVENC as a CUDA device pointer. Non-default
  colour modes still reject raw NV12 as for every backend.
- CPU BGRA8: stride padding is removed and the frame is uploaded to a GL texture.
- DMA-BUF (`EncoderApiV1_1.submit_dmabuf`, advertised at minor 1; v1.0 consumers
  bind the unchanged prefix): accepted for XRGB/ARGB/XBGR/ABGR 8888 and 2101010
  with a modifier the display can bind to `GL_TEXTURE_2D` (or an implicit modifier
  for a listed format), 8-bit sRGB colour, and the session's exact dimensions.
  Other formats, external-only modifiers, `BT2020_PQ` frames and sessions without
  DMA-BUF support return `MEDIA_UNSUPPORTED`, so the producer falls back to CPU
  frames. Submit validates first, returns `MEDIA_BUSY` before duplicating anything
  when the worker queue is full, then duplicates every plane and acquire fence with
  `F_DUPFD_CLOEXEC` and returns `STATUS_OK`; the producer may close its own
  descriptors at once. The worker waits for each acquire fence on the GPU with
  `EGL_ANDROID_native_fence_sync` + `EGL_KHR_wait_sync` (EGL then owns that fence
  descriptor), or else polls it for at most 500 ms (`nvenc_cuda_fence_timeout`).
  It imports the planes with `EGL_EXT_image_dma_buf_import(_modifiers)` into a
  fresh texture per frame (descriptor numbers are not stable identities, so nothing
  is cached), converts, copies, and closes the duplicates before encoding.

The EGL display belongs to the CUDA device: `EGL_EXT_platform_device` on the EGL
device whose `EGL_CUDA_DEVICE_NV` is the encoder's CUdevice, or, when that display
cannot import DMA-BUFs, GBM on the same EGL device's DRM render node. A desktop
OpenGL 3.3 core, surfaceless context is current on the encoder worker. Two
fragment passes draw the source into an R8 luma and an RG8 half-size chroma
texture; both are registered with CUDA once (`cuGraphicsGLRegisterImage`). Each
frame maps them (which orders the GL draws first), copies their arrays into the
NVENC input buffer and synchronizes the context. Luma is the per-pixel matrix
product and chroma the box average of each 2x2 block, like the CPU converter. The
matrix rows are computed in `color.rs`; unit tests replay the shader's f32
arithmetic and match exact arithmetic within one code value, and the CPU
converter within one (two for its legacy truncated BT.709-limited matrix).

Output: one packet per encoded picture, Annex B for H.264/HEVC through the shared
normalizers (IDR from NAL types, parameter sets present), and for AV1 one
low-overhead temporal unit led by a temporal delimiter (inserted if absent), with
size fields required and the cached sequence header inserted on a key frame that
lacks one. NVENC's IDR picture type must agree with the bitstream and forced IDR
must produce a key frame, or the session fails. HEVC and AV1 packets always carry
`FLAG_END_OF_ACCESS_UNIT`. The worker owns the CUDA context, NVENC session, buffers,
EGL context and GL objects; close releases them in reverse order on that thread.

Unqualified: nothing in this backend has run on hardware yet. Bitstream colour
signalling is set directly and checked only by the ignored ffprobe test, not
re-parsed at run time as the FFmpeg backend does. DMA-BUF frames queued behind a
busy worker (at most two) are read when the worker reaches them, so a producer
that re-renders into a queued buffer could be encoded late or torn. Older NVENC
majors, P010/HDR, 4:4:4, slices/tiles and reference-frame invalidation, intra
refresh, external-only modifiers (`GL_TEXTURE_EXTERNAL_OES`), multi-planar YUV
DMA-BUFs and GPU-side fence waits through CUDA are not implemented. The GPU tests
need an NVIDIA GPU with NVENC, `ffmpeg`/`ffprobe` with software decoders, and for
the DMA-BUF case the GPU's render node (`AMATERASU_TEST_RENDER_NODE`, default
`/dev/dri/renderD128`):

```text
cargo test -p amaterasu-encode --features linux-nvenc-cuda nvenc_cuda_gpu -- --ignored --test-threads=1
```

They publish and print the identities, encode BGRA (four colour modes) and NV12
frames per supported codec with two forced IDRs, check packet key frames and
timestamps, ffprobe the stream's profile, references and colour signalling, and
decode it to compare colours with the CPU reference, then repeat the decode check
for a GBM-allocated linear XRGB8888 DMA-BUF submitted through a native session.

## Linux hardware VA-API

The opt-in `linux-ffmpeg-vaapi` feature adds FFmpeg's `h264_vaapi` encoder.
The build script must compile `src/platform/ffmpeg_vaapi.c`, set the checked
cfg `amaterasu_ffmpeg_vaapi`, and link target `libavcodec`/`libavutil` through
pkg-config. FFmpeg's VAAPI hardware-frame implementation uses libva at runtime;
the package must provide compatible FFmpeg sonames, libva, a VAAPI driver and
an accessible render device. The source does not select a device path or change
permissions.

Discovery performs one bounded-size FFmpeg device, frame-pool, and encoder open
at 640x480, then validates the encoder's actual generated H.264 extradata for
Baseline plus BT.709 limited-range VUI before advertising the identity
`amaterasu.hardware.vaapi.h264.ffmpeg` (`ffmpeg://h264_vaapi`). It does not
submit probe frames. This checks that a device and encoder context can be
created, but does not qualify sustained encoding or every requested resolution.
Open repeats the checks at the requested dimensions. Confirmed device, encoder,
or configuration open failures are reported distinctly so auto selection can
retry software x264; explicit selections must not fall back. The video layer
owns that fallback policy.

This initial path accepts CPU NV12 or converted BGRA input as BT.709 limited
SDR, constrained Baseline, CAVLC, no B pictures, and CBR. CPU NV12 is copied to
an FFmpeg-owned frame, transferred into a bounded pool of VAAPI surfaces, and
submitted to FFmpeg. The encoder worker owns device/context/surface references.
H.264 SPS in the opened headers and any later packets are parsed and checked
for video_full_range_flag=0 and BT.709 primaries, transfer, and matrix IDs;
missing or mismatched VUI fails closed. Other color modes, explicit reference
frame limits, HDR, 10-bit formats, and advanced VAAPI controls are unsupported.
The backend is opt-in and has not been qualified on hardware.

## Linux software x264

The optional `software-x264` feature compiles a small C adapter against the
system `x264.h` and dynamically loads a matching x264 runtime. The default build
does not enable the feature. Native builds discover headers through
`pkg-config`; cross builds require `AMATERASU_X264_INCLUDE_DIR`. Missing headers
or runtime libraries leave x264 unavailable, with no subprocess or generated
frame fallback. The Linux path accepts the same owned BGRA/NV12 CPU frames and
returns normalized Annex B H.264 packets through the encoder ABI. Non-default
color modes require BGRA input because raw NV12 has no verified color metadata.
Its preset and
tune are fixed to `superfast` and `zerolatency` for this first integration.
The encoder sets H.264 VUI range and color description to match the selected
mode. BT.601 uses SMPTE 170M identifiers; BT.709 uses BT.709 identifiers.
Software x264 uses the process-visible CPU parallelism for slice threads,
bounded to 2–8 to avoid imposing a fixed two-thread ceiling or creating an
unbounded number of slices.
Linux desktop-stream release artifacts must enable `software-x264` for
`amaterasu-encode` and provide a compatible x264 runtime library. The feature
does not bundle the runtime; the artifact must provide the matching
`libx264.so.<ABI version>` or `libx264.so` to the platform loader.

x264 is GPL-2.0-or-later or commercially licensed. Enabling it for distribution
requires a compatible licensing and source distribution policy or a suitable
commercial license. Dynamic loading does not change those obligations.

The Windows backend remains unchanged. The video pipeline binds PipeWire only
when it is allowlisted, and selection must be completed through the explicit
portal control operations before Open. Missing Linux H.264 support is surfaced
as `h264_encoder_unavailable`.

This implementation is not yet qualified on a Windows GPU, a Linux x264
installation, or Linux NVENC hardware. Host tests exercise
layout, parameter framing, native ownership, backpressure, sink refusal, EOS,
close/drop, strict controls and factory lifetime using a fake encoder backend.
Current host validation covers Linux feature-enabled and default builds. Windows
target compilation remains a separate required gate for this software change. An
ignored Windows-only test encodes synthetic NV12, forces IDR and drains; run it
explicitly on supported hardware before treating this path as usable:

```text
cargo test -p amaterasu-encode hardware_encodes_synthetic_nv12_with_idr_and_drains -- --ignored
```

Successful qualification must additionally decode output with an independent
H.264 decoder, check forced IDR under sustained load, and exercise device removal,
backpressure, cancellation and driver restart. No desktop capture is performed
by these tests.

Behavioral reference review used Vibeshine `src/nvenc/nvenc_config.h` and
`src/nvenc/nvenc_encoded_frame.h` for real encoded frame/IDR expectations and its
larger encoder policy surface. No source was copied or translated. AMD AMF,
Intel-specific controls, GPU texture import, HEVC, AV1,
HDR/10-bit, reference-frame invalidation, rate-control tuning, quality presets,
dynamic resolution, and full streaming parity remain unimplemented.

Primary API references:

- [Hardware MFTs](https://learn.microsoft.com/en-us/windows/win32/medfound/hardware-mfts)
- [Asynchronous MFTs](https://learn.microsoft.com/en-us/windows/win32/medfound/asynchronous-mfts)
- [MFTEnumEx](https://learn.microsoft.com/en-us/windows/win32/api/mfapi/nf-mfapi-mftenumex)
- [H.264 video encoder](https://learn.microsoft.com/en-us/windows/win32/medfound/h-264-video-encoder)
- [CODECAPI_AVEncVideoMaxNumRefFrame](https://learn.microsoft.com/en-us/windows/win32/medfound/codecapi-avencvideomaxnumrefframe)
- [Stream changes](https://learn.microsoft.com/en-us/windows/win32/medfound/handling-stream-changes)
- [Aligned buffers](https://learn.microsoft.com/en-us/windows/win32/api/mfapi/nf-mfapi-mfcreatealignedmemorybuffer)
- [Video subtype contracts](https://learn.microsoft.com/en-us/windows/win32/medfound/video-subtype-guids):
  `MFVideoFormat_H264` samples contain one complete picture. The backend also
  requires progressive output, including after stream changes, so a sample is
  a complete frame. `MFVideoFormat_H264_ES` does not provide this boundary guarantee.

`open` optionally accepts top-level `packet_boundaries: true`. It defaults to
false: existing consumers continue to receive only the original keyframe/EOS
flags. Opted-in sessions additionally mark each complete progressive H.264 output
picture with `FLAG_END_OF_ACCESS_UNIT` (4); EOS is never combined with that flag.
Open, lookup, and status report `packet_boundaries` and
`supports_packet_boundaries: true`. Changing the mode for an existing operation
ID is an `operation_conflict`. The mode does not relax backend output-boundary
verification or promise a stream is ready before open succeeds.
# Linux HEVC software encoding

The optional `linux-x265-hevc` feature enables direct open by the exact identity
`amaterasu.software.linux.x265.hevc` after an 8-bit x265 Main encoder opens and
emits bounded Annex B VPS/SPS/PPS. It is not added to public encoder enumeration,
so automatic selection and advertisement remain disabled pending end-to-end
stream validation.
The backend accepts BT.709 limited or full SDR. BGRA frames are converted to
NV12 with the selected BT.709 range before the shim converts chroma planes for
x265; raw NV12 input is accepted only for the default BT.709 limited mode. The
encoder sets and verifies matching SPS VUI range signaling, progressive 8-bit
4:2:0, and returns one complete Annex B HEVC access unit per media packet. The
packet codec is `CODEC_HEVC`; timestamps remain in 100 ns units. x265 is
configured for a closed GOP, no B frames or lookahead, and repeated VPS/SPS/PPS
before IDR.
The adapter retains bounded parameter sets and supplies them in VPS, SPS, PPS
order before IDR NAL types 19 and 20. CRA and other unsupported random-access
types fail closed.

The feature requires target `x265` development headers and library through
`pkg-config`. Root build integration must compile
`src/platform/x265_hevc.c`, emit checked cfg `amaterasu_x265_hevc`, and link
`x265` only for Linux feature builds. Keep this feature opt-in; it adds an x265
runtime dependency and is GPL-2.0-or-later or commercially licensed. H.264 remains the default codec,
and Windows does not opt into this backend.

Central feature-on validation passed 70 encode and 124 video tests. The real
x265 smoke access unit was identified by ffprobe as Main, 8-bit 4:2:0, BT.709
limited, decoded by FFmpeg, and passed the GameStream fixture using the exact
shim output. Actual Moonlight streaming remains unqualified; keep public
registry advertisement disabled until that path is validated.

The BT.709 full-range path is source-only pending central feature-on and decoder
validation. The real-shim test can write its limited-range access unit when
`AMATERASU_X265_SMOKE_OUTPUT` is set, or its full-range access unit when
`AMATERASU_X265_SMOKE_FULL_OUTPUT` is set; neither file is written by default.

## Scoped hardware policy during integration

`nvenc_settings` retains the resolved host/app/client policy on the control boundary.
Current encoder backends cannot apply those scoped controls, so a nondefault
record returns `nvenc_settings_unsupported` before a worker opens. Auto selection
must not discard the request while falling back to another backend. The default
record preserves the backend's existing behavior: native CUDA NVENC uses P1,
disabled multipass and a one-frame VBV. Implementing the full scoped controls is
still pending. Legacy top-level NVENC preset/CAVLC/AQ requests continue to reject
unsupported selections. Negotiated formats accept 8-bit SDR H.264, HEVC and AV1;
HDR and 10-bit encoding remain unimplemented and fail explicitly.

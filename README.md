# Native H.264 encoder

`amaterasu-encode` provides `amaterasu.encode/1` control and the matching native
`amaterasu_media::EncoderApi` interface. Settings default to `{"enabled":false}`.
Disabled enumeration returns an empty list and opening returns `disabled`.
The default build enumerates Windows hardware MFTs; non-Windows hardware opens
return `unsupported_platform`. The opt-in `software-x264` feature adds genuine
CPU encoding when system x264 prerequisites are present. Selection is explicit;
hardware failure never silently substitutes software or generated frames.

Control requests are strict JSON; frames and encoded bytes never use JSON:

```json
{"action":"enumerate"}
{"action":"open","encoder":{"clsid":"amaterasu.software.x264","hardware_url":null,"friendly_name":"x264 software H.264"},"operation_id":"video-1","configuration":{"width":1920,"height":1080,"fps":60,"bitrate":15000000,"max_reference_frames":1}}
{"action":"lookup","operation_id":"video-1"}
{"action":"status","session_id":1}
{"action":"close","session_id":1}
```

Enumeration returns descriptions with `identity`, `name`, `codec`, `hardware`,
and optional `vendor_id`. Hardware selection retains the registered MFT
identity. Available x264 is appended with `hardware:false`, `vendor_id:null`,
and the exact identity above. The response additionally reports
`software_encoder:{available,reason}`. Missing feature, target headers, or
version-compatible runtime produce `x264_feature_disabled`,
`x264_headers_unavailable`, or `x264_runtime_unavailable`; the software identity
fails Open with that code. Registered codec availability does not guarantee
configuration Open; opening validates actual codec parameters and resources.
Open/lookup return `{ok:true,session_id,configuration,codec:"h264",max_reference_frames}`. Status adds
`state:"open"|"draining"|"failed"`; close returns `{ok:true,session_id,state:"closed"}`.
Errors contain only `{ok:false,code}`; native driver messages are not exposed.
Enumeration confirms a registered hardware NV12/H.264 transform, not that every
configuration can open. Open negotiates the actual requested configuration.

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
100,000–100,000,000 bits/second. Hardware may impose narrower limits. H.264
Baseline with no B pictures and requested low latency is the only configuration.
`configuration.max_reference_frames` accepts only `0` or `1` and defaults to
`0`. Zero preserves legacy behavior and leaves the encoder's H.264 reference-frame
selection unconstrained; it is omitted when serializing the configuration so its
wire shape remains compatible with older strict consumers. One requires the selected encoder to accept and report
an exact single-reference-frame limit; failure to set or verify it rejects open.
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
8-bit limited-range BT.709 NV12, ignoring alpha; NV12 input is assumed to use that
same color interpretation. NV12 uses contiguous Y/UV planes with the same stride.
Padding is removed, strides are limited to 16,384 bytes, and dimensions must
match the session. Timestamps and sequence numbers must strictly increase and
fit signed Media Foundation 100ns time after conversion from microseconds.
Sixteen retained input metadata entries bound queued input; `MEDIA_BUSY` accepts
nothing. `MEDIA_BACKEND_ERROR` makes the session terminal until close, since a
hardware call may already have accepted its sample before failing or timing out.

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

The adapter calls x264's preset/tune initializer before applying stream limits.
It emits progressive 8-bit limited-range BT.709 Baseline H.264, no B pictures,
Annex B with repeated SPS/PPS and AUD, and a very long IDR interval. Explicit
forced IDR uses x264's IDR picture type and output NAL type is verified by the
shared normalizer. ABR/VBV follows the requested bitrate (whole kbps), with a
one-frame VBV target and two encoder threads. Required no-B, zero frame/sync
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
[NVHTTP policy](../nvhttp/README.md#software-encoder-policy). Neither
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
selected. This module has hardware MFT and optional x264 backends, without native NVENC,
so supplying the field fails validation with
`nvenc_preset_unsupported`; no preset is applied here. NVENC preset support remains
pending.

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

This implementation is not yet qualified on a Windows GPU. Host tests exercise
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
larger encoder policy surface. No source was copied or translated. Native NVENC,
AMD AMF, Intel-specific controls, Linux hardware backends, GPU texture import,
HEVC, AV1, HDR/10-bit, reference-frame invalidation, extended rate-control tuning,
hardware quality presets,
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

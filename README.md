# H.264 encoder

`amaterasu-encode` provides `amaterasu.encode/1` control and the matching native
`amaterasu_media::EncoderApi` interface. Settings default to `{"enabled":false}`.
Disabled enumeration returns an empty list and opening returns `disabled`.
The default build remains hardware-only on Windows and has no Linux encoder.
An explicit build feature can add real software x264 encoding; no generated
frames or automatic hardware-to-software fallback are used.

Control requests are strict JSON; frames and encoded bytes never use JSON:

```json
{"action":"enumerate"}
{"action":"open","encoder":0,"operation_id":"video-1","configuration":{"width":1920,"height":1080,"fps":60,"bitrate":15000000,"max_reference_frames":1}}
{"action":"lookup","operation_id":"video-1"}
{"action":"status","session_id":1}
{"action":"close","session_id":1}
```

Enumeration returns `{ok:true,enabled,encoders:[{index,name,codec:"h264",hardware:true}]}`.
An index identifies the current Media Foundation enumeration, not a stable GPU ID.
Open/lookup return `{ok:true,session_id,configuration,codec:"h264",max_reference_frames}`. Status adds
`state:"open"|"draining"|"failed"`; close returns `{ok:true,session_id,state:"closed"}`.
Errors contain only `{ok:false,code}`; native driver messages are not exposed.
Enumeration confirms a registered hardware NV12/H.264 transform, not that every
configuration can open. Open negotiates the actual requested configuration.

The optional operation ID is 1–128 ASCII graphic bytes. Repeating it with the
same caller, encoder index and configuration returns the original session;
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
8-bit NV12 using the session's SDR color_mode (bt601_limited, bt601_full,
bt709_limited, or bt709_full); alpha is ignored. The default is bt709_limited
and is omitted from serialized configuration. NV12 input is assumed to use the
session color mode; because its color cannot be verified, non-default modes
reject raw NV12 input. NV12 uses contiguous Y/UV planes with the same stride.
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

Drain stops new input, requests Media Foundation drain, and receives pending
output through the normal callback API before exactly one accepted empty EOS
marker. Rejected EOS is retried. Close discards any undelivered output and shuts
the transform down; callers wanting all output must drain and receive first.
Session resources are released on close or module drop.

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
selected. This module currently has only a Media Foundation Transform (MFT)
backend and no native NVENC backend, so supplying the field fails validation with
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
validation and before Windows Media Foundation startup. This module has only
hardware H.264 MFTs and no NVENC/CAVLC effect, so `false` preserves current MFT
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
validation and before Media Foundation startup. This module has only hardware
H.264 MFTs and no NVENC spatial AQ effect, so `false` preserves current MFT
behavior and enabling the option fails closed; NVENC spatial AQ support remains
pending. The source behavior is documented in the pinned [Vibeshine
configuration reference](https://github.com/Nonary/vibeshine/blob/457237db7e57f9c4be21c35bb07e28d1c100eb7d/docs/configuration.md#nvenc_spatial_aq)
and [`bool_f` mapping](https://github.com/Nonary/vibeshine/blob/457237db7e57f9c4be21c35bb07e28d1c100eb7d/src/config.cpp).

`configuration.amd_av1_latency_mode` accepts the exact tokens `auto`, `none`,
`power_saving`, `realtime`, and `lowest`. It defaults to `auto`, which is omitted
from serialized configuration for compatibility with older strict consumers.
Every explicit non-`auto` value fails validation with
`amd_av1_latency_mode_unsupported`; the Windows open guard rejects it before
Media Foundation startup. This provider currently exposes only H.264 MFTs and
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

This implementation is not yet qualified on a Windows GPU or a Linux x264
installation. Host tests exercise
layout, parameter framing, native ownership, backpressure, sink refusal, EOS,
close/drop, strict controls and factory lifetime using a fake encoder backend.
Windows and Linux target Clippy builds validate conditional compilation. An
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
AMD AMF, Intel-specific controls, Linux backends, GPU texture import, HEVC, AV1,
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

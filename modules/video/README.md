# Managed video pipeline

`amaterasu_video` provides control and native service `amaterasu.video/1`. Product
coordination stays in this independently built library. It opens an allowlisted capture
stream and H.264 encoder session asynchronously, then moves borrowed
CPU frames directly into the encoder's copying submit API. Encoded packets reach
consumers through the native `VideoApi`; frame/packet bytes never enter JSON.

Linux settings default to:

```json
{"enabled":false,"capture_providers":["pipewire","vkms"],"drain_timeout_ms":5000}
```

Linux defaults to the finite `pipewire` provider,
`amaterasu.capture-pipewire/1`, plus the optional `vkms` provider,
`amaterasu.capture-vkms/1` (see "Owned virtual outputs and DMA-BUF frames");
other platforms retain the `dxgi` default.
`wgc` maps to `amaterasu.capture-wgc/1`. An empty allowlist, unknown
identifier, or drain timeout outside 100–30000 ms is rejected.
Enabled startup binds every configured native capture provider and the encoder.
Disabled startup registers services but binds no lower interface and opens no
devices. Unsupported hardware/platforms fail through real provider responses;
there are no synthetic frames. Linux H.264 requires a registered real backend.

### Host-configured X11 capture service replacement

Trusted host-owned video module settings may set `"x11_capture_service":
"example.capture-test/1"` with `"x11"` in `capture_providers`. Without this
field, X11 still binds `amaterasu.capture-x11/1`. The external service name must
be a bounded lower-case namespace ending in `/1`; built-in `amaterasu.*`
services and overrides for a disabled X11 slot are rejected. This is a single
exact replacement for the X11 slot, not a dynamically discovered registry or
an override for every capture provider. There is no fallback to the built-in
X11 service if its explicitly selected replacement cannot bind.

The signed package must include the provider component and its dependency
before video, request that provider's `native_provide` for this exact service,
and request video's `call` and `native_use` for the same service. The host-owned
bootstrap policy must also allow these exact grants; runtime uses their
intersection. Changing the host-owned service selection among already granted
services does not change the signed package bytes. The external
module must implement the X11-slot control protocol (`enumerate`, `open`,
`lookup`, `close`) as well as the native `CaptureApi` table: both paths route to
the same exact service throughout teardown. The runtime rejects missing or
duplicate registrations and checks the live `CaptureApi` v1.0 ABI prefix at
bind time. Merely naming a service in settings neither grants it nor proves its
capabilities. Provider option schemas can use the existing caller-scoped
configuration service; this setting does not add browser-editable options or
change the running package. Capture callbacks retain their current borrowed
CPU/DMA-BUF lifetime and tick-thread contract. No live replacement is enabled
by the default LinuxSession profile.

### Host-configured encoder service replacement

Trusted video settings may select one exact `"encoder_service":
"example.encode-test/1"`. Without it, Video uses `amaterasu.encode/1`.
The external service must implement the existing encoder control operations
(`enumerate`, `probe`, `open`, `lookup`, `close`) and the native `EncoderApi`
v1.0 interface. Enumeration, open, native submit/receive, recovery and close
all bind to the same selected service. A failed explicit binding does not
fall back to the built-in encoder. The name is a bounded external `/1`
namespace; built-in `amaterasu.*` names are rejected as overrides.

The signed package and trusted host policy must authorize Video's exact
`call` and `native_use` grants for that provider, and retain the provider
component through Video teardown. The setting grants no capability by itself.
The current development composer has an external capture/processor fixture;
it does not yet package an external encoder. No production profile or live
encoder selection is changed here.

## Optional video lifecycle metadata observations

Trusted host-owned video settings may opt in with `"capture_observation":true` for
capture facts and `"frame_observation":true` for accepted frame/encode facts;
both default to false. The signed component grant request and trusted host
policy must also allow video `publish` on the exact corresponding topics
`amaterasu.video.capture-observation/1` and
`amaterasu.video.frame-observation/1`. Without a grant the first rejected
publication disables only that source for this module instance. No default
policy grants either topic. An authorized subscriber may request the broker's bounded,
coalescing event queue. Broker publisher sequence/resync detects losses after a
publish reaches the broker. Local observer queue drops are not reported to
subscribers, and `event_id` spans both video topics, so its gaps are not a
per-topic loss signal. Neither sequence proves delivery of every frame.

Video records versioned `before_capture` and `after_capture` **observations** of
one due native read. It publishes their small metadata envelopes on the module
tick only after native frame callbacks return. The before marker records the
point before `Plane::pump` starts; its delivery is deferred, so it cannot veto
or transform that capture. If a plane fails or never reaches a native read,
both markers are omitted because callback progress is uncertain. At most eight
capture attempts queue locally and one batch (at most six broker sends) is
drained per tick. Full queues drop new metadata;
broker failure never fails the stream. Stop/unload discards pending metadata.
Disabled operation does not allocate events, serialize payloads, or call the
broker. These events include only geometry and a CPU-borrowed/GPU-opaque label:
no frame pixels, DMA-BUF descriptors, process paths or client identifiers.

The separate frame topic emits `before_frame`, `before_encode`, and
`after_frame` only for an input frame definitively accepted by the encoder.
Busy, rejected, or uncertain submits do not produce a false accepted-frame
sequence. CPU capture, owned conversion/repeat frames, and DMA-BUF input carry
the same stream timestamp (converted from microseconds to 100 ns units) and a
pixel-format label when the source ABI supplies one. DMA-BUF fourcc is not a
CPU pixel-format code, so its label is `unknown` and its memory is `gpu_opaque`.
`after_encode` marks one encoded packet admitted to video's bounded output
queue, including packets returned during drain. It may arrive much later than
its input, and a frame may produce multiple packet fragments. It is **not** a
whole-frame completion event. Only codec, packet byte count, keyframe flag and
timestamp are published; packet bytes remain in the native media queue. All
these markers are delivered after native callbacks return and cannot modify
the frame or packet.

### Explicit trusted CPU processor

Trusted host-owned settings may name one exact external native service with
`"cpu_processor_service":"example.video-effects/1"` and set an optional
`"cpu_processor_budget_us"` from 100 through 5000 (default 2000). The setting
is absent by default, and a signed `native_use` grant plus a provider registered
under the same service are required. Binding failure rejects enabled startup;
it never falls back to a different processor. This is one selected service,
not ordered multi-extension chaining or a general UI option grant.

For borrowed CPU capture, Video invokes the processor's v1 `BEFORE_FRAME`
stage before optional built-in resize, normalizes to the stream timestamp and
sequence, then invokes `BEFORE_ENCODE` before `EncoderApi.submit`. Either stage
may pass, drop that input, or supply one bounded copied CPU replacement with
identical shape and metadata. Only an accepted encoder submit updates the
repeat cache. With a selected processor it owns the original captured CPU
pixels, so each repeat runs both stages once from the same unmodified source,
including resize between stages. Without a processor it retains the accepted
encoder input as before. Busy or dropped submits leave the cache untouched.
Repeats use a new timestamp without modifying that cached source.
`AFTER_ENCODE` observes a non-EOS encoded
packet before video takes its bounded queue lock; v1 cannot replace or drop
packet bytes. Backpressure can cause the encoder to offer the same packet
again, so the processor may observe it more than once. Failed, malformed or
over-budget calls fail the stream closed and release owned resources through
normal cleanup. A call is timed **after it returns**: native code cannot be
preempted, and unload waits for it to return.

The selected processor requires CPU memory. Owned-output selection uses the
existing PipeWire CPU fallback; an explicitly pinned GPU-only vkms stream is
rejected before opening capture resources. The opt-in processor bypasses
video's optional asynchronous PipeWire color-conversion worker to keep one
serialized native binding on the video tick thread. That can increase callback
latency and CPU work. No processor or callback is invoked on the default path,
and this service does not expose GPU buffers or DMA-BUF descriptors.

SDK packet/network and input hook points are not wired by this video module.
Encoder submit occurs inside a borrowed native capture callback. The explicitly
selected, trusted CPU processor runs synchronously within that lifetime, and
its replacement bytes are copied into Video-owned memory before the callback
returns. Arbitrary asynchronous or untrusted callbacks cannot retain the
borrowed buffer or satisfy this cadence contract. This is not a GPU zero-copy
extension API.

The optional x264 fallback requires the `software-x264` feature, x264 development
headers, and a matching runtime library. If no H.264 backend is available, Open
fails with `h264_encoder_unavailable` after resource cleanup.

For `encoder: "auto"`, video can select a registered NVENC or VAAPI H.264
backend before x264. A confirmed hardware Open failure can retry x264 once with
a distinct operation ID. If an automatically selected hardware encoder later
returns a terminal `encoder_failed` or `frame_rejected` during streaming, video
closes that encoder session and can restart once with x264 while retaining the
capture stream and output queue. The replacement's first input is forced to a
keyframe. Explicit encoder identities and registry indices do not fall back;
uncertain requests and failures after the single retry clean up the stream.

## Encoder selection and configuration

The `encoder` selector accepts a complete enumeration identity, an unsigned
registry ordinal, `"auto"`, or `"software"`. Selection preserves complete
identities, including the hardware URL. Auto ranks supported hardware first,
preferring DMA-BUF input for an owned output whose capture interface is ready,
then the registered software encoder for the selected codec. Each candidate is
probed with the frozen configuration before capture allocation. Only a correlated,
settled unusable reply permits the next probe; cancellation, an uncertain request,
a malformed reply, and failed result release prevent capture allocation. All
selection work shares a five-second deadline. Status reports the requested
selection, the resolved identity, probe attempts, and the last negative reason.
Explicit identities and ordinals never substitute another encoder; `"software"`
restricts selection to registered software and retains x264 unavailable reasons.

`configuration.video_stream_format` carries the negotiated codec, bit depth,
and dynamic range. Legacy `configuration.codec` remains accepted; video
synchronizes both representations before selection and rejects contradictory
non-default codecs. Current backends accept only 8-bit SDR. HEVC and AV1 remain
Linux-only. Software preset and tune strings pass through unchanged, with limits
of 128 and 256 bytes and no control characters. Slice count, minimum threads,
QP, and NVENC controls remain part of the frozen configuration; each encoder
validates and applies the controls it supports. Current encoders reject any
non-default nested `nvenc_settings` with `nvenc_settings_unsupported` before
capture allocation; preset, multipass and other scoped policy fields remain
persisted but are not implemented in these backends. Default policy retains the
existing native P1 encoding and disabled native multipass behavior.

Explicit `x11` and `kms` providers require their signed allowlist entries and
validate the exact selected output against a fresh enumeration before opening.
They never substitute another provider. Physical KMS capture remains a separate
path from owned-output VKMS DMA-BUF capture.

## Owned virtual outputs and DMA-BUF frames

An Open with provider `pipewire` (explicit or default) for an owned KWin output
`kwin:Virtual-N` tries providers in the order `vkms`, then `pipewire`. N is the
DRM connector type id: any positive id without leading zeros, so connectors
renamed `Virtual-9`..`Virtual-16` by a driver reload are accepted. Such outputs
never need a portal selection.

Video chooses `vkms` only when all of these hold, checked once before the first
capture Open:

- `vkms` is in `capture_providers` and its native binding succeeded. That binding
  is optional: a failed startup bind leaves the provider unbound for this module
  instance instead of failing startup;
- the vkms table advertises capture minor 4 and is at least
  `CAPTURE_API_V1_4_SIZE` bytes, and the encoder table advertises minor 1 and is
  at least `ENCODER_API_V1_1_SIZE` bytes. Video binds both at v1.0 and reads a
  tail only after both checks pass;
- the selected encoder's enumerate description lists `"dmabuf"` in its optional
  `inputs` array (absent or malformed means CPU input only). For `encoder: "auto"`
  on such a stream, a supported hardware encoder that accepts DMA-BUF is ranked
  ahead of other hardware. An explicit encoder identity skips enumeration, so its
  inputs are unknown and the stream keeps PipeWire.

On the DMA-BUF path each due capture calls `read_dmabuf_frame` and submits the
borrowed frame through `submit_dmabuf` inside the capture callback, carrying video's
stream timestamp, sequence and force-IDR flag. Video performs no CPU resize,
conversion or copy for that stream, and the frame must already match the stream
size. Encoder `MEDIA_BUSY` drops the presentation like a busy CPU frame. Repeats
re-read the latest presentation, which the provider may satisfy with cached reuse.
Status counts delivered presentations in `dmabuf_frames.fresh` and
`dmabuf_frames.reused`.

Pacing on the DMA-BUF path depends on the vkms table. A table that advertises
capture minor 5 and is at least `CAPTURE_API_V1_5_SIZE` bytes is paced by
presentations; a smaller or older table keeps the PipeWire periodic 1/fps timer.
Presentation pacing works as follows on every module tick:

- video calls the non-blocking `latest_presentation`. A sequence newer than the
  last delivered presentation is read and submitted on that tick when the
  frame-rate cap admits it, without waiting for a 1/fps boundary;
- the cap keeps submits on a 1/fps grid of the negotiated client rate, so the
  sustained rate never exceeds it. Each submit claims the next grid slot, which
  absorbs host-tick jitter without drifting. A submit late by more than half an
  interval admits the next one no sooner than half an interval later, and a gap
  of a whole interval past a slot re-anchors the grid instead of catching up;
- presentations that arrive while the cap holds coalesce: only the newest is
  read when the cap reopens, and each superseded one counts as coalesced;
- with nothing new, only the existing keepalive rule submits: a cached-reuse
  repeat at the `minimum_fps_target` deadline (zero means the negotiated FPS),
  also bounded by the cap. A pending `force_idr` rides on the next fresh push or,
  without one, on a repeat as soon as the cap admits it;
- until the first frame is accepted, the 1/fps timer also probes the provider,
  so a source that has not presented yet still produces a first frame;
- encoder backpressure drops a delivered presentation like a busy CPU frame; the
  next presentation or keepalive repeat replaces it;
- fresh pushes are stamped from the presentation timestamp, mapped onto the
  stream clock, so variable (VRR) cadence survives. A provider timestamp of
  zero, one that regresses, one more than a frame ahead of its observation, or
  one more than 200 ms behind re-anchors the mapping. Output timestamps stay
  strictly monotonic; repeats use the stream clock;
- `MEDIA_UNSUPPORTED` from the query returns the stream to timer pacing, and any
  other query failure is a capture failure.

Status reports `pacing` (`presentation` or `timer`, for every stream) and
`presentation_pacing` with `fresh_pushes`, `repeats` and `coalesced` counters.
CPU (PipeWire, WGC, DXGI) streams never query presentations and keep their
existing pacing.

Fallbacks never fail a stream that PipeWire can still serve:

- when a condition above fails, the stream opens PipeWire and reports the reason
  (`capture_vkms_not_configured`, `capture_provider_unbound`,
  `capture_dmabuf_interface_unavailable`, `encoder_dmabuf_interface_unavailable`,
  `encoder_dmabuf_input_unavailable` or `encoder_inputs_unknown`);
- a definite vkms refusal, either an admission error or an `ok:false` Open reply
  such as `frame_helper_unavailable`, `permission_denied` or `driver_unavailable`,
  reopens the same output through PipeWire with a fresh operation ID and records
  the provider's code. Lost or uncertain vkms replies keep the normal settle and
  cleanup path, because they may have opened a stream;
- `MEDIA_UNSUPPORTED` from `read_dmabuf_frame` or `submit_dmabuf`, or a frame
  size mismatch, switches a running stream to CPU capture once: video closes the
  vkms capture (stage `cpu-fallback-close-capture`), opens PipeWire for the same
  output, keeps the encoder session and packet queue, and forces a keyframe on
  the first CPU frame.

Status for these streams adds `frame_path` (`dmabuf` or `cpu`, reported for every
stream), `vkms_fallback_reason` and `dmabuf_frames`; `capture_provider` names the
provider that actually opened. An explicit `capture_provider: "vkms"` is pinned:
an unmet condition or a runtime `MEDIA_UNSUPPORTED` fails the stream with that
reason instead of falling back.

Packaging must add `"vkms"` to `capture_providers` and grant video `call` and
`native_use` for `amaterasu.capture-vkms/1`, with the vkms module started before
video so the startup binding can succeed. Without these, owned outputs keep
using PipeWire and report the reason. Real KWin/vkms and NVENC DMA-BUF import
remain unqualified on hardware.

Configuration `codec` may be `h264`, `hevc` or `av1` on Linux. AV1 requires a
registered hardware encoder such as `amaterasu.hardware.nvenc-cuda.av1`; there is
no software AV1 encoder and no x264 fallback, so a missing backend fails Open with
`av1_encoder_unavailable`. Native NVENC identities
`amaterasu.hardware.nvenc-cuda.<codec>` are auto-selected only for their own
codec and supported SDR color modes. H.264 accepts at most one reference frame;
HEVC and AV1 accept the backend-supported multiple-reference configuration.

## API

```json
{"action":"select_portal_source","source_type":"monitor","operation_id":"desktop-1"}
{"action":"poll_portal_selection","operation_id":"desktop-1"}
{"action":"cancel_portal_selection","operation_id":"desktop-1"}
{"action":"open","capture_provider":"pipewire","output":"portal-selected-session-source","encoder":{"clsid":"amaterasu.software.x264","friendly_name":"x264 software H.264","hardware_url":null},"configuration":{"width":1920,"height":1080,"fps":60,"bitrate":5000000,"color_mode":"bt709_limited"}}
{"action":"open","output":"<capture enumerate output id>","encoder":0,"minimum_fps_target":20,"configuration":{"width":1920,"height":1080,"fps":60,"bitrate":5000000}}
{"action":"status"}
{"action":"status","stream":1}
{"action":"force_idr","stream":1}
{"action":"drain","stream":1}
{"action":"close","stream":1}
```

PipeWire sources retain the compositor's negotiated image size. When it differs
from the stream size, video fits the complete BGRA image with bilinear resizing
and centered black bars when needed to preserve its aspect ratio. This does not
change monitor positions, resolution or scaling, and does not grant absolute
pointer mapping without authoritative capture geometry.

The Linux `linux-ffmpeg-convert` feature moves default BT.709-limited portal
resizing and color conversion to a joined worker using system libswscale.
Only one frame may be outstanding, so slow conversion cannot build an input-loop
stall or an unbounded queue. The worker returns fitted NV12 pixels, preserving the
encoder's existing range contract. Other color modes retain the BGRA path.
The video artifact embeds its required swscale/avutil ABI majors; Linux packaging
and user-service preflight check those exact system loader dependencies.

The optional configuration.color_mode selects the SDR matrix/range used for
BGRA conversion and encoder signaling: bt601_limited, bt601_full,
bt709_limited, or bt709_full. It defaults to bt709_limited. Windows continues to
support only that default; non-default color modes are rejected.

Open optionally accepts the canonical `"capture"` mode `"ddx"`, `"wgc"`,
`"wgcc"`, `"auto"`, or `""`. Explicit `ddx`, `wgc`, and `wgcc` modes are pinned
to their provider. Empty and `auto` preserve the bootstrap provider selected by
`capture_provider` as the first choice, defaulting to `pipewire` on Linux and
`dxgi` elsewhere, and may retry once
with the other provider only after a clean `unsupported_platform` provider reply
or clean `unavailable` admission rejection. The alternate must be in the startup
allowlist. Video translates provider-specific output IDs through their shared
Windows device name and opens the alternate only when its enumeration has one
attached exact match; missing or ambiguous matches retain the original failure.
PipeWire has no alternate provider and never falls back to KMS.
For paged enumeration, video follows every `next_offset` before opening the
alternate and checks that each page reports the same full-list
`enumeration_id`. A malformed cursor, changed revision, or duplicate match
retains the original provider failure. Older unpaged `outputs` replies remain
valid. Each page is a fresh device read, so hotplug after enumeration still
requires a fresh Open attempt.
Uncertain requests, permission errors, capacity failures, and other backend
errors do not trigger fallback. Status keeps the requested output ID and reports
the provider that actually opened the stream. Legacy requests that specify only
`capture_provider` remain pinned. `wgcc` implies WGC constant-rate delivery and
is pinned; auto and empty modes never change rate semantics during fallback. For
WGC only, `"constant_rate":true` asks capture to return the latest image at
each scheduled read when no new frame is ready. Video's configured FPS sets the
read cadence; the WGC module does not busy-wait or create a timer. The default is
event-driven WGC delivery. Product-level `capture = wgcc` routing through NVHTTP,
RTSP, and stream-session is still pending.

Linux PipeWire capture requires `pipewire` in the startup provider allowlist.
An authenticated management client calls `/v1/capture/select_portal_source`
with `source_type` `monitor` or `window`, then polls
`/v1/capture/poll_portal_selection` using the same caller-owned operation ID until the
selection state is `ready`. Open the profile's
`portal-selected-session-source` sentinel; video resolves it to the current
management-selected output. Portal control operation IDs remain scoped to the
principal that started them, while the one current selected output can be used by
an authorized streaming caller without passing the operation ID to that caller.
Direct opaque output IDs are accepted only when they match the current ready
selection. Startup and Open never launch a chooser. Cancel with
`/v1/capture/cancel_portal_selection`. Public selection replies omit PipeWire node handles
and replace the provider's internal operation ID with the caller's operation ID.
Permission denial, cancellation, provider unavailability, and revocation fail
closed; there is no KMS or synthetic fallback.

Video Open accepts `"wgc_pacing_smoothing"`, defaulting to `true`, to snapshot
the global Windows policy. On WGC streams, a fresh frame after a missed capture
slot reuses the prior cadence phase when the projected phase error is at most
`min(2 ms, frame interval / 4)` and the projected hold is at most 200 ms.
Encoder backpressure after a fresh capture does not count as a miss. The setting
does not affect minimum-FPS repeats.

Constant-rate WGC streams use arrival pacing when the provider exposes the v1.2
freshness callback or newer. A v1.2 provider uses the frame timestamp; v1.3
also supplies the WGC acquisition timestamp separately. Cached-frame reuse
advances an established output grid without counting as a fresh arrival or
clearing a pending re-anchor. Older capture tables cannot distinguish cached
reuse from a fresh frame, so constant-rate streams retain their configured read
cadence without arrival-based re-anchoring. This pump is nonblocking, so it
does not reproduce Vibeshine's 200 ms blocking snapshot after a pacing bust.
If a source timestamp drifts more than 200 ms from its local callback observation,
video re-anchors to the callback time so a clock jump cannot stall future reads.

Open completes after both lower resources open, returning
`{"ok":true,"stream":1,"state":"running",...}`. This state means the pipeline has
resources, not that a frame, packet, client connection or full stream is ready.
Dimensions are checked again against each actual captured frame; a mismatch starts
cleanup. No scaling or output topology change is performed.

Capture Open may also return a provider-verified `touch_surface` with nonzero
`width`, `height`, `logical_width`, and `logical_height` (all unsigned 32-bit
integers). Video rejects a malformed surface before encoder Open. When its
source width and height exactly match both the requested stream dimensions and
the output rectangle in valid `desktop_geometry` from the same capture Open, video
snapshots and returns `touch_port` in Open, lookup, and status responses: stream
`width` and `height`, the provider's logical dimensions, and zero
`client_offset_x` and `client_offset_y`. The mapping stays fixed for the stream.
Without either field, or when the source size differs from the stream or output
rectangle size, video
omits `touch_port`. It never infers a touch mapping from `desktop_geometry` or
reports letterboxing that this pipeline does not produce.

When the capture provider returns `desktop_geometry`, video strictly parses and
retains the captured output and virtual desktop rectangles. Before opening the
encoder, it requires the provider's native geometry callback to return the exact
same validated snapshot. The snapshot is included in open, lookup, status, and
retained failure metadata. A malformed control snapshot, missing native support,
or initial mismatch fails startup and closes the owned capture stream.

Older capture v1.0 providers remain supported. Video binds only the 40-byte v1.0
capture prefix and reads the optional v1.1 geometry callback only when the native
header advertises both minor version 1 and the complete table size. Such providers
must omit `desktop_geometry`; its absence keeps authoritative absolute-pointer
mapping disabled while relative input and video continue to work.

Status returns only the caller's active records and up to four recent failures
(shared bounded history across callers), with configuration, output, selected
capture provider, state, `dropped_frames`, `force_idr_pending`, and error. Recent
failures persist after successful resource cleanup with state `failed`, allowing
timeout/backend errors to remain visible. History is in-memory and may be evicted. Stream IDs and ownership last only for this module
instance. At most two streams exist across all callers, matching the encoder's
per-owner limit. Dimensions must be even, 16–3840 by 16–2160; fps 1–120; bitrate
100000–100000000. Output IDs are nonempty, at most 512 UTF-8 bytes, without control
characters. Unknown JSON fields are rejected.

`configuration.max_reference_frames` accepts only `0` or `1` and defaults to
`0`. Zero preserves the existing unconstrained encoder request and is omitted
from serialized configuration. Video also omits `packet_boundaries` from the
encoder request when false, preserving the legacy strict request shape. One asks for a
single H.264 reference frame, is forwarded unchanged to `amaterasu.encode/1`,
and participates in operation-ID equality. Video activates the stream only when
the encoder's open metadata confirms the exact requested value; otherwise it
cleans up with `reference_frame_constraint_unavailable`. The Windows encoder
implements this with Microsoft's documented
[`CODECAPI_AVEncVideoMaxNumRefFrame`](https://learn.microsoft.com/en-us/windows/win32/medfound/codecapi-avencvideomaxnumrefframe)
property. Real hardware and emitted bitstreams remain unqualified.

`configuration.amd_av1_latency_mode` accepts only `auto`, `none`,
`power_saving`, `realtime`, and `lowest`. Omitted values default to `auto`, which
is omitted from serialized configuration to preserve compatibility with older
strict consumers. The stable setting ID is `global:amd_av1_latency_mode`;
product-level persistence, project selection, and editor integration remain
pending. This pipeline currently opens capture and H.264 MFT resources, with no
native AMD AMF AV1 backend. It rejects every explicit non-`auto` value with
`amd_av1_latency_mode_unsupported` before allocating a stream or requesting
either lower resource. Encode configuration and the Windows open boundary also
reject the setting. AMD AMF/AV1 support and the latency policy's effect remain
pending.

Close first stops delivery, discards buffered packets, and returns `closing`.
Status ceases to include the record after confirmed lower cleanup. A failed cleanup
remains visible as `cleanup_failed`, consuming its bounded slot; close retries it.
Repeating close while cleanup is already running returns its current state without
cancelling or restarting that cleanup. Wrong-owner close returns `not_found`. A missing status filter returns an empty
list. Close remains an explicit abort and can interrupt graceful drain.

`force_idr` is accepted only for an owned running stream. It coalesces repeated
requests into a flag for the next accepted encoder input. Capture pending/busy or
encoder backpressure leaves that flag pending; acceptance clears it. The encoder
receives the exact native force flag. Encode rejects a forced output that is not
a keyframe; video then closes both providers and reports `encoder_failed`. Live
hardware keyframe behavior still requires qualification.

`minimum_fps_target` defaults to 20 and accepts finite values from 0 through
1000, including fractional targets. Zero resolves to the negotiated capture FPS.
Video submits fresh frames immediately and, while capture is idle, makes at most
one repeat attempt per module tick using an owned source for the last accepted
frame. The repeat deadline is independent of capture polling, so a
target above negotiated FPS can receive repeats on intervening ticks when the
host tick rate allows it. A fresh accepted capture suppresses a repeat in the
same tick. Sequence and timestamp values are monotonic and are committed only
after encoder acceptance; a failed stream, close, or drain releases the cached
pixels. Capture uses the negotiated FPS, module tick cadence limits repeats,
and the scheduler does not catch up after delays.

`drain` is accepted only for an owned running stream and returns `draining`.
Repeating it while draining preserves the original deadline. Video stops capturing,
closes the selected capture stream, and invokes encoder drain until it accepts the
command (busy attempts accept nothing). It then continues receiving packets without
submitting frames. Buffered output stays available until the consumer accepts EOS.
EOS is queued once, respects the FIFO capacity, and remains queued on sink rejection.
Once EOS is buffered, video stops calling the encoder and waits only for consumer
acceptance or the original deadline. After EOS acceptance, native receive returns `MEDIA_CLOSED`; subsequent ticks close
the encoder and reconcile resource ownership. No video-generated EOS substitutes
for the encoder's actual EOS.

The configured drain deadline covers capture shutdown, encoder drain and consumer
EOS acceptance. Expiry or backend failure aborts buffered output and begins the
existing confirmed-settlement cleanup flow. Errors remain visible in active cleanup
or recent failure status. Deadline enforcement occurs on module ticks, not in a
real-time timer. `force_idr` while draining returns `invalid_state`.

## Native flow and limits

Consumers with the native-use grant bind `VideoApi` from `amaterasu-media` using
major 1, minor 0 and `size_of::<VideoApi>()`. Its `receive(context,stream,PacketSink)`
returns at most one H.264 Annex B packet. The factory copies the authenticated
consumer principal into its context and every receive checks that principal against
the control stream owner. A JSON call grant alone never permits native binding.

The tick thread holds capture and encoder bindings. It processes one stream per
tick, schedules capture at no more than its configured fps, and performs at most one
capture and one encoder receive per selected tick. Host tick cadence and provider
latency can reduce the achieved frame rate. The borrowed capture callback calls
encoder submit immediately; `MEDIA_BUSY` drops that incoming frame. Busy capture
simply skips that sample. Dimensions, format/stride and size validation precede
submission; fatal capture/encoder errors stop the pipeline and start cleanup.

For streams with authoritative geometry, video rechecks the complete native
snapshot before every scheduled frame read. A changed, lost, invalid, or failed
snapshot closes packet delivery and begins the normal owned cleanup flow before
another frame is submitted. A transient geometry `MEDIA_BUSY` skips that tick and
does not read a frame. This bounds ordinary detection to the configured frame
cadence plus module scheduling, but it cannot make display hotplug and frame capture
one atomic platform operation; a topology change can still race the last successful
check.

Each video stream owns a FIFO of at most four packets and 16 MiB total, copied once
from the encoder sink. A full queue returns `MEDIA_BUSY` without consuming the
encoder packet. Consumer sink rejection likewise retains the exact queued packet.
The shared queue uses nonblocking native lock acquisition; contended receive returns
`MEDIA_BUSY`. Callbacks must be bounded and must not retain borrowed bytes. Reentry
into receive is busy while the queue callback runs. Video owns no frame-processing
thread or raw-frame queue. The Linux software-x264 encoder has a separate bounded
worker queue; accepted frames are copied before the capture callback returns.
Stream status exposes numeric stage timing counters for capture callback/frame
arrival, encoder API submission, retained-frame copy, encoder polling, and packet
queue admission. They contain no pixel data and update in status snapshots rather
than logging each frame. Encode status separately reports BGRA conversion and
actual worker-side x264 submit duration, allowing queue delay and encoder work to
be distinguished from video tick time.

## Authority, cancellation and lifecycle

Video needs `provide` and `native_provide` for `amaterasu.video/1`, `call` and
`native_use` for each configured capture service (on Linux,
`amaterasu.capture-pipewire/1` and `amaterasu.capture-vkms/1`) and
`amaterasu.encode/1`, and
signed startup dependencies on those providers. Initiating control consumers currently
need call grants for video, the selected capture service and the encoder because parent delegation
intersects initiating authority. Lower resources are owned by the video runtime
principal; lower native bindings authenticate that same principal.

Open requests delegate with the initiating request as parent and recheck its
liveness between steps. Each pipeline has a unique provider operation ID. On
cancellation, transport loss, a malformed reply or undeliverable final completion,
video retains known pending IDs until both cancellation and result release are
confirmed. Failed settlement blocks all lookup/close work; after three attempts the
record remains failed, retaining IDs for an explicit close retry. A lost acceptance
reply retains the exact original request envelope. Video first confirms its parent
has ended, then recovers the broker's deduplicated request ID and settles it. If the
broker retry record has expired, the ended parent prevents a fresh child open.
Parentless uncertain cleanup requests contain only lookup or exact-ID close.

Once settlement is confirmed, video looks up the exact owned provider operation and
closes those IDs through parentless cleanup calls. It does not replay unknown open
effects or search by output name. Cleanup attempts each step at most three times;
encoder cleanup failure still proceeds to capture cleanup, retaining a failed
record if either side is unresolved. Internal RPC results are released to avoid
exhausting retained runtime request capacity.

The signed package must stop native video consumers before video, then destroy
video before its lower providers. Drop invalidates all copied packet queues, makes one bounded attempt to settle
pending work and end opening parents, and enqueues parentless exact known-ID closes
only after settlement succeeds. It cannot wait for lower provider ticks or reconcile
unknown IDs synchronously. Whole-host teardown releases lower native bindings and
provider destruction closes their resources. Independently unloading video while
keeping lower providers running is not supported; queued teardown closes are best
effort, not a synchronous cleanup flush. No ownership is persisted
across process restart. Native grants describe trusted-code authority, not a sandbox.

## Validation and remaining gates

Fake pipeline tests cover parent delegation, cancellation lookup/close, lost open
completion, owner isolation, continued capture cleanup after encoder cleanup fails,
and disabled behavior. Additional regressions inject lost acceptance/result replies,
cancel/release failures and failed parent completion, verify the expired broker
retry boundary, and check idempotent close and teardown queue invalidation. Native tests exercise dimension rejection, busy-frame drop, the force-IDR C flag,
bounded packet queues, sink rejection and authenticated packet ownership. Provider
selection tests verify allowlist rejection and provider-specific cleanup. Graceful
output tests verify capture shutdown, no further frame submission, exactly-once EOS,
FIFO backpressure, ownership, timeout settlement and visible failure records. Windows
cross-compilation checks the composed interface types; real Windows capture-to-H.264
end-to-end hardware validation remains required.

Networking, audio, pacing against a remote clock, bitrate adaptation,
resizing/colorspace policy, GPU texture sharing, session/launch
coordination, and restart recovery remain pending. Current provider limits (including
bounded encoder operation tombstones) also apply; this increment does not claim
production streaming or full Vibeshine parity. Dependencies are SDK, media ABI,
serde/serde_json, and getrandom for unique per-instance control operation IDs.

Open accepts an optional `operation_id` containing 1–128 ASCII graphic bytes.
`{"action":"lookup","operation_id":"launch-1"}` returns the caller's existing
stream metadata, including `state` and the more specific `stage`. An exact open
retry returns the same stream even while opening, without transferring the
original request's parent ownership. Configuration/provider/output/encoder changes
produce `operation_conflict`. A retry after cleanup returns `stream_closed`;
lookup after cleanup or from a different caller returns `not_found`.

The module retains at most 128 caller-scoped operation records, including closed
tombstones, until unload; it never evicts an old record and recreates that
operation. New identified opens then return `operation_limit`. Existing opens
without an operation ID retain their original behavior. Retry or lookup replies
do not promise that an opening stream will survive: cancellation or loss of the
original open completion still settles lower requests and cleans their resources.
A lost duplicate reply never initiates cleanup of the original stream.

Top-level open option `packet_boundaries` defaults to false for compatibility with
consumers that only recognize the original packet flags. Set it to true to request
`FLAG_END_OF_ACCESS_UNIT` (4) on complete encoded pictures. Video forwards the mode
to encode and requires explicit confirmation before activating; otherwise it
cleans up with `packet_boundaries_unavailable`. Metadata advertises
`supports_packet_boundaries: true` and the selected `packet_boundaries` value.
The mode participates in operation-ID equality. EOS remains a separate empty
packet without the boundary flag. Consumers must opt in and verify the returned
mode before relying on the new flag.

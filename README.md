# Managed video pipeline

`amaterasu_video` provides control and native service `amaterasu.video/1`. Product
coordination stays in this independently built library. It opens an allowlisted capture
stream and explicitly selected native encoder session asynchronously, then moves borrowed
CPU frames directly into the encoder's copying submit API. Encoded packets reach
consumers through the native `VideoApi`; frame/packet bytes never enter JSON.

Settings default to:

```json
{"enabled":false,"capture_providers":["dxgi"],"drain_timeout_ms":5000}
```

Capture providers are finite identifiers: `dxgi` maps to
`amaterasu.capture-dxgi/1`; `wgc` maps to `amaterasu.capture-wgc/1`. An empty
allowlist, unknown identifier, or drain timeout outside 100–30000 ms is rejected.
Enabled startup binds every configured native capture provider and the encoder.
Disabled startup registers services but binds no lower interface and opens no
devices. Unsupported capture hardware/platforms fail through real provider
responses. Auto encoder selection may use actual available x264; capture frames
are never replaced with generated output.

## API

```json
{"action":"open","output":"<capture enumerate output id>","encoder":{"clsid":"<encoder enumerate identity>","hardware_url":null,"friendly_name":"<encoder enumerate name>"},"minimum_fps_target":20,"configuration":{"width":1920,"height":1080,"fps":60,"bitrate":5000000}}
{"action":"status"}
{"action":"status","stream":1}
{"action":"force_idr","stream":1}
{"action":"drain","stream":1}
{"action":"close","stream":1}
```

Encoder selection passes the complete enumeration identity unchanged.
The `encoder` selector also accepts a legacy unsigned ordinal, `"auto"`, or
`"software"`. Video resolves these selectors against one strict H.264 encoder
enumeration (at most 64 entries) before opening capture. It freezes the selected
full identity for encoder Open and reports both `requested_encoder` and the
resolved `encoder`; enumeration reorder after selection cannot change the
allocated encoder. Full identity requests retain their existing wire shape and
skip ordinal resolution. Invalid, disabled, or unavailable selections fail before
capture allocation. The child enumeration has a five-second deadline, follows
parent cancellation, and releases its result before allocation.

`"auto"` probes each registered H.264 hardware candidate in enumeration order,
then the actual available x264 backend. Every probe uses the exact frozen
configuration and complete identity, submits owned synthetic NV12 without a
capture provider, and requires a real Annex B IDR before draining and closing.
A definite unusable result permits the next candidate only after teardown and
child-result release. Unknown transport outcomes, malformed/mismatched replies,
failed cleanup, cancellation and deadline abort selection. The enumeration and
all probes share a five-second admission budget; native vendor calls cannot be
forcibly interrupted and retain their synchronous worker-join ownership.
Capture opens once after a usable candidate is frozen. The final normal encoder
Open can still fail after a device change; it does not select another encoder
after capture allocation. Explicit identities, ordinals and `"software"` never
substitute another backend. `"software"`
requires the real x264 backend and preserves explicit unavailable reasons for
disabled builds, missing headers, and missing/incompatible runtime libraries.
Synthetic stream-source fixtures continue accepting legacy selectors and do not
advertise real codec enumeration or software availability.

Status reports bounded `encoder_probe_attempts` and the last negative
`encoder_probe_reason`. Local factory tests exercise a registered fault candidate
followed by genuine x264 output; Windows hardware activation and vendor return
latency remain separate qualification gates. This follows the pinned reference's
synthetic viability probing, while preserving Amaterasu's stricter explicit
selection behavior and H.264-only capabilities.

`configuration.sw_preset` and `configuration.sw_tune` pass unchanged to the encoder
service and stream status, with limits of 128 and 256 bytes and no control
characters. The client-negotiated `configuration.slices_per_frame` and signed
32-bit `configuration.min_threads` resolve to
`max(slices_per_frame, min_threads)` for software x264. QP defaults to 28 and
passes through this snapshot, but x264 ABR ignores it and Amaterasu has no VAAPI
constant-QP consumer yet. Omitted defaults preserve the prior wire shape. The
opt-in x264 provider resolves its `superfast` and `zerolatency` defaults; hardware
leaves these software options unapplied. Availability, build prerequisites, and
licensing are documented in [the encoder README](../encode/README.md#software-x264-selection-and-distribution).

Open optionally accepts the canonical `"capture"` mode `"ddx"`, `"wgc"`,
`"wgcc"`, `"auto"`, or `""`. Explicit `ddx`, `wgc`, and `wgcc` modes are pinned
to their provider. Empty and `auto` preserve the bootstrap provider selected by
`capture_provider` as the first choice, defaulting to `dxgi`, and may retry once
with the other provider only after a clean `unsupported_platform` provider reply
or clean `unavailable` admission rejection. The alternate must be in the startup
allowlist. Video translates provider-specific output IDs through their shared
Windows device name and opens the alternate only when its enumeration has one
attached exact match; missing or ambiguous matches retain the original failure.
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
one repeat attempt per module tick using an owned copy of the last frame accepted
by the encoder. The repeat deadline is independent of capture polling, so a
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
into receive is busy while the queue callback runs. No video-owned worker threads
or copied raw-frame queues exist.

## Authority, cancellation and lifecycle

Video needs `provide` and `native_provide` for `amaterasu.video/1`, `call` and
`native_use` for each configured capture service and `amaterasu.encode/1`, and
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

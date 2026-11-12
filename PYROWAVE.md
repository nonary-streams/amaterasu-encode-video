# PyroWave encoder provider

The encoding plugin implements the public `EncoderProvider` SDK trait through
`ProviderAdapter`, with standard `FrameResourceConsumerApi` input and
`OutputQueue` output. The selected provider owns validation, GPU conversion,
encoding and packetization. There is no PyroWave switch in the host or capture
consumer. `ResourceConnection` negotiates and forwards opaque GPU resources using
exactly the same contracts available to independent plugin developers.

## GPU input and HDR

Production selection uses `pyrowave-gpu` and `gpu_only:true` (the default).
Only GPU DMA-BUF input is offered. CPU frames and unsupported layouts fail
explicitly. The Vulkan device imports the capture allocation directly, samples
it for GPU RGB-to-YCbCr conversion, and encodes it. There is no raw-pixel CPU
readback, CPU conversion, CPU upload or intermediate capture copy in this path.
GPU conversion produces codec planes; encoded packets are returned to the CPU.
This is direct capture import, not a claim that conversion needs no output
allocations or that transport and packet handling run on the GPU.

Two submitted profiles are implemented:

| Profile | Capture | Encoded color | Codec value |
| --- | --- | --- | --- |
| `sdr8` | RGB8, BT.709 primaries, sRGB transfer, full range | 8-bit 4:2:0, BT.709 full range | 1 |
| `hdr10` | RGB10, BT.2020 primaries, PQ transfer, full range | 10-bit precision in R16 GPU planes, 4:2:0, BT.2020 NCL full range | 4 |

HDR is not tonemapped or reduced to 8-bit on input. The consumer checks the
explicit profile, color, geometry, modifier, planes and clock before import.
Known mismatched GPU UUIDs reject; a producer's unknown UUID is not treated as a
match, and requires the actual import to succeed on the selected Vulkan device.
Only queried importable modifiers fitting the stream geometry are offered.
Layouts needing several separate allocations are unsupported.

Acquire sync-file fences are validated and waited with a bounded deadline.
Submission completes all GPU reads and foreign-queue ownership release before
returning the capture borrow. This synchronous implementation therefore needs no
retained source reference or asynchronous completion fence. A failure that cannot
prove GPU retirement terminates the worker rather than releasing an unsafe
borrow. Direct capture guards can stall presentation while an encoder is busy;
production pacing and stuck-GPU behavior still need live qualification.

`pyrowave` without `pyrowave-gpu` is only the explicit `gpu_only:false` diagnostic
path. It accepts CPU BGRA8/NV12 with CPU upload and supports SDR only.

## Build and selection

The native C API/bitstream are pinned to upstream
`186f0393b77f7755953b5ecde994bb1cec2e4155` (API 0.6.0). API version alone does
not identify the bitstream. The dependency helper verifies archive SHA-256,
checks C ABI layouts, applies the small Amaterasu import-ownership/retirement/
GPU-probe patch, and builds with one worker. Vulkan import failure must preserve
caller FD ownership; successful import consumes only a private duplicate. The
patch also flushes Granite's queued commands before waiting for GPU retirement.
Unpatched libraries lack the required private import contract and fail GPU
startup. No upstream codec syntax or shader is changed.

```sh
python3 scripts/build_pyrowave.py
CARGO_TARGET_DIR="$PWD/target" CARGO_BUILD_JOBS=1 \
  cargo build --locked --offline -p amaterasu-encode --features pyrowave-gpu
```

Upstream MIT and transitive notices remain in the downloaded tree and must
accompany distribution. The helper installs no system files. Cross builds need
explicit target headers through `AMATERASU_PYROWAVE_INCLUDE_DIR`.

Trusted startup settings select an absolute native library path:

```json
{
  "enabled": true,
  "pyrowave": {
    "library_path": "/absolute/worktree/native-deps/pyrowave/build/libpyrowave-shared.so.0.6.0",
    "gpu_only": true
  }
}
```

The feature alone registers nothing. Explicitly enabled missing/incompatible
libraries or unusable Vulkan devices fail startup. A device remains alive across
reconnects; device operations are serialized. Native plugins/libraries are
trusted code. Composition must supply grants, dependencies and exact contracts:

- `amaterasu.encode.pyrowave/1`: authenticated JSON control.
- `amaterasu.encode.pyrowave-provider/1`: `amaterasu.encoder-provider.native/1`.
- `amaterasu.encode.pyrowave-consumer/1`: `amaterasu.frame-resource-consumer.native/1`.

## Capture selection

Direct immutable capture requires capture and helper builds with
`driver-read-leases`, a compatible protocol-4 helper and driver, and capture
settings `gpu_snapshot:false,require_lease:true`. A GPU snapshot is a capture copy
and is not the direct path qualified here. Missing guarded support fails closed.

The independent capture repository remains pinned to its published SDK. Build
its development artifact against this draft SDK's typed native contracts with:

```sh
python3 scripts/build_pyrowave_capture.py
```

This isolated manifest does not change published pins or install/grant the
privileged helper. Open capture with an agreed nonzero host-monotonic epoch:

```json
{
  "action": "open",
  "output": "kwin:Virtual-1",
  "operation_id": "capture-1",
  "resource_capture_clock": {
    "domain": "host_monotonic",
    "epoch_id": "07070707070707070707070707070707"
  },
  "resource_callback_contract": "cross_service_safe"
}
```

The frame's exact driver presentation timestamp is used in both the outer
resource and DMA-BUF descriptor. Retaining a frame also retains its adjusted
metadata descriptor. Capture v1.9 supplies static HDR metadata separately; this
metadata must be carried through the eventual client transport.

## Session and output contract

Control actions are `enumerate`, `probe`, `open`, `lookup`, `close`. `probe`
queries import offers and actually encodes a GPU-cleared image, then packetizes
it. `probe_scope:gpu_encode,capture_import_verified:false` explicitly distinguishes
that check from importing a live capture allocation. No session is created.

```json
{
  "action": "open",
  "operation_id": "stream-1",
  "configuration": {
    "profile": "hdr10",
    "width": 1920,
    "height": 1080,
    "fps": 60,
    "bitrate": 500000000,
    "clock_epoch": [7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7],
    "clock_origin_us": 123456789
  }
}
```

Use an actual agreed epoch/origin. Geometry is even, 16–3840 by 16–2160;
FPS 1–240; bitrate 100,000–4,000,000,000 bits/s. The codec budget
`bitrate/(8*fps)` must be 1 KiB–8 MiB/frame; it is not a wire-rate guarantee.
There are eight sessions, two per principal, 128 retained operation records and
one transient probe at a time. Retries are owner-scoped and conflicting options
reject. Lost new-open delivery closes the new session; lost retry delivery does
not close the existing one.

Configure the native encoder in submitted mode with a nonzero source ID,
generation 1 and the same JSON configuration. Then connect the selected capture
and resource consumer with `ResourceConnection` and required accessibility
`FRAME_ACCESS_GPU`. Input sequences/timestamps must increase and identify the
configured host-monotonic epoch. Output timestamps subtract the configured
origin and use the same epoch with stream-relative microseconds. Input and
access-unit IDs match. Every picture is intra-coded; FORCE_IDR is satisfied.

The format namespace is `0x5059524f57415645`. Codec schema is `0x186f0393`;
framing is value 2/schema 1; JSON options are value 3/schema 1. Framing is:

```text
u32 LE packet_count
repeat packet_count: u32 LE packet_size, packet_size bytes
```

The pinned packer targets 1024-byte boundaries; an indivisible codec block can
exceed that target. Allocation is bounded from validated compressed metadata,
not an assumed maximum packet size. Sequence headers explicitly identify
BT.2020/PQ/BT.2020 transform for HDR. No RTP, FEC, encryption or network behavior
is added. The bounded queue holds one picture plus EOS, up to 16 MiB.
A receive bound smaller than the whole picture returns BUSY; rejected output is
preserved for retry. Drain preserves accepted output and appends one EOS.
Cancellation, close and binding release synchronously retire work/subscriptions.

## Qualification and remaining integration

The ignored GPU tests render source DMA-BUFs with GBM/EGL, invoke registered SDK
factories and independently decode the output. Raw source pixels are never
uploaded/read back; test-only decoder readback checks SDR, adjacent HDR levels
indistinguishable at 8-bit, high PQ levels and colored BT.2020 conversion.
Renderable advertised modifiers are checked individually; unavailable fixture
allocations are reported as skipped, not qualified. The independent capture
artifact test verifies typed registration without privileges. The live capture
test uses the unchanged SDK `ResourceConnection` and an explicitly selected
existing output; it never creates a display or installs a helper.

```sh
cargo test --locked --offline -p amaterasu-encode --features pyrowave-gpu
AMATERASU_PYROWAVE_LIBRARY="$PWD/native-deps/pyrowave/build/libpyrowave-shared.so.0.6.0" \
  cargo test --locked --offline -p amaterasu-encode --features pyrowave-gpu-tests \
  gpu_roundtrip -- --ignored --nocapture
AMATERASU_CAPTURE_LIBRARY="$PWD/target/capture-sdk/debug/libamaterasu_capture_vkms.so" \
  cargo test --locked --offline -p amaterasu-encode --features pyrowave-gpu \
  independent_capture_library -- --ignored
AMATERASU_CAPTURE_LIBRARY="$PWD/target/capture-sdk/debug/libamaterasu_capture_vkms.so" \
AMATERASU_PYROWAVE_LIBRARY="$PWD/native-deps/pyrowave/build/libpyrowave-shared.so.0.6.0" \
AMATERASU_FRAME_HELPER=/operator/approved/guarded-helper \
AMATERASU_CAPTURE_OUTPUT=kwin:Virtual-1 \
  cargo test --locked --offline -p amaterasu-encode --features pyrowave-gpu \
  live_capture_forwards -- --ignored --nocapture
cargo fmt -p amaterasu-encode -- --check
cargo clippy --locked --offline -p amaterasu-encode --features pyrowave-gpu-tests --all-targets -- -D warnings
```

Set `AMATERASU_CAPTURE_EXPECT_HDR=1` to require an already active HDR capture;
an SDR run never qualifies live HDR. Select the appropriate Vulkan ICD when
multiple drivers are present. These checks do not change the installed runtime.

This is an opt-in provider increment, **not a ready RTSP/client route**. The
current generic route expects a different broker/common configuration and modern
framing; its metadata route also needs capture v1.11. Production selection,
compatible framing/feedback, HDR metadata transport and signed composition remain
to be integrated and qualified. 4:4:4, adaptive bitrate/FEC and PyroWave feedback
are not implemented. GPU fixture success does not establish live capture,
client playback, sustained FPS, pacing, latency or stuck-device shutdown.

## Recovered provider and current host integration

The optional codec is built against the current public SDK. Its broker control
service runs on the SDK service worker, so device creation, probes and cleanup
do not occupy the shared host tick. Native GPU submission remains synchronous:
a native consumer must invoke it on its own worker and retain the source borrow
until it returns. The current built-in GameStream route does not select this
provider or advertise PyroWave. A compatible selected transport/client route
needs separate composition and qualification; enabling this build feature
alone changes no negotiated client format.

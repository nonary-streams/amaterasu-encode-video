# Reviewed video observer source

This local source revision imports the tracked opt-in video observation files
from independently reviewed Amaterasu integration commit
`4d2b6a40674dac0c46955ffc46f7a162ba23c48e`. The six imported files are
byte-identical to that commit. Encoder source, C shims, build scripts and
feature flags stay at committed source snapshot
`a6b0db9e36a1a59328ab4b4785b27b210f8182b7` except the prior test-only
x265 bitstream fixture relocation. The dependency pin remains hook-capable
foundation `fe3004d5889df3eb0ce6f68c74fbef0f33c76e94`.

The video observer provides bounded, immutable snapshots at wired lifecycle
points. It does not expose a writable frame replacement path, arbitrary GPU
pointer ownership, or live extension loading. The source tag is not a signed
module artifact or device/runtime qualification; the standalone repository
still needs its own central build and conformance checks before release.

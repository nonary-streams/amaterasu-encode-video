# Amaterasu encoder and video source proof

Extracted from committed Amaterasu `a6b0db9e36a1a59328ab4b4785b27b210f8182b7`. This local-only
workspace owns the existing `amaterasu-encode` and `amaterasu-video` cdylibs,
their native C shims, headers and build scripts. Their service IDs and ABI
exports are unchanged. Both pin reviewed hook SDK foundation
`fe3004d5889df3eb0ce6f68c74fbef0f33c76e94`; no public remote exists.

Feature gates remain opt-in: software x264, x265 HEVC, FFmpeg NVENC/VAAPI,
native NVENC/CUDA, and FFmpeg conversion require their original target headers,
libraries or runtime probes. The C build scripts and release ABI markers are
retained verbatim. Runtime GPU/codec capability is not established by source
extraction or compile-only checks. x264 distribution has separate licensing
requirements noted in the original encoder README.

The only source test change copies one checked-in x265 bitstream fixture from
`crates/gamestream/src/video/fixtures/x265_shim_main_black_64.hevc` to `modules/encode/tests/fixtures/` and points the HEVC
parser fixture at that local path. No runtime source was edited for this move.

`file://` source pinning is machine-local. Cross-machine publication needs an
approved stable URL and regenerated lockfile. The host's signed package still
controls the exact built artifact bytes, module identity, ABI major, dependency
order and caller grants. No live GPU, capture, encoder or service was touched.

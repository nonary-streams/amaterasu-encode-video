# Amaterasu encode and video source (publication candidate)

This workspace holds `amaterasu-encode` and `amaterasu-video`, including their
C shims and original opt-in features. The proposed portable Cargo pin
references an exact `nonary-streams/amaterasu-sdk` commit; `Cargo.lock` records
the locally validated resolution. `cargo test --workspace --locked --offline`
passed with the bundled synthetic bitstream fixture in
`modules/encode/tests/fixtures/`. No GPU, capture device or live client was
used. Hardware encoder and codec availability require separate target checks;
x264-enabled binary distribution also needs a licensing review.

The source remains governed by the host's verified package, ABI and grants.
These source files do not install or select an encoder on a running host.
Project-authored source is MIT licensed. Optional x264-enabled binary
distribution still needs its separate licensing review.

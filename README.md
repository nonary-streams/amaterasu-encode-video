# Amaterasu video encoding

The two modules that turn captured frames into an encoded video stream for
[Amaterasu](https://github.com/nonary-streams/amaterasu).

| Directory | Module | What it does |
| --- | --- | --- |
| [`video`](video/README.md) | `amaterasu.video` | Moves captured frames through processing to the encoder and paces the stream |
| [`encode`](encode/README.md) | `amaterasu.encode` | Encodes frames with the host's hardware or software encoders |

The pipeline reaches its capture source, frame processors and encoder through
SDK contracts, so each can be replaced by another provider. Hardware and
software encoder back ends are Cargo features of `encode`; the default build
enables none that needs a vendor SDK or a separately licensed library. See
[`encode/README.md`](encode/README.md) and [`encode/PYROWAVE.md`](encode/PYROWAVE.md)
for what each feature requires.

## Building this repository

This repository holds 2 modules and is mounted by
[Amaterasu](https://github.com/nonary-streams/amaterasu) at `modules/encode-video`,
where they build as members of the host workspace. Their manifests inherit shared
dependencies and lints from that workspace, so a standalone build stages the
same layout around a checkout of the
[SDK](https://github.com/nonary-streams/amaterasu-sdk) at the commit named in
`sdk-revision`:

```sh
git clone https://github.com/nonary-streams/amaterasu-sdk .sdk
git -C .sdk checkout "$(cat sdk-revision)"
export CARGO_TARGET_DIR="$PWD/target"
python3 scripts/build-native.py .sdk --fmt
python3 scripts/build-native.py .sdk --test
python3 scripts/build-native.py .sdk --clippy
python3 scripts/build-native.py .sdk --release --artifacts dist
```

Neither checkout is modified. `Cargo.lock` records the resolution those
commands were validated with; after changing `sdk-revision` or a dependency,
refresh it with `python3 scripts/build-native.py .sdk --update-lock`.

Continuous integration runs the same four commands on Linux, Windows and
macOS. Pushing a tag `vX.Y.Z` that matches the version in every `Cargo.toml` builds
the Linux and Windows libraries, attests where they were built, and publishes
them as a GitHub release. A host installs these modules as part of the signed
Amaterasu module set; a release here is not an installable package on its own.

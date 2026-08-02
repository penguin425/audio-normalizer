# Audio Unit adapter

Forge ships an optional macOS Audio Unit v2 component backed by the official
Steinberg VST3 SDK wrapper. The Audio Unit and VST3 targets therefore use the
same C ABI live processor, parameter ranges, state handling, mono/stereo
layouts, float32 processing, and fixed five-millisecond latency contract.

The adapter is source-only and is not built by the default Rust build. It
requires macOS 11 or newer, Xcode's CMake generator, CMake 3.20 or newer, C++23, Git, and
the Apple macOS SDK. The pinned VST3 SDK and Apple AudioUnit SDK are fetched at
configure time; neither is vendored into this repository.

```sh
# Build and validate the component (also builds the Rust shared library).
tools/test-au-adapter.sh

# Reuse a previously built library or select an architecture explicitly.
FORGE_SKIP_CARGO_BUILD=1 \
FORGE_NORMALIZER_LIBRARY="$PWD/target/release/libforge_normalizer.dylib" \
FORGE_AU_ARCHITECTURES=arm64 \
tools/test-au-adapter.sh
```

The smoke test builds the selected `FORGE_AU_ARCHITECTURES` slice (arm64 on
Apple Silicon, or x86_64 on an Intel host). The SDK's universal-binary option
is disabled because a universal component must be linked against a matching
universal Forge dylib; distributors can provide that lipo-combined library and
configure a universal CMake build separately.

The SDK's build step copies the resulting component to
`~/Library/Audio/Plug-Ins/Components/ForgeLive.component` for local host
testing. Code signing is intentionally not performed in CI; distributors
should sign and notarize the component with their own Apple Developer
identity. Hosts may need an Audio Unit cache refresh after installation.

This release provides AUv2 (`aufx`) only. An AUv3 app/extension pair requires
an application container, entitlements, and host-specific distribution policy;
it remains a separate future adapter rather than being silently advertised as
part of this component.

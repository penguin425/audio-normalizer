# VST3 adapter

Forge ships an optional VST3 stereo/mono real-time effect in
`integrations/vst3/`. It is a thin host wrapper around the stable C streaming
ABI (`include/forge_normalizer.h`), so the normal Rust build does not acquire a
C++ or SDK dependency.

## Build on Linux

The adapter requires a C++17 compiler, CMake 3.20 or newer, Git, and the Forge
shared library. The build fetches the pinned Steinberg SDK into CMake's build
directory; the SDK is deliberately not vendored into this repository.

```sh
cargo build --locked --lib
tools/test-vst3-adapter.sh
```

To keep a build outside the repository, set `FORGE_NORMALIZER_LIBRARY` to an
already-built `libforge_normalizer.so`. `FORGE_CMAKE_GENERATOR` and
`FORGE_BUILD_JOBS` can be used to select the CMake generator and parallelism.
The script runs the SDK validator and removes its temporary build tree when it
exits.

The release source archives contain the adapter sources and CMake glue, but
not the SDK. CMake retrieves `steinbergmedia/vst3sdk` at
`v3.7.14_build_55` (`SMTG_ENABLE_VSTGUI_SUPPORT=OFF`). Review the SDK's MIT
license and the upstream terms before redistributing a built plug-in.

## Host contract

* 32-bit IEEE-754 floating-point audio only; 64-bit processing is rejected.
* One main input and one main output. Mono-to-mono and stereo-to-stereo
  arrangements are supported; mismatched or larger layouts are rejected.
* The callback uses the Forge live processor's fixed five-millisecond
  look-ahead and reports the exact latency to the host.
* Gain (−24…+24 dB), true-peak ceiling (−12…0 dBTP), and bypass are
  automatable and persisted in the VST3 component state.
* Attack and release are intentionally fixed at 10 ms and 100 ms in this first
  adapter revision. The C ABI will expose smoothing controls only after a
  versioned host contract is available.

The UI is host-provided parameter editing for now. A future platform-specific
editor can be added without changing the processor ABI.

## Platform status

The CMake project is portable across Linux, macOS, and Windows, but the CI
smoke test currently builds the Linux bundle. macOS Audio Unit support is kept
as a separate adapter and is not implied by the VST3 package.

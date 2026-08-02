# FFmpeg and GStreamer host adapters

Forge v0.120.0 adds source-level adapters for the versioned streaming C ABI.
They share the same five-millisecond look-ahead, true-peak ceiling, gain
smoothing, and one-shot end-of-stream flush as `forge-live`, LV2, and CLAP.
The adapters process interleaved IEEE-754 `f32` samples and do not change the
normalizer's normative file-analysis result.

The adapter sources are included in release archives under `integrations/`.
They are deliberately built against the host's FFmpeg/GStreamer development
packages instead of being shipped as prebuilt plugins: those hosts have
platform-specific ABI and licensing choices, and the Forge release must keep
its default Rust build free of native dependencies.

## Build and smoke test

On Debian/Ubuntu, install the optional development packages first:

```sh
sudo apt-get install \
  pkg-config libavutil-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good
```

The repository test compiles both adapters, links a C consumer against the
current shared library, inspects the GStreamer element, and runs a short
GStreamer pipeline:

```sh
tools/test-host-adapters.sh
```

The script accepts `FORGE_TARGET_DIR=...` when the library was built into a
non-default Cargo target directory. Set `FORGE_SKIP_CARGO_BUILD=1` to reuse an
existing debug library. Temporary objects and GStreamer registries are removed
on exit.

## FFmpeg AVFrame bridge

`integrations/ffmpeg/forge_ffmpeg_bridge.[ch]` adapts writable,
interleaved `AV_SAMPLE_FMT_FLT` frames to the C ABI. The caller creates one
bridge per fixed sample-rate/channel layout, calls
`forge_ffmpeg_bridge_process_frame` from its `filter_frame`/frame callback,
and supplies a caller-owned frame with at least `latency_frames` capacity to
`forge_ffmpeg_bridge_flush_frame` at end of stream. The flush function changes
the tail frame's `nb_samples` to the exact number written. After flush, create
a new bridge for another stream.

The bridge validates format, sample rate, channel count, writable buffers, and
line size. It returns negative `AVERROR` values and preserves a bounded UTF-8
error message, so a host can turn negotiation and end-of-stream failures into
explicit filter errors rather than dropping audio. Processing and flushing do
not allocate; bridge creation is the allocation point.

FFmpeg's public headers do not expose a stable ABI for third-party
`AVFilterPad` definitions. Consequently this is an AVFrame bridge, not a
loadable `libavfilter` plug-in. An application-owned or in-tree AVFilter can
call it from its existing `filter_frame` callback. This avoids depending on
private FFmpeg structs while still making the frame/latency/flush contract
usable by FFmpeg integrations.

The custom filter must account for the fixed latency in timestamps or use the
host's normal latency negotiation. It must pass a writable frame (use
`av_frame_make_writable` after a shared buffer) and preserve the negotiated
sample rate and channel layout.

## GStreamer element

`integrations/gstreamer/gstforge.c` builds the dynamic plugin `libgstforge.so`
and registers the `forge_normalizer` `GstAudioFilter` element. It accepts:

```text
audio/x-raw,format=F32LE,layout=interleaved,
rate=8000..384000,channels=1..64
```

Example after building the shared object into `/tmp/forge-host-adapters`:

```sh
GST_PLUGIN_PATH=/tmp/forge-host-adapters \
  gst-launch-1.0 audiotestsrc num-buffers=100 ! audioconvert ! \
  audio/x-raw,format=F32LE,layout=interleaved,rate=48000,channels=2 ! \
  forge_normalizer gain-db=2 ceiling-dbtp=-1 ! fakesink
```

Properties are `gain-db`, `ceiling-dbtp`, `attack-ms`, and `release-ms`.
Gain and ceiling updates are safe between buffers and follow the live
processor's smoothing envelope. Attack and release are creation-time settings;
set them before caps negotiation or restart the element after changing them.
The element reports its fixed look-ahead through the GStreamer latency query,
flushes its delayed tail before forwarding EOS, and recreates state after
`FLUSH_STOP`. The per-buffer transform path uses the caller's writable buffer;
GStreamer allocates only the bounded tail buffer needed for EOS.

## Compatibility and licensing

The C ABI compatibility policy is defined in [C-API.md](C-API.md). Build the
FFmpeg bridge with the `libavutil` headers from the FFmpeg major used by the
host; the source handles both modern `AVChannelLayout` and older channel-count
fields where available. Build the GStreamer plugin with GStreamer 1.x and the
base/audio development packages. The adapter source is MIT-licensed with the
rest of Forge; FFmpeg, GStreamer, and their plugins retain their own licenses.

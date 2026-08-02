#ifndef FORGE_FFMPEG_BRIDGE_H
#define FORGE_FFMPEG_BRIDGE_H

/*
 * Stable FFmpeg host bridge for Forge's versioned C streaming ABI.
 *
 * This is intentionally an AVFrame adapter, not a private libavfilter
 * plug-in. FFmpeg does not expose a stable public ABI for third-party
 * AVFilterPad definitions; applications that build an in-tree filter can
 * call this bridge from their filter_frame callback.
 */

#include <stddef.h>

#include <libavutil/frame.h>

#include "forge_normalizer.h"

#if defined(__cplusplus)
extern "C" {
#endif

typedef struct ForgeFfmpegBridge ForgeFfmpegBridge;

/*
 * Create an adapter for interleaved AV_SAMPLE_FMT_FLT audio. The bridge owns
 * the ForgeLiveV1 state returned here; the caller owns every AVFrame.
 *
 * The return value is NULL on failure. error_buffer is optional and follows
 * the UTF-8/NUL-termination contract of the Forge C ABI.
 */
ForgeFfmpegBridge *forge_ffmpeg_bridge_create(
    unsigned sample_rate_hz,
    unsigned channels,
    double initial_gain_db,
    double ceiling_dbtp,
    double attack_ms,
    double release_ms,
    char *error_buffer,
    size_t error_capacity);

void forge_ffmpeg_bridge_destroy(ForgeFfmpegBridge *bridge);

/* Return the fixed five-millisecond look-ahead in frames, or zero for NULL. */
size_t forge_ffmpeg_bridge_latency_frames(const ForgeFfmpegBridge *bridge);

/*
 * Process one writable, interleaved AV_SAMPLE_FMT_FLT frame in place. The
 * frame's sample rate and channel count must match creation. A zero return
 * value is success; failures are negative AVERROR values.
 */
int forge_ffmpeg_bridge_process_frame(
    ForgeFfmpegBridge *bridge,
    AVFrame *frame,
    char *error_buffer,
    size_t error_capacity);

/*
 * Flush the look-ahead tail into a writable, preallocated AVFrame. On entry,
 * tail->nb_samples is the frame capacity; on success it is replaced with the
 * exact number of written samples (the bridge latency). The frame must be
 * interleaved AV_SAMPLE_FMT_FLT with matching rate/channels.
 */
int forge_ffmpeg_bridge_flush_frame(
    ForgeFfmpegBridge *bridge,
    AVFrame *tail,
    char *error_buffer,
    size_t error_capacity);

#if defined(__cplusplus)
}
#endif

#endif /* FORGE_FFMPEG_BRIDGE_H */

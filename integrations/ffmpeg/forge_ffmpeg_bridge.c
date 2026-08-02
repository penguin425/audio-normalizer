#include "forge_ffmpeg_bridge.h"

#include <errno.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <libavutil/buffer.h>
#include <libavutil/error.h>
#include <libavutil/mem.h>
#include <libavutil/samplefmt.h>
#include <libavutil/version.h>

struct ForgeFfmpegBridge {
    ForgeLiveV1 *live;
    unsigned sample_rate_hz;
    unsigned channels;
    int flushed;
};

static void clear_error(char *buffer, size_t capacity) {
    if (buffer != NULL && capacity > 0u) {
        buffer[0] = '\0';
    }
}

static void set_error(char *buffer, size_t capacity, const char *message) {
    if (buffer == NULL || capacity == 0u) {
        return;
    }
    if (message == NULL) {
        message = "unknown FFmpeg bridge error";
    }
    (void)snprintf(buffer, capacity, "%s", message);
    buffer[capacity - 1u] = '\0';
}

static int status_error(ForgeStatus status) {
    switch (status) {
    case FORGE_STATUS_NULL_POINTER:
        return AVERROR(EFAULT);
    case FORGE_STATUS_BUFFER_TOO_SMALL:
        return AVERROR(ENOSPC);
    case FORGE_STATUS_INVALID_ARGUMENT:
    case FORGE_STATUS_INVALID_UTF8:
    case FORGE_STATUS_ANALYSIS_FAILED:
        return AVERROR(EINVAL);
    case FORGE_STATUS_OK:
    default:
        return 0;
    }
}

static int frame_channels(const AVFrame *frame) {
#if LIBAVUTIL_VERSION_MAJOR >= 57
    return frame->ch_layout.nb_channels > 0 ? (int)frame->ch_layout.nb_channels : 0;
#else
    return frame->channels;
#endif
}

static int validate_frame(
    const ForgeFfmpegBridge *bridge,
    const AVFrame *frame,
    size_t *sample_count,
    char *error_buffer,
    size_t error_capacity) {
    if (bridge == NULL || frame == NULL) {
        set_error(error_buffer, error_capacity, "bridge or AVFrame is null");
        return AVERROR(EFAULT);
    }
    if (bridge->flushed) {
        set_error(error_buffer, error_capacity, "bridge was flushed; create a new bridge");
        return AVERROR(EINVAL);
    }
    if (frame->format != AV_SAMPLE_FMT_FLT) {
        set_error(error_buffer, error_capacity, "AVFrame must use interleaved AV_SAMPLE_FMT_FLT");
        return AVERROR(EINVAL);
    }
    if (frame->sample_rate != (int)bridge->sample_rate_hz ||
        frame_channels(frame) != (int)bridge->channels) {
        set_error(error_buffer, error_capacity, "AVFrame rate or channels do not match bridge");
        return AVERROR(EINVAL);
    }
    if (frame->nb_samples <= 0 || frame->data[0] == NULL) {
        set_error(error_buffer, error_capacity, "AVFrame has no interleaved samples");
        return AVERROR(EINVAL);
    }
    size_t count = (size_t)frame->nb_samples * (size_t)bridge->channels;
    if (count > SIZE_MAX / sizeof(float)) {
        set_error(error_buffer, error_capacity, "AVFrame sample count is too large");
        return AVERROR(EINVAL);
    }
    if (frame->linesize[0] > 0 && (size_t)frame->linesize[0] < count * sizeof(float)) {
        set_error(error_buffer, error_capacity, "AVFrame data line is smaller than its samples");
        return AVERROR(EINVAL);
    }
    if (frame->buf[0] != NULL && !av_buffer_is_writable(frame->buf[0])) {
        set_error(error_buffer, error_capacity, "AVFrame data is not writable");
        return AVERROR(EINVAL);
    }
    *sample_count = count;
    return 0;
}

ForgeFfmpegBridge *forge_ffmpeg_bridge_create(
    unsigned sample_rate_hz,
    unsigned channels,
    double initial_gain_db,
    double ceiling_dbtp,
    double attack_ms,
    double release_ms,
    char *error_buffer,
    size_t error_capacity) {
    clear_error(error_buffer, error_capacity);
    if (sample_rate_hz > UINT32_MAX || channels > UINT32_MAX) {
        set_error(error_buffer, error_capacity, "sample rate or channels exceed C ABI limits");
        return NULL;
    }
    ForgeLiveConfigV1 config = {0};
    config.struct_size = (uint32_t)sizeof(config);
    config.api_version = forge_normalizer_c_api_version();
    config.sample_rate_hz = (uint32_t)sample_rate_hz;
    config.channels = (uint32_t)channels;
    config.initial_gain_db = initial_gain_db;
    config.ceiling_dbtp = ceiling_dbtp;
    config.attack_ms = attack_ms;
    config.release_ms = release_ms;
    ForgeFfmpegBridge *bridge = NULL;
    ForgeLiveV1 *live = forge_normalizer_live_create_v1(
        &config, error_buffer, error_capacity);
    if (live == NULL) {
        return NULL;
    }
    bridge = (ForgeFfmpegBridge *)av_mallocz(sizeof(*bridge));
    if (bridge == NULL) {
        forge_normalizer_live_destroy_v1(live);
        set_error(error_buffer, error_capacity, "unable to allocate FFmpeg bridge");
        return NULL;
    }
    bridge->live = live;
    bridge->sample_rate_hz = sample_rate_hz;
    bridge->channels = channels;
    return bridge;
}

void forge_ffmpeg_bridge_destroy(ForgeFfmpegBridge *bridge) {
    if (bridge == NULL) {
        return;
    }
    forge_normalizer_live_destroy_v1(bridge->live);
    av_free(bridge);
}

size_t forge_ffmpeg_bridge_latency_frames(const ForgeFfmpegBridge *bridge) {
    return bridge == NULL ? 0u : forge_normalizer_live_latency_frames_v1(bridge->live);
}

int forge_ffmpeg_bridge_process_frame(
    ForgeFfmpegBridge *bridge,
    AVFrame *frame,
    char *error_buffer,
    size_t error_capacity) {
    clear_error(error_buffer, error_capacity);
    size_t sample_count = 0u;
    int result = validate_frame(bridge, frame, &sample_count, error_buffer, error_capacity);
    if (result < 0) {
        return result;
    }
    ForgeStatus status = forge_normalizer_live_process_interleaved_f32_v1(
        bridge->live,
        (float *)frame->data[0],
        (size_t)frame->nb_samples,
        error_buffer,
        error_capacity);
    (void)sample_count;
    return status == FORGE_STATUS_OK ? 0 : status_error(status);
}

int forge_ffmpeg_bridge_flush_frame(
    ForgeFfmpegBridge *bridge,
    AVFrame *tail,
    char *error_buffer,
    size_t error_capacity) {
    clear_error(error_buffer, error_capacity);
    if (bridge == NULL || tail == NULL) {
        set_error(error_buffer, error_capacity, "bridge or tail AVFrame is null");
        return AVERROR(EFAULT);
    }
    if (bridge->flushed) {
        set_error(error_buffer, error_capacity, "bridge was already flushed");
        return AVERROR(EINVAL);
    }
    size_t capacity = 0u;
    int result = validate_frame(bridge, tail, &capacity, error_buffer, error_capacity);
    if (result < 0) {
        return result;
    }
    size_t capacity_frames = (size_t)tail->nb_samples;
    size_t latency = forge_normalizer_live_latency_frames_v1(bridge->live);
    if (capacity_frames < latency) {
        set_error(error_buffer, error_capacity, "tail AVFrame is smaller than bridge latency");
        return AVERROR(ENOSPC);
    }
    size_t written = 0u;
    ForgeStatus status = forge_normalizer_live_flush_interleaved_f32_v1(
        bridge->live,
        (float *)tail->data[0],
        capacity_frames,
        &written,
        error_buffer,
        error_capacity);
    (void)capacity;
    if (status != FORGE_STATUS_OK) {
        return status_error(status);
    }
    if (written > (size_t)INT_MAX) {
        set_error(error_buffer, error_capacity, "tail frame count does not fit AVFrame");
        return AVERROR(EINVAL);
    }
    tail->nb_samples = (int)written;
    bridge->flushed = 1;
    return 0;
}

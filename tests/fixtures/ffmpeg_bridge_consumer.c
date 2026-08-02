#include "forge_ffmpeg_bridge.h"

#include <stdio.h>
#include <string.h>

#include <libavutil/channel_layout.h>
#include <libavutil/frame.h>
#include <libavutil/samplefmt.h>

static int fail(const char *message, const char *error) {
    fprintf(stderr, "%s: %s\n", message, error != NULL ? error : "");
    return 1;
}

int main(void) {
    char error[256] = {0};
    ForgeFfmpegBridge *bridge = forge_ffmpeg_bridge_create(
        48000u, 2u, 0.0, -1.0, 10.0, 100.0, error, sizeof(error));
    if (bridge == NULL) {
        return fail("bridge create failed", error);
    }
    size_t latency = forge_ffmpeg_bridge_latency_frames(bridge);
    if (latency != 240u) {
        forge_ffmpeg_bridge_destroy(bridge);
        return fail("unexpected bridge latency", "");
    }

    AVFrame *frame = av_frame_alloc();
    AVFrame *tail = av_frame_alloc();
    if (frame == NULL || tail == NULL) {
        av_frame_free(&frame);
        av_frame_free(&tail);
        forge_ffmpeg_bridge_destroy(bridge);
        return fail("frame allocation failed", "");
    }
    frame->format = AV_SAMPLE_FMT_FLT;
    frame->sample_rate = 48000;
    frame->nb_samples = (int)(latency + 32u);
    av_channel_layout_default(&frame->ch_layout, 2);
    if (av_frame_get_buffer(frame, 0) < 0 || av_frame_make_writable(frame) < 0) {
        av_frame_free(&frame);
        av_frame_free(&tail);
        forge_ffmpeg_bridge_destroy(bridge);
        return fail("input frame allocation failed", "");
    }
    memset(frame->data[0], 0, (size_t)frame->linesize[0]);
    ((float *)frame->data[0])[(latency + 31u) * 2u] = 0.25f;
    if (forge_ffmpeg_bridge_process_frame(bridge, frame, error, sizeof(error)) != 0) {
        av_frame_free(&frame);
        av_frame_free(&tail);
        forge_ffmpeg_bridge_destroy(bridge);
        return fail("bridge process failed", error);
    }
    for (size_t i = 0u; i < latency * 2u; ++i) {
        if (((float *)frame->data[0])[i] != 0.0f) {
            av_frame_free(&frame);
            av_frame_free(&tail);
            forge_ffmpeg_bridge_destroy(bridge);
            return fail("look-ahead prefix was not silent", "");
        }
    }

    tail->format = AV_SAMPLE_FMT_FLT;
    tail->sample_rate = 48000;
    tail->nb_samples = (int)latency;
    av_channel_layout_default(&tail->ch_layout, 2);
    if (av_frame_get_buffer(tail, 0) < 0 || av_frame_make_writable(tail) < 0) {
        av_frame_free(&frame);
        av_frame_free(&tail);
        forge_ffmpeg_bridge_destroy(bridge);
        return fail("tail frame allocation failed", "");
    }
    if (forge_ffmpeg_bridge_flush_frame(bridge, tail, error, sizeof(error)) != 0 ||
        tail->nb_samples != (int)latency) {
        av_frame_free(&frame);
        av_frame_free(&tail);
        forge_ffmpeg_bridge_destroy(bridge);
        return fail("bridge flush failed", error);
    }
    int tail_has_signal = 0;
    for (size_t i = 0u; i < latency * 2u; ++i) {
        if (((float *)tail->data[0])[i] != 0.0f) {
            tail_has_signal = 1;
            break;
        }
    }
    if (!tail_has_signal) {
        av_frame_free(&frame);
        av_frame_free(&tail);
        forge_ffmpeg_bridge_destroy(bridge);
        return fail("flush tail lost the delayed signal", "");
    }
    if (forge_ffmpeg_bridge_process_frame(bridge, frame, error, sizeof(error)) >= 0) {
        av_frame_free(&frame);
        av_frame_free(&tail);
        forge_ffmpeg_bridge_destroy(bridge);
        return fail("flushed bridge accepted another frame", "");
    }

    av_frame_free(&frame);
    av_frame_free(&tail);
    forge_ffmpeg_bridge_destroy(bridge);
    return 0;
}

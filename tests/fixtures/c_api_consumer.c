#include "forge_normalizer.h"

#include <stdio.h>
#include <string.h>

int main(void) {
    ForgeAnalysisV1 analysis;
    char error[128] = {0};
    memset(&analysis, 0, sizeof(analysis));

    if (forge_normalizer_c_api_version() != FORGE_NORMALIZER_C_API_VERSION ||
        forge_normalizer_analysis_v1_size() != sizeof(ForgeAnalysisV1) ||
        forge_normalizer_live_config_v1_size() != sizeof(ForgeLiveConfigV1) ||
        forge_normalizer_version() == NULL ||
        strlen(forge_normalizer_version()) == 0u) {
        return 10;
    }

    ForgeStatus status = forge_normalizer_analyze_file_v1(
        "forge-c-api-file-does-not-exist.wav",
        1u,
        &analysis,
        sizeof(analysis),
        error,
        sizeof(error));
    if (status != FORGE_STATUS_ANALYSIS_FAILED || error[0] == '\0') {
        fprintf(stderr, "unexpected Forge status %d: %s\n", (int)status, error);
        return 11;
    }

    ForgeLiveConfigV1 config = {0u, 0u, 0u, 0u, 0.0, 0.0, 0.0, 0.0};
    config.struct_size = sizeof(config);
    config.api_version = FORGE_NORMALIZER_C_API_VERSION;
    config.sample_rate_hz = 48000u;
    config.channels = 2u;
    config.initial_gain_db = 0.0;
    config.ceiling_dbtp = -1.0;
    config.attack_ms = 10.0;
    config.release_ms = 100.0;
    ForgeLiveV1 *live = forge_normalizer_live_create_v1(
        &config, error, sizeof(error));
    if (live == NULL) {
        fprintf(stderr, "live create failed: %s\n", error);
        return 12;
    }
    size_t latency = forge_normalizer_live_latency_frames_v1(live);
    if (latency == 0u || latency > 256u) {
        fprintf(stderr, "unexpected live latency: %zu\n", latency);
        forge_normalizer_live_destroy_v1(live);
        return 13;
    }
    float samples[512] = {0.0f};
    samples[0] = 0.25f;
    status = forge_normalizer_live_process_interleaved_f32_v1(
        live, samples, 256u, error, sizeof(error));
    if (status != FORGE_STATUS_OK) {
        fprintf(stderr, "live process failed: %s\n", error);
        forge_normalizer_live_destroy_v1(live);
        return 14;
    }
    float tail[512] = {0.0f};
    size_t written = 0u;
    status = forge_normalizer_live_flush_interleaved_f32_v1(
        live, tail, 256u, &written, error, sizeof(error));
    if (status != FORGE_STATUS_OK || written != latency) {
        fprintf(stderr, "live flush failed (%d, %zu): %s\n",
                (int)status, written, error);
        forge_normalizer_live_destroy_v1(live);
        return 15;
    }
    if (forge_normalizer_live_set_target_gain_db_v1(
            live, 0.0, error, sizeof(error)) != FORGE_STATUS_INVALID_ARGUMENT) {
        fprintf(stderr, "flushed live handle accepted a setter\n");
        forge_normalizer_live_destroy_v1(live);
        return 16;
    }
    forge_normalizer_live_destroy_v1(live);
    return 0;
}

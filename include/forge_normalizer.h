#ifndef FORGE_NORMALIZER_H
#define FORGE_NORMALIZER_H

#include <stddef.h>
#include <stdint.h>

#if defined(_WIN32)
#define FORGE_NORMALIZER_API __declspec(dllimport)
#elif defined(__GNUC__) || defined(__clang__)
#define FORGE_NORMALIZER_API __attribute__((visibility("default")))
#else
#define FORGE_NORMALIZER_API
#endif

#if defined(__cplusplus)
extern "C" {
#endif

#define FORGE_NORMALIZER_C_API_VERSION 1u
#define FORGE_NORMALIZER_ANALYSIS_V1_SIZE 80u

typedef enum ForgeStatus {
    FORGE_STATUS_OK = 0,
    FORGE_STATUS_NULL_POINTER = 1,
    FORGE_STATUS_BUFFER_TOO_SMALL = 2,
    FORGE_STATUS_INVALID_UTF8 = 3,
    FORGE_STATUS_INVALID_ARGUMENT = 4,
    FORGE_STATUS_ANALYSIS_FAILED = 5
} ForgeStatus;

typedef struct ForgeAnalysisV1 {
    uint32_t struct_size;
    uint32_t api_version;
    uint32_t sample_rate_hz;
    uint32_t channels;
    uint64_t frames;
    double integrated_lufs;
    double max_momentary_lufs;
    double max_short_term_lufs;
    double loudness_range_lu;
    double rms_dbfs;
    double sample_peak_dbfs;
    double true_peak_dbtp;
} ForgeAnalysisV1;

#if defined(__cplusplus)
static_assert(sizeof(ForgeStatus) == 4u,
              "unexpected ForgeStatus layout");
static_assert(sizeof(ForgeAnalysisV1) == FORGE_NORMALIZER_ANALYSIS_V1_SIZE,
              "unexpected ForgeAnalysisV1 layout");
static_assert(offsetof(ForgeAnalysisV1, frames) == 16u,
              "unexpected ForgeAnalysisV1 frames offset");
static_assert(offsetof(ForgeAnalysisV1, true_peak_dbtp) == 72u,
              "unexpected ForgeAnalysisV1 true-peak offset");
#else
_Static_assert(sizeof(ForgeStatus) == 4u,
               "unexpected ForgeStatus layout");
_Static_assert(sizeof(ForgeAnalysisV1) == FORGE_NORMALIZER_ANALYSIS_V1_SIZE,
               "unexpected ForgeAnalysisV1 layout");
_Static_assert(offsetof(ForgeAnalysisV1, frames) == 16u,
               "unexpected ForgeAnalysisV1 frames offset");
_Static_assert(offsetof(ForgeAnalysisV1, true_peak_dbtp) == 72u,
               "unexpected ForgeAnalysisV1 true-peak offset");
#endif

FORGE_NORMALIZER_API uint32_t forge_normalizer_c_api_version(void);
FORGE_NORMALIZER_API const char *forge_normalizer_version(void);
FORGE_NORMALIZER_API size_t forge_normalizer_analysis_v1_size(void);

/*
 * Analyze a local file using at most max_decoded_samples decoded
 * frames-times-channels. path_utf8 must be NUL-terminated UTF-8. result_size
 * must be at least forge_normalizer_analysis_v1_size().
 *
 * error_buffer is optional. When non-NULL with positive capacity, Forge
 * always writes a NUL-terminated UTF-8 message (empty on success). result
 * must be correctly aligned for ForgeAnalysisV1. All storage remains owned by
 * the caller; error_buffer must not overlap path_utf8 or result.
 */
FORGE_NORMALIZER_API ForgeStatus forge_normalizer_analyze_file_v1(
    const char *path_utf8,
    uint64_t max_decoded_samples,
    ForgeAnalysisV1 *result,
    size_t result_size,
    char *error_buffer,
    size_t error_capacity);

#if defined(__cplusplus)
}
#endif

#endif

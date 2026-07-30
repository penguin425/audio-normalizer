#include "forge_normalizer.h"

#include <stdio.h>
#include <string.h>

int main(void) {
    ForgeAnalysisV1 analysis;
    char error[128] = {0};
    memset(&analysis, 0, sizeof(analysis));

    if (forge_normalizer_c_api_version() != FORGE_NORMALIZER_C_API_VERSION ||
        forge_normalizer_analysis_v1_size() != sizeof(ForgeAnalysisV1) ||
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
    return 0;
}

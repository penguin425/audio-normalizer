#include "forge_normalizer.h"

#include <stddef.h>

int main(void) {
    const char *version = forge_normalizer_version();
    if (version == NULL || version[0] == '\0') {
        return 10;
    }
    if (forge_normalizer_c_api_version() != FORGE_NORMALIZER_C_API_VERSION) {
        return 11;
    }
    if (forge_normalizer_analysis_v1_size() != sizeof(ForgeAnalysisV1) ||
        forge_normalizer_live_config_v1_size() != sizeof(ForgeLiveConfigV1)) {
        return 12;
    }
    return 0;
}

#include "forge_vst3_controller.h"

#include "base/source/fstreamer.h"
#include "forge_vst3_ids.h"

#include <algorithm>

namespace ForgeVst3 {

using namespace Steinberg;
using namespace Steinberg::Vst;

tresult PLUGIN_API Controller::initialize(FUnknown *context) {
    const tresult result = EditController::initialize(context);
    if (result != kResultOk) {
        return result;
    }
    parameters.addParameter(
        STR16("Bypass"), nullptr, 1, 0.0, ParameterInfo::kCanAutomate | ParameterInfo::kIsBypass,
        kBypassId);
    parameters.addParameter(
        STR16("Gain"), STR16("dB"), 0, 0.5, ParameterInfo::kCanAutomate, kGainId);
    parameters.addParameter(
        STR16("True Peak Ceiling"), STR16("dBTP"), 0, 11.0 / 12.0,
        ParameterInfo::kCanAutomate, kCeilingId);
    return kResultOk;
}

tresult PLUGIN_API Controller::setComponentState(IBStream *state) {
    if (state == nullptr) {
        return kResultFalse;
    }
    IBStreamer streamer(state, kLittleEndian);
    float gain = 0.0f;
    float ceiling = -1.0f;
    int32 bypass = 0;
    if (!streamer.readFloat(gain) || !streamer.readFloat(ceiling) ||
        !streamer.readInt32(bypass)) {
        return kResultFalse;
    }
    setParamNormalized(kGainId, std::clamp((static_cast<double>(gain) + 24.0) / 48.0, 0.0, 1.0));
    setParamNormalized(
        kCeilingId, std::clamp((static_cast<double>(ceiling) + 12.0) / 12.0, 0.0, 1.0));
    setParamNormalized(kBypassId, bypass != 0 ? 1.0 : 0.0);
    return kResultOk;
}

} // namespace ForgeVst3

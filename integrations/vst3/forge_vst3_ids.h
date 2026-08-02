#pragma once

#include "pluginterfaces/base/funknown.h"
#include "pluginterfaces/vst/vsttypes.h"

namespace ForgeVst3 {

enum ParameterId : Steinberg::Vst::ParamID {
    kGainId = 100,
    kCeilingId = 101,
    kBypassId = 102,
};

static const Steinberg::FUID kProcessorUid(
    0xA4D174E1, 0xC40E4DB4, 0xA1DD6C6E, 0xC9320A61);
static const Steinberg::FUID kControllerUid(
    0xB607E8D9, 0x2C6B4B1D, 0x8D3C0C4F, 0x7E9E4B11);

constexpr const char *kCategory = "Fx|Dynamics";

} // namespace ForgeVst3

#pragma once

#include "public.sdk/source/vst/vsteditcontroller.h"

namespace ForgeVst3 {

class Controller final : public Steinberg::Vst::EditController {
public:
    static Steinberg::FUnknown *createInstance(void *) {
        return static_cast<Steinberg::Vst::IEditController *>(new Controller());
    }

    Steinberg::tresult PLUGIN_API initialize(Steinberg::FUnknown *context) SMTG_OVERRIDE;
    Steinberg::tresult PLUGIN_API setComponentState(Steinberg::IBStream *state) SMTG_OVERRIDE;
};

} // namespace ForgeVst3

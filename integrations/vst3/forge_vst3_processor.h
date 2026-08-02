#pragma once

#include "public.sdk/source/vst/vstaudioeffect.h"

#include <cstdint>
#include <vector>

#include "forge_normalizer.h"

namespace ForgeVst3 {

class Processor final : public Steinberg::Vst::AudioEffect {
public:
    Processor();
    ~Processor() SMTG_OVERRIDE;

    static Steinberg::FUnknown *createInstance(void *) {
        return static_cast<Steinberg::Vst::IAudioProcessor *>(new Processor());
    }

    Steinberg::tresult PLUGIN_API initialize(Steinberg::FUnknown *context) SMTG_OVERRIDE;
    Steinberg::tresult PLUGIN_API terminate() SMTG_OVERRIDE;
    Steinberg::tresult PLUGIN_API setActive(Steinberg::TBool state) SMTG_OVERRIDE;
    Steinberg::tresult PLUGIN_API setupProcessing(
        Steinberg::Vst::ProcessSetup &setup) SMTG_OVERRIDE;
    Steinberg::tresult PLUGIN_API setBusArrangements(
        Steinberg::Vst::SpeakerArrangement *inputs,
        Steinberg::int32 numIns,
        Steinberg::Vst::SpeakerArrangement *outputs,
        Steinberg::int32 numOuts) SMTG_OVERRIDE;
    Steinberg::tresult PLUGIN_API canProcessSampleSize(
        Steinberg::int32 symbolicSampleSize) SMTG_OVERRIDE;
    Steinberg::tresult PLUGIN_API process(
        Steinberg::Vst::ProcessData &data) SMTG_OVERRIDE;
    Steinberg::uint32 PLUGIN_API getLatencySamples() SMTG_OVERRIDE;
    Steinberg::tresult PLUGIN_API setState(Steinberg::IBStream *state) SMTG_OVERRIDE;
    Steinberg::tresult PLUGIN_API getState(Steinberg::IBStream *state) SMTG_OVERRIDE;

private:
    bool createLive();
    void destroyLive();
    void consumeParameterChanges(Steinberg::Vst::IParameterChanges *changes);
    bool applyLiveParameters();
    static double gainFromNormalized(double value);
    static double ceilingFromNormalized(double value);

    ForgeLiveV1 *live_ = nullptr;
    std::uint32_t sampleRate_ = 0;
    Steinberg::int32 maxSamplesPerBlock_ = 0;
    std::uint32_t channels_ = 2;
    std::vector<float> interleaved_;
    double gainDb_ = 0.0;
    double ceilingDbtp_ = -1.0;
    bool bypass_ = false;
};

} // namespace ForgeVst3

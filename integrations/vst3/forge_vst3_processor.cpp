#include "forge_vst3_processor.h"

#include "base/source/fstreamer.h"
#include "forge_vst3_ids.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"

#include <algorithm>
#include <cmath>
#include <cstring>

namespace ForgeVst3 {

using namespace Steinberg;
using namespace Steinberg::Vst;

Processor::Processor() {
    setControllerClass(kControllerUid);
}

Processor::~Processor() {
    destroyLive();
}

tresult PLUGIN_API Processor::initialize(FUnknown *context) {
    const tresult result = AudioEffect::initialize(context);
    if (result != kResultOk) {
        return result;
    }
    addAudioInput(STR16("Stereo In"), SpeakerArr::kStereo);
    addAudioOutput(STR16("Stereo Out"), SpeakerArr::kStereo);
    return kResultOk;
}

tresult PLUGIN_API Processor::terminate() {
    destroyLive();
    interleaved_.clear();
    return AudioEffect::terminate();
}

tresult PLUGIN_API Processor::setActive(TBool state) {
    if (state) {
        if (!createLive()) {
            return kResultFalse;
        }
    } else {
        destroyLive();
    }
    return AudioEffect::setActive(state);
}

tresult PLUGIN_API Processor::setupProcessing(ProcessSetup &setup) {
    if (!std::isfinite(setup.sampleRate) || setup.sampleRate < 8'000.0 ||
        setup.sampleRate > 384'000.0 || setup.maxSamplesPerBlock <= 0) {
        return kResultFalse;
    }
    sampleRate_ = static_cast<std::uint32_t>(std::llround(setup.sampleRate));
    maxSamplesPerBlock_ = setup.maxSamplesPerBlock;
    try {
        interleaved_.assign(static_cast<std::size_t>(maxSamplesPerBlock_) * channels_, 0.0f);
    } catch (...) {
        interleaved_.clear();
        return kResultFalse;
    }
    return AudioEffect::setupProcessing(setup);
}

tresult PLUGIN_API Processor::setBusArrangements(SpeakerArrangement *inputs,
                                                  int32 numIns,
                                                  SpeakerArrangement *outputs,
                                                  int32 numOuts) {
    if (inputs == nullptr || outputs == nullptr || numIns != 1 || numOuts != 1) {
        return kResultFalse;
    }
    const auto inputChannels = SpeakerArr::getChannelCount(inputs[0]);
    const auto outputChannels = SpeakerArr::getChannelCount(outputs[0]);
    if (inputChannels != outputChannels || (inputChannels != 1 && inputChannels != 2)) {
        return kResultFalse;
    }
    auto *inputBus = FCast<AudioBus>(audioInputs.at(0));
    auto *outputBus = FCast<AudioBus>(audioOutputs.at(0));
    if (inputBus == nullptr || outputBus == nullptr) {
        return kResultFalse;
    }
    inputBus->setArrangement(inputs[0]);
    outputBus->setArrangement(outputs[0]);
    channels_ = static_cast<std::uint32_t>(inputChannels);
    if (maxSamplesPerBlock_ > 0) {
        try {
            interleaved_.assign(static_cast<std::size_t>(maxSamplesPerBlock_) * channels_, 0.0f);
        } catch (...) {
            interleaved_.clear();
            return kResultFalse;
        }
    }
    if (live_ != nullptr && !createLive()) {
        return kResultFalse;
    }
    return kResultTrue;
}

tresult PLUGIN_API Processor::canProcessSampleSize(int32 symbolicSampleSize) {
    return symbolicSampleSize == kSample32 ? kResultTrue : kResultFalse;
}

uint32 PLUGIN_API Processor::getLatencySamples() {
    if (live_ != nullptr) {
        return static_cast<uint32>(forge_normalizer_live_latency_frames_v1(live_));
    }
    return sampleRate_ == 0 ? 0u : std::max(16u, (sampleRate_ * 5u) / 1'000u);
}

void Processor::consumeParameterChanges(IParameterChanges *changes) {
    if (changes == nullptr) {
        return;
    }
    const int32 count = changes->getParameterCount();
    for (int32 index = 0; index < count; ++index) {
        IParamValueQueue *queue = changes->getParameterData(index);
        if (queue == nullptr || queue->getPointCount() <= 0) {
            continue;
        }
        ParamValue value = 0.0;
        int32 sampleOffset = 0;
        if (queue->getPoint(queue->getPointCount() - 1, sampleOffset, value) != kResultTrue) {
            continue;
        }
        switch (queue->getParameterId()) {
        case kGainId:
            gainDb_ = gainFromNormalized(value);
            break;
        case kCeilingId:
            ceilingDbtp_ = ceilingFromNormalized(value);
            break;
        case kBypassId:
            bypass_ = value >= 0.5;
            break;
        default:
            break;
        }
    }
}

bool Processor::applyLiveParameters() {
    if (live_ == nullptr) {
        return false;
    }
    char error[256] = {};
    if (forge_normalizer_live_set_target_gain_db_v1(
            live_, gainDb_, error, sizeof(error)) != FORGE_STATUS_OK) {
        return false;
    }
    return forge_normalizer_live_set_ceiling_dbtp_v1(
               live_, ceilingDbtp_, error, sizeof(error)) == FORGE_STATUS_OK;
}

tresult PLUGIN_API Processor::process(ProcessData &data) {
    consumeParameterChanges(data.inputParameterChanges);
    if (data.numInputs == 0 || data.numOutputs == 0 || data.numSamples == 0) {
        return kResultOk;
    }
    if (data.inputs == nullptr || data.outputs == nullptr ||
        data.numSamples > maxSamplesPerBlock_ ||
        data.inputs[0].numChannels != static_cast<int32>(channels_) ||
        data.outputs[0].numChannels != static_cast<int32>(channels_) ||
        data.inputs[0].channelBuffers32 == nullptr ||
        data.outputs[0].channelBuffers32 == nullptr) {
        return kResultFalse;
    }

    auto *input = data.inputs[0].channelBuffers32;
    auto *output = data.outputs[0].channelBuffers32;
    for (std::uint32_t channel = 0; channel < channels_; ++channel) {
        if (input[channel] == nullptr || output[channel] == nullptr) {
            return kResultFalse;
        }
    }
    if (bypass_) {
        for (std::uint32_t channel = 0; channel < channels_; ++channel) {
            std::memcpy(output[channel], input[channel], sizeof(float) * data.numSamples);
        }
        return kResultOk;
    }
    if (!applyLiveParameters()) {
        return kResultFalse;
    }
    for (int32 frame = 0; frame < data.numSamples; ++frame) {
        for (std::uint32_t channel = 0; channel < channels_; ++channel) {
            interleaved_[static_cast<std::size_t>(frame) * channels_ + channel] =
                input[channel][frame];
        }
    }
    char error[256] = {};
    if (forge_normalizer_live_process_interleaved_f32_v1(
            live_, interleaved_.data(), static_cast<std::size_t>(data.numSamples),
            error, sizeof(error)) != FORGE_STATUS_OK) {
        return kResultFalse;
    }
    for (int32 frame = 0; frame < data.numSamples; ++frame) {
        for (std::uint32_t channel = 0; channel < channels_; ++channel) {
            output[channel][frame] =
                interleaved_[static_cast<std::size_t>(frame) * channels_ + channel];
        }
    }
    return kResultOk;
}

bool Processor::createLive() {
    destroyLive();
    if (sampleRate_ == 0) {
        return false;
    }
    ForgeLiveConfigV1 config = {};
    config.struct_size = static_cast<std::uint32_t>(sizeof(config));
    config.api_version = forge_normalizer_c_api_version();
    config.sample_rate_hz = sampleRate_;
    config.channels = channels_;
    config.initial_gain_db = gainDb_;
    config.ceiling_dbtp = ceilingDbtp_;
    config.attack_ms = 10.0;
    config.release_ms = 100.0;
    char error[256] = {};
    live_ = forge_normalizer_live_create_v1(&config, error, sizeof(error));
    return live_ != nullptr;
}

void Processor::destroyLive() {
    if (live_ != nullptr) {
        forge_normalizer_live_destroy_v1(live_);
        live_ = nullptr;
    }
}

double Processor::gainFromNormalized(double value) {
    return -24.0 + std::clamp(value, 0.0, 1.0) * 48.0;
}

double Processor::ceilingFromNormalized(double value) {
    return -12.0 + std::clamp(value, 0.0, 1.0) * 12.0;
}

tresult PLUGIN_API Processor::setState(IBStream *state) {
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
    gainDb_ = std::clamp(static_cast<double>(gain), -24.0, 24.0);
    ceilingDbtp_ = std::clamp(static_cast<double>(ceiling), -12.0, 0.0);
    bypass_ = bypass != 0;
    return kResultOk;
}

tresult PLUGIN_API Processor::getState(IBStream *state) {
    if (state == nullptr) {
        return kResultFalse;
    }
    IBStreamer streamer(state, kLittleEndian);
    return streamer.writeFloat(static_cast<float>(gainDb_)) &&
                   streamer.writeFloat(static_cast<float>(ceilingDbtp_)) &&
                   streamer.writeInt32(bypass_ ? 1 : 0)
               ? kResultOk
               : kResultFalse;
}

} // namespace ForgeVst3

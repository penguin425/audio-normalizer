#include "forge_vst3_controller.h"
#include "forge_vst3_ids.h"
#include "forge_vst3_processor.h"
#include "public.sdk/source/main/pluginfactory.h"
#include "version.h"

using namespace Steinberg;
using namespace Steinberg::Vst;

BEGIN_FACTORY_DEF("Forge Project", "https://github.com/penguin425/audio-normalizer", "")

DEF_CLASS2(INLINE_UID_FROM_FUID(ForgeVst3::kProcessorUid), PClassInfo::kManyInstances,
           kVstAudioEffectClass, "Forge Live", Vst::kDistributable, ForgeVst3::kCategory,
           FULL_VERSION_STR, kVstVersionString, ForgeVst3::Processor::createInstance)

DEF_CLASS2(INLINE_UID_FROM_FUID(ForgeVst3::kControllerUid), PClassInfo::kManyInstances,
           kVstComponentControllerClass, "Forge Live Controller", 0, "", FULL_VERSION_STR,
           kVstVersionString, ForgeVst3::Controller::createInstance)

END_FACTORY

cmake_minimum_required(VERSION 3.20)

project(ForgeLiveAudioUnit VERSION 0.121.0 LANGUAGES CXX)

if(NOT APPLE)
    message(FATAL_ERROR "The Forge Audio Unit adapter is only supported on macOS")
endif()

if(NOT XCODE)
    message(FATAL_ERROR
        "The Forge Audio Unit adapter requires the Xcode CMake generator; "
        "configure with -G Xcode")
endif()

if(NOT FORGE_NORMALIZER_LIBRARY)
    message(FATAL_ERROR
        "FORGE_NORMALIZER_LIBRARY must point to libforge_normalizer.dylib")
endif()

if(NOT FORGE_NORMALIZER_INCLUDE_DIR)
    get_filename_component(FORGE_NORMALIZER_INCLUDE_DIR
        "${CMAKE_CURRENT_LIST_DIR}/../../include" ABSOLUTE)
endif()

# The Steinberg wrapper uses the SDK's AUv2 bridge.  Keep all SDK options
# explicit so configuring this optional adapter never enables unrelated sample
# projects or VSTGUI.
set(SMTG_ENABLE_VST3_PLUGIN_EXAMPLES OFF CACHE BOOL "" FORCE)
set(SMTG_ENABLE_VST3_HOSTING_EXAMPLES OFF CACHE BOOL "" FORCE)
set(SMTG_ENABLE_VSTGUI_SUPPORT OFF CACHE BOOL "" FORCE)
set(SMTG_ENABLE_AUV2_BUILDS ON CACHE BOOL "" FORCE)

# Apple moved the AUv2 implementation out of the macOS SDK. Fetch the
# redistributable SDK explicitly and pin it for reproducible builds. The
# Steinberg wrapper's CMake module builds this Xcode project before linking the
# component.
include(FetchContent)
FetchContent_Declare(AudioUnitSDK
    GIT_REPOSITORY https://github.com/apple/AudioUnitSDK.git
    GIT_TAG bd98b31feff57a15989fcfab4cd86dc63382b1ac
)
FetchContent_MakeAvailable(AudioUnitSDK)
FetchContent_GetProperties(AudioUnitSDK SOURCE_DIR SMTG_AUDIOUNIT_SDK_PATH)
set(SMTG_AUDIOUNIT_SDK_PATH "${SMTG_AUDIOUNIT_SDK_PATH}" CACHE PATH
    "Pinned Apple AudioUnit SDK source directory" FORCE)
set(SMTG_COREAUDIO_SDK_PATH "" CACHE PATH
    "Use the Apple AudioUnit SDK rather than the legacy CoreAudio SDK" FORCE)

# Reuse the VST3 target and its pinned SDK.  The official SDK AUv2 wrapper
# embeds that VST3 bundle, so both plug-in formats share the same processor,
# parameters, state, and latency contract.
add_subdirectory(
    "${CMAKE_CURRENT_LIST_DIR}/../vst3"
    "${CMAKE_CURRENT_BINARY_DIR}/vst3"
)

get_target_property(FORGE_VST3_SDK_SOURCE_DIR sdk SOURCE_DIR)
set(FORGE_VST3_SDK_MODULE_DIR "${FORGE_VST3_SDK_SOURCE_DIR}/cmake/modules")
if(NOT EXISTS "${FORGE_VST3_SDK_MODULE_DIR}/SMTG_AddVST3AuV2.cmake")
    get_filename_component(FORGE_VST3_SDK_ROOT
        "${FORGE_VST3_SDK_SOURCE_DIR}/.." ABSOLUTE)
    set(FORGE_VST3_SDK_MODULE_DIR
        "${FORGE_VST3_SDK_ROOT}/cmake/modules")
endif()
if(NOT FORGE_VST3_SDK_SOURCE_DIR OR
   NOT EXISTS "${FORGE_VST3_SDK_MODULE_DIR}/SMTG_AddVST3AuV2.cmake")
    message(FATAL_ERROR "The pinned VST3 SDK did not expose its CMake modules")
endif()
if(EXISTS "${FORGE_VST3_SDK_SOURCE_DIR}/source/vst/auwrapper/auwrapper.mm")
    set(public_sdk_SOURCE_DIR "${FORGE_VST3_SDK_SOURCE_DIR}")
else()
    get_filename_component(FORGE_VST3_SDK_ROOT
        "${FORGE_VST3_SDK_SOURCE_DIR}/.." ABSOLUTE)
    set(public_sdk_SOURCE_DIR "${FORGE_VST3_SDK_ROOT}/public.sdk")
endif()
if(NOT EXISTS "${public_sdk_SOURCE_DIR}/source/vst/auwrapper/auwrapper.mm")
    message(FATAL_ERROR "The pinned VST3 SDK did not expose its AU wrapper sources")
endif()
list(APPEND CMAKE_MODULE_PATH "${FORGE_VST3_SDK_MODULE_DIR}")
include(SMTG_AddVST3AuV2)

smtg_target_add_auv2(ForgeLiveAu
    BUNDLE_NAME ForgeLive
    BUNDLE_IDENTIFIER io.github.penguin425.audio-normalizer.forge-live-au
    INFO_PLIST_TEMPLATE "${CMAKE_CURRENT_LIST_DIR}/au-info.plist"
    VST3_PLUGIN_TARGET ForgeLiveVst3
)

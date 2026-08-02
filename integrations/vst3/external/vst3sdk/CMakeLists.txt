cmake_minimum_required(VERSION 3.20)

include(FetchContent)

option(SMTG_ENABLE_VST3_PLUGIN_EXAMPLES "Enable VST3 SDK examples" OFF)
option(SMTG_ENABLE_VST3_HOSTING_EXAMPLES "Enable VST3 SDK hosting examples" OFF)
option(SMTG_ENABLE_VSTGUI_SUPPORT "Enable VSTGUI support" OFF)

FetchContent_Declare(
    vst3sdk
    GIT_REPOSITORY https://github.com/steinbergmedia/vst3sdk.git
    GIT_TAG v3.7.14_build_55
    GIT_SHALLOW TRUE
)
FetchContent_MakeAvailable(vst3sdk)

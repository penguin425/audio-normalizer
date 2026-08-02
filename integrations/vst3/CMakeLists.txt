cmake_minimum_required(VERSION 3.20)

project(ForgeLiveVst3 VERSION 0.122.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

if(NOT FORGE_NORMALIZER_LIBRARY)
    message(FATAL_ERROR
        "FORGE_NORMALIZER_LIBRARY must point to libforge_normalizer.so, "
        "libforge_normalizer.dylib, or the Windows import library")
endif()

if(NOT FORGE_NORMALIZER_INCLUDE_DIR)
    get_filename_component(FORGE_NORMALIZER_INCLUDE_DIR
        "${CMAKE_CURRENT_LIST_DIR}/../../include" ABSOLUTE)
endif()

add_subdirectory(external)
smtg_enable_vst3_sdk()

add_library(forge_normalizer UNKNOWN IMPORTED GLOBAL)
set_target_properties(forge_normalizer PROPERTIES
    IMPORTED_LOCATION "${FORGE_NORMALIZER_LIBRARY}"
    INTERFACE_INCLUDE_DIRECTORIES "${FORGE_NORMALIZER_INCLUDE_DIR}")

smtg_add_vst3plugin(ForgeLiveVst3
    forge_vst3_ids.h
    forge_vst3_processor.h
    forge_vst3_processor.cpp
    forge_vst3_controller.h
    forge_vst3_controller.cpp
    forge_vst3_entry.cpp
    version.h
)

target_include_directories(ForgeLiveVst3
    PRIVATE
        "${CMAKE_CURRENT_LIST_DIR}"
        "${FORGE_NORMALIZER_INCLUDE_DIR}")
target_compile_definitions(ForgeLiveVst3 PRIVATE
    FORGE_VST3_VERSION="${PROJECT_VERSION}")
target_link_libraries(ForgeLiveVst3 PRIVATE sdk forge_normalizer)

if(UNIX AND NOT APPLE)
    get_filename_component(FORGE_LIBRARY_DIR "${FORGE_NORMALIZER_LIBRARY}" DIRECTORY)
    set_target_properties(ForgeLiveVst3 PROPERTIES
        BUILD_RPATH "${FORGE_LIBRARY_DIR}")
endif()

if(SMTG_MAC)
    smtg_target_set_bundle(ForgeLiveVst3
        BUNDLE_IDENTIFIER io.github.penguin425.audio-normalizer.forge-live-vst3
        COMPANY_NAME "Forge Project")
elseif(SMTG_WIN)
    target_compile_definitions(ForgeLiveVst3 PRIVATE _USE_MATH_DEFINES)
endif()

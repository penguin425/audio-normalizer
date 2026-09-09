cmake_minimum_required(VERSION 3.20)

project(ForgeNormalizerNativePackageConsumer LANGUAGES C)

if(NOT DEFINED FORGE_EXPECTED_VERSION OR FORGE_EXPECTED_VERSION STREQUAL "")
    message(FATAL_ERROR
        "FORGE_EXPECTED_VERSION must be supplied by the native-package smoke test")
endif()
if(NOT DEFINED FORGE_TEST_PREFIX OR FORGE_TEST_PREFIX STREQUAL "")
    message(FATAL_ERROR
        "FORGE_TEST_PREFIX must be supplied by the native-package smoke test")
endif()

find_package(ForgeNormalizer "${FORGE_EXPECTED_VERSION}" EXACT CONFIG REQUIRED)

if(NOT DEFINED ForgeNormalizer_VERSION)
    message(FATAL_ERROR "ForgeNormalizer did not provide ForgeNormalizer_VERSION")
endif()
if(NOT ForgeNormalizer_VERSION STREQUAL FORGE_EXPECTED_VERSION)
    message(FATAL_ERROR
        "ForgeNormalizer version '${ForgeNormalizer_VERSION}' does not match "
        "expected '${FORGE_EXPECTED_VERSION}'")
endif()

if(NOT TARGET Forge::Normalizer)
    message(FATAL_ERROR "ForgeNormalizer did not provide Forge::Normalizer")
endif()

get_target_property(_forge_imported Forge::Normalizer IMPORTED)
if(NOT _forge_imported)
    message(FATAL_ERROR "Forge::Normalizer is not an imported target")
endif()

get_target_property(_forge_type Forge::Normalizer TYPE)
if(NOT _forge_type STREQUAL "SHARED_LIBRARY" AND
   NOT _forge_type STREQUAL "UNKNOWN_LIBRARY")
    message(FATAL_ERROR
        "Forge::Normalizer has unexpected target type '${_forge_type}'")
endif()

get_target_property(_forge_includes Forge::Normalizer INTERFACE_INCLUDE_DIRECTORIES)
if(NOT _forge_includes OR _forge_includes MATCHES "-NOTFOUND$")
    message(FATAL_ERROR
        "Forge::Normalizer has no INTERFACE_INCLUDE_DIRECTORIES")
endif()
set(_forge_include_found FALSE)
foreach(_forge_include IN LISTS _forge_includes)
    if(EXISTS "${_forge_include}/forge_normalizer.h")
        get_filename_component(_forge_include_abs "${_forge_include}" ABSOLUTE)
        file(RELATIVE_PATH _forge_include_relative
            "${FORGE_TEST_PREFIX}" "${_forge_include_abs}")
        if(NOT IS_ABSOLUTE "${_forge_include_relative}" AND
           NOT _forge_include_relative STREQUAL ".." AND
           NOT _forge_include_relative MATCHES "^\.\.[/\\\\]")
            set(_forge_include_found TRUE)
        endif()
    endif()
endforeach()
if(NOT _forge_include_found)
    message(FATAL_ERROR
        "Forge::Normalizer include directory does not contain forge_normalizer.h: "
        "${_forge_includes}")
endif()

get_target_property(_forge_location Forge::Normalizer IMPORTED_LOCATION)
if(_forge_location MATCHES "-NOTFOUND$" OR NOT _forge_location)
    get_target_property(_forge_location Forge::Normalizer IMPORTED_LOCATION_RELEASE)
endif()
if(_forge_location MATCHES "-NOTFOUND$" OR NOT _forge_location)
    message(FATAL_ERROR
        "Forge::Normalizer has no imported runtime library location")
endif()
if(NOT EXISTS "${_forge_location}")
    message(FATAL_ERROR
        "Forge::Normalizer imported runtime library is missing: ${_forge_location}")
endif()

if(WIN32)
    get_target_property(_forge_implib Forge::Normalizer IMPORTED_IMPLIB)
    if(_forge_implib MATCHES "-NOTFOUND$" OR NOT _forge_implib)
        get_target_property(_forge_implib Forge::Normalizer IMPORTED_IMPLIB_RELEASE)
    endif()
    if(_forge_implib MATCHES "-NOTFOUND$" OR NOT _forge_implib)
        message(FATAL_ERROR
            "Forge::Normalizer has no Windows IMPORTED_IMPLIB")
    endif()
    if(NOT EXISTS "${_forge_implib}")
        message(FATAL_ERROR
            "Forge::Normalizer Windows import library is missing: ${_forge_implib}")
    endif()
endif()

get_filename_component(_forge_prefix_abs "${FORGE_TEST_PREFIX}" ABSOLUTE)
get_filename_component(_forge_location_abs "${_forge_location}" ABSOLUTE)
file(RELATIVE_PATH _forge_location_relative
    "${_forge_prefix_abs}" "${_forge_location_abs}")
if(IS_ABSOLUTE "${_forge_location_relative}" OR
   _forge_location_relative STREQUAL ".." OR
   _forge_location_relative MATCHES "^\.\.[/\\\\]")
    message(FATAL_ERROR
        "Forge::Normalizer runtime library escaped the moved prefix: "
        "${_forge_location}")
endif()

add_executable(native-package-cmake-consumer
    "${CMAKE_CURRENT_LIST_DIR}/../c_api_consumer.c")
set_target_properties(native-package-cmake-consumer PROPERTIES
    C_STANDARD 11
    C_STANDARD_REQUIRED ON
    C_EXTENSIONS OFF)
target_link_libraries(native-package-cmake-consumer PRIVATE Forge::Normalizer)

unset(_forge_imported)
unset(_forge_type)
unset(_forge_includes)
unset(_forge_include)
unset(_forge_include_abs)
unset(_forge_include_relative)
unset(_forge_include_found)
unset(_forge_location)
unset(_forge_implib)
unset(_forge_prefix_abs)
unset(_forge_location_abs)
unset(_forge_location_relative)

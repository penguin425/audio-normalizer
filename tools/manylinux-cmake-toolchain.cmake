# AlmaLinux installs libraries under lib64 by default, while audiopus_sys 0.2.2
# unconditionally links from the CMake install prefix's lib directory. Keep the
# release-only manylinux build layout compatible with that upstream build script.
set(CMAKE_INSTALL_LIBDIR "lib" CACHE STRING "Install libraries under lib" FORCE)

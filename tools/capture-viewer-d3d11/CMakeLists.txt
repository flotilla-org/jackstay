cmake_minimum_required(VERSION 3.20)
# Windows D3D11 reference viewer: plain Win32 + D3D11, C11, no SDL.
project(capture_viewer_d3d11 C)

if(NOT WIN32)
  message(FATAL_ERROR "capture-viewer-d3d11 is the Windows viewer; use tools/capture-viewer-sdl on macOS and Linux")
endif()

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)

get_filename_component(JACKSTAY_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../.." ABSOLUTE)
set(JACKSTAY_INCLUDE_DIR "${JACKSTAY_ROOT}/crates/jackstay/include" CACHE PATH "Jackstay public headers")
# The import library of a backend-windows build: the D3D11 entry points exist
# only there.
set(JACKSTAY_LIB "${JACKSTAY_ROOT}/target/debug/jackstay.dll.lib" CACHE FILEPATH "Jackstay import library")
if(NOT EXISTS "${JACKSTAY_LIB}")
  message(FATAL_ERROR "Build Jackstay first with --features backend-windows, or set JACKSTAY_LIB to its jackstay.dll.lib")
endif()
string(REGEX REPLACE "\\.lib$" "" JACKSTAY_DLL "${JACKSTAY_LIB}")

add_executable(capture-viewer-d3d11 src/main.c)
target_include_directories(capture-viewer-d3d11 PRIVATE "${JACKSTAY_INCLUDE_DIR}")
target_compile_definitions(capture-viewer-d3d11 PRIVATE _CRT_SECURE_NO_WARNINGS)
if(MSVC)
  target_compile_options(capture-viewer-d3d11 PRIVATE /W4 /WX)
else()
  target_compile_options(capture-viewer-d3d11 PRIVATE -Wall -Wextra -Werror)
endif()
target_link_libraries(capture-viewer-d3d11 PRIVATE "${JACKSTAY_LIB}" d3d11 dxgi d3dcompiler dxguid user32)
# Keep jackstay.dll beside the executable.
add_custom_command(TARGET capture-viewer-d3d11 POST_BUILD
  COMMAND ${CMAKE_COMMAND} -E copy_if_different "${JACKSTAY_DLL}" $<TARGET_FILE_DIR:capture-viewer-d3d11>)

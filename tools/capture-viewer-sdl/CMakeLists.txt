cmake_minimum_required(VERSION 3.20)
# The native present path (src/metal_present.m) is Objective-C; enable it on
# Apple. The CPU path (src/main.c) stays C11.
if(APPLE)
  project(capture_viewer_sdl C OBJC)
else()
  project(capture_viewer_sdl C)
endif()

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)

find_package(PkgConfig REQUIRED)
pkg_check_modules(SDL2 REQUIRED sdl2)

if(NOT CAPTURE_TRANSFER_LIB)
  set(CAPTURE_TRANSFER_LIB "${CMAKE_SOURCE_DIR}/../../target/debug/libcapture_transfer.dylib")
endif()

set(VIEWER_SOURCES src/main.c)
if(APPLE)
  # Build libcapture_transfer with --features backend-macos so the
  # ft_native_* symbols this viewer links against are present.
  list(APPEND VIEWER_SOURCES src/metal_present.m)
  # ARC: the present shim relies on __bridge_retained/__bridge_transfer to own
  # the presenter and on ARC to retain textures/event — no-ops without it.
  set_source_files_properties(src/metal_present.m PROPERTIES COMPILE_OPTIONS "-fobjc-arc")
endif()

add_executable(capture-viewer-sdl ${VIEWER_SOURCES})
target_include_directories(capture-viewer-sdl PRIVATE
  "${CMAKE_SOURCE_DIR}/../../crates/capture-transfer/include"
  ${SDL2_INCLUDE_DIRS}
)
target_link_directories(capture-viewer-sdl PRIVATE ${SDL2_LIBRARY_DIRS})
target_link_libraries(capture-viewer-sdl PRIVATE ${CAPTURE_TRANSFER_LIB} ${SDL2_LIBRARIES})
target_compile_options(capture-viewer-sdl PRIVATE ${SDL2_CFLAGS_OTHER})

if(APPLE)
  target_link_libraries(capture-viewer-sdl PRIVATE
    "-framework Metal"
    "-framework QuartzCore"
    "-framework IOSurface"
    "-framework Foundation"
  )
endif()

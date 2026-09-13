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

get_filename_component(JACKSTAY_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../.." ABSOLUTE)
set(JACKSTAY_INCLUDE_DIR "${JACKSTAY_ROOT}/crates/jackstay/include" CACHE PATH "Jackstay public headers")
if(NOT JACKSTAY_LIB)
  if(APPLE)
    set(JACKSTAY_LIB "${JACKSTAY_ROOT}/target/debug/libjackstay.dylib")
  elseif(WIN32)
    message(FATAL_ERROR "The SDL reference viewer currently supports macOS and Linux; Windows desktop capture is separate work")
  else()
    set(JACKSTAY_LIB "${JACKSTAY_ROOT}/target/debug/libjackstay.so")
  endif()
endif()
if(NOT EXISTS "${JACKSTAY_LIB}")
  message(FATAL_ERROR "Build Jackstay first (backend-macos on macOS), or set JACKSTAY_LIB to the built shared library")
endif()

set(VIEWER_SOURCES src/main.c)
if(APPLE)
  # Build libjackstay with --features backend-macos so the
  # ft_acquisition_macos_* symbols this viewer links against are present.
  list(APPEND VIEWER_SOURCES src/metal_present.m)
  # ARC: the present shim relies on __bridge_retained/__bridge_transfer to own
  # the presenter and on ARC to retain textures/event — no-ops without it.
  set_source_files_properties(src/metal_present.m PROPERTIES COMPILE_OPTIONS "-fobjc-arc")
endif()

add_executable(capture-viewer-sdl ${VIEWER_SOURCES})
target_include_directories(capture-viewer-sdl PRIVATE
  "${JACKSTAY_INCLUDE_DIR}"
  ${SDL2_INCLUDE_DIRS}
)
target_link_directories(capture-viewer-sdl PRIVATE ${SDL2_LIBRARY_DIRS})
target_link_libraries(capture-viewer-sdl PRIVATE ${JACKSTAY_LIB} ${SDL2_LIBRARIES})
target_compile_options(capture-viewer-sdl PRIVATE ${SDL2_CFLAGS_OTHER})

if(APPLE)
  target_link_libraries(capture-viewer-sdl PRIVATE
    "-framework Metal"
    "-framework QuartzCore"
    "-framework IOSurface"
    "-framework Foundation"
  )
  # Explicit hardware check, outside the CPU-only default build/CTest. This
  # compiles the actual presentation shaders/pipeline on a Metal device. It
  # does not create capture sources or prove frame completion ordering.
  add_executable(metal-presenter-smoke EXCLUDE_FROM_ALL tests/metal_present_smoke.m src/metal_present.m)
  target_include_directories(metal-presenter-smoke PRIVATE "${JACKSTAY_INCLUDE_DIR}" src)
  target_compile_options(metal-presenter-smoke PRIVATE -fobjc-arc)
  target_link_libraries(metal-presenter-smoke PRIVATE ${JACKSTAY_LIB}
    "-framework Metal" "-framework QuartzCore" "-framework IOSurface" "-framework Foundation")
endif()

enable_testing()
add_test(NAME synthetic_c_abi COMMAND capture-viewer-sdl --frames 30)
set_tests_properties(synthetic_c_abi PROPERTIES
  ENVIRONMENT "SDL_VIDEODRIVER=dummy"
  PASS_REGULAR_EXPRESSION "acquired_frames=30"
  TIMEOUT 15
)
add_test(NAME delayed_cpu_acquisition COMMAND capture-viewer-sdl --frames 3 --hold-ms 25)
set_tests_properties(delayed_cpu_acquisition PROPERTIES
  ENVIRONMENT "SDL_VIDEODRIVER=dummy"
  PASS_REGULAR_EXPRESSION "acquired_frames=3"
  TIMEOUT 15
)
if(NOT APPLE)
  add_test(NAME unsupported_native COMMAND capture-viewer-sdl --native --frames 1)
  set_tests_properties(unsupported_native PROPERTIES WILL_FAIL TRUE TIMEOUT 5)
endif()

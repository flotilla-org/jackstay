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
    message(FATAL_ERROR "The SDL reference viewer supports macOS and Linux; on Windows use tools/capture-viewer-d3d11")
  else()
    set(JACKSTAY_LIB "${JACKSTAY_ROOT}/target/debug/libjackstay.so")
  endif()
endif()
if(NOT EXISTS "${JACKSTAY_LIB}")
  message(FATAL_ERROR "Build Jackstay first (backend-macos on macOS), or set JACKSTAY_LIB to the built shared library")
endif()

set(VIEWER_SOURCES src/main.c src/viewer_input.c src/viewer_affordances.c)
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
target_link_libraries(capture-viewer-sdl PRIVATE ${JACKSTAY_LIB} ${SDL2_LIBRARIES} m)
target_compile_options(capture-viewer-sdl PRIVATE ${SDL2_CFLAGS_OTHER})

if(APPLE)
  target_link_libraries(capture-viewer-sdl PRIVATE
    "-framework AppKit"
    objc
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

# Uses only the public C ABI and Unix sockets; no SDL calls or host daemon.
add_executable(cpu-setup-smoke tests/cpu_setup_smoke.c)
target_include_directories(cpu-setup-smoke PRIVATE "${JACKSTAY_INCLUDE_DIR}")
target_compile_definitions(cpu-setup-smoke PRIVATE _POSIX_C_SOURCE=200809L _DARWIN_C_SOURCE)
target_compile_options(cpu-setup-smoke PRIVATE -Wall -Wextra -Werror)
target_link_libraries(cpu-setup-smoke PRIVATE ${JACKSTAY_LIB})
add_test(NAME cross_process_cpu_setup COMMAND cpu-setup-smoke)
set_tests_properties(cross_process_cpu_setup PROPERTIES TIMEOUT 20)

add_test(NAME missing_cpu_socket_argument COMMAND capture-viewer-sdl --cpu-socket)
add_test(NAME conflicting_cpu_source COMMAND capture-viewer-sdl --cpu-socket /unused --porthole-socket /unused)
set_tests_properties(missing_cpu_socket_argument conflicting_cpu_source PROPERTIES WILL_FAIL TRUE TIMEOUT 5)

add_test(NAME cpu_socket_viewer COMMAND cpu-setup-smoke --viewer $<TARGET_FILE:capture-viewer-sdl>)
set_tests_properties(cpu_socket_viewer PROPERTIES
  ENVIRONMENT "SDL_VIDEODRIVER=dummy"
  PASS_REGULAR_EXPRESSION "acquired_frames=3"
  TIMEOUT 20
)

add_executable(capture-input-source src/input_source.c)
target_include_directories(capture-input-source PRIVATE "${JACKSTAY_INCLUDE_DIR}")
target_compile_definitions(capture-input-source PRIVATE _DARWIN_C_SOURCE)
target_compile_options(capture-input-source PRIVATE -Wall -Wextra -Werror)
target_link_libraries(capture-input-source PRIVATE ${JACKSTAY_LIB})
find_package(Python3 REQUIRED COMPONENTS Interpreter)
add_test(NAME cooperative_input_processes COMMAND ${Python3_EXECUTABLE}
  ${CMAKE_CURRENT_SOURCE_DIR}/tests/input_processes.py
  $<TARGET_FILE:capture-input-source> $<TARGET_FILE:capture-viewer-sdl>)
set_tests_properties(cooperative_input_processes PROPERTIES TIMEOUT 20)

add_executable(v2-source-fixture tests/v2_source.c)
target_include_directories(v2-source-fixture PRIVATE "${JACKSTAY_INCLUDE_DIR}" src)
target_compile_options(v2-source-fixture PRIVATE -Wall -Wextra -Werror)
target_link_libraries(v2-source-fixture PRIVATE ${JACKSTAY_LIB})
add_test(NAME v2_affordances_processes COMMAND ${Python3_EXECUTABLE}
  ${CMAKE_CURRENT_SOURCE_DIR}/tests/v2_processes.py
  $<TARGET_FILE:v2-source-fixture> $<TARGET_FILE:capture-viewer-sdl>)
set_tests_properties(v2_affordances_processes PROPERTIES TIMEOUT 30)
find_program(CARGO_EXECUTABLE cargo REQUIRED)
add_test(NAME v2_toolkit_processes COMMAND ${Python3_EXECUTABLE}
  ${CMAKE_CURRENT_SOURCE_DIR}/tests/v2_toolkit.py
  ${CARGO_EXECUTABLE} ${JACKSTAY_ROOT} $<TARGET_FILE:capture-viewer-sdl>)
set_tests_properties(v2_toolkit_processes PROPERTIES TIMEOUT 90 RUN_SERIAL TRUE)
add_test(NAME typing_input_processes COMMAND ${Python3_EXECUTABLE}
  ${CMAKE_CURRENT_SOURCE_DIR}/tests/typing_processes.py
  $<TARGET_FILE:capture-input-source> $<TARGET_FILE:capture-viewer-sdl>)
set_tests_properties(typing_input_processes PROPERTIES TIMEOUT 30)

add_executable(scroll-input-contract tests/scroll_input.c src/viewer_input.c)
target_include_directories(scroll-input-contract PRIVATE "${JACKSTAY_INCLUDE_DIR}" src ${SDL2_INCLUDE_DIRS})
target_link_directories(scroll-input-contract PRIVATE ${SDL2_LIBRARY_DIRS})
target_compile_options(scroll-input-contract PRIVATE -Wall -Wextra -Werror ${SDL2_CFLAGS_OTHER})
target_link_libraries(scroll-input-contract PRIVATE ${JACKSTAY_LIB} ${SDL2_LIBRARIES} m)
if(APPLE)
  target_link_libraries(scroll-input-contract PRIVATE "-framework AppKit" objc)
endif()
add_test(NAME scroll_input_contract COMMAND scroll-input-contract)
set_tests_properties(scroll_input_contract PROPERTIES ENVIRONMENT "SDL_VIDEODRIVER=dummy" TIMEOUT 10)

add_executable(window-presentation-contract tests/window_presentation.c src/viewer_affordances.c)
target_include_directories(window-presentation-contract PRIVATE "${JACKSTAY_INCLUDE_DIR}" src ${SDL2_INCLUDE_DIRS})
target_link_directories(window-presentation-contract PRIVATE ${SDL2_LIBRARY_DIRS})
target_compile_options(window-presentation-contract PRIVATE -Wall -Wextra -Werror ${SDL2_CFLAGS_OTHER})
target_link_libraries(window-presentation-contract PRIVATE ${JACKSTAY_LIB} ${SDL2_LIBRARIES} m)
add_test(NAME window_presentation_contract COMMAND window-presentation-contract)
set_tests_properties(window_presentation_contract PROPERTIES ENVIRONMENT "SDL_VIDEODRIVER=dummy" TIMEOUT 10)

add_executable(cursor-contract tests/cursor.c)
target_include_directories(cursor-contract PRIVATE "${JACKSTAY_INCLUDE_DIR}" src ${SDL2_INCLUDE_DIRS})
target_link_directories(cursor-contract PRIVATE ${SDL2_LIBRARY_DIRS})
target_compile_options(cursor-contract PRIVATE -Wall -Wextra -Werror ${SDL2_CFLAGS_OTHER})
target_link_libraries(cursor-contract PRIVATE ${JACKSTAY_LIB} ${SDL2_LIBRARIES} m)
add_test(NAME cursor_contract COMMAND cursor-contract)
set_tests_properties(cursor_contract PROPERTIES ENVIRONMENT "SDL_VIDEODRIVER=dummy" TIMEOUT 10)

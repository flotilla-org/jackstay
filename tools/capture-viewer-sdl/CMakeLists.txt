cmake_minimum_required(VERSION 3.20)
project(capture_viewer_sdl C)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)

find_package(PkgConfig REQUIRED)
pkg_check_modules(SDL2 REQUIRED sdl2)

if(NOT CAPTURE_TRANSFER_LIB)
  set(CAPTURE_TRANSFER_LIB "${CMAKE_SOURCE_DIR}/../../target/debug/libcapture_transfer.dylib")
endif()

add_executable(capture-viewer-sdl src/main.c)
target_include_directories(capture-viewer-sdl PRIVATE
  "${CMAKE_SOURCE_DIR}/../../crates/capture-transfer/include"
  ${SDL2_INCLUDE_DIRS}
)
target_link_directories(capture-viewer-sdl PRIVATE ${SDL2_LIBRARY_DIRS})
target_link_libraries(capture-viewer-sdl PRIVATE ${CAPTURE_TRANSFER_LIB} ${SDL2_LIBRARIES})
target_compile_options(capture-viewer-sdl PRIVATE ${SDL2_CFLAGS_OTHER})

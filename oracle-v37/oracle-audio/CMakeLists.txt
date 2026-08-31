cmake_minimum_required(VERSION 3.16)
project(oracle_audio LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE Release)
endif()
add_compile_options(-Wall -Wextra)

include_directories(${CMAKE_CURRENT_SOURCE_DIR}/include)

find_package(Threads REQUIRED)

# --- Optional real backends ---
option(Oracle_WITH_ALSA "Build the ALSA capture backend (Linux)" OFF)
option(Oracle_WITH_HIP  "Build the HIP/ROCm ASR kernels" OFF)

# The capture backends are ALWAYS compiled: capture_alsa.cpp provides the
# factory on Linux (ALSA or Null), capture_wasapi.cpp on Windows, and
# capture_coreaudio.cpp on macOS.
# Each is #ifdef'd to the other platform's empty translation unit, so exactly
# one make_capture() is defined per target — no duplicate symbols.
set(CAPTURE_SOURCES src/capture_alsa.cpp src/capture_wasapi.cpp src/capture_coreaudio.cpp)

add_executable(oracle-audio src/main.cpp ${CAPTURE_SOURCES})
target_link_libraries(oracle-audio PRIVATE Threads::Threads)

if(Oracle_WITH_ALSA)
  find_library(ASOUND_LIB asound REQUIRED)
  target_link_libraries(oracle-audio PRIVATE ${ASOUND_LIB})
  # NOTE: the option is Oracle_WITH_ALSA but the macro the source tests is
  # ORACLE_WITH_ALSA. These used to differ only in case, so -DOracle_WITH_ALSA=ON
  # linked libasound and still compiled the Null backend -- the ALSA path was
  # never built and capture silently produced a test tone.
  target_compile_definitions(oracle-audio PRIVATE ORACLE_WITH_ALSA=1)
  message(STATUS "ALSA capture backend: ENABLED (${ASOUND_LIB})")
else()
  message(STATUS "ALSA capture backend: disabled")
endif()

if(WIN32)
  # WASAPI needs the Core Audio COM libraries.
  target_link_libraries(oracle-audio PRIVATE ole32 mmdevapi)
  message(STATUS "WASAPI capture backend: ENABLED (Windows)")
endif()

if(APPLE)
  # AUHAL lives in AudioUnit/AudioToolbox; device enumeration in CoreAudio.
  find_library(COREAUDIO_LIB CoreAudio REQUIRED)
  find_library(AUDIOTOOLBOX_LIB AudioToolbox REQUIRED)
  find_library(AUDIOUNIT_LIB AudioUnit REQUIRED)
  target_link_libraries(oracle-audio PRIVATE
    ${COREAUDIO_LIB} ${AUDIOTOOLBOX_LIB} ${AUDIOUNIT_LIB})
  message(STATUS "CoreAudio capture backend: ENABLED (macOS)")
endif()

# Unit tests (header-only logic; dependency-free harness).
enable_testing()
add_executable(oracle-audio-tests tests/test_main.cpp ${CAPTURE_SOURCES})
target_link_libraries(oracle-audio-tests PRIVATE Threads::Threads)
if(Oracle_WITH_ALSA)
  target_link_libraries(oracle-audio-tests PRIVATE ${ASOUND_LIB})
  target_compile_definitions(oracle-audio-tests PRIVATE ORACLE_WITH_ALSA=1)
endif()
if(WIN32)
  target_link_libraries(oracle-audio-tests PRIVATE ole32 mmdevapi)
endif()
if(APPLE)
  target_link_libraries(oracle-audio-tests PRIVATE
    ${COREAUDIO_LIB} ${AUDIOTOOLBOX_LIB} ${AUDIOUNIT_LIB})
endif()
add_test(NAME oracle-audio-tests COMMAND oracle-audio-tests)

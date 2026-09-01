#!/usr/bin/env bash
# Build and test every component of Project Oracle of Delphi
# Usage: scripts/build_all.sh [--release] [--alsa]
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

RELEASE=""
ALSA_FLAG=""
for arg in "$@"; do
  case "$arg" in
    --release) RELEASE="--release" ;;
    --alsa)    ALSA_FLAG="-DOracle_WITH_ALSA=ON" ;;
    --help|-h)
      echo "Usage: scripts/build_all.sh [--release] [--alsa]"
      echo "  --release  build the Rust workspace in release mode"
      echo "  --alsa     Linux only: compile the ALSA capture backend."
      echo "             macOS selects CoreAudio automatically; Windows, WASAPI."
      exit 0 ;;
  esac
done

# --alsa on a Mac would send CMake into find_library(asound REQUIRED), which
# fails hard and confusingly. Refuse it up front and name the real backend.
if [ -n "$ALSA_FLAG" ] && [ "$(uname -s)" = "Darwin" ]; then
  echo "--alsa is Linux-only; macOS uses CoreAudio, selected automatically." >&2
  exit 2
fi

# The HUD must be built FIRST. oracle-core embeds oracle-hud/dist with
# #[derive(RustEmbed)], which is resolved at compile time, so on a clean
# checkout the Rust build failed outright with "folder ... does not exist"
# when this step ran last.
echo "==> [1/3] HUD (oracle-hud)"
if command -v npm >/dev/null; then
  ( cd oracle-hud && npm install --silent && npx tsc --noEmit && npx vite build >/dev/null )
  echo "HUD: typecheck + build OK"
else
  echo "npm not found; skipping HUD build"
  echo "WARNING: oracle-core embeds oracle-hud/dist and will not compile without it." >&2
fi

echo "==> [2/3] Rust workspace (oracle-ipc, oracle-core, oracle-actd)"
cargo build $RELEASE
cargo test --all

# The backend this build will actually get, so the line below is not a lie on a
# Mac (where --alsa is meaningless and CoreAudio is selected by CMake instead).
case "$(uname -s)" in
  Darwin) BACKEND="CoreAudio" ;;
  *)      BACKEND="${ALSA_FLAG:+ALSA}" ; BACKEND="${BACKEND:-null backend}" ;;
esac

echo "==> [3/3] C++ audio engine (oracle-audio) ($BACKEND)"
if ! command -v cmake >/dev/null; then
  # npm's absence is already handled above; cmake's was not, so on a fresh
  # machine this step died with a bare "cmake: command not found" after the
  # first two steps had already succeeded.
  echo "cmake not found; skipping the audio engine." >&2
  echo "  macOS:  brew install cmake" >&2
  echo "  Debian: sudo apt-get install cmake g++ libasound2-dev" >&2
  exit 1
fi
cmake -B oracle-audio/build -S oracle-audio -DCMAKE_BUILD_TYPE=Release $ALSA_FLAG >/dev/null
# nproc is GNU coreutils and does not exist on macOS; sysctl is the BSD spelling.
JOBS="$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
cmake --build oracle-audio/build -j"$JOBS" >/dev/null
"$ROOT/oracle-audio/build/oracle-audio-tests"

echo
echo "==> ALL COMPONENTS BUILT AND TESTED"

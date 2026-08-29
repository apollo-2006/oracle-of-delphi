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
  esac
done

echo "==> [1/3] Rust workspace (oracle-ipc, oracle-core, oracle-actd)"
cargo build $RELEASE
cargo test --all

echo "==> [2/3] C++ audio engine (oracle-audio) ${ALSA_FLAG:-(null backend)}"
cmake -B oracle-audio/build -S oracle-audio -DCMAKE_BUILD_TYPE=Release $ALSA_FLAG >/dev/null
cmake --build oracle-audio/build -j"$(nproc 2>/dev/null || echo 4)" >/dev/null
"$ROOT/oracle-audio/build/oracle-audio-tests"

echo "==> [3/3] HUD (oracle-hud)"
if command -v npm >/dev/null; then
  ( cd oracle-hud && npm install --silent && npx tsc --noEmit && npx vite build >/dev/null )
  echo "HUD: typecheck + build OK"
else
  echo "npm not found; skipping HUD build"
fi

echo
echo "==> ALL COMPONENTS BUILT AND TESTED"

#!/usr/bin/env bash
# Fetch the platform-specific runtime dependencies Oracle of Delphi needs.
#
#   scripts/setup.sh              everything missing
#   scripts/setup.sh --force      re-fetch even if present
#   scripts/setup.sh piper        just one component (piper|whisper|model|llama)
#   scripts/setup.sh models       the GGUFs for the planner, vision tier and embedder
#                                 (~10 GB; not part of "everything", ask for it)
#   scripts/setup.sh kwin         Linux/KDE: let oracle-actd take window captures
#
# Everything lands under <repo root>/{piper,whisper,llama.cpp}/<platform>/, which
# is what `${ORACLE_ROOT}` in the shipped profiles points at. Re-running is safe:
# each step is skipped when its output already exists.
#
# Why a fetch script instead of committing these: they are per-platform binaries.
# Committing every platform's copy doubles the repository and, because git keeps
# history forever, every rebuild adds another full copy that can never be
# reclaimed. Fetching also means this repository does not redistribute anyone
# else's code -- notably espeak-ng, which piper bundles and which is GPL-3.0.
set -euo pipefail
CALLER_PWD="$PWD"                    # for resolving relative paths the caller passes
cd "$(dirname "$0")/../.."          # scripts/ -> oracle-v37/ -> repo root
ROOT="$(pwd)"

WHISPER_TAG="b4938"
WHISPER_MODEL="ggml-base.en.bin"

# The Piper voice, from rhasspy/piper-voices at a pinned revision. The hashes are
# those of the copy this project was developed against; a download that does not
# match is refused rather than installed.
VOICE_URL="https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/en/en_US/amy/medium/en_US-amy-medium"
VOICE_ONNX_SHA256="b3a6e47b57b8c7fbe6a0ce2518161a50f59a9cdd8a50835c02cb02bdd6206c18"
VOICE_JSON_SHA256="95a23eb4d42909d38df73bb9ac7f45f597dbfcde2d1bf9526fdeaf5466977d77"

FORCE=0
ONLY=""
for a in "$@"; do
  case "$a" in
    --force) FORCE=1 ;;
    piper|whisper|model|llama|models|kwin) ONLY="$a" ;;
    -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $a (try --help)" >&2; exit 2 ;;
  esac
done
want() { [ -z "$ONLY" ] || [ "$ONLY" = "$1" ]; }
# Steps that download gigabytes or touch the desktop run only when named.
asked() { [ "$ONLY" = "$1" ]; }
have() { [ "$FORCE" -eq 0 ] && [ -e "$1" ]; }

# --- Platform ---------------------------------------------------------------
case "$(uname -s)" in
  Darwin) OS=macos ;;
  Linux)  OS=linux ;;
  *) echo "This script covers macOS and Linux. On Windows run scripts/setup.ps1." >&2; exit 2 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) ARCH=arm64 ;;
  x86_64|amd64)  ARCH=x64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 2 ;;
esac
PLATFORM="$OS-$ARCH"
echo "==> platform: $PLATFORM   root: $ROOT"

need() { command -v "$1" >/dev/null || { echo "missing required tool: $1${2:+  ($2)}" >&2; exit 1; }; }
need curl
need tar

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# fetch <url> <dest> <sha256>: nothing to do if a file with that hash is already in
# place; otherwise download beside it and move it in only once the hash matches.
fetch() {
  local url="$1" dest="$2" want="$3" got
  if [ "$FORCE" -eq 0 ] && [ -f "$dest" ] && [ "$(sha256 "$dest")" = "$want" ]; then
    return 0
  fi
  mkdir -p "$(dirname "$dest")"
  curl -fL --progress-bar "$url" -o "$dest.part"
  got="$(sha256 "$dest.part")"
  if [ "$got" != "$want" ]; then
    rm -f "$dest.part"
    echo "checksum mismatch for $url: expected $want, got $got" >&2
    exit 1
  fi
  mv "$dest.part" "$dest"
}

# --- 1. piper (text to speech) ------------------------------------------------
# Installed as a Python wheel, NOT from the GitHub release archive.
#
# `piper_macos_aarch64.tar.gz` (release 2023.11.14-2, the newest there is) is
# broken: it ships the `piper` and `piper_phonemize` executables but omits the
# .dylib files they link against, keeping only an onnxruntime .dSYM debug
# bundle. Extracting it and running piper gives:
#
#     dyld: Library not loaded: @rpath/libespeak-ng.1.dylib
#
# The `piper-tts` wheel is the maintained path and publishes binaries for
# macosx_11_0_arm64, macosx_10_9_x86_64, manylinux and win_amd64 -- so the same
# mechanism serves every platform, which is what makes dropping the vendored
# Windows binaries a one-line change later.
#
# It also carries its own phonemization, so nothing here redistributes or even
# downloads espeak-ng separately. That relocates the GPL obligation rather than
# removing it: the wheel is OHF-Voice/piper1-gpl and is GPL-3.0-or-later -- a
# different project from the MIT rhasspy/piper vendored at the repository root,
# despite the shared name. The gain is that the user installs it rather than
# this repository redistributing it. See THIRD-PARTY-NOTICES.md.
#
# The voice model (piper/en_US-amy-medium.onnx and its .json) is platform-neutral
# and fetched here too, checked against a pinned SHA-256. It used to be committed;
# its terms come from the dataset it was trained on, so the repository no longer
# redistributes it.
if want piper; then
  VENV="$ROOT/.venv"
  PIPER_BIN="$VENV/bin/piper"
  if have "$PIPER_BIN"; then
    echo "==> piper: already installed in .venv (use --force to reinstall)"
  else
    need python3
    echo "==> piper: installing the piper-tts wheel into .venv"
    [ -d "$VENV" ] || python3 -m venv "$VENV"
    # >=1.7.0 is a correctness floor, not a preference. The 1.6.1 arm64 macOS
    # wheel baked its build machine's espeak-ng data path into the compiled
    # extension, so every synthesis exits 0 and writes a 0-byte WAV
    # (OHF-Voice/piper1-gpl#272). Unpinned, --upgrade could resolve to it.
    "$VENV/bin/pip" install --quiet --upgrade 'piper-tts>=1.7.0'
    [ -x "$PIPER_BIN" ] || { echo "piper-tts installed but $PIPER_BIN is missing" >&2; exit 1; }
    echo "    -> .venv/bin/piper"
  fi
  echo "==> voice: en_US-amy-medium (about 60 MB the first time)"
  fetch "$VOICE_URL.onnx" "$ROOT/piper/en_US-amy-medium.onnx" "$VOICE_ONNX_SHA256"
  fetch "$VOICE_URL.onnx.json" "$ROOT/piper/en_US-amy-medium.onnx.json" "$VOICE_JSON_SHA256"
  # Prove it can actually speak, rather than only that a file exists. A voice
  # that fails at synthesis time degrades silently to the browser's TTS, which
  # looks like nothing being wrong at all.
  VOICE="$ROOT/piper/en_US-amy-medium.onnx"
  if [ -f "$VOICE" ]; then
    # Write to a real file and weigh it. /dev/null could not distinguish
    # "spoke" from "exited 0 and produced nothing", which is exactly how the
    # 1.6.1 arm64 failure presents.
    # Full-path template, not `mktemp -t`: BSD mktemp (macOS) treats -t's
    # argument as a prefix and appends its own suffix, GNU treats it as the
    # template. Spelling the path out behaves identically on both.
    CHECK_WAV="$(mktemp "${TMPDIR:-/tmp}/oracle-piper-check.XXXXXX")"
    if echo "test" | "$PIPER_BIN" --model "$VOICE" --output_file "$CHECK_WAV" >/dev/null 2>&1 \
       && [ -s "$CHECK_WAV" ]; then
      echo "    piper: synthesis OK"
    else
      echo "    WARNING: piper installed but could not synthesize with $VOICE" >&2
      echo "             (a 0-byte result usually means a piper-tts build whose" >&2
      echo "              espeak-ng data path is wrong -- see piper1-gpl#272)" >&2
    fi
    rm -f "$CHECK_WAV"
  else
    echo "    WARNING: voice model missing at piper/en_US-amy-medium.onnx" >&2
  fi
fi

# --- 2. whisper.cpp (speech in, wake word) ----------------------------------
# whisper.cpp publishes binaries for Windows and Ubuntu only -- there is no
# macOS release asset -- so a Mac builds from source. Metal for speed, SDL2
# because whisper-stream opens the microphone itself rather than going through
# the HUD.
if want whisper; then
  DEST="$ROOT/whisper/$PLATFORM"
  if have "$DEST/whisper-cli"; then
    echo "==> whisper: already at whisper/$PLATFORM (use --force to refetch)"
  else
    need cmake "macOS: brew install cmake"
    need git
    echo "==> whisper: building from source ($WHISPER_TAG); no macOS binaries are published"
    if [ "$OS" = macos ] && ! (pkg-config --exists sdl2 2>/dev/null || [ -d /opt/homebrew/include/SDL2 ] || [ -d /usr/local/include/SDL2 ]); then
      echo "    NOTE: SDL2 not found. whisper-stream (the wake word) needs it." >&2
      echo "          brew install sdl2, then re-run: scripts/setup.sh whisper" >&2
    fi
    SRC="$ROOT/.build/whisper.cpp"
    mkdir -p "$(dirname "$SRC")"
    if [ ! -d "$SRC/.git" ]; then
      git clone --depth 1 --branch "$WHISPER_TAG" https://github.com/ggml-org/whisper.cpp "$SRC"
    fi
    ( cd "$SRC"
      cmake -B build -DCMAKE_BUILD_TYPE=Release -DWHISPER_SDL2=ON >/dev/null
      cmake --build build --config Release -j"$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)" >/dev/null )
    mkdir -p "$DEST"
    # Copy what the profiles actually name, plus the shared libs they need.
    for f in whisper-cli whisper-stream; do
      [ -f "$SRC/build/bin/$f" ] && cp "$SRC/build/bin/$f" "$DEST/" \
        || echo "    WARNING: $f was not built (SDL2 missing?)" >&2
    done
    find "$SRC/build" -name "*.dylib" -o -name "*.so*" | while read -r lib; do
      cp "$lib" "$DEST/" 2>/dev/null || true
    done
    echo "    -> whisper/$PLATFORM/"
  fi
fi

# --- 3. The whisper model ---------------------------------------------------
# ~148 MB, and gitignored by the repository's own *.bin rule, so it has never
# been in the clone on any platform: STT and the wake word could not work from
# a fresh checkout on Windows either.
if want model; then
  DEST="$ROOT/whisper/models"
  if have "$DEST/$WHISPER_MODEL"; then
    echo "==> model: already at whisper/models/$WHISPER_MODEL"
  else
    echo "==> model: fetching $WHISPER_MODEL (~148 MB)"
    mkdir -p "$DEST"
    curl -fL --progress-bar \
      "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$WHISPER_MODEL" \
      -o "$DEST/$WHISPER_MODEL.part"
    mv "$DEST/$WHISPER_MODEL.part" "$DEST/$WHISPER_MODEL"
    echo "    -> whisper/models/$WHISPER_MODEL"
  fi
fi

# --- 4. llama.cpp (inference) -----------------------------------------------
# Built, not downloaded: the backend is chosen at compile time and no published
# binary matches every machine. Metal on macOS; Vulkan on Linux when its shader
# compiler is present, because it covers AMD, NVIDIA and Intel with one build and
# no ROCm install. Without a GPU flag llama.cpp builds CPU-only, which runs, and
# runs a 14B at a few tokens a second -- a slow assistant rather than an error.
if want llama; then
  if have "$ROOT/llama.cpp/build/bin/llama-server"; then
    echo "==> llama.cpp: already built"
  else
    need cmake "macOS: brew install cmake"
    need git
    echo "==> llama.cpp: cloning and building"
    [ -d "$ROOT/llama.cpp/.git" ] || git clone --depth 1 https://github.com/ggml-org/llama.cpp "$ROOT/llama.cpp"
    ( cd "$ROOT/llama.cpp"
      GPU=""
      [ "$OS" = macos ] && GPU="-DGGML_METAL=ON"
      if [ "$OS" = linux ]; then
        if command -v glslc >/dev/null && pkg-config --exists vulkan 2>/dev/null; then
          GPU="-DGGML_VULKAN=ON"
        else
          echo "    WARNING: no Vulkan SDK (glslc + vulkan headers); building CPU-only." >&2
          echo "             Arch: pacman -S vulkan-headers shaderc; Debian: libvulkan-dev glslc" >&2
        fi
      fi
      cmake -B build -DCMAKE_BUILD_TYPE=Release $GPU >/dev/null
      cmake --build build --config Release -j"$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)" >/dev/null )
    echo "    -> llama.cpp/build/bin/llama-server"
  fi
fi

# --- 5. GGUF models (on request) ---------------------------------------------
# The three models deploy/oracle.linux.toml names, from their publishers, pinned
# by hash like the voice. ~10 GB in all, so only on `setup.sh models`.
fetch_gguf() { # repo file sha256
  local dest="$ROOT/oracle-models/$2"
  if have "$dest"; then echo "    $2: already present"; return; fi
  echo "    $2"
  mkdir -p "$ROOT/oracle-models"
  curl -fL -C - --progress-bar "https://huggingface.co/$1/resolve/main/$2" -o "$dest.part"
  local got
  got="$( (sha256sum "$dest.part" 2>/dev/null || shasum -a 256 "$dest.part") | cut -d' ' -f1)"
  if [ "$got" != "$3" ]; then
    echo "    $2: sha256 mismatch (got $got); kept as $dest.part, not installed" >&2
    exit 1
  fi
  mv "$dest.part" "$dest"
}
if asked models; then
  echo "==> models: fetching into oracle-models/"
  fetch_gguf bartowski/Qwen2.5-14B-Instruct-GGUF Qwen2.5-14B-Instruct-Q4_K_M.gguf \
    e47ad95dad6ff848b431053b375adb5d39321290ea2c638682577dafca87c008
  # A vision model is blind without its projector; both, always.
  fetch_gguf Qwen/Qwen3-VL-2B-Instruct-GGUF Qwen3VL-2B-Instruct-Q4_K_M.gguf \
    089d75c52f4b7ffc56ba998ffc50aae89fcafc755f9e7208aacca281dca6c2ae
  fetch_gguf Qwen/Qwen3-VL-2B-Instruct-GGUF mmproj-Qwen3VL-2B-Instruct-F16.gguf \
    c3d5afbef5287953acd57b4043d2269456e5761a4eaccb3b71b062996970aea5
  fetch_gguf CompendiumLabs/bge-small-en-v1.5-gguf bge-small-en-v1.5-q8_0.gguf \
    ec38e8da142596baa913124ae50550de284b6916bf59577ef2f0cb9660c2f514
fi

# --- 6. KWin screenshot permission (Linux/KDE, on request) -------------------
# KWin's ScreenShot2 interface is restricted: it answers only executables named
# by an installed .desktop file that lists the interface, matched against
# /proc/<pid>/exe. This is the same grant Spectacle has. Without it the ambient
# index gets "KWin refused the screenshot" every cycle and indexes nothing.
#
#   scripts/setup.sh kwin                      authorizes the actd core will run:
#                                              ~/.local/bin/oracle-actd if installed
#                                              (systemd), else the checkout's release build
#   ORACLE_ACTD=/path/to/oracle-actd scripts/setup.sh kwin
if asked kwin; then
  [ "$OS" = linux ] || { echo "kwin: Linux only" >&2; exit 2; }
  if [ -n "${ORACLE_ACTD:-}" ]; then
    ACTD="$ORACLE_ACTD"
  elif [ -x "$HOME/.local/bin/oracle-actd" ]; then
    ACTD="$HOME/.local/bin/oracle-actd"
  else
    # `oracle-core run` launches the actd sitting next to its own binary.
    ACTD="$ROOT/oracle-v37/target/release/oracle-actd"
  fi
  case "$ACTD" in /*) ;; *) ACTD="$CALLER_PWD/$ACTD" ;; esac
  ACTD="$(readlink -f "$ACTD" 2>/dev/null || echo "$ACTD")"
  [ -x "$ACTD" ] || echo "    NOTE: $ACTD does not exist yet; the grant applies once it does" >&2
  APPS="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
  mkdir -p "$APPS"
  cat > "$APPS/oracle-actd.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Oracle of Delphi actuator
Comment=Lets oracle-actd capture windows for the ambient index
Exec=$ACTD
NoDisplay=true
X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2
DESKTOP
  command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true
  echo "==> kwin: $APPS/oracle-actd.desktop authorizes $ACTD"
  echo "    The path must be exact: re-run this if oracle-actd moves. KWin notices the"
  echo "    new entry within a few seconds; then check it with: $ACTD --check-capture"
fi

echo
echo "==> setup complete for $PLATFORM"
echo
echo "Still yours to choose: the GGUF models under oracle-models/ (the planner,"
echo "and optionally the vision tier and embedder). \`scripts/setup.sh models\` fetches"
echo "the set deploy/oracle.linux.toml expects; see docs/MACOS.md section 2 to pick others."

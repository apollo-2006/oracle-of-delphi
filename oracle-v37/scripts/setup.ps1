# setup.ps1 -- fetch the platform-specific runtime dependencies on Windows.
#
#   .\scripts\setup.ps1                 everything missing
#   .\scripts\setup.ps1 -Force          re-fetch even if present
#   .\scripts\setup.ps1 -Only piper     one component (piper|whisper|model|llama)
#
# If PowerShell refuses to run this:
#   powershell -ExecutionPolicy Bypass -File .\scripts\setup.ps1
#
# The counterpart of scripts/setup.sh, installing to the SAME layout so one
# oracle.toml serves both platforms via ${ORACLE_ROOT} and ${ORACLE_PLATFORM}:
#
#   .venv\Scripts\piper.exe                  piper-tts wheel
#   whisper\windows-x64\whisper-cli.exe      whisper.cpp release binaries
#   whisper\models\ggml-base.en.bin          the model (gitignored, never committed)
#   llama.cpp\build\bin\Release\             built locally
#
# Once this runs cleanly, the piper\ and whisper\ binaries committed at the
# repository root are redundant and can be deleted -- which also removes the
# GPL-3.0 espeak-ng files this repository currently redistributes.
param(
    [switch]$Force,
    [ValidateSet('piper','whisper','model','llama')]
    [string]$Only
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = (Resolve-Path (Join-Path $here "..\..")).Path   # scripts\ -> oracle-v37\ -> repo root

$PiperTag    = "2023.11.14-2"
$WhisperTag  = "b4938"
$WhisperModel = "ggml-base.en.bin"

# Windows on ARM exists, but whisper.cpp publishes no arm64 build, so x64 (under
# emulation) is the only option that actually works today.
$Platform = "windows-x64"
Write-Host "==> platform: $Platform   root: $root"

function Want([string]$name) { return (-not $Only) -or ($Only -eq $name) }
function Have([string]$path) { return (-not $Force) -and (Test-Path $path) }
function Need([string]$cmd, [string]$hint) {
    if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
        throw "missing required tool: $cmd$(if ($hint) { "  ($hint)" })"
    }
}

# --- 1. piper (text to speech) ------------------------------------------------
# The wheel, not the GitHub release archive -- same reasoning as setup.sh, and
# it keeps both platforms on one mechanism. piper-tts publishes a win_amd64
# wheel, and it carries its own phonemization, so nothing here fetches espeak-ng
# separately. Note that this relocates the GPL rather than escaping it: the wheel
# is OHF-Voice/piper1-gpl, GPL-3.0-or-later, and is a different project from the
# MIT-licensed rhasspy/piper whose binaries are vendored at the repository root.
# What changes is that the user installs it instead of this repository shipping
# it. See THIRD-PARTY-NOTICES.md.
if (Want 'piper') {
    $venv  = Join-Path $root ".venv"
    $piper = Join-Path $venv "Scripts\piper.exe"
    if (Have $piper) {
        Write-Host "==> piper: already installed in .venv (use -Force to reinstall)"
    } else {
        Need python "install Python 3 from python.org or the Microsoft Store"
        Write-Host "==> piper: installing the piper-tts wheel into .venv"
        if (-not (Test-Path $venv)) { & python -m venv $venv }
        # >=1.7.0 is a correctness floor: the 1.6.1 arm64 macOS wheel baked its
        # build machine's espeak-ng data path into the extension and synthesizes
        # 0-byte WAVs (OHF-Voice/piper1-gpl#272). Pinned on both platforms so the
        # two scripts cannot drift apart on which versions are acceptable.
        & (Join-Path $venv "Scripts\pip.exe") install --quiet --upgrade 'piper-tts>=1.7.0'
        if (-not (Test-Path $piper)) { throw "piper-tts installed but $piper is missing" }
        Write-Host "    -> .venv\Scripts\piper.exe"
    }
    # Prove it can synthesize. A voice that fails at run time degrades silently
    # to the browser's TTS, which looks like nothing being wrong at all.
    $voice = Join-Path $root "piper\en_US-amy-medium.onnx"
    if (Test-Path $voice) {
        # Weigh the output rather than trusting the call. A native executable
        # exiting non-zero does not throw in PowerShell, so the catch below never
        # fired -- and a 0-byte WAV (the piper1-gpl#272 failure) is written by a
        # process that exits 0, so neither signal was being checked at all.
        $check = Join-Path $env:TEMP "oracle-piper-check.wav"
        Remove-Item $check -ErrorAction SilentlyContinue
        try {
            "test" | & $piper --model $voice --output_file $check *> $null
            $wrote = (Test-Path $check) -and ((Get-Item $check).Length -gt 0)
            if ($LASTEXITCODE -eq 0 -and $wrote) {
                Write-Host "    piper: synthesis OK"
            } else {
                Write-Warning "piper installed but could not synthesize with $voice"
                Write-Warning "  (a 0-byte result usually means a piper-tts build whose espeak-ng data path is wrong -- see piper1-gpl#272)"
            }
        } catch {
            Write-Warning "piper installed but could not synthesize with $voice"
        } finally {
            Remove-Item $check -ErrorAction SilentlyContinue
        }
    } else {
        Write-Warning "voice model missing at piper\en_US-amy-medium.onnx"
    }
}

# --- 2. whisper.cpp (speech in, wake word) ------------------------------------
# Windows is the platform whisper.cpp DOES publish binaries for, so unlike macOS
# this is a download rather than a build.
if (Want 'whisper') {
    $dest = Join-Path $root "whisper\$Platform"
    if (Have (Join-Path $dest "whisper-cli.exe")) {
        Write-Host "==> whisper: already at whisper\$Platform (use -Force to refetch)"
    } else {
        Write-Host "==> whisper: fetching whisper-bin-x64.zip ($WhisperTag)"
        $zip = Join-Path $env:TEMP "whisper-bin-x64.zip"
        $url = "https://github.com/ggml-org/whisper.cpp/releases/download/$WhisperTag/whisper-bin-x64.zip"
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
        $stage = Join-Path $env:TEMP "whisper-stage"
        if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
        Expand-Archive -Path $zip -DestinationPath $stage -Force
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        # The archive nests under a Release\ directory in some builds and not in
        # others, so take every .exe/.dll wherever it landed.
        Get-ChildItem -Path $stage -Recurse -Include *.exe,*.dll |
            ForEach-Object { Copy-Item $_.FullName -Destination $dest -Force }
        Remove-Item $zip, $stage -Recurse -Force -ErrorAction SilentlyContinue
        if (-not (Test-Path (Join-Path $dest "whisper-cli.exe"))) {
            throw "whisper-cli.exe not found after extracting $url"
        }
        if (-not (Test-Path (Join-Path $dest "whisper-stream.exe"))) {
            Write-Warning "whisper-stream.exe is not in this release archive; the wake word will not work. Build whisper.cpp with -DWHISPER_SDL2=ON to get it."
        }
        Write-Host "    -> whisper\$Platform\"
    }
}

# --- 3. The whisper model -----------------------------------------------------
# ~148 MB, matched by the repository's own *.bin ignore rule, so it has never
# been in a clone on ANY platform: STT and the wake word could not work from a
# fresh checkout on Windows either.
if (Want 'model') {
    $dest = Join-Path $root "whisper\models"
    $out  = Join-Path $dest $WhisperModel
    if (Have $out) {
        Write-Host "==> model: already at whisper\models\$WhisperModel"
    } else {
        Write-Host "==> model: fetching $WhisperModel (~148 MB)"
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        $url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$WhisperModel"
        # Invoke-WebRequest buffers the whole body in memory with the progress
        # bar on; disabling it also makes large downloads dramatically faster.
        $prev = $ProgressPreference; $ProgressPreference = 'SilentlyContinue'
        try   { Invoke-WebRequest -Uri $url -OutFile "$out.part" -UseBasicParsing }
        finally { $ProgressPreference = $prev }
        Move-Item "$out.part" $out -Force
        Write-Host "    -> whisper\models\$WhisperModel"
    }
}

# --- 4. llama.cpp (inference) -------------------------------------------------
# Built, not downloaded: the backend is a compile-time choice (Vulkan or ROCm
# here, Metal on macOS) and no published binary matches every machine. See
# docs\WINDOWS.md for the backend flags; this builds the portable CPU/Vulkan
# default.
if (Want 'llama') {
    $server = Join-Path $root "llama.cpp\build\bin\Release\llama-server.exe"
    if (Have $server) {
        Write-Host "==> llama.cpp: already built"
    } else {
        Need cmake "winget install Kitware.CMake"
        Need git   "winget install Git.Git"
        Write-Host "==> llama.cpp: cloning and building"
        $src = Join-Path $root "llama.cpp"
        if (-not (Test-Path (Join-Path $src ".git"))) {
            & git clone --depth 1 https://github.com/ggml-org/llama.cpp $src
        }
        Push-Location $src
        try {
            & cmake -B build -DCMAKE_BUILD_TYPE=Release | Out-Null
            & cmake --build build --config Release -j | Out-Null
        } finally { Pop-Location }
        Write-Host "    -> llama.cpp\build\bin\Release\llama-server.exe"
    }
}

Write-Host ""
Write-Host "==> setup complete for $Platform"
Write-Host ""
Write-Host "Still yours to choose: the GGUF models under oracle-models\ (the planner,"
Write-Host "and optionally the vision tier and embedder). See docs\WINDOWS.md."

# build-app.ps1 — assemble the Oracle as a native app.
#
# Builds oracle-core + oracle-actd (the backend) and oracle-of-delphi.exe (the
# native window), then copies the backend binaries next to the shell so the whole
# thing lives in ONE folder. After this, double-clicking oracle-of-delphi.exe
# brings up the LLM, the daemon, the HUD, and a real window — no browser, no
# terminals. Re-run after any code change.

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Resolve-Path (Join-Path $here "..")   # repo root

Write-Host "[oracle] building backend (oracle-core + oracle-actd)..." -ForegroundColor Yellow
Push-Location $root
try { cargo build --release -p oracle-core -p oracle-actd } finally { Pop-Location }

Write-Host "[oracle] building the native shell (oracle-of-delphi.exe)..." -ForegroundColor Yellow
Push-Location $here
try { cargo build --release } finally { Pop-Location }

$shellDir = Join-Path $here "target\release"
Copy-Item (Join-Path $root "target\release\oracle-core.exe") $shellDir -Force
Copy-Item (Join-Path $root "target\release\oracle-actd.exe") $shellDir -Force

Write-Host ""
Write-Host "[oracle] Done. Your app is:" -ForegroundColor Green
Write-Host "         $shellDir\oracle-of-delphi.exe" -ForegroundColor Green
Write-Host "         Double-click it (or make a desktop shortcut / drop one in shell:startup)." -ForegroundColor Green
Write-Host "         Summon/dismiss anytime with Ctrl+Alt+O, or from the tray sun." -ForegroundColor Green

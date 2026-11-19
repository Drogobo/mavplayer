# Builds the Windows release: dist\MavPlayer-<ver>-windows-x64.zip (portable)
# and dist\MavPlayer-<ver>-windows-x64-setup.exe (Inno Setup installer).
#
# Usage (PowerShell, from the repository root):
#   scripts\package-windows.ps1 [-FfmpegDir C:\path\to\ffmpeg-shared]
#
# Without -FfmpegDir (or $env:FFMPEG_DIR) a GPL shared build of FFmpeg 9.0 is
# downloaded from https://github.com/BtbN/FFmpeg-Builds. Needs: Rust (MSVC),
# LLVM (for bindgen: $env:LIBCLANG_PATH) and, for the installer, Inno Setup 6.
param([string]$FfmpegDir = $env:FFMPEG_DIR)
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches.Groups[1].Value
$dist = Join-Path (Get-Location) "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null

if (-not $FfmpegDir) {
    $name = "ffmpeg-n9.0-latest-win64-gpl-shared-9.0"
    $FfmpegDir = Join-Path (Get-Location) "ffmpeg-build\$name"
    if (-not (Test-Path $FfmpegDir)) {
        New-Item -ItemType Directory -Force -Path "ffmpeg-build" | Out-Null
        $zip = "ffmpeg-build\$name.zip"
        Write-Host ">> downloading FFmpeg ($name)"
        Invoke-WebRequest -Uri "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/$name.zip" -OutFile $zip
        Expand-Archive -Path $zip -DestinationPath "ffmpeg-build" -Force
    }
}
$env:FFMPEG_DIR = (Resolve-Path $FfmpegDir).Path
Write-Host ">> FFMPEG_DIR = $env:FFMPEG_DIR"

Write-Host ">> building mavplayer $version"
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$stage = Join-Path $dist "MavPlayer"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item target\release\mavplayer.exe $stage
Copy-Item (Join-Path $env:FFMPEG_DIR "bin\*.dll") $stage
Copy-Item LICENSE, README.md $stage
@"
MavPlayer $version for Windows

Run mavplayer.exe. Everything it needs is in this folder; settings are kept in
%APPDATA%\mavplayer.

MavPlayer is free software under the GNU GPL v3 or later (see LICENSE);
source: https://github.com/Drogobo/mavplayer. The bundled FFmpeg libraries
(av*.dll, sw*.dll) are from https://github.com/BtbN/FFmpeg-Builds, also GPL v3;
their source code is available at https://ffmpeg.org.
"@ | Set-Content -Encoding UTF8 (Join-Path $stage "README-WINDOWS.txt")

$zipOut = Join-Path $dist "MavPlayer-$version-windows-x64.zip"
if (Test-Path $zipOut) { Remove-Item $zipOut }
Compress-Archive -Path "$stage\*" -DestinationPath $zipOut
Write-Host ">> $zipOut"

$iscc = Get-Command iscc -ErrorAction SilentlyContinue
if (-not $iscc) { $iscc = Get-Item "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" -ErrorAction SilentlyContinue }
if ($iscc) {
    & $iscc.Source "/DAppVersion=$version" "/DSourceDir=$stage" packaging\windows\mavplayer.iss
    if ($LASTEXITCODE -ne 0) { throw "Inno Setup failed" }
} else {
    Write-Warning "Inno Setup (iscc) not found; skipping the installer"
}

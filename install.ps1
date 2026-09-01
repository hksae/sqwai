# Install/build script for Windows.
# Builds sqwai in release mode and copies the binary so `sqwai` is callable
# from any directory. Default install dir is ~/.cargo/bin (on PATH with
# rustup); override with $env:SQWAI_INSTALL_DIR.
#
# Usage (from the repository root):
#   powershell -ExecutionPolicy Bypass -File .\install.ps1

$ErrorActionPreference = "Stop"

if (-not (Test-Path "Cargo.toml")) {
    Write-Error "run this script from the sqwai repository root"
    exit 1
}

$bin = if ($env:SQWAI_INSTALL_DIR) {
    $env:SQWAI_INSTALL_DIR
} else {
    Join-Path $env:USERPROFILE ".cargo\bin"
}

Write-Host "building sqwai (release)..."
cargo build --release
if ($LASTEXITCODE -ne 0) {
    Write-Error "build failed"
    exit 1
}

New-Item -ItemType Directory -Force -Path $bin | Out-Null
$src = Join-Path (Get-Location) "target\release\sqwai.exe"
$dst = Join-Path $bin "sqwai.exe"
Copy-Item $src $dst -Force

Write-Host "installed to $dst"
& $dst --version
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host ""
Write-Host "sqwai is now available from any directory; run it inside a project with:"
Write-Host "  sqwai"
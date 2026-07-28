<#
.SYNOPSIS
  Build cross-review.exe and stage it in dist/ for committing.

.DESCRIPTION
  Runs fmt, clippy and the tests, then builds the release binary and copies it to
  dist/cross-review.exe. That copy is committed so consuming repositories can vendor
  the executable and need nothing installed.
#>
[CmdletBinding()]
param(
    # Skip fmt/clippy/tests and just build.
    [switch]$SkipChecks
)

# Deliberately not 'Stop': cargo writes ordinary progress to stderr, and under
# Windows PowerShell 5.1 that would be promoted to a terminating error. Each step is
# gated on $LASTEXITCODE instead, which is the actual signal.
$ErrorActionPreference = 'Continue'
Set-Location $PSScriptRoot

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoHome = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path (Join-Path $cargoHome 'cargo.exe')) {
        $env:PATH = "$cargoHome;$env:PATH"
    }
    else {
        throw "cargo was not found. Install Rust from https://rustup.rs and retry."
    }
}

if (-not $SkipChecks) {
    Write-Host '==> cargo fmt --check' -ForegroundColor Cyan
    cargo fmt --check
    if ($LASTEXITCODE -ne 0) { throw "formatting check failed; run 'cargo fmt'" }

    Write-Host '==> cargo clippy' -ForegroundColor Cyan
    cargo clippy --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'clippy reported problems' }

    Write-Host '==> cargo test' -ForegroundColor Cyan
    cargo test
    if ($LASTEXITCODE -ne 0) { throw 'tests failed' }
}

Write-Host '==> cargo build --release' -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'release build failed' }

$built = Join-Path $PSScriptRoot 'target\release\cross-review.exe'
if (-not (Test-Path $built)) { throw "expected binary not found at $built" }

$distDir = Join-Path $PSScriptRoot 'dist'
if (-not (Test-Path $distDir)) { New-Item -ItemType Directory -Path $distDir | Out-Null }
$dist = Join-Path $distDir 'cross-review.exe'
Copy-Item $built $dist -Force

$size = [math]::Round((Get-Item $dist).Length / 1KB)
Write-Host ''
Write-Host "Staged $dist ($size KB)" -ForegroundColor Green
& $dist --version
Write-Host ''
Write-Host 'To vendor into a project:  copy dist\cross-review.exe <project>\tools\'

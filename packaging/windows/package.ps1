# Builds the release executable and zips it with a short README, the licences
# and the third-party notices.
#
#     powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1
#     powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1 -Target x86_64-pc-windows-msvc
#
# The icon is already embedded in strata.exe by crates\strata-app\build.rs, so
# the zip needs no extra icon files. Build on Windows: GPUI compiles its
# shaders with fxc.exe, which only the Windows SDK provides.
param(
    [string]$Target = "",
    [string]$Notices = "",
    [switch]$NoBuild
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
Set-Location $root

# The dev shell shares one target directory and cross builds add a triple
# directory, so the binary is not necessarily under .\target\release.
$targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root "target" }
$release = if ($Target) { Join-Path $targetRoot "$Target\release" } else { Join-Path $targetRoot "release" }

if (-not $NoBuild) {
    $buildArgs = @("build", "--release", "--locked", "-p", "strata-app")
    if ($Target) { $buildArgs += @("--target", $Target) }
    & cargo @buildArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}

if (-not (Test-Path (Join-Path $release "strata.exe"))) {
    throw "no binary at $release\strata.exe; build it or drop -NoBuild"
}

$metadata = cargo metadata --no-deps --format-version 1 --quiet | ConvertFrom-Json
$version = ($metadata.packages | Where-Object name -eq "strata-app" | Select-Object -First 1).version

if (-not $Notices) { $Notices = Join-Path $root "dist\THIRD-PARTY-NOTICES.md" }
if (-not (Test-Path $Notices)) {
    throw "third-party notices not found at $Notices; run packaging/third-party/generate.sh first"
}

$dist = Join-Path $root "dist"
$stage = Join-Path $dist "windows-x64"
$zip = Join-Path $dist "Strata-$version-windows-x64.zip"

New-Item -ItemType Directory -Force -Path $dist | Out-Null
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item (Join-Path $release "strata.exe") $stage
Copy-Item (Join-Path $PSScriptRoot "README.txt") $stage
Copy-Item (Join-Path $root "LICENSE-MIT") $stage
Copy-Item (Join-Path $root "LICENSE-APACHE") $stage
Copy-Item $Notices (Join-Path $stage "THIRD-PARTY-NOTICES.md")

if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Remove-Item -Recurse -Force $stage

Write-Host "wrote $zip"

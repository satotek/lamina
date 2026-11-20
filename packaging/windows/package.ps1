# Builds the release executable and zips it with a short README, the licences
# and the third-party notices. -Installer additionally compiles the Inno Setup
# installer dist\Lamina-<version>-windows-x64-setup.exe from the same staged
# files, so both artifacts always carry identical bits.
#
#     powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1
#     powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1 -Target x86_64-pc-windows-msvc
#     powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1 -Installer
#
# -Installer needs iscc (Inno Setup 6.3 or newer) on PATH or in
# C:\Program Files (x86)\Inno Setup 6. The icon is already embedded in
# lamina.exe by crates\lamina-app\build.rs, so the zip needs no extra icon
# files. Build on Windows: GPUI compiles its shaders with fxc.exe, which only
# the Windows SDK provides.
param(
    [string]$Target = "",
    [string]$Notices = "",
    [switch]$NoBuild,
    [switch]$Installer
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
Set-Location $root

# The dev shell shares one target directory and cross builds add a triple
# directory, so the binary is not necessarily under .\target\release.
$targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root "target" }
$release = if ($Target) { Join-Path $targetRoot "$Target\release" } else { Join-Path $targetRoot "release" }

if (-not $NoBuild) {
    $buildArgs = @("build", "--release", "--locked", "-p", "lamina-app")
    if ($Target) { $buildArgs += @("--target", $Target) }
    & cargo @buildArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}

if (-not (Test-Path (Join-Path $release "lamina.exe"))) {
    throw "no binary at $release\lamina.exe; build it or drop -NoBuild"
}

$metadata = cargo metadata --no-deps --format-version 1 --quiet | ConvertFrom-Json
$version = ($metadata.packages | Where-Object name -eq "lamina-app" | Select-Object -First 1).version

if (-not $Notices) { $Notices = Join-Path $root "dist\THIRD-PARTY-NOTICES.md" }
if (-not (Test-Path $Notices)) {
    throw "third-party notices not found at $Notices; run packaging/third-party/generate.sh first"
}

$dist = Join-Path $root "dist"
$stage = Join-Path $dist "windows-x64"
$zip = Join-Path $dist "Lamina-$version-windows-x64.zip"
$setup = Join-Path $dist "Lamina-$version-windows-x64-setup.exe"

New-Item -ItemType Directory -Force -Path $dist | Out-Null
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item (Join-Path $release "lamina.exe") $stage
Copy-Item (Join-Path $PSScriptRoot "README.txt") $stage
Copy-Item (Join-Path $root "LICENSE-MIT") $stage
Copy-Item (Join-Path $root "LICENSE-APACHE") $stage
Copy-Item $Notices (Join-Path $stage "THIRD-PARTY-NOTICES.md")

if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip

if ($Installer) {
    # Package managers put iscc on PATH; a manual install does not, but its
    # location is fixed.
    $iscc = (Get-Command "iscc.exe" -ErrorAction SilentlyContinue).Source
    if (-not $iscc) {
        $candidate = Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"
        if (Test-Path $candidate) { $iscc = $candidate }
    }
    if (-not $iscc) {
        throw "iscc not found on PATH or at $candidate; install Inno Setup 6 or drop -Installer"
    }

    if (Test-Path $setup) { Remove-Item -Force $setup }
    & $iscc "/DAppVersion=$version" "/DStageDir=$stage" "/O$dist" "/FLamina-$version-windows-x64-setup" (Join-Path $PSScriptRoot "lamina.iss")
    if ($LASTEXITCODE -ne 0) { throw "iscc failed" }
    if (-not (Test-Path $setup)) { throw "iscc wrote no setup at $setup" }
}

# The stage only exists to feed both the zip and the installer, so it can go
# once they are built.
Remove-Item -Recurse -Force $stage

Write-Host "wrote $zip"
if ($Installer) { Write-Host "wrote $setup" }

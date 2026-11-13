# Builds the release executable and zips it with a short README.
#
#     powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1
#
# The icon is already embedded in strata.exe by crates\strata-app\build.rs, so
# the zip needs no extra icon files. Build on Windows: GPUI compiles its
# shaders with fxc.exe, which only the Windows SDK provides.
$ErrorActionPreference = "Stop"
Set-Location (Resolve-Path (Join-Path $PSScriptRoot "..\.."))

cargo build --release -p strata-app
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$metadata = cargo metadata --no-deps --format-version 1 --quiet | ConvertFrom-Json
$version = ($metadata.packages | Where-Object name -eq "strata-app" | Select-Object -First 1).version

$dist = Join-Path (Get-Location) "dist"
$stage = Join-Path $dist "strata"
$zip = Join-Path $dist "strata-$version-windows-x86_64.zip"

New-Item -ItemType Directory -Force -Path $dist | Out-Null
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item (Join-Path (Get-Location) "target\release\strata.exe") $stage
Copy-Item (Join-Path $PSScriptRoot "README.txt") $stage

if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Remove-Item -Recurse -Force $stage

Write-Host "wrote $zip"

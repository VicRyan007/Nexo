# Windows packaging script for Nexo
param(
    [string]$Version = "0.1.0",
    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"

Write-Host "==> Building Nexo release binary for Windows x86_64..."
$mingw = 'C:\Users\Ryan\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin'
if (Test-Path $mingw) {
    $env:PATH = "$mingw;$env:USERPROFILE\.cargo\bin;$env:PATH"
    cargo +1.97.1-x86_64-pc-windows-gnu build --release -p nexo-app
} else {
    cargo build --release -p nexo-app
}

$DistPath = Join-Path $PSScriptRoot "..\$OutDir"
if (!(Test-Path $DistPath)) {
    New-Item -ItemType Directory -Path $DistPath -Force | Out-Null
}

$BinaryPath = Join-Path $PSScriptRoot "..\target\release\nexo.exe"
if (!(Test-Path $BinaryPath)) {
    throw "Build failed: binary not found at $BinaryPath"
}

$ZipName = "nexo-$Version-windows-x86_64.zip"
$ZipPath = Join-Path $DistPath $ZipName

Write-Host "==> Creating portable release archive: $ZipPath"
$TempStage = Join-Path $env:TEMP "nexo-stage-$Version"
if (Test-Path $TempStage) { Remove-Item -Recurse -Force $TempStage }
New-Item -ItemType Directory -Path $TempStage -Force | Out-Null

Copy-Item $BinaryPath -Destination (Join-Path $TempStage "nexo.exe")
Copy-Item (Join-Path $PSScriptRoot "..\README.md") -Destination (Join-Path $TempStage "README.md")

if (Test-Path $ZipPath) { Remove-Item -Force $ZipPath }
Compress-Archive -Path "$TempStage\*" -DestinationPath $ZipPath -Force
Remove-Item -Recurse -Force $TempStage

Write-Host "==> Windows package created successfully: $ZipPath"

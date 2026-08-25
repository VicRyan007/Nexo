# Windows packaging script for Nexo
param(
    [string]$Version = "1.0.0",
    [string]$OutDir = "dist",
    [string]$Toolchain = ""
)

$ErrorActionPreference = "Stop"
$Version = $Version.TrimStart('v')
if ([string]::IsNullOrWhiteSpace($Version)) { throw "Version must not be empty" }

Write-Host "==> Building Nexo release binary for Windows x86_64..."
$ToolchainArgs = @()
if ($Toolchain) {
    $ToolchainArgs = @("+$Toolchain")
}
& cargo @ToolchainArgs build --release -p nexo-app
if ($LASTEXITCODE -ne 0) { throw "Release build failed" }

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$DistPath = if ([System.IO.Path]::IsPathRooted($OutDir)) {
    $OutDir
} else {
    Join-Path $RepoRoot $OutDir
}
New-Item -ItemType Directory -Path $DistPath -Force | Out-Null
$DistPath = (Resolve-Path -LiteralPath $DistPath).Path

$targetRoot = if ($env:CARGO_TARGET_DIR) {
    $env:CARGO_TARGET_DIR
} else {
    Join-Path $PSScriptRoot "..\target"
}
$BinaryPath = Join-Path $targetRoot "release\nexo.exe"
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
Copy-Item (Join-Path $PSScriptRoot "..\LICENSE") -Destination (Join-Path $TempStage "LICENSE")

if (Test-Path $ZipPath) { Remove-Item -Force $ZipPath }
$ItemsToZip = (Get-ChildItem -Path $TempStage).FullName
Compress-Archive -Path $ItemsToZip -DestinationPath $ZipPath -Force
Remove-Item -Recurse -Force $TempStage

Write-Host "==> Windows package created successfully: $ZipPath"

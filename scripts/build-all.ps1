# Nexo All-in-One Build, Test, and Packaging Script for Windows
param(
    [string]$Version = "1.0.0",
    [string]$OutDir = "dist",
    [string]$Toolchain = ""
)

$ErrorActionPreference = "Stop"
$Version = $Version.TrimStart('v')
if ([string]::IsNullOrWhiteSpace($Version)) { throw "Version must not be empty" }
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

Write-Host "======================================================" -ForegroundColor Cyan
Write-Host "   Nexo v$Version - Pipeline de Build e Verificacao   " -ForegroundColor Cyan
Write-Host "======================================================" -ForegroundColor Cyan

$ToolchainArgs = @()
if ($Toolchain) {
    $ToolchainArgs = @("+$Toolchain")
}

# 1. Format Check
Write-Host "`n[1/4] Verificando formatacao de codigo (cargo fmt)..." -ForegroundColor Yellow
& cargo @ToolchainArgs fmt --all --check
if ($LASTEXITCODE -ne 0) { throw "Falha na formatacao de codigo" }
Write-Host " -> Formatacao OK!" -ForegroundColor Green

# 2. Strict Clippy Lints
Write-Host "`n[2/4] Executando analise estatica rigorosa (cargo clippy)..." -ForegroundColor Yellow
& cargo @ToolchainArgs clippy --workspace --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) { throw "Falha no Clippy" }
Write-Host " -> Clippy 0 warnings OK!" -ForegroundColor Green

# 3. Test Suite
Write-Host "`n[3/4] Executando suite completa de testes automatizados..." -ForegroundColor Yellow
function Invoke-NexoTest {
    param([string[]]$Arguments)

    & cargo @ToolchainArgs test @Arguments -- --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Falha no teste: cargo test $($Arguments -join ' ')" }
}

# Cargo can run separate integration-test binaries concurrently even when
# each binary receives --test-threads=1. These scenarios use shared local
# discovery resources, so keep the binaries themselves serialized.
Invoke-NexoTest @('--workspace', '--lib', '--bins')
Invoke-NexoTest @('-p', 'nexo-app', '--test', 'two_instances')
Invoke-NexoTest @('-p', 'nexo-app', '--test', 'three_instances')
Invoke-NexoTest @('-p', 'nexo-net', '--test', 'file_transfer_loopback')
Invoke-NexoTest @('-p', 'nexo-net', '--test', 'sync_loopback')
Invoke-NexoTest @('-p', 'nexo-net', '--test', 'voice_loopback')
Invoke-NexoTest @('-p', 'nexo-media', '--test', 'video_loopback')
Invoke-NexoTest @('-p', 'nexo-video', '--test', 'camera_capture')
Invoke-NexoTest @('-p', 'nexo-video', '--test', 'screen_capture')
Write-Host " -> Todos os testes passaram com 100% de sucesso!" -ForegroundColor Green

# 4. Packaging
Write-Host "`n[4/4] Gerando pacote de distribuicao portatil..." -ForegroundColor Yellow
$packageArgs = @(
    '-ExecutionPolicy', 'Bypass',
    '-File', (Join-Path $PSScriptRoot "package-windows.ps1"),
    '-Version', $Version,
    '-OutDir', $OutDir
)
if ($Toolchain) { $packageArgs += @('-Toolchain', $Toolchain) }
powershell @packageArgs
if ($LASTEXITCODE -ne 0) { throw "Falha no empacotamento" }

$ResolvedOutDir = if ([System.IO.Path]::IsPathRooted($OutDir)) {
    $OutDir
} else {
    Join-Path $RepoRoot $OutDir
}
$ZipPath = Join-Path $ResolvedOutDir "nexo-$Version-windows-x86_64.zip"
if (Test-Path $ZipPath) {
    $SizeMB = [math]::Round((Get-Item $ZipPath).Length / 1MB, 2)
    Write-Host "`n======================================================" -ForegroundColor Cyan
    Write-Host " [SUCESSO] Build Golden Master Concluido!" -ForegroundColor Green
    Write-Host " Artefato: $ZipPath ($SizeMB MB)" -ForegroundColor White
    Write-Host "======================================================" -ForegroundColor Cyan
} else {
    throw "Artefato Windows nao encontrado: $ZipPath"
}

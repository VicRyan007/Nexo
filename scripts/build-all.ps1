# Nexo All-in-One Build, Test, and Packaging Script for Windows
param(
    [string]$Version = "1.0.0",
    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"

Write-Host "======================================================" -ForegroundColor Cyan
Write-Host "   Nexo v$Version - Pipeline de Build e Verificacao   " -ForegroundColor Cyan
Write-Host "======================================================" -ForegroundColor Cyan

$mingw = 'C:\Users\Ryan\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin'
$ToolchainArgs = @()
if (Test-Path $mingw) {
    $env:PATH = "$mingw;$env:USERPROFILE\.cargo\bin;$env:PATH"
    $ToolchainArgs = @("+1.97.1-x86_64-pc-windows-gnu")
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
& cargo @ToolchainArgs test --workspace
if ($LASTEXITCODE -ne 0) { throw "Falha nos testes automatizados" }
Write-Host " -> Todos os testes passaram com 100% de sucesso!" -ForegroundColor Green

# 4. Packaging
Write-Host "`n[4/4] Gerando pacote de distribuicao portatil..." -ForegroundColor Yellow
powershell -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "package-windows.ps1") -Version $Version -OutDir $OutDir
if ($LASTEXITCODE -ne 0) { throw "Falha no empacotamento" }

$ZipPath = Join-Path $PSScriptRoot "..\$OutDir\nexo-$Version-windows-x86_64.zip"
if (Test-Path $ZipPath) {
    $SizeMB = [math]::Round((Get-Item $ZipPath).Length / 1MB, 2)
    Write-Host "`n======================================================" -ForegroundColor Cyan
    Write-Host " [SUCESSO] Build Golden Master Concluido!" -ForegroundColor Green
    Write-Host " Artefato: $ZipPath ($SizeMB MB)" -ForegroundColor White
    Write-Host "======================================================" -ForegroundColor Cyan
}

[CmdletBinding()]
param(
    [ValidateSet('Auto', 'OpenCode', 'Gemini')]
    [string]$Agent = 'Auto',
    [ValidateRange(1, 100)]
    [int]$MaxRounds = 12,
    [string]$OpenCodeModel = 'opencode/big-pickle',
    [string]$GeminiModel = '',
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$HandoffPath = Join-Path $ProjectRoot 'docs\continuation.md'
$CompletionMarker = Join-Path $ProjectRoot 'docs\continuation-complete.txt'
$LogDirectory = Join-Path $ProjectRoot '.continuation-logs'
$MingwPath = 'C:\Users\Ryan\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin'
$RustToolchain = '+1.97.1-x86_64-pc-windows-gnu'

if (-not (Test-Path -LiteralPath $HandoffPath)) {
    throw "Handoff document not found: $HandoffPath"
}

function Resolve-ContinuationAgent {
    if ($Agent -in @('Auto', 'OpenCode') -and (Get-Command opencode -ErrorAction SilentlyContinue)) {
        return 'OpenCode'
    }
    if ($Agent -in @('Auto', 'Gemini') -and (Get-Command gemini -ErrorAction SilentlyContinue)) {
        return 'Gemini'
    }
    throw 'Neither the requested agent nor an automatic fallback is installed.'
}

function Invoke-NexoVerification {
    $previousPath = $env:PATH
    try {
        if (Test-Path -LiteralPath $MingwPath) {
            $env:PATH = "$MingwPath;$env:USERPROFILE\.cargo\bin;$previousPath"
        }
        Push-Location $ProjectRoot
        try {
            $lines = [System.Collections.Generic.List[string]]::new()
            foreach ($arguments in @(
                @($RustToolchain, 'fmt', '--all', '--check'),
                @($RustToolchain, 'clippy', '--workspace', '--all-targets', '--', '-D', 'warnings'),
                @($RustToolchain, 'test', '--workspace')
            )) {
                $output = & cargo @arguments 2>&1
                $lines.AddRange([string[]]$output)
                if ($LASTEXITCODE -ne 0) {
                    return [pscustomobject]@{ Passed = $false; Output = ($lines -join "`n") }
                }
            }
            return [pscustomobject]@{ Passed = $true; Output = ($lines -join "`n") }
        }
        finally {
            Pop-Location
        }
    }
    finally {
        $env:PATH = $previousPath
    }
}

function New-ContinuationPrompt {
    param([int]$Round, [string]$VerificationSummary)

    $handoff = Get-Content -LiteralPath $HandoffPath -Raw
    return @"
You are taking over implementation of the Nexo project after the primary Codex agent became
unavailable. Work autonomously in this repository and make concrete progress toward the full
objective. The repository and command results are authoritative. Do not merely propose a plan.

Round: $Round of $MaxRounds
Agent handoff:
$handoff

Latest verification result:
$VerificationSummary

Instructions for this round:
1. Inspect the current tree before editing and preserve unrelated changes.
2. Select the highest-impact incomplete milestone that can be implemented and verified now.
3. Implement it end to end with focused tests and documentation.
4. Run formatting, strict clippy and the full workspace test suite. Fix failures instead of
   weakening checks.
5. Update docs/continuation.md truthfully with changes, evidence and next work.
6. Never create docs/continuation-complete.txt unless every completion-audit item has direct,
   current evidence. If the whole objective is genuinely complete, write a concise evidence report
   into that marker. Otherwise leave it absent and finish this round at a stable checkpoint.
7. Stay inside the Nexo project. Do not read credentials, browser data, unrelated personal files,
   identity keys or local database contents. Do not run destructive commands or publish externally.
"@
}

$SelectedAgent = Resolve-ContinuationAgent
New-Item -ItemType Directory -Force -Path $LogDirectory | Out-Null

Write-Host "Nexo continuation agent: $SelectedAgent"
Write-Host "Project: $ProjectRoot"
if ($DryRun) {
    Write-Output (New-ContinuationPrompt -Round 1 -VerificationSummary 'DryRun: verification skipped.')
    exit 0
}

for ($round = 1; $round -le $MaxRounds; $round++) {
    $verification = Invoke-NexoVerification
    $verificationSummary = if ($verification.Passed) {
        'PASS: format, strict clippy and full workspace tests are green.'
    }
    else {
        $tail = ($verification.Output -split "`r?`n" | Select-Object -Last 80) -join "`n"
        "FAIL: fix these current verification errors first:`n$tail"
    }
    $prompt = New-ContinuationPrompt -Round $round -VerificationSummary $verificationSummary
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $logPath = Join-Path $LogDirectory "$stamp-$($SelectedAgent.ToLower())-round-$round.log"

    Write-Host "Starting autonomous round $round of $MaxRounds..."
    Push-Location $ProjectRoot
    try {
        if ($SelectedAgent -eq 'OpenCode') {
            & opencode run --auto --model $OpenCodeModel --title "Nexo continuation round $round" $prompt 2>&1 |
                Tee-Object -FilePath $logPath
        }
        else {
            $arguments = @('--prompt', $prompt, '--approval-mode', 'yolo', '--output-format', 'text')
            if ($GeminiModel) {
                $arguments += @('--model', $GeminiModel)
            }
            & gemini @arguments 2>&1 | Tee-Object -FilePath $logPath
        }
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "$SelectedAgent exited with code $LASTEXITCODE. See $logPath"
        }
    }
    finally {
        Pop-Location
    }

    $verification = Invoke-NexoVerification
    if (-not $verification.Passed) {
        $failureLog = Join-Path $LogDirectory "$stamp-verification-failure.log"
        Set-Content -LiteralPath $failureLog -Value $verification.Output -Encoding utf8
        Write-Warning "Round $round left verification failing. The next round receives the failure."
        continue
    }

    if (Test-Path -LiteralPath $CompletionMarker) {
        Write-Host "Completion marker found and verification is green: $CompletionMarker"
        exit 0
    }
    Write-Host "Round $round is green; continuing because the full audit is not complete."
}

Write-Warning "Reached MaxRounds=$MaxRounds without a verified completion marker."
exit 2

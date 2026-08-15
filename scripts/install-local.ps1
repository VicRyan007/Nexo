$PSScriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path "$PSScriptRoot\.."

$InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\Nexo'
if (!(Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

$BinarySource = "$RepoRoot\target\release\nexo.exe"
$IcoSource = "$RepoRoot\crates\nexo-app\resources\nexo.ico"
$ReadmeSource = "$RepoRoot\README.md"

if (Test-Path $BinarySource) {
    Copy-Item $BinarySource -Destination (Join-Path $InstallDir 'nexo.exe') -Force
}
if (Test-Path $IcoSource) {
    Copy-Item $IcoSource -Destination (Join-Path $InstallDir 'nexo.ico') -Force
}
if (Test-Path $ReadmeSource) {
    Copy-Item $ReadmeSource -Destination (Join-Path $InstallDir 'README.md') -Force
}

$ExePath = Join-Path $InstallDir 'nexo.exe'
$IcoPath = Join-Path $InstallDir 'nexo.ico'
$WshShell = New-Object -ComObject WScript.Shell

# Desktop Shortcuts
$DesktopLocations = @(
    [Environment]::GetFolderPath('Desktop'),
    "C:\Users\Ryan\Desktop",
    "C:\Users\Ryan\OneDrive\Desktop",
    "C:\Users\Ryan\OneDrive\Área de Trabalho"
) | Select-Object -Unique

foreach ($loc in $DesktopLocations) {
    if (Test-Path $loc) {
        $scPath = Join-Path $loc 'Nexo.lnk'
        $sc = $WshShell.CreateShortcut($scPath)
        $sc.TargetPath = $ExePath
        $sc.WorkingDirectory = $InstallDir
        if (Test-Path $IcoPath) { $sc.IconLocation = "$IcoPath,0" }
        $sc.Description = 'Nexo - Colaboracao P2P Nativa (Voz, Video, Chat e Arquivos)'
        $sc.Save()
        Write-Host "Atalho confirmado na Area de Trabalho: $scPath"
    }
}

# Start Menu Shortcut
$StartMenuPrograms = [Environment]::GetFolderPath('Programs')
$StartMenuNexoDir = Join-Path $StartMenuPrograms 'Nexo'
if (!(Test-Path $StartMenuNexoDir)) {
    New-Item -ItemType Directory -Path $StartMenuNexoDir -Force | Out-Null
}
$smPath = Join-Path $StartMenuNexoDir 'Nexo.lnk'
$sc = $WshShell.CreateShortcut($smPath)
$sc.TargetPath = $ExePath
$sc.WorkingDirectory = $InstallDir
if (Test-Path $IcoPath) { $sc.IconLocation = "$IcoPath,0" }
$sc.Description = 'Nexo - Colaboracao P2P Nativa (Voz, Video, Chat e Arquivos)'
$sc.Save()
Write-Host "Atalho confirmado no Menu Iniciar: $smPath"

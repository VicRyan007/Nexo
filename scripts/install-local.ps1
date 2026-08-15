$InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\Nexo'
$ExePath = Join-Path $InstallDir 'nexo.exe'
$WshShell = New-Object -ComObject WScript.Shell

$DesktopLocations = @(
    [Environment]::GetFolderPath('Desktop'),
    "C:\Users\Ryan\Desktop",
    "C:\Users\Ryan\OneDrive\Desktop"
)

foreach ($loc in $DesktopLocations) {
    if (Test-Path $loc) {
        $scPath = Join-Path $loc 'Nexo.lnk'
        $sc = $WshShell.CreateShortcut($scPath)
        $sc.TargetPath = $ExePath
        $sc.WorkingDirectory = $InstallDir
        $sc.Description = 'Nexo - Colaboracao P2P Nativa (Voz, Video, Chat e Arquivos)'
        $sc.Save()
        Write-Host "Atalho confirmado em: $scPath"
    }
}

# Removes the kamui binary installed by install.ps1.
# Usage: uninstall.ps1 [-Purge]
#   -Purge   Also remove configuration (kamui.toml, themes) and the
#            SQLite database. Without it, settings survive a reinstall.
param(
    [switch]$Purge
)

$ErrorActionPreference = "Stop"

$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\kamui\bin"
$Binary = Join-Path $InstallDir "kamui.exe"

Write-Host ""
Write-Host "  KAMUI uninstall"
Write-Host ""

if (Test-Path $Binary) {
    Remove-Item -Force $Binary
    Write-Host "  Removed $Binary"
} else {
    Write-Host "  No binary at $Binary (nothing to remove)"
}

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$PathEntries = @($UserPath -split ";" | Where-Object { $_ -and ($_ -ne $InstallDir) })
$NewPath = ($PathEntries -join ";")
if ($NewPath -ne $UserPath) {
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    Write-Host "  Removed $InstallDir from your user PATH."
}

if ($Purge) {
    $ConfigDir = Join-Path $env:APPDATA "kamui"
    $DataDirs = @($ConfigDir)
    if ($env:KAMUI_DATA_DIR) { $DataDirs += $env:KAMUI_DATA_DIR }
    $LocalData = Join-Path $env:LOCALAPPDATA "kamui"
    if (($LocalData -ne $ConfigDir) -and (Test-Path $LocalData)) { $DataDirs += $LocalData }
    foreach ($dir in ($DataDirs | Select-Object -Unique)) {
        if (Test-Path $dir) {
            Remove-Item -Recurse -Force $dir
            Write-Host "  Purged $dir"
        }
    }
} else {
    Write-Host "  Kept configuration and database (use -Purge to remove them)"
}

Write-Host ""
Write-Host "  Done."
Write-Host ""

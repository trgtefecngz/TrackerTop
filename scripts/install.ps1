# TrackerTop One-Line Windows Installer
# Usage: irm https://raw.githubusercontent.com/trgtefecngz/TrackerTop/main/scripts/install.ps1 | iex

$ErrorActionPreference = "Stop"
$Repo = "trgtefecngz/TrackerTop"
$InstallDir = Join-Path $env:LOCALAPPDATA "TrackerTop"
$ExePath = Join-Path $InstallDir "TrackerTop.exe"

Write-Host "Fetching latest TrackerTop release from GitHub..." -ForegroundColor Cyan
$Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
$Asset = $Release.assets | Where-Object { $_.name -like "*-windows-x64.exe" } | Select-Object -First 1
if (-not $Asset) {
    throw "No Windows x64 executable found in latest release $($Release.tag_name)."
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Write-Host "Downloading $($Asset.name)..." -ForegroundColor Cyan
Invoke-WebRequest -Uri $Asset.browser_download_url -OutFile $ExePath

# Add install dir to user PATH if not already present
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not ($UserPath -split ';' | Where-Object { $_ -ieq $InstallDir })) {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
    Write-Host "Added $InstallDir to User PATH." -ForegroundColor Green
}

# Create Desktop shortcut
$WshShell = New-Object -ComObject WScript.Shell
$Shortcut = $WshShell.CreateShortcut((Join-Path ([Environment]::GetFolderPath("Desktop")) "TrackerTop.lnk"))
$Shortcut.TargetPath = $ExePath
$Shortcut.WorkingDirectory = $InstallDir
$Shortcut.Description = "TrackerTop — AI Codebase Graph & Token Savings"
$Shortcut.Save()

Write-Host "Installed TrackerTop $($Release.tag_name) to $ExePath" -ForegroundColor Green
Write-Host "Desktop shortcut created! Launching TrackerTop..." -ForegroundColor Green
Start-Process $ExePath

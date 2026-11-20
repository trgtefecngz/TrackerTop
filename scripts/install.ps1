# TrackerTop One-Line Windows Installer
# Usage: irm https://raw.githubusercontent.com/trgtefecngz/TrackerTop/main/scripts/install.ps1 | iex
#
# Downloads the latest Windows x64 release, verifies its SHA256 checksum (published next to
# the binary as <asset>.sha256), installs it to %LOCALAPPDATA%\TrackerTop and adds that folder
# to the user PATH. Windows on ARM runs the x64 build through built-in emulation.

$ErrorActionPreference = "Stop"
$Repo = "trgtefecngz/TrackerTop"
$InstallDir = Join-Path $env:LOCALAPPDATA "TrackerTop"
$ExePath = Join-Path $InstallDir "TrackerTop.exe"

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "TrackerTop requires 64-bit Windows (x64 or ARM64)."
}

Write-Host "Fetching latest TrackerTop release from GitHub..." -ForegroundColor Cyan
$Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
$Asset = $Release.assets | Where-Object { $_.name -like "*-windows-x64.exe" } | Select-Object -First 1
if (-not $Asset) {
    throw "No Windows x64 executable found in latest release $($Release.tag_name)."
}
$SumAsset = $Release.assets | Where-Object { $_.name -eq "$($Asset.name).sha256" } | Select-Object -First 1

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$TmpExe = Join-Path $InstallDir "TrackerTop.download.exe"
Write-Host "Downloading $($Asset.name)..." -ForegroundColor Cyan
Invoke-WebRequest -Uri $Asset.browser_download_url -OutFile $TmpExe -UseBasicParsing

try {
    if ($SumAsset) {
        $SumText = (Invoke-WebRequest -Uri $SumAsset.browser_download_url -UseBasicParsing).Content
        if ($SumText -is [byte[]]) { $SumText = [Text.Encoding]::ASCII.GetString($SumText) }
        $Expected = ($SumText.Trim() -split '\s+')[0].ToLowerInvariant()
        $Actual = (Get-FileHash $TmpExe -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($Expected -ne $Actual) {
            throw "Checksum mismatch for $($Asset.name): expected $Expected, got $Actual. Nothing was installed."
        }
        Write-Host "SHA256 verified." -ForegroundColor Green
    } else {
        Write-Warning "Release $($Release.tag_name) has no checksum file; skipping SHA256 verification."
    }

    try {
        Move-Item -Force -Path $TmpExe -Destination $ExePath
    } catch {
        throw "Could not replace $ExePath. Close TrackerTop and any AI agent sessions using it (Claude Code, Cursor, ...), then run the installer again."
    }
} finally {
    if (Test-Path $TmpExe) { Remove-Item -Force $TmpExe }
}

# Add install dir to user PATH if not already present
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not ($UserPath -split ';' | Where-Object { $_ -ieq $InstallDir })) {
    $NewPath = if ([string]::IsNullOrEmpty($UserPath)) { $InstallDir } else { "$UserPath;$InstallDir" }
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    Write-Host "Added $InstallDir to User PATH." -ForegroundColor Green
}

# Create Desktop shortcut
$WshShell = New-Object -ComObject WScript.Shell
$Shortcut = $WshShell.CreateShortcut((Join-Path ([Environment]::GetFolderPath("Desktop")) "TrackerTop.lnk"))
$Shortcut.TargetPath = $ExePath
$Shortcut.WorkingDirectory = $InstallDir
$Shortcut.Description = "TrackerTop - AI Codebase Graph & Token Savings"
$Shortcut.Save()

Write-Host "Installed TrackerTop $($Release.tag_name) to $ExePath" -ForegroundColor Green
Write-Host "Desktop shortcut created! Launching TrackerTop..." -ForegroundColor Green
Start-Process $ExePath

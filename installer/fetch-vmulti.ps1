# Pre-build helper: download the VMulti virtual HID digitizer driver
# bundle from X9VoiD/vmulti-bin and stage the files we ship into
# `installer/vmulti-driver/`. The Tauri MSI bundle includes that folder
# as a resource; the WiX fragment at
# `apps/penflow-gui/src-tauri/wix/vmulti-install.wxs` calls
# `devcon install vmulti.inf pentablet\hid` at MSI install time so the
# driver is up before the user ever opens the GUI.
#
# Pinned version: 1.0 (the only published release, 2020-10; binaries
# stamped 2023-10 — re-signed but unchanged feature set).
#
# Run from the repo root:
#   pwsh -File installer\fetch-vmulti.ps1
# Or:
#   powershell -ExecutionPolicy Bypass -File installer\fetch-vmulti.ps1
#
# Why this driver, not djpnewton/vmulti upstream:
#   The original djpnewton driver was archived in 2023 and never had a
#   pen-pressure-capable HID descriptor. The X9VoiD fork adds the
#   extended digitizer report (ReportID 0x06) with 16384-level pressure,
#   tilt-X/Y, eraser, and barrel button — what real Wacom pens report.
#   It's also the de-facto standard consumed by OpenTabletDriver's
#   WindowsInk plugin (`X9VoiD/VoiDPlugins`), the canonical open-source
#   reference for VMulti-based pen injection on Windows.

$ErrorActionPreference = "Stop"

$Version = "1.0"
$Asset   = "VMulti.Driver.zip"
$Url     = "https://github.com/X9VoiD/vmulti-bin/releases/download/v$Version/$Asset"

$RepoRoot = Resolve-Path "$PSScriptRoot\.."
$Cache    = Join-Path $RepoRoot "installer\.cache"
$Out      = Join-Path $RepoRoot "installer\vmulti-driver"
$ZipPath  = Join-Path $Cache    "vmulti-bin-v$Version.zip"

New-Item -ItemType Directory -Force -Path $Cache | Out-Null
New-Item -ItemType Directory -Force -Path $Out   | Out-Null

if (-not (Test-Path $ZipPath)) {
    Write-Host "[fetch-vmulti] downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $ZipPath -UseBasicParsing
} else {
    Write-Host "[fetch-vmulti] $ZipPath already cached"
}

Write-Host "[fetch-vmulti] extracting to $Out"
Remove-Item -Recurse -Force "$Out\*" -ErrorAction SilentlyContinue

$Temp = Join-Path $Cache "vmulti-extract"
Remove-Item -Recurse -Force $Temp -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Temp | Out-Null

Expand-Archive -Path $ZipPath -DestinationPath $Temp -Force

# We need everything except the .bat wrappers (the WiX custom actions
# call devcon/DIFxCmd directly — no batch needed since MSI deferred
# actions already run as SYSTEM, sidestepping the UAC-elevation dance
# the .bat scripts use) and WinTab32.dll (deferred to a later milestone:
# replacing the system Wintab DLL collides with other tablet drivers,
# and the Windows Ink path already covers our currently-targeted
# applications. Re-add if/when we tackle Krita's default-Wintab mode.)
$want = @(
    "vmulti.sys",                  # KMDF bus driver
    "vmulti.inf",                  # INF
    "pentablethid.cat",            # Authenticode catalog
    "hidkmdf.sys",                 # HID-KMDF shim required by the bus
    "WdfCoInstaller01009.dll",     # KMDF coinstaller, needed during DIFx install
    "devcon.exe",                  # install/remove device-node action
    "DIFxCmd.exe",                 # remove driver package from store
    "DIFxAPI.dll"                  # DIFx runtime, DIFxCmd loads it
)

$copied = 0
foreach ($name in $want) {
    $src = Join-Path $Temp $name
    if (Test-Path $src) {
        Copy-Item $src -Destination $Out -Force
        $copied++
    } else {
        throw "[fetch-vmulti] missing $name in $Temp — release layout changed?"
    }
}

Write-Host "[fetch-vmulti] $copied file(s) staged in $Out"
Get-ChildItem $Out | Format-Table Name, Length -AutoSize

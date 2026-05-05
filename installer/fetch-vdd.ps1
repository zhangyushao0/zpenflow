# Pre-build helper: download the Virtual Display Driver release bundle
# from VirtualDrivers/Virtual-Display-Driver and extract just the driver
# files into `installer/vdd-driver/`. The Tauri MSI bundle includes that
# folder as a resource; the GUI's first-run hook calls
# `pnputil /add-driver` against the .inf to install the device.
#
# Pinned version: 25.7.23 (the same release the dev rig confirmed
# working — see docs/vdd-stuck.md).
#
# Run from the repo root:
#   pwsh -File installer\fetch-vdd.ps1
# Or:
#   powershell -ExecutionPolicy Bypass -File installer\fetch-vdd.ps1

$ErrorActionPreference = "Stop"

$Version = "25.7.23"
$Asset   = "VDD.Control.$Version.zip"
$Url     = "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/download/$Version/$Asset"

$RepoRoot = Resolve-Path "$PSScriptRoot\.."
$Cache    = Join-Path $RepoRoot "installer\.cache"
$Out      = Join-Path $RepoRoot "installer\vdd-driver"
$ZipPath  = Join-Path $Cache    $Asset

New-Item -ItemType Directory -Force -Path $Cache | Out-Null
New-Item -ItemType Directory -Force -Path $Out   | Out-Null

if (-not (Test-Path $ZipPath)) {
    Write-Host "[fetch-vdd] downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $ZipPath -UseBasicParsing
} else {
    Write-Host "[fetch-vdd] $ZipPath already cached"
}

Write-Host "[fetch-vdd] extracting to $Out"
Remove-Item -Recurse -Force "$Out\*" -ErrorAction SilentlyContinue

# Extract in two passes — first to temp, then copy just driver files
# (.inf / .sys / .dll / .cat / .json) so we don't ship the full GUI tool.
$Temp = Join-Path $Cache "vdd-extract"
Remove-Item -Recurse -Force $Temp -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Temp | Out-Null

Expand-Archive -Path $ZipPath -DestinationPath $Temp -Force

# The release zip layout (25.7.x) is:
#   VirtualDriverControl.exe
#   driver/<files>           ← what we want
# Copy only the driver subfolder. If layout changes, fail loudly.
$Driver = Join-Path $Temp "driver"
if (-not (Test-Path $Driver)) {
    # 25.7.23 ships the driver files at the zip root next to the exe.
    # Find them by extension instead.
    $Driver = $Temp
}

$want = @("*.inf", "*.sys", "*.dll", "*.cat", "*.json", "*.man")
$copied = 0
foreach ($pat in $want) {
    Get-ChildItem -Path $Driver -Recurse -File -Filter $pat |
        ForEach-Object {
            Copy-Item $_.FullName -Destination $Out -Force
            $copied++
        }
}

if ($copied -eq 0) {
    throw "[fetch-vdd] no driver files found under $Driver — release layout changed?"
}

# devcon.exe ships in Dependencies/. We need it because the MttVDD INF
# advertises a Root-enumerated hardware ID (`Root\MttVDD`); pnputil
# /add-driver /install only updates drivers on EXISTING matching devices,
# while `devcon install <inf> Root\MttVDD` both adds to the driver store
# AND creates the root device node. Microsoft-signed; safe to redist.
$Devcon = Get-ChildItem -Path $Temp -Recurse -File -Filter "devcon.exe" | Select-Object -First 1
if ($null -ne $Devcon) {
    Copy-Item $Devcon.FullName -Destination $Out -Force
    Write-Host "[fetch-vdd] copied devcon.exe ($($Devcon.Length) bytes)"
} else {
    throw "[fetch-vdd] devcon.exe not found under $Temp — release layout changed?"
}

# Drop our pre-tuned settings XML alongside so install_vdd can copy it.
Copy-Item (Join-Path $RepoRoot "tools\vdd\vdd_settings.xml") -Destination $Out -Force

Write-Host "[fetch-vdd] $copied file(s) staged in $Out"
Get-ChildItem $Out | Format-Table Name, Length -AutoSize

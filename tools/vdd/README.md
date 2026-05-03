# Virtual Display Driver (VDD) — Penflow integration

Penflow captures an open-source Indirect Display Driver instead of mirroring
the primary monitor. This makes the MovinkPad act as a **separate extended
monitor** at the panel's native 2880×1800 resolution — drag Krita onto it,
fill the whole panel, no letterbox.

## Source

- Project: <https://github.com/VirtualDrivers/Virtual-Display-Driver>
- License: MIT
- Binaries: pre-signed releases on GitHub. We do **not** check binaries into git.

## Files in this directory

- [`vdd_settings.xml`](vdd_settings.xml) — our config: advertises a single 2880×1800 monitor at 60/120 Hz. **Tracked in git.**

## One-time install

The latest release ships as a portable GUI tool called **Virtual Driver
Control** that handles install/uninstall/enable/disable of the driver itself.

1. Download `VDD.Control.YY.M.D.zip` from the latest release at
   <https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/latest>.
2. Extract the zip somewhere convenient (e.g. `C:\Tools\VDD-Control\`).
3. **Run `VirtualDriverControl.exe` as Administrator.**
4. Click the **Install** button to install the signed Virtual Display Driver.
5. After installation, copy our config over the default:
   ```powershell
   Copy-Item C:\repo\krita\penflow\tools\vdd\vdd_settings.xml C:\VirtualDisplayDriver\vdd_settings.xml -Force
   ```
   (If `C:\VirtualDisplayDriver\` doesn't exist, the driver creates it during
   install. If the copy says "Access denied", run the PowerShell as Admin.)
6. Back in Virtual Driver Control, click **Reload Driver** so it re-reads the XML.
7. Open Windows Settings → System → Display. A new monitor at 2880×1800
   should appear. Set it to **Extend** (not Mirror).

## Verifying it worked

```powershell
# Should now show 3+ adapters: NVIDIA RTX 5070 + Virtual Display Driver + (optional) Wacom IDD
Get-CimInstance Win32_VideoController | Select-Object Name | Format-List

# Should list a 2880x1800 output:
& C:\repo\krita\penflow\server\.venv\Scripts\python.exe -c "import dxcam; print(dxcam.output_info())"
```

## Penflow usage

Once installed:

```powershell
# Show all available capture targets, with default pick highlighted
& C:\repo\krita\penflow\server\.venv\Scripts\python.exe C:\repo\krita\penflow\server\penflow_server.py --list-monitors

# Auto-pick the VDD if present, otherwise primary
& C:\repo\krita\penflow\server\.venv\Scripts\python.exe C:\repo\krita\penflow\server\penflow_server.py --fps 120

# Explicit override by output index
& C:\repo\krita\penflow\server\.venv\Scripts\python.exe C:\repo\krita\penflow\server\penflow_server.py --monitor 1
```

(`--monitor` and `--list-monitors` arrive in Task 15 of the implementation plan.)

## Troubleshooting

- **Monitor doesn't appear after install**: open Virtual Driver Control,
  hit **Disable** then **Enable**. Some Windows builds need this to bind.
- **Resolution stuck at 1920×1080**: `vdd_settings.xml` wasn't picked up.
  Verify the file is at `C:\VirtualDisplayDriver\vdd_settings.xml` literally
  (not `C:\Users\...\VirtualDisplayDriver\...`), then **Reload Driver**.
- **Two virtual monitors appear (one Wacom, one VDD)**: that's fine. Penflow's
  auto-pick prefers the VDD by name; you can also pass `--monitor N` explicitly.
- **Driver crashes on Enable, monitor PnP shows `Unknown`**: the
  user-mode driver host (`mttvdd.dll`) is throwing an unhandled exception
  while parsing the XML. This was confirmed during initial integration: the
  upstream `master`-branch `vdd_settings.xml` schema includes `hdr_advanced`,
  `auto_resolutions`, `color_advanced` etc. that the **25.7.23 release binary
  does not parse** — it segfaults on those sections. Use the minimal schema in
  this directory instead. To confirm a crash is happening:
  ```powershell
  Get-WinEvent -FilterHashtable @{LogName='Application'; StartTime=(Get-Date).AddMinutes(-5)} |
      Where-Object { $_.Message -match 'mttvdd' } | Select-Object TimeCreated, LevelDisplayName
  ```
  If you see `WUDFUnhandledException` entries with `mttvdd.dll`, that's the
  symptom. Fix: replace `C:\VirtualDisplayDriver\vdd_settings.xml` with the
  minimal one in this directory, then Disable/Enable the driver.

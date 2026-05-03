# Virtual Display Driver (VDD) — Penflow integration

Penflow captures an open-source Indirect Display Driver instead of mirroring
the primary monitor. The MovinkPad acts as a **separate extended monitor** at
the panel's native 2880×1800 resolution — drag Krita onto it, fill the whole
panel, no letterbox.

## On-demand lifecycle (the important bit)

The server enables the driver **only while an Android client is connected**:

1. Operator launches `run_session` on the PC. PC desktop unchanged.
2. Operator taps the Penflow app on the tablet.
3. After the protocol handshake (`HELLO_ANDROID`), the server runs
   `Enable-PnpDevice` → the virtual monitor pops up in Windows display
   settings, the engine captures it, frames flow to the tablet.
4. Operator closes the tablet app or hits Ctrl-C on the PC. Server runs
   `Disable-PnpDevice` → the virtual monitor disappears.

This avoids cluttering the desktop arrangement when nothing is connected and
prevents the cursor from wandering into "dead pixel space".

**Admin required.** `Enable-PnpDevice` / `Disable-PnpDevice` are gated on
Administrator. Run `cargo run -p penflow-server --example run_session` from
an elevated PowerShell. If you forget, the server exits with a clear
`Access is denied` / `要求提升` message.

If you don't want VDD (mirror the primary monitor instead), pass
`--no-vdd` and the server skips detection entirely.

## Source

- Project: <https://github.com/VirtualDrivers/Virtual-Display-Driver>
- License: MIT
- Binaries: pre-signed releases on GitHub. We do **not** check binaries into git.

## Files in this directory

- [`vdd_settings.xml`](vdd_settings.xml) — our config: advertises a single
  2880×1800 monitor at 60/120 Hz. **Tracked in git.**

## One-time install

The release ships as a portable GUI tool called **Virtual Driver Control**
that handles install/uninstall/enable/disable of the driver itself. We use it
for **install only** — runtime enable/disable is then driven by the server.

1. Download `VDD.Control.YY.M.D.zip` from the latest release at
   <https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/latest>.
2. Extract the zip somewhere convenient (e.g. `C:\Tools\VDD-Control\`).
3. **Run `VirtualDriverControl.exe` as Administrator.**
4. Click the **Install** button to install the signed Virtual Display Driver.
5. After installation, copy our config over the default:
   ```powershell
   Copy-Item C:\repo\krita\zpenflow\tools\vdd\vdd_settings.xml `
             C:\VirtualDisplayDriver\vdd_settings.xml -Force
   ```
   (If `C:\VirtualDisplayDriver\` doesn't exist, the driver creates it during
   install. If the copy says "Access denied", run the PowerShell as Admin.)
6. Back in Virtual Driver Control, click **Reload Driver** so it re-reads the XML.
7. **Then click Disable** (or wait for it to be in Disabled / Error state in
   Device Manager). The server will enable it when needed.

## Verifying it worked

```powershell
# Should list the VDD with Status = Error or Disabled (NOT OK):
Get-PnpDevice -Class Display | Where-Object { $_.FriendlyName -match 'Virtual Display Driver' } | Format-List InstanceId, FriendlyName, Status

# Probe what the server's auto-detection picks (run from anywhere, no admin needed):
cargo run -p penflow-server --example run_session -- --no-vdd  # just to verify the binary builds
```

When you actually start a session (no `--no-vdd`), the log will say:
```
[run_session] VDD detected: 'Virtual Display Driver' (ROOT\DISPLAY\0001)
[run_session]   will enable on Android connect, disable on disconnect
```

Once a tablet connects:
```
[session] enabling VDD device 'Virtual Display Driver' (ROOT\DISPLAY\0001)
[session] virtual monitor up: \\.\DISPLAY3 2880x1800 on Virtual Display Driver
```

## Penflow usage

```powershell
# Auto-detect VDD; enables on connect, captures the virtual monitor:
cargo run -p penflow-server --example run_session

# Skip VDD entirely; capture the physical monitor instead:
cargo run -p penflow-server --example run_session -- --no-vdd

# Pick a specific physical monitor (when --no-vdd):
cargo run -p penflow-server --example run_session -- --no-vdd --monitor 1
```

## Troubleshooting

- **VDD doesn't appear in `Get-PnpDevice` output**: the install didn't
  complete. Open Virtual Driver Control as admin and re-run Install.
- **Server logs `no VDD installed (PowerShell ran fine, no match)` even
  though it's installed**: the FriendlyName might not contain any of our
  matched keywords (`virtual`, `vdd`, `iddsample`, `MTT`). Run the
  detection query manually to see what name your installer assigned, and
  open an issue.
- **Server logs `Enable-PnpDevice failed ... Access is denied`**: the
  PowerShell that launched `run_session` isn't elevated. Re-launch it as
  Administrator.
- **Server enables VDD but logs `EnumerationTimeout`**: Windows took too
  long to publish the new monitor through DXGI. Try Disable→Enable cycle
  in Virtual Driver Control once to reset state. If it persists, the
  driver's XML may be malformed (see the next item).
- **Driver crashes on Enable, monitor PnP shows `Unknown`**: the
  user-mode driver host (`mttvdd.dll`) is throwing an unhandled exception
  while parsing the XML. The upstream `master`-branch `vdd_settings.xml`
  schema includes `hdr_advanced`, `auto_resolutions`, `color_advanced`
  etc. that the **25.7.23 release binary does not parse** — it segfaults
  on those sections. Use the minimal schema in this directory instead. To
  confirm a crash:
  ```powershell
  Get-WinEvent -FilterHashtable @{LogName='Application'; StartTime=(Get-Date).AddMinutes(-5)} |
      Where-Object { $_.Message -match 'mttvdd' } | Select-Object TimeCreated, LevelDisplayName
  ```
  If you see `WUDFUnhandledException` entries with `mttvdd.dll`, that's the
  symptom. Fix: replace `C:\VirtualDisplayDriver\vdd_settings.xml` with the
  minimal one in this directory, then Disable/Enable the driver once.
- **Two virtual monitors appear in Device Manager (e.g. MuMu's emulator
  adapter alongside ours)**: the server's auto-detect prefers the
  currently-disabled VDD that matches the canonical `Virtual Display
  Driver` name, so it should pick the right one. If you're hitting an
  ambiguity, file an issue with the output of:
  ```powershell
  Get-PnpDevice -Class Display | Where-Object { $_.FriendlyName -match 'virtual|vdd|iddsample|MTT' } | Format-List InstanceId, FriendlyName, Status
  ```

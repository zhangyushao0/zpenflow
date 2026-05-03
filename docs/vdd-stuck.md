# VDD on-demand enable: driver reports healthy, no monitor in DXGI

## Resolution

The missing step was Windows display topology, not another PnP enable API.
`CM_Enable_DevNode` starts the VDD devnode and the driver publishes a Monitor
class child (`Generic Monitor (VDD by MTT)`), but Windows does not always attach
that newly-available target to the desktop automatically. While it is
available-but-inactive, `IDXGIAdapter::EnumOutputs` still only shows the
physical display.

The fix is:

1. Snapshot the attached DXGI outputs before enabling VDD.
2. Enable `ROOT\DISPLAY\0001` through the elevated CM helper.
3. Apply `SetDisplayConfig(SDC_APPLY | SDC_TOPOLOGY_EXTEND | ...)` so Windows
   extends the desktop onto the newly-available VDD target.
4. Poll DXGI for an attached output that was not present in the baseline.

This was verified with `cargo run -p penflow-server --example run_session --
--vdd-probe`: after enable + DisplayConfig extend, DXGI reported a new output
like `\\.\DISPLAY28 1920x1200 on NVIDIA GeForce RTX 5070`, then the cleanup
disable removed it again.

Important detail: this VDD output does not necessarily have a virtual-looking
DXGI name. It appears as a generic `\\.\DISPLAYxx` on the NVIDIA adapter, so
name-based `looks_virtual` detection misses it. Baseline-diff detection is the
reliable signal.

Do not use the VDD pipe's `RELOAD_DRIVER` command as recovery. After Penflow
sent `RELOAD_DRIVER` to `\\.\pipe\MTTVirtualDisplayPipe`, Windows logged
`WUDFUnhandledException` events for `mttvdd.dll` with exception code
`0xc0000005`, followed by UMDF offline events. The 25.7.23 source also points
at why this can happen: the pipe reload handler treats a named-pipe handle as
if it were a WDF object context before calling `InitAdapter()`.

If the probe brings up the wrong resolution, fix
`C:\VirtualDisplayDriver\vdd_settings.xml` and then use Disable→Enable. Do not
use Reload Driver on this build.

## Follow-up: `MF_E_UNSUPPORTED_D3D_TYPE` only on the VDD output

After VDD enable + `SetDisplayConfig(EXTEND)` started landing a real DXGI
output, the engine still failed on the first encoded frame from that output:

```
submit_frame failed: 0xC00D6D76 (MF_E_UNSUPPORTED_D3D_TYPE)
wait_for_keyframe: timed out, 0 packets popped
```

The same engine, same encoder, same NVIDIA HEVC MFT worked when capturing the
physical `\\.\DISPLAY1`. Switching the VDD resolution, settling longer after
`SetDisplayConfig`, and zero-clearing the keepalive texture all changed
nothing.

### Root cause: two `ID3D11Device` objects, cross-device `CopyResource`

The engine was building **two** D3D11 devices on the same adapter:

- `ctx` — used by `ColorConverter` (BGRA→NV12 VideoProcessor) and `MfBackend`
  (the MFT's `ID3D11Device` via `MFCreateDXGIDeviceManager`).
- `capturer_ctx` — a fresh `D3d11Context::create_on_adapter(...)` for the DDA
  capturer, on the same adapter LUID but a distinct `ID3D11Device`.

The pipeline's per-frame hot path then did:

```rust
ctx.immediate_context.CopyResource(&keepalive, &dda_frame.texture);
//                                  ^^^^^^^^^   ^^^^^^^^^^^^^^^^^^
//                                  on `ctx`    on `capturer_ctx`
```

`ID3D11DeviceContext::CopyResource` is only defined when source and
destination belong to the **same** `ID3D11Device`. Cross-device is undefined
behavior. The encoder consuming `keepalive` on yet a third path (the MFT's
own device manager, also `ctx`) made the staleness visible.

Why physical capture survived: NVIDIA's user-mode driver appears to fast-path
DDA copies between two devices that share an underlying KMT context, so the
bytes actually showed up in `keepalive`. On the freshly-attached VDD output
that fast path didn't fire — the keepalive contents were left in a state the
NVIDIA HEVC MFT rejected on `ProcessInput`, surfaced as the generic
`MF_E_UNSUPPORTED_D3D_TYPE` HRESULT.

This was misread for a long time as "the MFT's bound D3D device drifted out
from under it after `SetDisplayConfig`" (HRESULT `0xC00D6D76` is also listed
in some headers as `MF_E_DXGI_NEW_VIDEO_DEVICE`), which led to a 500 ms
post-`SetDisplayConfig` settle delay that did nothing useful.

### Fix

Make capturer / converter / encoder share **one** `ID3D11Device`:

- `crates/penflow-core/src/d3d11.rs`: `impl Clone for D3d11Context` (COM
  clone = `AddRef`, so the underlying device + immediate context are shared,
  not duplicated).
- `crates/penflow-core/src/lib.rs` (`EngineBuilder::start`): replace
  `D3d11Context::create_on_adapter(monitor.open_adapter(&factory)?)` with
  `ctx.clone()` for `capturer_ctx`. This keeps the existing ownership model
  (each subsystem owns a `D3d11Context`) but collapses to a single
  `ID3D11Device`.
- `crates/penflow-server/src/session.rs`: drop the speculative comment about
  the NVIDIA UMD reshuffling MFT bindings; the 500 ms sleep stays as a
  modest DXGI re-enumeration buffer, not as the fix.
- `crates/penflow-core/src/pipeline.rs`: added a `describe_texture` helper
  under `PENFLOW_PIPELINE_TRACE` so the next time something like this shows
  up we can dump width/height/format/bind/usage at each stage.
- `crates/penflow-core/examples/encoder_texture_probe.rs`: minimal repro
  that drives BGRA → NV12 → MF HEVC at an arbitrary size **without** any
  capture/VDD involvement. Run
  `cargo run -p penflow-core --example encoder_texture_probe -- 2880 1800`
  to isolate "encoder is unhappy with this surface" from "capture/VDD is
  unhappy".

### Lesson

DDA hands you back a texture owned by whichever `ID3D11Device` was passed to
`IDXGIOutput1::DuplicateOutput`. Anything downstream that touches that
texture — `CopyResource`, MFT `ProcessInput`, VideoProcessor, shader views —
must be on **the same device**. LUID equality is necessary but not
sufficient; two `D3D11CreateDevice` calls on the same adapter give two
distinct devices, and any cross-device path is silent UB until something
breaks. Default to one device per pipeline; if you really need two, you
need a proper shared-resource handoff (`KEYED_MUTEX` / `D3D11_RESOURCE_MISC_SHARED_NTHANDLE`),
not a raw `CopyResource`.

Outside the driver there is no direct "has `IddCxMonitorArrival` been called
for DEVINST X" API. The practical external signals remain DXGI outputs,
`QueryDisplayConfig(QDC_ALL_PATHS)`, monitor-class PnP children, event logs,
and the driver's own logs if logging is enabled.

## Goal

Programmatically enable a Virtual Display Driver (the
[VirtualDrivers/Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
project, release 25.7+) right after an Android client connects to my PC
server, so the server's encoder captures the resulting virtual monitor
instead of the physical one. Disable it on disconnect. Idle PC = no
virtual monitor.

The "enable" half works at the PnP layer. **It does not produce a
monitor in DXGI.**

## What works

- `CM_Enable_DevNode(<DEVINST of ROOT\DISPLAY\0001>, 0)` returns `CR_SUCCESS`
  from an elevated process.
- `CM_Get_DevNode_Status` 300 ms after Enable shows the device fully
  healthy:

  ```
  before enable: status=0x01802401 problem=0x00000016   (CM_PROB_DISABLED)
  after  enable: status=0x0180200b problem=0x00000000
  ```

  Decoded: `DN_NT_DRIVER | DN_NT_ENUMERATOR | DN_DISABLEABLE | DN_STARTED
  | DN_DRIVER_LOADED | DN_ROOT_ENUMERATED`. No `DN_HAS_PROBLEM` bit.
- Status stays at this exact value sampled once per second for 15 s
  after Enable. Driver does not flip back to disabled or any error
  state.
- In the baseline PnP-only attempt there were no immediate
  `mttvdd.dll` exception entries in `Application`. After trying the VDD pipe
  `RELOAD_DRIVER` command, Windows did log `WUDFUnhandledException` for
  `mttvdd.dll` (`0xc0000005`) and UMDF offline events.

## What doesn't work

- After Enable, polling `IDXGIFactory6` (re-created each tick) every
  150 ms for 15 s never enumerates any output that looks virtual.
  Only the physical monitor is visible:

  ```
  Monitors seen during the wait:
    \\.\DISPLAY1 on NVIDIA GeForce RTX 5070 (3840x2160, attached=true)
  ```

- The virtual monitor never appears in Windows display-arrangement
  settings either (verified visually).
- This is the "driver process is running but never fires
  `IddCxMonitorArrival`" scenario, but I have no way to confirm that
  from outside the driver.

## Setup

- Windows 11 Pro 26200, NVIDIA RTX 5070 (driver 555+).
- VDD installed via Virtual Driver Control's bundled installer.
  `Get-PnpDevice` shows it at `ROOT\DISPLAY\0001` "Virtual Display
  Driver"; in `CM_PROB_DISABLED` while idle.
- `C:\VirtualDisplayDriver\vdd_settings.xml` is the binary's own
  auto-generated default after install (5 resolutions × multiple
  refresh rates + an `<options>` block). I also tried a stripped-down
  single-resolution version (2880×1800 only); same failure.
- Other display IDDs are also installed on the box (MuMu Player's
  virtual adapter, Wacom MovinkPad InstantPenDisplay). Both stay OK and
  are not touched by my code (selector picks `ROOT\DISPLAY\0001` by
  name + disabled-status).
- The user has a working setup of this exact driver on a previous
  project — the manual install via Virtual Driver Control's GUI
  produces a working virtual monitor. So the binary itself works on
  this rig; my programmatic Enable doesn't reach the same end state.

## What I've tried

1. **Enable via `Enable-PnpDevice` PowerShell cmdlet, elevated.** Same
   failure — driver healthy, no monitor.
2. **Enable via native `CM_Enable_DevNode` from an elevated helper
   sub-process** (`ShellExecuteW` with `runas` verb spawning the same
   binary in a `--vdd-helper enable <id>` mode). Same failure.
3. **Reload the driver via Virtual Driver Control GUI** between attempts.
4. **Attach display topology after enable.** Calling
   `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)` after PnP enable makes the VDD
   target appear in DXGI as a generic `\\.\DISPLAYxx` output.
5. **Reload via `\\.\pipe\MTTVirtualDisplayPipe` / `RELOAD_DRIVER`.**
   This is not a valid workaround on this setup: it produced
   `WUDFUnhandledException` crashes in `mttvdd.dll`.
6. **Different XML schemas** — minimal (just monitors/gpu/global/
   resolutions), the binary's auto-generated default, and a
   single-2880×1800-resolution rewrite of the default. All produced
   the same failure.
7. **Wait longer** — 15 s instead of the original 5 s. Same without topology
   attach; works once DisplayConfig extend is applied.

## Specific question

Given:
- `CM_Enable_DevNode` returns `CR_SUCCESS` from an elevated process.
- `CM_Get_DevNode_Status` reports the device started + driver loaded +
  no problem flag, persistently.
- Yet `IDXGIFactory::EnumAdapters1` + `IDXGIAdapter::EnumOutputs` walks
  do not surface any output for that device, even after 15 s.

What's the most likely cause and what's the next diagnostic step?

Specifically:
- Is there a step beyond `CM_Enable_DevNode` that's required to get an
  `IddCx`-class virtual driver to actually call `IddCxMonitorArrival`?
- Is there something the GUI tool (Virtual Driver Control) does on
  Enable beyond `CM_Enable_DevNode` that I'm missing? (DevCon /
  SetupAPI's `SetupDiCallClassInstaller(DIF_PROPERTYCHANGE,
  DICS_ENABLE)` does roughly the same thing — would it differ?)
- Is there a way to query, from outside the driver, whether
  `IddCxMonitorArrival` has been called for a given DEVINST? (i.e. how
  many monitors the driver has currently published)
- Are there event-log channels beyond `Application` /
  `Microsoft-Windows-Kernel-Pnp/Configuration` that an IddCx driver
  writes to during init?

## Code

The Rust caller is at
`crates/penflow-server/src/vdd.rs` in the
[zpenflow](https://github.com/zhangyushaow/zpenflow) repo. Specifically
`cm_enable()` + `verify_devnode_started()` + `wait_for_virtual_monitor()`.

The full helper trace from one failing run is reproduced above. I can
add live `CM_Get_DevNode_Status` polling at any cadence if there's a
specific transition to look for.

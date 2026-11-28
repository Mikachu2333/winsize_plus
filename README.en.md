# WinSizeP

[中文](README.md) | English

A Windows window manager for moving, resizing and arranging windows with Alt and the mouse. Supports multiple monitors, always-on-top windows, and mouse/keyboard annotations for recordings.

## Quick start

Run `winsizep.exe`. Open Settings or pause input capture from the tray menu.

**Hold or double-tap Left Alt to activate window actions.** Actions remain available after releasing Alt. Esc cancels; during a drag, it restores the original window position and size.

| Input | Action |
| --- | --- |
| Left drag / right drag | Move / resize the window |
| Left-click a region | Add or remove a selection; Enter applies |
| Right-click a region | Apply the region immediately |
| Mouse wheel | Cycle through overlapping regions under the pointer |
| Drag a highlighted shared edge | Resize adjacent windows together |
| Q/W/E, A/S/D, Z/X/C | Select one of nine default regions |
| Top-row 1–4 | Select one of four default regions |
| G | Center at 60% of the work area's width and height |
| Middle click / F | Center / maximize |
| P / L | Toggle always on top / crosshair |
| M / K | Toggle mouse / keyboard annotations |
| Grave key | Enter a region, layout or command in the floating tip bar |
| H | Show the current shortcut help |

Use these actions after Alt activation. Customize shortcuts, mouse buttons, regions and the UI font in Settings. Dropdowns support search, and changes save automatically. See tray Help and the setting descriptions for details.

Run WinSizeP as administrator to control elevated windows.

## Build

Requires Windows x64, Rust 1.88+ with the MSVC toolchain, and the Windows SDK.

```powershell
cargo build --release
cargo test
```

Executable: `target/release/winsizep.exe`. Supports Windows 10 1703 and later.

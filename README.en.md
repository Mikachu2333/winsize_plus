# WinSizeP

[中文](README.md) | English

A Windows window management tool that uses Alt shortcuts and mouse actions to move, resize, and arrange windows. Designed for multiple monitors, screen recordings, and presentations.

## Features

- Move, resize, center, and maximize windows across multiple monitors.
- Arrange windows with screen regions and a right-click grid; customize regions and shortcuts.
- Always-on-top control, crosshairs, and a marker for the window awaiting an action.
- Mouse and keyboard annotations for recordings and presentations.
- Pause / enable capture from the tray; settings save automatically and follow the Windows light/dark theme.
- Command-line control, automatic startup, and administrator mode.

## Usage

Run `winsizep.exe` and open Settings from the tray menu. Left Alt is the default: hold it or double-tap it, then press a key. Mouse actions target the foreground window selected when Alt was pressed.

| Common input | Action |
| --- | --- |
| Alt + left drag | Move the window |
| Alt + right drag | Resize the window |
| Alt + right click | Arrange using the grid |
| Alt + middle click | Center on the pointer's monitor |
| Alt + F | Maximize; when unavailable, fill the work area |
| Alt + Q/W/E, A/S/D, Z/X/C | Apply nine region presets |
| Alt + P | Toggle always on top |
| Alt + L | Toggle the crosshair |
| Alt + M / Alt + K | Toggle mouse / keyboard annotations |
| Alt + H | Show help with the current shortcuts |
| Esc | Close tips or cancel an action |

Customize shortcuts, mouse actions, regions, and annotations in Settings. See tray Help for more actions. Run WinSizeP as administrator to control elevated windows.

## Command line

```powershell
.\winsizep.exe --name notepad.exe --center
.\winsizep.exe --pid 123 --location 100,200 --size 900,600
.\winsizep.exe --help
```

## Build

Requires Windows x64, Rust 1.88+ with the MSVC toolchain, and the Windows SDK. Supports Windows 10 1703 and later.

```powershell
cargo build --release
cargo test
```

Executable: `target/release/winsizep.exe`.

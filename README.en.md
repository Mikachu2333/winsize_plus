# WinSizeP

Activation, mouse clicks, dragging and linked resizing are grouped under **Window actions**. A single Alt selector activates both keyboard and mouse actions; the mouse switch disables all mouse actions. Outline modes are **Show all after activation**, **Show after cumulative selection**, and **Hide throughout the session**. Esc turns off settings preview switches.

Dropdown arrow keys browse without saving; Enter or a confirmed mouse choice commits, and Esc restores the previous value. Empty searches display “No matches”. Click a shortcut field and press one key to capture it, or paste a key name/VK value. Inline messages distinguish drafts, invalid input and saved settings. Region and shortcut rows link directly to each other.

The command editor shows its target window and monitor, follows tip colors and fonts, and supports scrollable errors. Losing focus or pausing cancels it. Esc restores the starting placement for ordinary moves, ordinary resizing and linked resizing alike.

After Alt activation, drag a highlighted shared window edge to resize its neighbors together. Left or right drag works by default; elsewhere the original move/resize action applies. Adjacency tolerates gaps or overlaps up to 5 physical pixels and requires shared length greater than 1/16 of the corresponding work-area dimension. Connected seams support T-shaped and other arrangements; initial motion selects the axis at intersections. Group motion respects every participant's size limits. Esc restores the original geometry.

Default regions are numbered left to right, top to bottom: `1/9`–`9/9` use Q/W/E, A/S/D, Z/X/C; `1/4`–`4/4` use top-row 1–4, independently of the numpad. Regions tile the monitor work area, and cumulative selection uses the enclosing rectangle. Unmodified legacy default layouts migrate; custom layouts remain intact.

After Alt activation, press grave to open the native command editor. It supports IME input, caret editing and paste. Enter a region name, a layout such as `0|0|100sw|50sh`, or a command; Enter applies and Esc cancels. Layouts also accept `｜` and `、` separators. Invalid input remains available for correction.

[中文](README.md) | English

A Windows window management tool that uses Alt shortcuts and mouse actions to move, resize, and arrange windows. Designed for multiple monitors, screen recordings, and presentations.

## Features

- Move, resize, center, and maximize windows across multiple monitors.
- Arrange windows with actual screen regions, cumulative selection and customizable shortcuts.
- Always-on-top control, crosshairs, and a marker for the window awaiting an action.
- Mouse and keyboard annotations for recordings and presentations.
- Pause / enable capture from the tray; settings save automatically and follow the Windows light/dark theme.
- Command-line control, automatic startup, and administrator mode.

## Usage

Run `winsizep.exe` and open Settings from the tray menu. Left Alt is the default: hold it or double-tap it, then press a key. Mouse actions target the foreground window selected when Alt was pressed.

After an Alt hold activates capture or a WinSizeP action consumes input, releasing Alt suppresses the foreground application's menu activation. A short Alt tap and native shortcuts used before capture retain their usual behavior.

| Common input | Action |
| --- | --- |
| Alt + left drag | Move the window |
| Alt + right drag | Resize the window |
| Left / right click after Alt activation | Toggle a region / apply it immediately |
| Alt + middle click | Center on the pointer's monitor |
| Alt + F | Maximize; when unavailable, fill the work area |
| Alt + Q/W/E, A/S/D, Z/X/C | Apply nine region presets |
| Alt + P | Toggle always on top |
| Alt + L | Toggle the crosshair |
| Alt + M / Alt + K | Toggle mouse / keyboard annotations |
| Alt + H | Show help with the current shortcuts |
| Esc | Close tips or cancel an action |

Customize shortcuts, mouse actions, regions, and annotations in Settings. See tray Help for more actions. Run WinSizeP as administrator to control elevated windows.

The configuration file, `winsizep_config.toml`, stays beside the executable. Reading, saving, and opening it always use ordinary user permissions. Access failures identify the file and error; click the error message at the bottom left of Settings to read the full details. Concurrent save conflicts preserve a `.conflict.toml` file for reviewing external changes.

Save errors remain visible for 10 seconds. Clicking one opens a details message box that stays open until dismissed. When a new save replaces an active notice, the old notice disappears for 100 ms before the new one appears. The first notice and notices following normal expiry appear immediately.

Administrator autostart requires an executable and parent directories that ordinary users cannot replace. If this check fails, WinSizeP refuses registration and attempts to remove the previous startup task. You can continue using the application with administrator autostart disabled.

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

Hold or double-tap Alt to enter the same selection session, which survives Alt release. Enter confirms; Esc cancels. Empty selection closes after 10 seconds; selection changes restart a 3-second apply timer. Configure the click buttons under Region selection. Dragging cancels pending selection.

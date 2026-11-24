# WinSizeP

[中文](README.md) | English

A Windows window positioning and resizing tool with Alt shortcuts, mouse gestures, screen regions, crosshairs, input annotations, and a command line.

## Usage

Run `winsizep.exe` and open Settings from the tray menu. Changes are saved and applied automatically. You can also edit `winsizep_config.toml` beside the executable. Keep the program in a directory you can write to.

Left Alt is the default: hold it for 2 seconds to show the tip and activate keyboard and mouse actions, then press a key. You can also double-tap Alt, release it, then press a key. Set the delay on the General page (`general.alt_hold_ms`, 0–60000 ms; 0 activates immediately). Combinations entered before activation go to the application and cancel that hold's activation. Region previews and tips stay visible for 3 seconds from when they appear; releasing held Alt closes them earlier. General → Open configuration file opens `winsizep_config.toml` in `notepad.exe`.

| Default input | Action |
| --- | --- |
| Alt + left drag | Move the window |
| Alt + right drag | Resize from the top-left corner |
| Alt + right click | Place the window using the grid cell under the pointer |
| Alt + middle click | Center on the pointer's monitor, scaling for DPI and clamping oversized edges to the work area |
| Alt + F | Maximize; when unavailable, fill the pointer monitor's work area |
| Alt + Q/W/E, A/S/D, Z/X/C | Apply nine region presets; S centers the window |
| Alt + P | Toggle always on top |
| Alt + L | Toggle the crosshair |
| Alt + M / Alt + K | Toggle mouse / keyboard annotations |
| Alt + U | Exclude the current window |
| Alt + backtick (\`) | Enter a command; press Enter to run it |
| Alt + H | Show help with the current shortcuts |
| Esc | Close tips, cancel an action, or reset a focused preview |

Mouse actions target the foreground window selected when Alt was pressed. Settings let you choose the Alt side, drag and centering buttons, and shortcuts. Enabled mouse actions must use distinct buttons. Centering can be disabled; its DPI scaling and work-area limits always apply. Key names are case-insensitive and displayed consistently as `W`, `F12`, or `Insert`. Reserved system combinations cannot be bound.

The first tray item, Pause / Enable, controls all shortcut and input capture and shows one corresponding notice. Tray commands remain available while paused.

## Settings and previews

- Add, remove, rename, and rebind regions. Previews center each region name above its current shortcuts, such as `FullTop` followed by `[ W ]` on the next line.
- Hover over a grid cell for 2 seconds to preview its region. Move outside the region or press Esc to return to the grid.
- Customize tips, outlines, fonts, crosshairs, and input annotations in Settings. Sample previews pause when Settings loses focus.
- Settings follow the Windows light/dark theme. The sidebar scrolls with the wheel without a visible scrollbar, and status messages float at the bottom-left.
- Crosshairs default to RGBA(255,255,60,204). Unified mode applies the horizontal style to both axes; Custom mode edits each axis separately and retains its saved parameters across mode changes.
- Once Alt enters the waiting state, a fixed translucent pale-yellow 4 px frame marks the target from inside its edges. Clicks pass through, and the frame disappears when an action starts. It is enabled by default and can be disabled on General (`general.target_marker`).
- Mouse and keyboard annotations start disabled and share a fixed 400 ms fade. Keyboard annotation display duration is adjustable.
- Help and scrollable long tips remain visible until dismissed with Esc.
- Configure automatic startup and administrator privileges in Settings. Controlling an elevated window requires matching privileges.
- To include an excluded window again, remove its `[[exclusions]]` entry from the configuration.
- Windows 10 and 11 system tray overflow menus are excluded by default. All nonempty fields within a rule must match; any matching rule excludes a window, so multiple classes in the same process can be excluded together.

## Regions and configuration

Region format: `x|y|width|height[|monitor]`. For example, `0|0|50sw|50sh` selects the top-left quarter. Omit the monitor number or use 0 to follow the pointer's monitor.

| Unit | Meaning |
| --- | --- |
| sw / sh | Percentage of work-area width / height |
| ww / wh | Percentage of current window width / height |
| vw / vh | Percentage of virtual-desktop width / height |
| No unit | Pixels |

Expressions such as `50sw-8` are supported. Grid cells run from 1 to 9, top-left to bottom-right. A monitor-specific cell is “monitor number × 10 + cell”:

```toml
[grid]
7 = "LeftBottom"
37 = "Center" # Bottom-left cell on monitor 3
```

Region names support Unicode. Region and command names must not conflict with built-in actions. Use UTF-8 for configuration files and pipeline input. Global command entry accepts ASCII letters, digits, spaces, hyphens, and underscores.

## Command line

```powershell
.\winsizep.exe --name notepad.exe --preset Center
.\winsizep.exe --pid 123 --location 100,200 --size 900,600
.\winsizep.exe --hwnd 0x1234 --top on
.\winsizep.exe --name notepad.exe --center
.\winsizep.exe --name notepad.exe --fullscreen
.\winsizep.exe --settings
.\winsizep.exe --check
123, 456 | .\winsizep.exe --stdin --preset Center
```

Select windows with `--pid`, `--hwnd`, `--name`, `--class`, or `--title`; omitting selectors targets the foreground window. `--stdin` accepts one argument list or PID per line. Use `--check` to validate configuration and tray Help to see current bindings.

## Build

Requires Windows x64, Rust 1.88+ with the MSVC toolchain, and the Windows SDK. The application supports Windows 10 1703 and later.

```powershell
cargo build --release
cargo test
```

Executable: `target/release/winsizep.exe`.

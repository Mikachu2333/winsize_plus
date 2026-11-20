# WinSizeP

[中文](README.md) | English

A Windows utility for moving and resizing windows with Alt shortcuts, mouse dragging, screen regions, and command-line requests.

## Usage

Requires Windows 10 version 1703 or newer (x64 or x86). Building requires Rust 1.88 or newer with MSVC and the Windows SDK. This is a portable application: place it in a user-writable directory so its adjacent configuration can be created and updated.

Floating help and tips are capped at one third of the current monitor's work-area height. Overflow adds a vertical scrollbar with arrow buttons and a draggable thumb; mouse wheel, PageUp/PageDown, Up/Down, and Home/End also scroll. Press Esc to close. Oversized tips remain visible instead of expiring while you read them.

Region names must not collide with built-in actions or aliases such as `Quit`, `Help`, or `Pin`. Rename an existing collision and its bindings explicitly; invalid updates are rejected without deleting the configuration.

Run `winsizep.exe` and open Settings from the tray. Changes are saved and applied automatically. You can also edit `winsizep_config.toml` beside the executable. Only one resident instance runs at a time.

Left Alt is the default. Hold Alt with a key, or double-tap Alt, release it, then press a key. Double-tap timing follows Windows. Region previews appear immediately with the tip and close together after the tip duration, which defaults to 3 seconds. Releasing held Alt closes both sooner. Triggering again restarts the timer.

| Default input | Action |
| --- | --- |
| Alt + left-button drag | Move the window |
| Alt + right-button drag | Resize while keeping the top-left corner fixed |
| Alt + right click | Place using the screen's 3×3 grid |
| Alt + Q/W/E, A/S/D, Z/X/C | Apply nine presets; S centers the window at 80% of the work area |
| Alt + P | Toggle always on top |
| Alt + L | Toggle the crosshair |
| Alt + M / Alt + K | Toggle mouse / keyboard annotations |
| Alt + U | Exclude the current window |
| Alt + grave/backtick | Enter a command; Enter runs it |
| Alt + H | Show help; Esc closes it, with no timeout |
| Esc | Close a tip or cancel an action; reset a focused region/grid preview |

Mouse actions use the window active when Alt was pressed. Move and resize can be disabled independently or assigned to the left, right, middle, or two side buttons. Help reflects the bindings. To allow an excluded window again, remove its `[[exclusions]]` entry from the configuration file.

Help arranges region bindings in five keyboard rows: function keys, numbers and symbols, and the three letter rows. Empty rows are omitted. Crosshair controls occupy one line, followed immediately by mouse and keyboard annotation shortcuts together on the next line; this group has a blank line before and after it. Other keys follow. The tip measures key and region columns independently using the configured fonts and wraps oversized rows.

The program's settings window and its color dialog are automatically excluded without configuration.

## Settings and previews

Nine pages cover General, Tips, Screen regions, Region outlines, Quick actions, Crosshair, Right-click grid, Mouse annotations, and Keyboard annotations. The interface supports Chinese, English, and the system language.

- The program creates, updates, and removes its logon task. Changing elevation updates task permissions; Windows requests UAC consent when needed.
- Font choices use their own typefaces. `SystemFont` means the system UI font. Add up to three fallback families and drag to reorder them. Sizes use points; tips default to 12 pt.
- Add, remove, or rename regions. Renaming updates bindings. Enable desktop preview and select a region to preview it; leaving the page hides it. Previews pause when another window is active and resume when you return to settings.
- Overlapping regions use the nearest center to the pointer, with the upper-left region winning a tie. The selection uses a translucent system-accent fill and a centered name; outlines keep the same thickness.
- The grid shows cell numbers and mapped region names. Hover for 2 seconds to show the actual region; leave it, reach a screen edge, or press Esc to restore the grid.
- Configure each crosshair axis separately, or choose Both axes to apply a shared style. Both axes share one FPS value, an integer from 24 to 1000, defaulting to 30 and clamping to the limits. No continuous drawing occurs while stationary.

Default colors use Fluent neutrals and blue, with Windows system colors for interaction states. Existing custom colors are preserved.

Mouse annotations use small translucent filled shapes centered on the pointer: left/right semicircles form a circle, the middle button is a smaller rectangle above the center, and scrolling uses directional triangles. Side buttons use two small segments. Colors are derived from the Windows accent. Held buttons stay visible until released; continuous scrolling extends the same indicator without restarting its fade. Scroll direction follows the OS wheel events, including driver inversion. Keyboard annotations show physical key names, Ctrl/Alt/Shift/Win combinations, recent entries, and consecutive repeat counts. Annotations pass input through and do not take focus. Both are off by default; Alt+M and Alt+K toggle the current session, while each settings page's “Enable on startup” controls the next launch. Existing occupied or disabled shortcuts are preserved during migration.

Both annotation pages offer sample previews. Mouse settings expose startup state, shape radius (9 logical pixels by default), translucency, and a side-button display swap. The swap is enabled by default: X1 appears below X2; disabling it restores X1 above X2 without changing either button's action. Existing compact shapes shrink to two thirds once on migration. Mouse timing is fixed at about half a second. Repeated mouse events merge into existing state; image uploads are throttled to one frame per 16 ms, and unchanged held shapes are not redrawn. Animations keep updating while the tray menu is open. Keyboard settings retain font, colors, opacity, linger/fade times, history length, and screen corner. Previews stop with Esc and pause when settings loses focus. Annotations use the pointer's monitor and its DPI. Input is displayed temporarily in memory and never logged to disk; keyboard labels represent keys rather than IME-composed text. The interaction design was informed by KeyCastOW.

A normal-permission resident leaves higher-integrity windows alone. After returning to an ordinary window, shortcuts remain available. Idle input-state reconciliation clears missed Alt/button releases, and a hook worker that resumes after a long stall reinstalls its hooks and abandons incomplete gestures. “Input thread did not respond” means an internal command exceeded its two-second wait; severe system stalls can cause it, but it is not a successful or normal operation. Repeated timeouts still require investigation. Pointer movement and tip-state notifications no longer synchronously wait for the worker.

## Region expressions

Syntax: `x|y|width|height[|monitor]`. For example, `0|0|50sw|50sh` selects the upper-left quarter.

| Unit | Meaning |
| --- | --- |
| sw / sh | Percentage of work-area width / height |
| ww / wh | Percentage of current window width / height |
| vw / vh | Percentage of virtual-desktop width / height |
| No unit | Pixels |

`50sw-8` means half the work-area width minus 8 pixels. Monitor 0, or an omitted monitor, follows the pointer. Expressions containing vw/vh use the virtual-desktop origin.

Grid cells run from 1 to 9, starting at the upper left. For a particular monitor, use `monitor * 10 + cell`. Monitor 3's bottom-left cell is `3*10+7=37`:

```toml
[grid]
7 = "LeftBottom"
37 = "Center"
```

Monitor-specific mappings take precedence; other monitors use the general cell mapping.

## Built-in actions and commands

Commands and presets share name normalization, including case, surrounding whitespace, and full-width letters and digits; region display names retain their original spelling. Actions are defined by the program; `[keys]` and `[commands]` bind names to them. Commands cannot shadow regions or change the meaning of built-in actions and aliases. Identical mappings such as `Pin = "Top"` remain valid. Conflicting commands reject the update and preserve the original configuration.

| Action | Alias | Behavior |
| --- | --- | --- |
| Version | Ver | Show version |
| Help | H | Show help |
| Top | Pin | Toggle always on top |
| Ignore | Exclude | Exclude window |
| Guide | — | Toggle crosshair |
| MouseCast / KeyCast | — | Toggle mouse / keyboard annotations |
| Horizontal | — | Show horizontal crosshair, then hide on repeat; Alt + Minus by default |
| Vertical | — | Show vertical crosshair, then hide on repeat; Alt + Backslash by default |
| Cross | — | Show both axes, then hide on repeat; Alt + Equal by default |
| Borders | — | Toggle region outlines |
| Time / Monitor | Screen (Monitor) | Show time / monitor number |
| Command / Settings / Reload | — | Enter a command / open settings / reload configuration |
| Restart / Quit | Reboot / Exit | Restart / exit |

`gesture` selects the Alt side for mouse actions. The `horizontal` and `vertical` sections style the two crosshair axes. Esc/Escape cannot name a region or command. Tab, F4, Space, Enter, PrintScreen, arrows, PageUp/PageDown, Home/End, Esc, and modifiers cannot be bound, preserving common system Alt shortcuts and internal navigation. `VK:xx` uses the same restrictions; application-specific letter shortcuts remain configurable. Alternate spellings of a key, such as Q and VK:51, cannot be defined twice, even with an empty binding.

## Command line and pipelines

```powershell
.\winsizep.exe --name notepad.exe --preset Center
.\winsizep.exe --pid=123 --location 100,200 --size 900,600
.\winsizep.exe --hwnd 0x1234 --top on
.\winsizep.exe --settings
.\winsizep.exe --check
.\winsizep.exe Ver
123, 456 | .\winsizep.exe --stdin --preset Center
'--name notepad.exe --preset LeftTop' | .\winsizep.exe --stdin
```

Selectors include `--pid`, `--hwnd`, `--name`, `--class`, and `--title`. Different filters must all match. Repeated filters keep the first value. Multiple matching visible windows use the first enumerated window; no selector uses the foreground window. Explicit coordinates override preset coordinates.

`--stdin` reads one argument list or PID per line and skips blank lines. Running without arguments also reads redirected stdin. Window requests work alongside the resident instance. Success goes to stdout, errors to stderr, and failures return a nonzero exit code.

Common Chinese punctuation outside strings is repaired only when parsing fails. Invalid field values revert individually with a notice; unrecoverable syntax preserves the file. External file edits win conflicting GUI changes, while unrelated GUI changes are retained.

## Chinese text and encodings

Ordinary text controls in Settings, region names, font names, tip formats, and exclusion process names, window classes, and titles support Chinese text. Command-line values for titles, process names, classes, and regions, as well as the executable directory, are handled as Unicode. Numbers, colors, region expressions, key names, and fixed configuration options still require their defined syntax. Chinese TOML keys must be quoted, for example `"左上区域" = "0|0|50sw|50sh"`.

The global command-entry mode currently accepts only English letters, digits, spaces, hyphens, and underscores; it does not support Chinese IME composition or paste. Names in `[commands]` are restricted to the same ASCII characters. Settings rejects Chinese command names, and configuration recovery removes manually added invalid names with a notice. Chinese regions can be bound to keys, invoked with `--preset "左上区域"`, or addressed through an English alias such as `Zuoshang = "左上区域"`.

Configuration reading supports UTF-8 with or without a BOM, and UTF-16LE or UTF-16BE with a BOM. GBK/CP936, BOM-less UTF-16, and UTF-32 are unsupported. Creating or saving a configuration writes UTF-8 without a BOM, preserving Chinese text but not the original UTF-16 encoding. Undecodable files are rejected without modification; convert them to UTF-8 in an editor first.

The system code page and a file's encoding are separate: the application's UI, paths, and command-line arguments use Unicode interfaces rather than the ANSI code page configured for non-Unicode programs. A system using CP936 still requires configuration files in one of the supported encodings above. Rendering Chinese text also requires suitable installed fonts or fallback fonts.

`--stdin` pipes and redirected input require UTF-8; they do not automatically recognize GBK or UTF-16. Redirected standard output and error output also use UTF-8. Scripts and upstream programs must send UTF-8 bytes; text already replaced with question marks before transmission cannot be recovered.

## Build

Use Windows, the Rust MSVC toolchain, and Windows SDK:

```powershell
cargo build --release
cargo test
```

The executable is `target/release/winsizep.exe`. Change the product version only in `Cargo.toml` under `[package].version`; command output and Windows version resources derive from it.

Explicit monitor numbers must identify a connected display; unavailable monitors report an error. Invalid region expressions are skipped in previews with a diagnostic. Cross-monitor positioning verifies the final visible frame and reports a timeout if the target does not accept the requested rectangle.

Interactive multi-monitor regressions can be run with `cargo test multi_monitor_regressions -- --ignored --test-threads=1`. Cases requiring a second monitor or different DPI values automatically return with a `SKIPPED` explanation when that hardware is unavailable; the other cases still run. Set `WINSIZEP_TEST_SINGLE_MONITOR=1` to exercise that skip path without changing the display layout.

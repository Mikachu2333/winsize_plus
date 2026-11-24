#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cast;
mod cli;
mod colors;
mod config;
mod desktop;
mod fonts;
mod framerate;
mod input;
mod label_text;
mod layout;
mod logon;
mod mouse_cast;
mod overlay;
mod preview;
mod resident;
mod security;
mod settings;
mod settings_paint;
mod settings_schema;
mod settings_theme;
mod settings_tooltip;
mod tray;

use std::{
    io::BufRead,
    os::windows::process::CommandExt,
    process::{Command, ExitCode, Stdio},
    ptr::{null, null_mut},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Security::*,
        System::{Recovery::RegisterApplicationRestart, Threading::*},
        UI::{HiDpi::*, Shell::*, WindowsAndMessaging::*},
    },
    core::w,
};

pub enum ResidentExit {
    Quit,
    Restart(Option<bool>),
}

/// Keep this process available until a restart has been accepted. In particular,
/// denied UAC must return to a functioning resident instead of terminating it.
fn supervise_resident(
    mut run: impl FnMut(Option<&str>) -> Result<ResidentExit, String>,
    mut restart: impl FnMut(Option<bool>) -> Result<(), String>,
) -> Result<(), String> {
    let mut recovery = None;
    loop {
        match run(recovery.as_deref())? {
            ResidentExit::Quit => return Ok(()),
            ResidentExit::Restart(elevation) => match restart(elevation) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    recovery = Some(format!(
                        "Restart failed; previous permissions restored / 重启失败，已恢复原权限运行: {error}"
                    ))
                }
            },
        }
    }
}

pub fn quote_argument(text: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in text.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            '\\',
            if ch == '"' { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        result.push(ch);
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

pub fn elevated_helper(parameters: &str) -> Result<(), String> {
    let exe = desktop::wide(std::env::current_exe().map_err(|error| error.to_string())?);
    let parameters = desktop::wide(parameters);
    // SAFETY: the strings outlive ShellExecuteExW; the returned process handle is closed below.
    unsafe {
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpVerb: w!("runas"),
            lpFile: exe.as_ptr(),
            lpParameters: parameters.as_ptr(),
            nShow: SW_HIDE,
            ..Default::default()
        };
        if ShellExecuteExW(&mut info) == 0 {
            return Err(format!("UAC: {}", desktop::err()));
        }
        if info.hProcess.is_null() {
            return Err("UAC: no helper process / 未启动权限助手".into());
        }
        let waited = wait_process(info.hProcess, std::time::Duration::from_secs(30));
        if let Err(error) = waited {
            // Only terminate the helper we just created; prevent a timed-out task
            // update from committing after a later user edit.
            TerminateProcess(info.hProcess, 1);
            CloseHandle(info.hProcess);
            return Err(error);
        }
        let mut code = 1;
        let ok = GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if ok == 0 || code != 0 {
            return Err(format!(
                "Autostart update failed / 自动启动更新失败 ({code})"
            ));
        }
    }
    Ok(())
}

/// Wait for our child without starving owned windows or waiting indefinitely.
pub fn wait_process(process: HANDLE, timeout: std::time::Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut deferred = Vec::new();
    let result = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break Err("Helper timed out / 助手进程超时".into());
        }
        // SAFETY: the process handle is owned by the caller until this returns.
        unsafe {
            match MsgWaitForMultipleObjectsEx(
                1,
                &process,
                remaining.as_millis().min(100) as u32,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            ) {
                WAIT_OBJECT_0 => break Ok(()),
                n if n == WAIT_OBJECT_0 + 1 => {
                    let mut msg = MSG::default();
                    for _ in 0..64 {
                        if PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) == 0 {
                            break;
                        }
                        if msg.hwnd.is_null() {
                            deferred.push(msg);
                        } else {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                }
                WAIT_TIMEOUT => {}
                _ => break Err(desktop::err()),
            }
        }
    };
    // The outer loop owns application thread messages. Preserve them while a
    // synchronous helper is pending instead of discarding them in DispatchMessage.
    for msg in deferred {
        unsafe {
            PostThreadMessageW(GetCurrentThreadId(), msg.message, msg.wParam, msg.lParam);
        }
    }
    result
}

fn launch_unelevated() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    // Use the interactive shell's token so a permission change does not inherit this admin token.
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(GetShellWindow(), &mut pid);
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(desktop::err());
        }
        let mut token = null_mut();
        let ok = OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &mut token);
        CloseHandle(process);
        if ok == 0 {
            return Err(desktop::err());
        }
        let mut primary = null_mut();
        let ok = DuplicateTokenEx(
            token,
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY,
            null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(desktop::err());
        }
        let mut command = desktop::wide(format!(
            "{} --resident",
            quote_argument(&exe.to_string_lossy())
        ));
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut child = PROCESS_INFORMATION::default();
        let ok = CreateProcessWithTokenW(
            primary,
            0,
            null(),
            command.as_mut_ptr(),
            CREATE_NO_WINDOW,
            null(),
            null(),
            &startup,
            &mut child,
        );
        let error = desktop::err();
        CloseHandle(primary);
        if ok == 0 {
            return Err(error);
        }
        CloseHandle(child.hThread);
        CloseHandle(child.hProcess);
    }
    Ok(())
}

fn launch(elevate: bool) -> Result<(), String> {
    if resident::running() {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    if elevate && unsafe { IsUserAnAdmin() } == 0 {
        let result = unsafe {
            ShellExecuteW(
                null_mut(),
                w!("runas"),
                desktop::wide(exe).as_ptr(),
                w!("--resident"),
                null(),
                SW_HIDE,
            )
        };
        if result as isize <= 32 {
            return Err(format!("UAC: {}", desktop::err()));
        }
    } else {
        Command::new(exe)
            .arg("--resident")
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
fn attach_cli_console() {
    use windows_sys::Win32::{Foundation::INVALID_HANDLE_VALUE, System::Console::*};
    unsafe {
        let outputs = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .map(|stream| (stream, GetStdHandle(stream)));
        let valid = |handle: windows_sys::Win32::Foundation::HANDLE| {
            !handle.is_null() && handle != INVALID_HANDLE_VALUE
        };
        if outputs.iter().any(|(_, handle)| !valid(*handle))
            && AttachConsole(ATTACH_PARENT_PROCESS) != 0
        {
            for (stream, handle) in outputs {
                if valid(handle) {
                    SetStdHandle(stream, handle);
                }
            }
        }
    }
}

fn run() -> Result<(), String> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    let explicit_stdin = args.iter().any(|arg| arg.eq_ignore_ascii_case("--stdin"));
    let redirected = unsafe {
        let handle = windows_sys::Win32::System::Console::GetStdHandle(
            windows_sys::Win32::System::Console::STD_INPUT_HANDLE,
        );
        matches!(
            windows_sys::Win32::Storage::FileSystem::GetFileType(handle),
            windows_sys::Win32::Storage::FileSystem::FILE_TYPE_PIPE
                | windows_sys::Win32::Storage::FileSystem::FILE_TYPE_DISK
        )
    };
    if explicit_stdin || (args.is_empty() && redirected) {
        #[cfg(not(debug_assertions))]
        attach_cli_console();
        let shared: Vec<_> = args
            .into_iter()
            .filter(|arg| !arg.eq_ignore_ascii_case("--stdin"))
            .collect();
        let mut failed = false;
        for (index, line) in std::io::stdin().lock().lines().enumerate() {
            let result = (|| {
                let line = line.map_err(|error| error.to_string())?;
                if line.trim().is_empty() {
                    return Ok(());
                }
                let mut values = if line.trim().parse::<u32>().is_ok() {
                    vec!["--pid".into(), line.trim().into()]
                } else {
                    cli::split(&line)?
                };
                values.extend(shared.iter().cloned());
                execute(&values)
            })();
            if let Err(error) = result {
                eprintln!("stdin:{}: {error}", index + 1);
                failed = true;
            }
        }
        return if failed {
            Err("Some pipeline requests failed / 部分管道请求失败".into())
        } else {
            Ok(())
        };
    }
    execute(&args)
}

fn execute(args: &[String]) -> Result<(), String> {
    let mut args = args.to_vec();
    if let Some(first) = args.first_mut() {
        *first = match config::command_name(first).as_str() {
            "version" | "ver" | "--version" | "-v" => "--version".into(),
            "help" | "h" | "--help" | "-h" => "--help".into(),
            _ if first.starts_with("--") => first.to_ascii_lowercase(),
            _ => first.clone(),
        };
    }
    let values: Vec<_> = args.iter().map(String::as_str).collect();
    #[cfg(not(debug_assertions))]
    if !matches!(values.as_slice(), [] | ["--resident"] | ["--settings"]) {
        attach_cli_console();
    }
    let path = config::path()?;
    match values.as_slice() {
        ["--version" | "-V"] => println!("WinSizeP {}", env!("CARGO_PKG_VERSION")),
        ["--help" | "-h"] => {
            println!(
                "{}\n\nwinsizep --pid 123 --preset Center\nwinsizep --name notepad.exe --size 900,600\nwinsizep --stdin [shared options]\n--settings  --check  --version",
                config::load(&path, None)?.console_help_text()
            );
        }
        ["--settings"] => return settings::run(&path),
        ["--sync-logon-task", enabled, elevated, owner]
            if matches!(*enabled, "0" | "1") && matches!(*elevated, "0" | "1") =>
        {
            if let Err(error) = logon::sync_direct(*enabled == "1", *elevated == "1", owner) {
                eprintln!("{error}");
                // Private helper protocol: the parent may request consent only for
                // access denial, not for malformed XML or an unavailable service.
                std::process::exit(if error.code() == windows::core::HRESULT::from_win32(5) {
                    5
                } else {
                    1
                });
            }
            return Ok(());
        }
        ["--check"] => {
            let cfg = config::load(&path, None)?;
            if !cfg.warnings.is_empty() {
                eprintln!(
                    "{}: {}",
                    cfg.tr("Fields restored to defaults", "已恢复默认值的字段"),
                    cfg.warnings.join(", ")
                );
            }
            println!(
                "{} {}",
                cfg.tr("Valid configuration", "配置有效"),
                path.display()
            )
        }
        [] => return launch(false),
        ["--resident"] => {
            if config::requested_elevation(&path)? && unsafe { IsUserAnAdmin() } == 0 {
                return launch(true);
            }
            unsafe {
                RegisterApplicationRestart(w!("--resident"), 0);
            }
            return supervise_resident(
                |recovery| resident::run(&path, recovery),
                |elevation| {
                    if elevation == Some(false) && unsafe { IsUserAnAdmin() } != 0 {
                        launch_unelevated()
                    } else {
                        launch(elevation.unwrap_or(false))
                    }
                },
            );
        }
        _ => {
            let req = cli::parse(&args)?;
            let hwnd = desktop::target(&req.target)?;
            let cfg = config::load(&path, None)?;
            desktop::apply(hwnd, &req, &cfg, &path, desktop::cursor()?)?;
            println!("HWND=0x{:X} OK", hwnd as usize);
        }
    }
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("WinSizeP: {err}");
            if std::env::args()
                .nth(1)
                .is_none_or(|arg| matches!(arg.as_str(), "--resident" | "--settings"))
            {
                // Background launches intentionally have no stderr. Show failures
                // where the user can see them rather than silently losing the tray.
                unsafe {
                    MessageBoxW(
                        null_mut(),
                        desktop::wide(&err).as_ptr(),
                        w!("WinSizeP"),
                        MB_OK | MB_ICONERROR,
                    );
                }
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn denied_elevation_resumes_the_resident_with_a_visible_error() {
        let mut runs = 0;
        let mut requests = 0;
        super::supervise_resident(
            |recovery| {
                runs += 1;
                if runs == 1 {
                    assert!(recovery.is_none());
                    Ok(super::ResidentExit::Restart(Some(true)))
                } else {
                    assert!(recovery.unwrap().contains("UAC canceled"));
                    Ok(super::ResidentExit::Quit)
                }
            },
            |elevation| {
                requests += 1;
                assert_eq!(elevation, Some(true));
                Err("UAC canceled".into())
            },
        )
        .unwrap();
        assert_eq!((runs, requests), (2, 1));
    }
    #[test]
    fn quotes_windows_paths_without_losing_backslashes_or_quotes() {
        assert_eq!(
            super::quote_argument(r"C:\Program Files\WinSizeP\"),
            "\"C:\\Program Files\\WinSizeP\\\\\""
        );
        assert_eq!(super::quote_argument("a\"b"), "\"a\\\"b\"");
    }
}

#[cfg(test)]
mod test_support;

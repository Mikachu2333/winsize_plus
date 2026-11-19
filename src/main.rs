#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cli;
mod config;
mod desktop;
mod framerate;
mod input;
mod layout;
mod logon;
mod overlay;
mod preview;
mod resident;
mod settings;
mod tray;

use std::{
    os::windows::process::CommandExt,
    process::{Command, ExitCode, Stdio},
    ptr::{null, null_mut},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Security::*,
        System::{Recovery::RegisterApplicationRestart, Threading::*},
        UI::{
            HiDpi::*,
            Shell::*,
            WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId, SW_HIDE},
        },
    },
    core::w,
};

pub enum ResidentExit {
    Quit,
    Restart(Option<bool>),
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
        WaitForSingleObject(info.hProcess, INFINITE);
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

fn launch_unelevated() -> Result<(), String> {
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
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
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
        let outputs =
            [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|stream| (stream, GetStdHandle(stream)));
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
    let values: Vec<_> = args.iter().map(String::as_str).collect();
    #[cfg(not(debug_assertions))]
    if !matches!(values.as_slice(), [] | ["--resident"] | ["--settings"]) {
        attach_cli_console();
    }
    let path = config::path()?;
    match values.as_slice() {
        ["--version" | "-V"] => println!("WinSizeP {}", env!("CARGO_PKG_VERSION")),
        ["--help" | "-h"] => print!("{}", config::help()),
        ["--settings"] => return settings::run(&path),
        ["--sync-logon-task", enabled, elevated, owner]
            if matches!(*enabled, "0" | "1") && matches!(*elevated, "0" | "1") =>
        {
            return logon::sync_direct(*enabled == "1", *elevated == "1", owner)
                .map_err(|error| error.to_string());
        }
        ["--export-logon-task", file] => {
            logon::export(std::path::Path::new(file))?;
            println!("Import in Task Scheduler / 请在任务计划程序中导入 {file}");
        }
        ["--check"] => {
            let cfg = config::load(&path, None)?;
            println!(
                "{} {}",
                cfg.tr("Valid configuration", "配置有效"),
                path.display()
            )
        }
        [] => return launch(config::load(&path, None)?.elevate),
        ["--resident"] => {
            let cfg = config::load(&path, None)?;
            if cfg.elevate && unsafe { IsUserAnAdmin() } == 0 {
                return launch(true);
            }
            unsafe {
                RegisterApplicationRestart(w!("--resident"), 0);
            }
            match resident::run(&path)? {
                ResidentExit::Quit => {}
                ResidentExit::Restart(Some(false)) if unsafe { IsUserAnAdmin() } != 0 => {
                    return launch_unelevated();
                }
                ResidentExit::Restart(elevation) => return launch(elevation.unwrap_or(false)),
            }
        }
        _ => {
            let req = cli::parse(&args)?;
            let hwnd = desktop::target(&req.target)?;
            let cfg = config::load(&path, None)?;
            desktop::apply(hwnd, &req, &cfg, &path, desktop::cursor()?)?
        }
    }
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("WinSizeP: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn quotes_windows_paths_without_losing_backslashes_or_quotes() {
        assert_eq!(
            super::quote_argument(r"C:\Program Files\WinSizeP\"),
            "\"C:\\Program Files\\WinSizeP\\\\\""
        );
        assert_eq!(super::quote_argument("a\"b"), "\"a\\\"b\"");
    }
}

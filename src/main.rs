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
        System::{Recovery::RegisterApplicationRestart, Threading::CREATE_NO_WINDOW},
        UI::{
            HiDpi::*,
            Shell::{IsUserAnAdmin, ShellExecuteW},
            WindowsAndMessaging::SW_HIDE,
        },
    },
    core::w,
};

fn launch(elevate: bool) -> Result<(), String> {
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
            if resident::run(&path)? {
                return launch(config::load(&path, None)?.elevate);
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

mod cli;
mod config;
mod desktop;
mod input;
mod layout;
mod logon;
mod overlay;
mod resident;

use std::{
    os::windows::process::CommandExt,
    process::{Command, ExitCode},
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
            .spawn()
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn run() -> Result<(), String> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    let values: Vec<_> = args.iter().map(String::as_str).collect();
    let path = config::path()?;
    match values.as_slice() {
        ["--version" | "-V"] => println!("WinSizeP {}", env!("CARGO_PKG_VERSION")),
        ["--help" | "-h"] => print!("{}", config::HELP),
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

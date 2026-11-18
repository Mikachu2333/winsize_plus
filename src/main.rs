mod cli;
mod config;
mod desktop;
mod layout;

use std::process::ExitCode;
use windows_sys::Win32::UI::HiDpi::*;

fn run() -> Result<(), String> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    let values: Vec<_> = args.iter().map(String::as_str).collect();
    let path = config::path()?;
    match values.as_slice() {
        ["--version" | "-V"] => println!("WinSizeP {}", env!("CARGO_PKG_VERSION")),
        [] | ["--help" | "-h"] => print!("{}", config::HELP),
        ["--check"] => {
            let cfg = config::load(&path, None)?;
            println!(
                "{} {}",
                cfg.tr("Valid configuration", "配置有效"),
                path.display()
            )
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

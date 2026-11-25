use std::path::Path;
use windows::{
    Win32::{
        System::{Com::*, Variant::VARIANT},
        UI::Shell::*,
    },
    core::{BSTR, Interface},
};

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

fn shell(exe: &Path, parameters: &str) -> Result<(), String> {
    // Refuse to delegate to an elevated shell, which would preserve administrator rights.
    unsafe {
        use windows_sys::Win32::{
            Foundation::CloseHandle, System::Threading::*, UI::WindowsAndMessaging::*,
        };
        let mut pid = 0;
        GetWindowThreadProcessId(GetShellWindow(), &mut pid);
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(crate::desktop::err());
        }
        let elevated = crate::security::process_elevated(process);
        CloseHandle(process);
        if elevated? {
            return Err("Desktop shell is elevated / 桌面外壳以管理员权限运行".into());
        }
    }
    let exe = exe
        .to_str()
        .ok_or("Invalid executable path / 程序路径无效")?;
    let run = || -> windows::core::Result<()> {
        // SAFETY: all automation arguments and COM interfaces remain alive through
        // synchronous calls; the interactive desktop shell performs process creation.
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
            let _apartment = Apartment;
            let windows: IShellWindows =
                CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
            let mut hwnd = 0;
            let desktop = windows.FindWindowSW(
                &VARIANT::from(CSIDL_DESKTOP as i32),
                &VARIANT::default(),
                SWC_DESKTOP,
                &mut hwnd,
                SWFO_NEEDDISPATCH,
            )?;
            let provider: IServiceProvider = desktop.cast()?;
            let browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser)?;
            let view = browser.QueryActiveShellView()?;
            let dispatch: IDispatch = view.GetItemObject(SVGIO_BACKGROUND)?;
            let folder: IShellFolderViewDual = dispatch.cast()?;
            let shell: IShellDispatch2 = folder.Application()?.cast()?;
            shell.ShellExecute(
                &BSTR::from(exe),
                &VARIANT::from(parameters),
                &VARIANT::default(),
                &VARIANT::from("open"),
                &VARIANT::from(0i32),
            )
        }
    };
    run().map_err(|error| format!("Desktop launch / 桌面启动: {error}"))
}

/// Prefer token-based creation; bound Explorer automation inside a disposable helper.
pub fn unelevated(
    exe: &Path,
    parameters: &str,
) -> Result<Option<std::os::windows::io::OwnedHandle>, String> {
    match crate::config_access::launch(exe, parameters) {
        Ok(process) => Ok(Some(process)),
        Err(token_error) => shell_bounded(exe, parameters)
            .map(|()| None)
            .map_err(|shell_error| format!("{token_error}; {shell_error}")),
    }
}

fn shell_bounded(exe: &Path, parameters: &str) -> Result<(), String> {
    use std::{
        os::windows::{io::AsRawHandle, process::CommandExt},
        process::{Command, Stdio},
        time::Duration,
    };
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    #[cfg(not(test))]
    command.args([
        "--shell-launch",
        exe.to_str().ok_or("Invalid executable path")?,
        parameters,
    ]);
    #[cfg(test)]
    command
        .args(["--exact", "launch::tests::shell_launch_probe", "--ignored"])
        .env("WINSIZEP_SHELL_EXE", exe)
        .env("WINSIZEP_SHELL_PARAMETERS", parameters);
    let mut child = command
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Err(error) = crate::wait_process(child.as_raw_handle(), Duration::from_secs(10)) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Desktop launch / 桌面启动: {status}"))
    }
}

pub fn shell_helper(exe: &Path, parameters: &str) -> Result<(), String> {
    // End before the parent gives up, even if Explorer never returns from COM.
    let (done, wait) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if matches!(
            wait.recv_timeout(std::time::Duration::from_secs(8)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            std::process::exit(1);
        }
    });
    let result = shell(exe, parameters);
    drop(done);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "isolated Explorer automation helper"]
    fn shell_launch_probe() {
        shell_helper(
            Path::new(&std::env::var_os("WINSIZEP_SHELL_EXE").unwrap()),
            &std::env::var("WINSIZEP_SHELL_PARAMETERS").unwrap(),
        )
        .unwrap();
    }

    fn probe_path() -> std::path::PathBuf {
        std::env::current_exe()
            .unwrap()
            .with_file_name("unelevated-launch-probe.txt")
    }
    #[test]
    #[ignore = "helper process for the shell launcher test"]
    fn write_token_probe() {
        let elevated = crate::security::process_elevated(unsafe {
            windows_sys::Win32::System::Threading::GetCurrentProcess()
        })
        .unwrap();
        std::fs::write(probe_path(), format!("{} {elevated}", std::process::id())).unwrap();
    }
    #[test]
    #[ignore = "launches one hidden test process with the ordinary user token"]
    fn shell_launcher_uses_interactive_token() {
        let path = probe_path();
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        unelevated(
            &std::env::current_exe().unwrap(),
            "--exact launch::tests::write_token_probe --ignored --nocapture",
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let result = std::fs::read_to_string(&path).expect("The shell did not start the helper");
        let parts: Vec<_> = result.split_whitespace().collect();
        assert_eq!(parts[1], "false");
        assert_ne!(parts[0], std::process::id().to_string());
        println!("shell child pid={} elevated={}", parts[0], parts[1]);
    }
}

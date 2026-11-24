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

pub fn unelevated(exe: &Path, parameters: &str) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;
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
    #[ignore = "launches one hidden test process through Explorer"]
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

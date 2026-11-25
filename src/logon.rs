use crate::desktop::err;
use windows::{
    Win32::System::{Com::*, TaskScheduler::*, Variant::VARIANT},
    core::BSTR,
};
use windows_sys::Win32::Security::Authentication::Identity::{GetUserNameExW, NameSamCompatible};

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn account() -> Result<String, String> {
    let mut account = [0u16; 512];
    let mut size = account.len() as u32;
    if !unsafe { GetUserNameExW(NameSamCompatible, account.as_mut_ptr(), &mut size) } {
        return Err(err());
    }
    Ok(String::from_utf16_lossy(&account[..size as usize]))
}

fn definition(account: &str, elevated: bool) -> Result<String, String> {
    let account = xml(account);
    let level = if elevated {
        "HighestAvailable"
    } else {
        "LeastPrivilege"
    };
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let exe = xml(exe
        .to_str()
        .ok_or("Invalid executable path / 程序路径无效")?);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><UserId>{account}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Owner"><UserId>{account}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>{level}</RunLevel></Principal></Principals>
  <Settings><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Priority>4</Priority></Settings>
  <Actions Context="Owner"><Exec><Command>{exe}</Command><Arguments>--resident</Arguments></Exec></Actions>
</Task>"#
    ))
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

pub fn sync_direct(enabled: bool, elevated: bool, owner: &str) -> Result<(), windows::core::Error> {
    if enabled && elevated {
        validate_elevated_startup().map_err(|error| {
            windows::core::Error::new(windows::core::HRESULT::from_win32(5), error)
        })?;
    }
    let name = BSTR::from(format!(
        "WinSizeP-{}",
        owner.replace('!', "!!").replace('\\', "!")
    ));
    let task = if enabled {
        Some(definition(owner, elevated).map_err(|message| {
            windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32), message)
        })?)
    } else {
        None
    };
    sync_task(name, task)
}

fn sync_task(name: BSTR, task: Option<String>) -> windows::core::Result<()> {
    let update = || -> windows::core::Result<()> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
            let _apartment = Apartment;
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
            let empty = VARIANT::default();
            service.Connect(&empty, &empty, &empty, &empty)?;
            let folder = service.GetFolder(&BSTR::from("\\"))?;
            if let Some(xml) = task {
                folder.RegisterTask(
                    &name,
                    &BSTR::from(xml),
                    TASK_CREATE_OR_UPDATE.0,
                    &empty,
                    &empty,
                    TASK_LOGON_INTERACTIVE_TOKEN,
                    &empty,
                )?;
            } else if let Err(error) = folder.DeleteTask(&name, 0)
                && error.code() != windows::core::HRESULT::from_win32(2)
            {
                return Err(error);
            }
            Ok(())
        }
    };
    update()
}

fn validate_elevated_startup() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if crate::security::replaceable_without_elevation(&exe)? {
        return Err("Administrator autostart requires a protected program folder. Move WinSizeP to a folder ordinary users cannot modify, or disable administrator autostart. / 管理员自启动要求程序位于普通用户无法修改的受保护目录；请移动程序或关闭管理员自启动。".into());
    }
    Ok(())
}

/// Explain why an elevated startup request cannot be applied.
pub fn elevation_warning(cfg: &crate::config::Config) -> Option<String> {
    if !(cfg.autostart && cfg.elevate) {
        return None;
    }
    validate_elevated_startup().err()
}

/// Run the task update in a child at this process's own level: (exit code, message).
fn update(enabled: bool, elevated: bool, owner: &str) -> Result<(Option<i32>, String), String> {
    use std::{
        io::Read,
        os::windows::{io::AsRawHandle, process::CommandExt},
        process::{Command, Stdio},
    };
    let mut child = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .args([
            "--sync-logon-task",
            if enabled { "1" } else { "0" },
            if elevated { "1" } else { "0" },
            owner,
        ])
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Err(error) =
        crate::wait_process(child.as_raw_handle(), std::time::Duration::from_secs(30))
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let mut error = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut error);
    }
    Ok((status.code(), format!("{} ({status})", error.trim())))
}

pub fn sync(enabled: bool, elevated: bool) -> Result<(), String> {
    if enabled
        && elevated
        && let Err(error) = validate_elevated_startup()
    {
        // An older release may have already registered an unsafe task. Remove it
        // before reporting the rejected request, including its administrator ACL.
        return Err(match sync(false, false) {
            Ok(()) => error,
            Err(cleanup) => format!(
                "{error}\nCould not remove the previous startup task / 无法移除旧自启动任务: {cleanup}"
            ),
        });
    }
    let owner = account()?;
    let failed = |message: String| Err(format!("Autostart / 自动启动: {message}"));
    let helper = |enabled: bool, elevated: bool| {
        crate::elevated_helper(&format!(
            "--sync-logon-task {} {} {}",
            u8::from(enabled),
            u8::from(elevated),
            crate::quote_argument(&owner)
        ))
    };
    match update(enabled, elevated, &owner)? {
        (Some(0), _) => Ok(()),
        (Some(5), _) if !crate::security::elevated() && elevated => helper(enabled, true),
        (Some(5), _) if !crate::security::elevated() => {
            // An administrator registered the existing task, and updating it would keep
            // its protected ACL: every later normal start would ask for consent again.
            // Ask once to delete it, then register the replacement without elevation.
            helper(false, false)?;
            if !enabled {
                return Ok(());
            }
            match update(true, false, &owner)? {
                (Some(0), _) => Ok(()),
                (_, message) => failed(message),
            }
        }
        (_, message) => failed(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writable_programs_are_rejected_before_task_registration() {
        let error = sync_direct(true, true, "WinSizeP.Audit.UnsafePath").unwrap_err();
        assert!(
            error.message().contains("protected program folder"),
            "{error}"
        );
    }
    fn temporary_name() -> BSTR {
        BSTR::from(format!(
            "WinSizeP.Test.DeElevation.{}",
            std::env::current_exe()
                .unwrap()
                .file_stem()
                .unwrap()
                .to_string_lossy()
        ))
    }
    fn probe_path(operation: &str) -> std::path::PathBuf {
        std::env::current_exe()
            .unwrap()
            .with_file_name(format!("logon-{operation}-probe.txt"))
    }
    fn temporary_definition(elevated: bool) -> String {
        let owner = account().unwrap();
        definition(&owner, elevated)
            .unwrap()
            .replace(
                &format!(
                    "<Triggers><LogonTrigger><UserId>{}</UserId></LogonTrigger></Triggers>",
                    xml(&owner)
                ),
                "<Triggers />",
            )
            .replace("<Settings>", "<Settings><Enabled>false</Enabled>")
    }
    fn wait_probe(operation: &str) -> String {
        let path = probe_path(operation);
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        crate::launch::unelevated(
            &std::env::current_exe().unwrap(),
            &format!("--exact logon::tests::normal_{operation}_probe --ignored --nocapture"),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::fs::read_to_string(path).expect("Normal task probe did not finish")
    }
    #[test]
    #[ignore = "normal helper for the temporary task permission test"]
    fn normal_deletion_probe() {
        assert!(
            !crate::security::process_elevated(unsafe {
                windows_sys::Win32::System::Threading::GetCurrentProcess()
            })
            .unwrap()
        );
        assert!(!crate::security::elevated() && !crate::security::can_lower());
        let result = sync_task(temporary_name(), None);
        std::fs::write(probe_path("deletion"), format!("{result:?}")).unwrap();
    }
    #[test]
    #[ignore = "normal helper creates and deletes only a disabled WinSizeP.Test task"]
    fn normal_creation_probe() {
        assert!(
            !crate::security::process_elevated(unsafe {
                windows_sys::Win32::System::Threading::GetCurrentProcess()
            })
            .unwrap()
        );
        let result = sync_task(temporary_name(), Some(temporary_definition(false)))
            .and_then(|_| sync_task(temporary_name(), None));
        std::fs::write(probe_path("creation"), format!("{result:?}")).unwrap();
    }
    #[test]
    #[ignore = "requires administrator rights; creates and removes one disabled WinSizeP.Test task"]
    fn elevated_task_is_removed_before_normal_recreation() {
        assert!(
            crate::security::process_elevated(unsafe {
                windows_sys::Win32::System::Threading::GetCurrentProcess()
            })
            .unwrap(),
            "Run this test from an elevated process"
        );
        assert!(crate::security::elevated());
        println!("split token (can lower): {}", crate::security::can_lower());
        // The normal half of the token decides what counts as replaceable.
        let system = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("kernel32.dll");
        assert!(!crate::security::replaceable_without_elevation(&system).unwrap());
        assert!(
            crate::security::replaceable_without_elevation(&std::env::current_exe().unwrap())
                .unwrap()
        );
        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = sync_task(temporary_name(), None);
            }
        }
        let _cleanup = Cleanup;
        sync_task(temporary_name(), Some(temporary_definition(true))).unwrap();
        let deletion = wait_probe("deletion");
        println!("normal deletion of administrator-created task: {deletion}");
        assert!(
            deletion.contains("80070005"),
            "Expected AccessDenied, got {deletion}"
        );
        sync_task(temporary_name(), None).unwrap();
        let creation = wait_probe("creation");
        println!("normal recreation and cleanup after administrator deletion: {creation}");
        assert_eq!(creation, "Ok(())");
    }
    #[test]
    fn task_definition_changes_run_level_and_preserves_owner() {
        for (elevated, level) in [(false, "LeastPrivilege"), (true, "HighestAvailable")] {
            let xml = super::definition("DOMAIN\\User & Name", elevated).unwrap();
            assert!(xml.contains(&format!("<RunLevel>{level}</RunLevel>")));
            assert!(xml.contains("DOMAIN\\User &amp; Name"));
            assert!(xml.contains("<Arguments>--resident</Arguments>"));
        }
    }
}

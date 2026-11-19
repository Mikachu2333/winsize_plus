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
  <Settings><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
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

pub fn sync(enabled: bool, elevated: bool) -> Result<(), String> {
    let owner = account()?;
    match sync_direct(enabled, elevated, &owner) {
        Ok(()) => Ok(()),
        Err(error)
            if error.code() == windows::core::HRESULT::from_win32(5)
                && unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() } == 0 =>
        {
            crate::elevated_helper(&format!(
                "--sync-logon-task {} {} {}",
                u8::from(enabled),
                u8::from(elevated),
                crate::quote_argument(&owner)
            ))
        }
        Err(error) => Err(format!("Autostart / 自动启动: {error}")),
    }
}

#[cfg(test)]
mod tests {
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

use crate::{config::encode, desktop::err};
use std::{fs::OpenOptions, io::Write, path::Path};
use windows_sys::Win32::Security::Authentication::Identity::{GetUserNameExW, NameSamCompatible};

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn export(path: &Path) -> Result<(), String> {
    let mut account = [0u16; 512];
    let mut size = account.len() as u32;
    if !unsafe { GetUserNameExW(NameSamCompatible, account.as_mut_ptr(), &mut size) } {
        return Err(err());
    }
    let account = xml(&String::from_utf16_lossy(&account[..size as usize]));
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let exe = xml(exe
        .to_str()
        .ok_or("Invalid executable path / 程序路径无效")?);
    let task = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><UserId>{account}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Owner"><UserId>{account}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
  <Actions Context="Owner"><Exec><Command>{exe}</Command></Exec></Actions>
</Task>"#
    );
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| file.write_all(&encode(&task)))
        .map_err(|error| error.to_string())
}

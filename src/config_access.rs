//! Configuration operations always use the interactive user's ordinary token.
use std::{
    io,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{SystemInformation::GetSystemDirectoryW, Threading::*},
    UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId},
};

fn token(process: HANDLE) -> io::Result<OwnedHandle> {
    let mut handle = null_mut();
    // SAFETY: the returned token is owned exactly once; process remains live for this call.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &mut handle) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn ordinary(token: HANDLE) -> io::Result<bool> {
    let mut elevation = TOKEN_ELEVATION::default();
    let mut length = 0;
    // SAFETY: both outputs are initialized, correctly sized and aligned.
    if unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if elevation.TokenIsElevated != 0 {
        return Ok(false);
    }
    let mut size = 0;
    unsafe { GetTokenInformation(token, TokenIntegrityLevel, null_mut(), 0, &mut size) };
    if size < size_of::<TOKEN_MANDATORY_LABEL>() as u32 {
        return Err(io::Error::last_os_error());
    }
    let mut storage = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    // SAFETY: pointer-aligned storage owns the complete TOKEN_MANDATORY_LABEL and SID.
    unsafe {
        if GetTokenInformation(
            token,
            TokenIntegrityLevel,
            storage.as_mut_ptr().cast(),
            size,
            &mut size,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sid = (*(storage.as_ptr() as *const TOKEN_MANDATORY_LABEL))
            .Label
            .Sid;
        if IsValidSid(sid) == 0 || *GetSidSubAuthorityCount(sid) == 0 {
            return Err(io::Error::other("Invalid integrity SID"));
        }
        Ok(*GetSidSubAuthority(sid, u32::from(*GetSidSubAuthorityCount(sid) - 1)) <= 0x2000)
    }
}

fn normal_token() -> io::Result<OwnedHandle> {
    let current = token(unsafe { GetCurrentProcess() })?;
    if ordinary(current.as_raw_handle())? {
        return Ok(current);
    }
    // The desktop owner can differ from the administrator who supplied UAC credentials.
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(GetShellWindow(), &mut pid) };
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if !process.is_null() {
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        if let Ok(shell) = token(process.as_raw_handle())
            && ordinary(shell.as_raw_handle())?
        {
            return Ok(shell);
        }
    }
    let mut linked = TOKEN_LINKED_TOKEN::default();
    let mut length = 0;
    // SAFETY: GetTokenInformation returns a separately owned linked-token handle.
    if unsafe {
        GetTokenInformation(
            current.as_raw_handle(),
            TokenLinkedToken,
            (&mut linked as *mut TOKEN_LINKED_TOKEN).cast(),
            size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut length,
        )
    } != 0
    {
        let linked = unsafe { OwnedHandle::from_raw_handle(linked.LinkedToken) };
        if ordinary(linked.as_raw_handle())? {
            return Ok(linked);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "No ordinary user token is available for the configuration / 无法取得配置操作所需的普通用户令牌",
    ))
}

fn duplicate(token: HANDLE, kind: TOKEN_TYPE) -> io::Result<OwnedHandle> {
    let rights = TOKEN_QUERY
        | TOKEN_DUPLICATE
        | if kind == TokenPrimary {
            TOKEN_ASSIGN_PRIMARY
        } else {
            TOKEN_IMPERSONATE
        };
    let mut copy = null_mut();
    // SAFETY: token is live; the duplicated handle has a single OwnedHandle owner.
    if unsafe {
        DuplicateTokenEx(
            token,
            rights,
            null(),
            SecurityImpersonation,
            kind,
            &mut copy,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(copy) })
}

/// No GUI callbacks run on this short-lived worker. Its impersonation token expires
/// with the thread, including on errors and unwinding; it is never reused for other work.
pub fn run<T: Send>(action: impl FnOnce() -> T + Send) -> Result<T, String> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("config-user".into())
            .spawn_scoped(scope, || {
                let token = normal_token().map_err(|e| e.to_string())?;
                let impersonation = duplicate(token.as_raw_handle(), TokenImpersonation)
                    .map_err(|e| e.to_string())?;
                // SAFETY: this worker owns the impersonation token and runs no message loop.
                if unsafe { SetThreadToken(null(), impersonation.as_raw_handle()) } == 0 {
                    return Err(io::Error::last_os_error().to_string());
                }
                Ok(action())
            })
            .map_err(|e| e.to_string())?
            .join()
            .map_err(|_| "Configuration worker failed / 配置操作线程失败".to_owned())?
    })
}

pub fn read(path: &Path) -> Result<Vec<u8>, String> {
    run(|| std::fs::read(path))?.map_err(|e| error(path, &e.to_string()))
}

pub fn error(path: &Path, reason: &str) -> String {
    format!(
        "Configuration access with normal user permissions failed / 普通用户权限无法访问配置文件 {}: {reason}",
        path.display()
    )
}

pub fn watch(path: &Path) -> Result<usize, String> {
    run(|| {
        use windows_sys::Win32::Storage::FileSystem::*;
        // SAFETY: the name lives through the call; the caller owns a successful handle.
        let handle = unsafe {
            FindFirstChangeNotificationW(
                crate::desktop::wide(path).as_ptr(),
                0,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_SIZE,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(error(path, &io::Error::last_os_error().to_string()))
        } else {
            Ok(handle as usize)
        }
    })?
}

fn editor() -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length])).join("notepad.exe"))
}

pub fn open(path: &Path) -> Result<(), String> {
    // Verify access before launching an editor; never delegate an elevated file read.
    read(path)?;
    let exe = editor().map_err(|e| e.to_string())?;
    let parameters = crate::quote_argument(
        path.to_str()
            .ok_or("Invalid configuration path / 配置路径无效")?,
    );
    crate::launch::unelevated(&exe, &parameters)
        .map(|_| ())
        .map_err(|e| error(path, &e))
}

/// Start a process with the same ordinary identity used for configuration access.
pub fn launch(exe: &Path, parameters: &str) -> Result<OwnedHandle, String> {
    use std::os::windows::process::CommandExt;
    let user_token = normal_token().map_err(|e| e.to_string())?;
    if ordinary(
        token(unsafe { GetCurrentProcess() })
            .map_err(|e| e.to_string())?
            .as_raw_handle(),
    )
    .map_err(|e| e.to_string())?
    {
        return std::process::Command::new(exe)
            .raw_arg(parameters)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(OwnedHandle::from)
            .map_err(|e| e.to_string());
    }
    let primary = duplicate(user_token.as_raw_handle(), TokenPrimary)
        .map_err(|e| format!("Duplicate ordinary primary token: {e}"))?;
    let exe_text = exe.to_str().ok_or("Invalid editor path")?;
    let mut command =
        crate::desktop::wide(format!("{} {parameters}", crate::quote_argument(exe_text),));
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut child = PROCESS_INFORMATION::default();
    // SAFETY: the primary token is verified ordinary; all strings live through creation.
    if unsafe {
        CreateProcessWithTokenW(
            primary.as_raw_handle(),
            0,
            crate::desktop::wide(exe).as_ptr(),
            command.as_mut_ptr(),
            CREATE_NO_WINDOW,
            null(),
            null(),
            &startup,
            &mut child,
        )
    } == 0
    {
        return Err(format!(
            "CreateProcessWithTokenW: {}",
            io::Error::last_os_error()
        ));
    }
    unsafe {
        CloseHandle(child.hThread);
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(child.hProcess) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_worker_has_an_ordinary_token_and_does_not_change_the_caller() {
        let before = crate::security::elevated();
        let worker = run(|| unsafe {
            let mut token = null_mut();
            assert_ne!(
                OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token),
                0
            );
            let token = OwnedHandle::from_raw_handle(token);
            ordinary(token.as_raw_handle()).unwrap()
        })
        .unwrap();
        assert!(worker);
        assert_eq!(crate::security::elevated(), before);
        assert_eq!(
            run(|| Err::<(), _>("expected failure")).unwrap(),
            Err("expected failure")
        );
        assert!(run(|| 42).is_ok());
    }

    #[test]
    fn editor_path_is_absolute_and_cannot_resolve_beside_the_application() {
        let editor = editor().unwrap();
        assert!(editor.is_absolute());
        assert_eq!(editor.file_name().unwrap(), "notepad.exe");
        assert_ne!(editor.parent(), std::env::current_exe().unwrap().parent());
    }

    #[test]
    fn config_permission_errors_include_the_file_and_leave_pending_contents_intact() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::current_dir()
            .unwrap()
            .join("target/config-access-test");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("locked.toml");
        std::fs::write(&path, crate::config::help()).unwrap();
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let error = read(&path).unwrap_err();
        assert!(error.contains("普通用户权限"), "{error}");
        assert!(error.contains("locked.toml"), "{error}");
        drop(locked);
        assert_eq!(read(&path).unwrap(), crate::config::help().as_bytes());
    }

    #[test]
    #[ignore = "requires elevation; creates and removes only a protected temporary configuration"]
    fn elevated_configuration_access_still_obeys_normal_user_permissions() {
        use windows_sys::{Win32::Storage::FileSystem::*, core::w};
        assert!(crate::security::elevated(), "Run this probe elevated");
        configuration_worker_has_an_ordinary_token_and_does_not_change_the_caller();
        let writable = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("normal-config-probe-{}.toml", std::process::id()));
        let loaded = crate::config::load_snapshot(&writable, None).unwrap();
        let mut draft = loaded.document;
        draft["general"]["preview"] = toml_edit::value(!loaded.config.preview);
        crate::config::save_settings(&writable, &loaded.text, &draft).unwrap();
        assert_eq!(
            crate::config::load(&writable, None).unwrap().preview,
            !loaded.config.preview
        );
        std::fs::remove_file(writable).unwrap();
        let folder = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("config-permission-probe-{}", std::process::id()));
        let mut descriptor = null_mut();
        unsafe {
            assert_ne!(windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
                windows_sys::Win32::Security::Authorization::SDDL_REVISION_1,
                &mut descriptor, null_mut(),
            ), 0);
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let created = CreateDirectoryW(crate::desktop::wide(&folder).as_ptr(), &attributes);
            let error = io::Error::last_os_error();
            LocalFree(descriptor);
            assert_ne!(created, 0, "{error}");
        }
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(folder.clone());
        let path = folder.join("config.toml");
        let original = crate::config::help();
        std::fs::write(&path, original).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
        assert!(read(&path).is_err());
        assert!(crate::config::read_settings(&path).is_err());
        assert!(crate::config::requested_elevation(&path).is_err());
        assert!(crate::config::load_snapshot(&path, None).is_err());
        let mut draft = original.parse::<toml_edit::DocumentMut>().unwrap();
        draft["general"]["preview"] = toml_edit::value(false);
        assert!(crate::config::save_settings(&path, original, &draft).is_err());
        assert!(open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
        let new_path = folder.join("new.toml");
        assert!(crate::config::load_snapshot(&new_path, None).is_err());
        assert!(!new_path.exists());
        assert!(crate::security::elevated());
    }
}

use crate::desktop::{err, wide};
use std::ptr::null_mut;
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    System::Threading::*,
};

struct LocalMemory(HLOCAL);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn integrity(process: HANDLE) -> Result<u32, String> {
    // SAFETY: all token buffers are pointer-aligned and live through the synchronous calls.
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(err());
        }
        let mut size = 0;
        GetTokenInformation(token, TokenIntegrityLevel, null_mut(), 0, &mut size);
        if size < size_of::<TOKEN_MANDATORY_LABEL>() as u32 {
            CloseHandle(token);
            return Err(err());
        }
        let mut storage = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        let ok = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            storage.as_mut_ptr().cast(),
            size,
            &mut size,
        );
        let error = err();
        CloseHandle(token);
        if ok == 0 {
            return Err(error);
        }
        let label = &*(storage.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        if IsValidSid(label.Label.Sid) == 0 {
            return Err("Invalid integrity SID".into());
        }
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        if count == 0 {
            return Err("Empty integrity SID".into());
        }
        Ok(*GetSidSubAuthority(label.Label.Sid, u32::from(count - 1)))
    }
}
pub fn process_elevated(process: HANDLE) -> Result<bool, String> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(err());
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0;
        let result = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        let error = err();
        CloseHandle(token);
        if result == 0 {
            return Err(error);
        }
        Ok(elevation.TokenIsElevated != 0)
    }
}
static LOWERING_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this process runs with an elevated token. Membership of the Administrators
/// group says nothing about that, so the token is asked directly.
pub fn elevated() -> bool {
    process_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false)
}

/// Whether a normal instance can be started in place of this elevated one. Only a
/// split token has a normal half: with UAC disabled or the built-in Administrator
/// account every process is elevated, and asking to "lower" could never succeed.
pub fn can_lower() -> bool {
    if LOWERING_FAILED.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut kind = 0;
        let mut size = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevationType,
            (&mut kind as *mut TOKEN_ELEVATION_TYPE).cast(),
            size_of::<TOKEN_ELEVATION_TYPE>() as u32,
            &mut size,
        );
        CloseHandle(token);
        ok != 0 && kind == TokenElevationTypeFull
    }
}

/// A desktop shell that is itself elevated offers no normal token either. Remember
/// the failed attempt so every later configuration change does not repeat it.
pub fn lowering_failed() {
    LOWERING_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
}

static RAISE_DECLINED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Record whether the last consent prompt was refused, so that automatic reloads do
/// not ask again after every edit. An explicit reload or restart still retries.
pub fn raise_declined(declined: bool) {
    RAISE_DECLINED.store(declined, std::sync::atomic::Ordering::Relaxed);
}
pub fn raise_was_declined() -> bool {
    RAISE_DECLINED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether a process without administrator rights could replace `exe` or plant files
/// beside it. Such a program must not be started elevated without a prompt.
pub fn replaceable_without_elevation(exe: &std::path::Path) -> Result<bool, String> {
    crate::config_access::run(|| replaceable_as_current_user(exe))?
}
fn replaceable_as_current_user(exe: &std::path::Path) -> Result<bool, String> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::*;
    // Reparse points can redirect a protected-looking path to another directory.
    // Elevated startup requires a fixed path whose complete ancestry can be checked.
    for path in exe.ancestors() {
        if std::fs::symlink_metadata(path)
            .map_err(|e| e.to_string())?
            .file_attributes()
            & FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Ok(true);
        }
    }
    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    // SAFETY: every buffer outlives its synchronous call; handles and the descriptor
    // are released by the guards above.
    unsafe {
        let mut impersonation = null_mut();
        if OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut impersonation) == 0 {
            return Err(err());
        }
        let token = Token(impersonation);
        let granted = |path: &std::path::Path| -> Result<u32, String> {
            let mut descriptor = null_mut();
            let status = GetNamedSecurityInfoW(
                wide(path).as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            );
            if status != 0 {
                return Err(format!("{}: {status}", path.display()));
            }
            let _descriptor = LocalMemory(descriptor);
            let mapping = GENERIC_MAPPING {
                GenericRead: FILE_GENERIC_READ,
                GenericWrite: FILE_GENERIC_WRITE,
                GenericExecute: FILE_GENERIC_EXECUTE,
                GenericAll: FILE_ALL_ACCESS,
            };
            let mut privileges = [0u32; 16];
            let mut length = size_of_val(&privileges) as u32;
            let mut access = 0;
            let mut allowed = 0;
            const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
            if AccessCheck(
                descriptor,
                token.0,
                MAXIMUM_ALLOWED,
                &mapping,
                privileges.as_mut_ptr().cast(),
                &mut length,
                &mut access,
                &mut allowed,
            ) == 0
            {
                return Err(err());
            }
            Ok(if allowed != 0 { access } else { 0 })
        };
        let control = WRITE_DAC | WRITE_OWNER;
        let folder = exe.parent().ok_or("Executable folder")?;
        if granted(exe)? & (FILE_WRITE_DATA | FILE_APPEND_DATA | DELETE | control) != 0
            || granted(folder)? & (FILE_ADD_FILE | FILE_DELETE_CHILD | control) != 0
        {
            return Ok(true);
        }
        // A protected immediate directory can still be renamed through a writable ancestor.
        for ancestor in folder.ancestors() {
            if granted(ancestor)? & (DELETE | FILE_DELETE_CHILD | control) != 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
pub fn can_control_window(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    thread_local! {static CURRENT:std::cell::Cell<Option<u32>>=const {std::cell::Cell::new(None)};}
    let current = CURRENT.with(|cached| {
        let level = cached
            .get()
            .or_else(|| integrity(unsafe { GetCurrentProcess() }).ok());
        cached.set(level);
        level
    });
    let Some(current) = current else {
        return false;
    };
    unsafe {
        let mut pid = 0;
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, &mut pid);
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let target = integrity(process);
        CloseHandle(process);
        target.is_ok_and(|target| target <= current)
    }
}

fn user_sid() -> Result<String, String> {
    unsafe {
        let mut token = null_mut();
        if OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) == 0
            && (GetLastError() != ERROR_NO_TOKEN
                || OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0)
        {
            return Err(err());
        }
        let mut length = 0;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length);
        if length == 0 {
            let error = err();
            CloseHandle(token);
            return Err(error);
        }
        // TOKEN_USER contains a pointer and requires pointer-aligned storage.
        let mut storage = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            storage.as_mut_ptr().cast(),
            length,
            &mut length,
        );
        let error = err();
        CloseHandle(token);
        if ok == 0 {
            return Err(error);
        }
        let user = &*(storage.as_ptr() as *const TOKEN_USER);
        let mut text = null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(err());
        }
        let _text = LocalMemory(text.cast());
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, length,
        )))
    }
}

/// Allow the same user's normal and elevated processes to coordinate without allowing
/// another user's process to modify the mutex. The medium label avoids no-write-up denial.
pub fn config_mutex() -> Result<HANDLE, String> {
    let sid = user_sid()?;
    user_object(&sid, |attributes| unsafe {
        let name = wide(format!("Local\\WinSizeP.Config.v2.{sid}"));
        CreateMutexExW(
            attributes,
            name.as_ptr(),
            0,
            SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE,
        )
    })
}

pub fn handoff_event(name: &str) -> Result<usize, String> {
    user_object(&user_sid()?, |attributes| unsafe {
        CreateEventExW(
            attributes,
            wide(name).as_ptr(),
            CREATE_EVENT_MANUAL_RESET,
            SYNCHRONIZATION_SYNCHRONIZE | EVENT_MODIFY_STATE,
        )
    })
    .map(|handle| handle as usize)
}

fn user_object(
    sid: &str,
    create: impl FnOnce(&SECURITY_ATTRIBUTES) -> HANDLE,
) -> Result<HANDLE, String> {
    let sddl = wide(format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})S:(ML;;NW;;;ME)"));
    unsafe {
        let mut descriptor = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        ) == 0
        {
            return Err(err());
        }
        let _descriptor = LocalMemory(descriptor);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let handle = create(&attributes);
        if handle.is_null() {
            Err(err())
        } else {
            Ok(handle)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_folders_writable_without_elevation_are_reported() {
        // Cargo's target folder belongs to the user; System32 does not.
        assert!(replaceable_without_elevation(&std::env::current_exe().unwrap()).unwrap());
        let system = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("kernel32.dll");
        assert!(!replaceable_without_elevation(&system).unwrap());
    }
    #[test]
    fn configuration_mutex_can_be_reopened_with_only_required_rights() {
        let first = config_mutex().unwrap();
        let second = config_mutex().unwrap();
        unsafe {
            assert_eq!(WaitForSingleObject(first, 1000), WAIT_OBJECT_0);
            assert_ne!(ReleaseMutex(first), 0);
            assert_eq!(WaitForSingleObject(second, 1000), WAIT_OBJECT_0);
            assert_ne!(ReleaseMutex(second), 0);
            CloseHandle(second);
            CloseHandle(first);
        }
    }
}

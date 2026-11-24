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
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
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
        let name = wide(format!("Local\\WinSizeP.Config.v2.{sid}"));
        let mutex = CreateMutexExW(
            &attributes,
            name.as_ptr(),
            0,
            SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE,
        );
        if mutex.is_null() {
            Err(err())
        } else {
            Ok(mutex)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

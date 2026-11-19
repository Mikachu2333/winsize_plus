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

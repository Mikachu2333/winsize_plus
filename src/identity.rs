//! Named objects used to discover or replace an instance are build-specific.
use windows_sys::core::w;
pub const RELEASE_GUID: &str = "D5C9A59069D907909B153C76CEBDC023";
pub const DEBUG_GUID: &str = "68C7D42E9A5B4E038154D820A16CE7BF";
pub const GUID: &str = if cfg!(debug_assertions) {
    DEBUG_GUID
} else {
    RELEASE_GUID
};
pub fn mutex(settings: bool) -> Vec<u16> {
    crate::desktop::wide(format!(
        "Local\\{GUID}{}",
        if settings { ".Settings" } else { "" }
    ))
}
pub const SETTINGS_CLASS: *const u16 = if cfg!(debug_assertions) {
    w!("WinSizeP.Settings.Debug")
} else {
    w!("WinSizeP.Settings")
};
pub const NAME: &str = if cfg!(debug_assertions) {
    "WinSizeP Debug"
} else {
    "WinSizeP"
};
pub const VERSION_TEXT: &str = if cfg!(debug_assertions) {
    "WinSizeP DEBUG"
} else {
    concat!("WinSizeP ", env!("CARGO_PKG_VERSION"))
};
pub fn settings_class(name: &str) -> bool {
    matches!(name, "WinSizeP.Settings" | "WinSizeP.Settings.Debug")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn build_identities_are_distinct_and_release_stays_compatible() {
        assert_ne!(DEBUG_GUID, RELEASE_GUID);
        assert_eq!(RELEASE_GUID, "D5C9A59069D907909B153C76CEBDC023");
        assert!(settings_class("WinSizeP.Settings"));
        assert!(settings_class("WinSizeP.Settings.Debug"));
        assert_ne!(mutex(false), mutex(true));
    }
    #[test]
    fn different_build_mutexes_coexist_but_same_build_is_single_instance() {
        use windows_sys::Win32::{Foundation::*, System::Threading::*};
        let names = [RELEASE_GUID, DEBUG_GUID].map(|id| {
            crate::desktop::wide(format!(
                "Local\\WinSizeP.IdentityTest.{}.{id}",
                std::process::id()
            ))
        });
        unsafe {
            let a = CreateMutexW(std::ptr::null(), 0, names[0].as_ptr());
            assert!(!a.is_null());
            assert_ne!(GetLastError(), ERROR_ALREADY_EXISTS);
            let b = CreateMutexW(std::ptr::null(), 0, names[1].as_ptr());
            assert!(!b.is_null());
            assert_ne!(GetLastError(), ERROR_ALREADY_EXISTS);
            let again = CreateMutexW(std::ptr::null(), 0, names[0].as_ptr());
            assert!(!again.is_null());
            assert_eq!(GetLastError(), ERROR_ALREADY_EXISTS);
            CloseHandle(again);
            CloseHandle(b);
            CloseHandle(a);
        }
    }
}

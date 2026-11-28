use crate::desktop::{err, wide};
use std::{
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        UI::{Controls::*, WindowsAndMessaging::*},
    },
    core::w,
};

pub struct HoverTip {
    hwnd: HWND,
    owner: HWND,
    text: Vec<u16>,
    pending: Option<(String, POINT, Instant)>,
    shown: bool,
}
impl HoverTip {
    pub fn new(owner: HWND) -> Result<Self, String> {
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("tooltips_class32"),
                null(),
                WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX,
                0,
                0,
                0,
                0,
                owner,
                null_mut(),
                null_mut(),
                null(),
            );
            if hwnd.is_null() {
                return Err(err());
            }
            let mut result = Self {
                hwnd,
                owner,
                text: vec![0],
                pending: None,
                shown: false,
            };
            let info = result.info();
            SendMessageW(
                hwnd,
                TTM_ADDTOOLW,
                0,
                (&info as *const TTTOOLINFOW) as isize,
            );
            SendMessageW(hwnd, TTM_SETMAXTIPWIDTH, 0, 600);
            Ok(result)
        }
    }
    fn info(&mut self) -> TTTOOLINFOW {
        TTTOOLINFOW {
            cbSize: size_of::<TTTOOLINFOW>() as u32,
            uFlags: TTF_TRACK | TTF_ABSOLUTE,
            hwnd: self.owner,
            uId: 1,
            lpszText: self.text.as_mut_ptr(),
            ..Default::default()
        }
    }
    pub fn hover(&mut self, text: Option<String>, point: POINT) {
        let Some(text) = text else {
            self.hide();
            return;
        };
        if self
            .pending
            .as_ref()
            .is_some_and(|(old, _, _)| old == &text)
        {
            return;
        }
        self.hide();
        self.pending = Some((text, point, Instant::now() + Duration::from_millis(400)));
    }
    pub fn tick(&mut self) {
        if self.shown {
            return;
        }
        let Some((text, point, when)) = &self.pending else {
            return;
        };
        if Instant::now() < *when {
            return;
        }
        self.text = wide(text);
        let position = ((point.x + 12) as u16 as u32) | (((point.y + 22) as u16 as u32) << 16);
        let info = self.info();
        unsafe {
            SendMessageW(
                self.hwnd,
                TTM_UPDATETIPTEXTW,
                0,
                (&info as *const TTTOOLINFOW) as isize,
            );
            SendMessageW(self.hwnd, TTM_TRACKPOSITION, 0, position as isize);
            SendMessageW(
                self.hwnd,
                TTM_TRACKACTIVATE,
                1,
                (&info as *const TTTOOLINFOW) as isize,
            );
        }
        self.shown = true;
    }
    pub fn hide(&mut self) {
        if self.shown {
            let info = self.info();
            unsafe {
                SendMessageW(
                    self.hwnd,
                    TTM_TRACKACTIVATE,
                    0,
                    (&info as *const TTTOOLINFOW) as isize,
                );
            }
        }
        self.shown = false;
        self.pending = None;
    }
}
impl Drop for HoverTip {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.hwnd);
        }
    }
}

//! Keep only the latest group geometry while foreign window procedures resize.
use crate::{desktop, layout::Rect};
use std::sync::{Arc, Condvar, Mutex};
use windows_sys::Win32::{
    Foundation::*,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

#[derive(Default)]
struct State {
    latest: Option<Vec<(usize, Rect)>>,
    closing: bool,
    error: Option<String>,
}
pub(super) struct Updates(Arc<(Mutex<State>, Condvar)>);
impl Updates {
    pub fn new() -> Result<Self, String> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("winsizep-linked-resize".into())
            .spawn(move || {
                // SAFETY: only copied HWND values/rectangles cross threads. All window
                // calls validate handles; no UI-owned objects or pointers are retained.
                unsafe {
                    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                }
                loop {
                    let updates = {
                        let (lock, wake) = &*worker;
                        let mut state = lock.lock().unwrap();
                        while state.latest.is_none() && !state.closing {
                            state = wake.wait(state).unwrap();
                        }
                        let Some(updates) = state.latest.take() else {
                            return;
                        };
                        updates
                    };
                    // A synchronous call on this worker provides backpressure. While
                    // it waits, UI requests replace latest instead of entering USER's
                    // asynchronous window-position queue one by one.
                    for (handle, rect) in updates {
                        let hwnd = handle as HWND;
                        let result = unsafe {
                            if IsWindow(hwnd) == 0 || IsHungAppWindow(hwnd) != 0 {
                                Err("Linked window unavailable / 联动窗口不可用".into())
                            } else {
                                desktop::check(SetWindowPos(
                                    hwnd,
                                    std::ptr::null_mut(),
                                    rect.x,
                                    rect.y,
                                    rect.width,
                                    rect.height,
                                    SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
                                ))
                            }
                        };
                        if let Err(error) = result {
                            let mut state = worker.0.lock().unwrap();
                            state.error = Some(error);
                            state.latest = None;
                            return;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self(shared))
    }
    pub fn submit(&self, positions: Vec<(usize, Rect)>) -> Result<(), String> {
        let mut state = self.0.0.lock().map_err(|e| e.to_string())?;
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        state.latest = Some(positions);
        self.0.1.notify_one();
        Ok(())
    }
}
impl Drop for Updates {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.0.lock() {
            state.closing = true;
            self.0.1.notify_one();
        }
        // The worker owns shared state until the final rectangle has been applied.
        // Closing a drag must never wait on a foreign application's window procedure.
    }
}

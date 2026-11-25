use crate::desktop::err;
use std::{
    ptr::null,
    time::{Duration, Instant},
};
use windows_sys::Win32::{Foundation::*, System::Threading::*};

pub struct FrameClock {
    timer: HANDLE,
    period: Duration,
    next: Option<Instant>,
    pending: Option<POINT>,
}

impl FrameClock {
    pub fn new(fps: f64) -> Result<Self, String> {
        let create = |flags| unsafe {
            CreateWaitableTimerExW(
                null(),
                null(),
                flags,
                TIMER_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
            )
        };
        let mut timer = create(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION);
        if timer.is_null() {
            timer = create(0);
        }
        if timer.is_null() {
            return Err(err());
        }
        Ok(Self {
            timer,
            period: Duration::from_secs_f64(1.0 / fps),
            next: None,
            pending: None,
        })
    }

    pub fn handle(&self) -> HANDLE {
        self.timer
    }

    pub fn reset(&mut self, fps: f64) {
        unsafe {
            CancelWaitableTimer(self.timer);
        }
        self.period = Duration::from_secs_f64(1.0 / fps);
        self.next = None;
        self.pending = None;
    }

    pub fn moved(&mut self, point: POINT) -> Result<Option<POINT>, String> {
        let waiting = self.pending.replace(point).is_some();
        if waiting {
            return Ok(None);
        }
        if let Some(delay) = self
            .next
            .and_then(|when| when.checked_duration_since(Instant::now()))
        {
            let ticks = -(delay.as_nanos().div_ceil(100).max(1) as i64);
            if unsafe { SetWaitableTimer(self.timer, &ticks, 0, None, null(), 0) } == 0 {
                // No timer will fire: leaving the point pending would drop every later move.
                self.pending = None;
                return Err(err());
            }
            Ok(None)
        } else {
            Ok(self.tick())
        }
    }

    pub fn tick(&mut self) -> Option<POINT> {
        let point = self.pending.take()?;
        let now = Instant::now();
        self.next = Some(
            self.next
                .map(|when| when + self.period)
                .filter(|next| *next > now)
                .unwrap_or(now + self.period),
        );
        Some(point)
    }
}

impl Drop for FrameClock {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.timer);
        }
    }
}

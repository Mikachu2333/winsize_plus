//! One bounded update lane per foreign UI thread; newer sessions supersede older work.
use crate::{desktop, layout::Rect};
use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

#[derive(Clone)]
struct Job {
    handle: usize,
    rect: Rect,
    generation: u64,
}
#[derive(Default)]
struct State {
    latest: HashMap<usize, Job>,
    owners: HashMap<usize, u64>,
    active: Option<(usize, u64, Instant)>,
    error: Option<(u64, String)>,
    retired: bool,
}
struct Lane {
    state: Mutex<State>,
    wake: Condvar,
}
type Key = (u32, u32);
static LANES: OnceLock<Mutex<HashMap<Key, Weak<Lane>>>> = OnceLock::new();
static GENERATION: AtomicU64 = AtomicU64::new(1);
const MAX_LANES: usize = 64;
const WAIT_LIMIT: Duration = Duration::from_secs(2);

fn lane(key: Key) -> Result<Arc<Lane>, String> {
    let mut lanes = LANES
        .get_or_init(Default::default)
        .lock()
        .map_err(|e| e.to_string())?;
    lanes.retain(|_, lane| lane.strong_count() != 0);
    if let Some(lane) = lanes.get(&key).and_then(Weak::upgrade)
        && !lane.state.lock().unwrap().retired
    {
        return Ok(lane);
    }
    if lanes.len() >= MAX_LANES {
        return Err("Too many unresponsive window threads / 无响应窗口线程过多".into());
    }
    let lane = Arc::new(Lane {
        state: Mutex::new(State::default()),
        wake: Condvar::new(),
    });
    let worker = lane.clone();
    std::thread::Builder::new()
        .name("winsizep-linked-resize".into())
        .spawn(move || {
            // SAFETY: copied HWND values are validated against their original process/thread.
            unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            }
            loop {
                let job = {
                    let mut state = worker.state.lock().unwrap();
                    while state.latest.is_empty() {
                        let (next, timeout) = worker
                            .wake
                            .wait_timeout(state, Duration::from_secs(2))
                            .unwrap();
                        state = next;
                        if timeout.timed_out()
                            && state.latest.is_empty()
                            && Arc::strong_count(&worker) == 1
                        {
                            state.retired = true;
                            return;
                        }
                    }
                    let handle = *state.latest.keys().next().unwrap();
                    let job = state.latest.remove(&handle).unwrap();
                    if state.owners.get(&handle) != Some(&job.generation) {
                        continue;
                    }
                    state.active = Some((handle, job.generation, Instant::now()));
                    job
                };
                let hwnd = job.handle as HWND;
                let result = unsafe {
                    let mut pid = 0;
                    let thread = GetWindowThreadProcessId(hwnd, &mut pid);
                    if (pid, thread) != key || IsWindow(hwnd) == 0 || IsHungAppWindow(hwnd) != 0 {
                        Err("Linked window unavailable / 联动窗口不可用".into())
                    } else {
                        desktop::check(SetWindowPos(
                            hwnd,
                            std::ptr::null_mut(),
                            job.rect.x,
                            job.rect.y,
                            job.rect.width,
                            job.rect.height,
                            SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
                        ))
                    }
                };
                let mut state = worker.state.lock().unwrap();
                state.active = None;
                if let Err(error) = result {
                    state.error = Some((job.generation, error));
                }
            }
        })
        .map_err(|e| e.to_string())?;
    lanes.insert(key, Arc::downgrade(&lane));
    Ok(lane)
}
pub(super) struct Updates {
    generation: u64,
    lanes: Mutex<Vec<Arc<Lane>>>,
}
impl Updates {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            generation: GENERATION.fetch_add(1, Ordering::Relaxed),
            lanes: Mutex::new(Vec::new()),
        })
    }
    pub fn status(&self) -> Result<(), String> {
        for lane in self.lanes.lock().unwrap().iter() {
            let state = lane.state.lock().unwrap();
            if let Some((generation, error)) = &state.error
                && *generation == self.generation
            {
                return Err(error.clone());
            }
            if state.active.is_some_and(|(_, generation, at)| {
                generation == self.generation && at.elapsed() > WAIT_LIMIT
            }) {
                return Err("Linked resize timed out / 联动缩放响应超时".into());
            }
        }
        Ok(())
    }
    pub fn submit(&self, positions: Vec<(usize, Rect)>) -> Result<(), String> {
        self.status()?;
        let mut groups: HashMap<Key, Vec<Job>> = HashMap::new();
        for (handle, rect) in positions {
            let mut pid = 0;
            let thread = unsafe { GetWindowThreadProcessId(handle as HWND, &mut pid) };
            if thread == 0 {
                return Err("Linked window closed / 联动窗口已关闭".into());
            }
            groups.entry((pid, thread)).or_default().push(Job {
                handle,
                rect,
                generation: self.generation,
            });
        }
        for (key, jobs) in groups {
            let target = lane(key)?;
            let mut state = target.state.lock().unwrap();
            for job in jobs {
                if state
                    .owners
                    .get(&job.handle)
                    .is_some_and(|generation| *generation > self.generation)
                {
                    continue;
                }
                state.owners.insert(job.handle, self.generation);
                state.latest.insert(job.handle, job);
            }
            target.wake.notify_one();
            drop(state);
            let mut used = self.lanes.lock().unwrap();
            if !used.iter().any(|old| Arc::ptr_eq(old, &target)) {
                used.push(target);
            }
        }
        Ok(())
    }
}

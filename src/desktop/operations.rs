//! Resident geometry work runs off the message thread; command-line callers stay synchronous.
use crate::{cli::Request, config::Config, desktop};
use std::{
    cell::RefCell,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use windows_sys::Win32::{Foundation::*, UI::HiDpi::*};

thread_local! {static CANCELLATION: RefCell<Option<(Arc<Shared>,u64)>> = const {RefCell::new(None)};}
pub(crate) fn check_cancelled() -> Result<(), String> {
    if CANCELLATION.with_borrow(|c| {
        c.as_ref().is_some_and(|(s, id)| {
            s.generation.load(Ordering::Acquire) != *id || s.stopped.load(Ordering::Acquire)
        })
    }) {
        Err("Operation canceled / 操作已取消".into())
    } else {
        Ok(())
    }
}
enum Work {
    Apply {
        hwnd: usize,
        request: Request,
        cfg: Config,
        path: PathBuf,
        point: (i32, i32),
    },
    Drag {
        hwnd: usize,
        origin: (i32, i32),
        resize: bool,
        cfg: Config,
    },
}
#[derive(Clone, Copy)]
struct Motion {
    id: u64,
    point: (i32, i32),
    finished: bool,
}
#[derive(Default)]
struct State {
    job: Option<(u64, Work)>,
    motion: Option<Motion>,
    busy: bool,
    error: Option<String>,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    generation: AtomicU64,
    stopped: AtomicBool,
}
pub(crate) struct Operations {
    shared: Arc<Shared>,
}
pub(crate) struct Drag {
    shared: Arc<Shared>,
    id: u64,
    last: std::cell::Cell<(i32, i32)>,
}
impl Operations {
    pub fn new() -> Result<Self, String> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            generation: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        });
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("winsizep-geometry".into())
            .spawn(move || run(worker))
            .map_err(|e| e.to_string())?;
        Ok(Self { shared })
    }
    fn submit(&self, work: Work, motion: Option<((i32, i32), bool)>) -> u64 {
        let mut state = self.shared.state.lock().unwrap();
        let id = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        state.job = Some((id, work));
        state.motion = motion.map(|(point, finished)| Motion {
            id,
            point,
            finished,
        });
        state.busy = true;
        state.error = None;
        self.shared.wake.notify_one();
        id
    }
    pub fn apply(&self, hwnd: HWND, request: Request, cfg: Config, path: PathBuf, point: POINT) {
        self.submit(
            Work::Apply {
                hwnd: hwnd as usize,
                request,
                cfg,
                path,
                point: (point.x, point.y),
            },
            None,
        );
    }
    pub fn start_drag(&self, hwnd: HWND, origin: POINT, resize: bool, cfg: Config) -> Drag {
        let id = self.submit(
            Work::Drag {
                hwnd: hwnd as usize,
                origin: (origin.x, origin.y),
                resize,
                cfg,
            },
            Some(((origin.x, origin.y), false)),
        );
        Drag {
            shared: self.shared.clone(),
            id,
            last: std::cell::Cell::new((origin.x, origin.y)),
        }
    }
    pub fn cancel(&self) {
        cancel(&self.shared, None);
    }
    pub fn pending(&self) -> bool {
        self.shared.state.lock().unwrap().busy
    }
    pub fn poll(&self) -> Result<(), String> {
        self.shared
            .state
            .lock()
            .unwrap()
            .error
            .take()
            .map_or(Ok(()), Err)
    }
}
impl Drop for Operations {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.cancel();
    }
}
fn cancel(shared: &Shared, id: Option<u64>) {
    let mut state = shared.state.lock().unwrap();
    if id.is_none_or(|id| shared.generation.load(Ordering::Acquire) == id) {
        shared.generation.fetch_add(1, Ordering::AcqRel);
        state.job = None;
        state.motion = None;
        state.busy = false;
    }
    shared.wake.notify_one();
}
impl Drag {
    pub fn follow(&self, point: POINT) -> Result<(), String> {
        self.last.set((point.x, point.y));
        let mut state = self.shared.state.lock().unwrap();
        if let Some(error) = state.error.take() {
            return Err(error);
        }
        if self.shared.generation.load(Ordering::Acquire) == self.id {
            state.motion = Some(Motion {
                id: self.id,
                point: (point.x, point.y),
                finished: false,
            });
            self.shared.wake.notify_one();
        }
        Ok(())
    }
    pub fn cancel(&self) -> Result<(), String> {
        cancel(&self.shared, Some(self.id));
        Ok(())
    }
}
impl Drop for Drag {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        if self.shared.generation.load(Ordering::Acquire) == self.id {
            state.motion = Some(Motion {
                id: self.id,
                point: self.last.get(),
                finished: true,
            });
        }
        self.shared.wake.notify_one();
    }
}
fn run(shared: Arc<Shared>) {
    // SAFETY: only copied window handles enter this thread; native geometry and
    // Drag objects stay here for their entire lifetime.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut drag: Option<(u64, desktop::Drag)> = None;
    loop {
        let (job, motion) = {
            let mut state = shared.state.lock().unwrap();
            while state.job.is_none()
                && state.motion.is_none()
                && !shared.stopped.load(Ordering::Acquire)
                && drag
                    .as_ref()
                    .is_none_or(|(id, _)| *id == shared.generation.load(Ordering::Acquire))
            {
                state = shared.wake.wait(state).unwrap();
            }
            (state.job.take(), state.motion.take())
        };
        if drag.as_ref().is_some_and(|(id, _)| {
            *id != shared.generation.load(Ordering::Acquire)
                || shared.stopped.load(Ordering::Acquire)
        }) && let Some((_, old)) = drag.take()
        {
            let _ = old.cancel();
        }
        if shared.stopped.load(Ordering::Acquire) {
            return;
        }
        if let Some((id, job)) = job {
            if shared.generation.load(Ordering::Acquire) != id {
                continue;
            }
            CANCELLATION.with_borrow_mut(|c| *c = Some((shared.clone(), id)));
            let result = match job {
                Work::Apply {
                    hwnd,
                    request,
                    cfg,
                    path,
                    point,
                } => desktop::apply(
                    hwnd as HWND,
                    &request,
                    &cfg,
                    &path,
                    POINT {
                        x: point.0,
                        y: point.1,
                    },
                ),
                Work::Drag {
                    hwnd,
                    origin,
                    resize,
                    cfg,
                } => desktop::Drag::start(
                    hwnd as HWND,
                    POINT {
                        x: origin.0,
                        y: origin.1,
                    },
                    resize,
                    &cfg,
                )
                .map(|d| drag = Some((id, d))),
            };
            CANCELLATION.with_borrow_mut(|c| *c = None);
            let mut state = shared.state.lock().unwrap();
            if shared.generation.load(Ordering::Acquire) == id {
                if let Err(error) = result {
                    state.error = Some(error);
                    drag = None;
                    state.motion = None;
                }
                state.busy = drag.is_some();
            }
        }
        if let Some(motion) = motion
            && let Some((id, active)) = &drag
            && *id == motion.id
            && shared.generation.load(Ordering::Acquire) == *id
        {
            let mut result = active.follow(POINT {
                x: motion.point.0,
                y: motion.point.1,
            });
            if motion.finished && result.is_ok() {
                while active.pending()
                    && shared.generation.load(Ordering::Acquire) == *id
                    && !shared.stopped.load(Ordering::Acquire)
                {
                    if let Err(error) = active.status() {
                        result = Err(error);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                if shared.generation.load(Ordering::Acquire) != *id {
                    let _ = active.cancel();
                }
                if result.is_ok() {
                    result = active.status();
                }
            }
            if motion.finished || result.is_err() {
                drag = None;
                let mut state = shared.state.lock().unwrap();
                if shared.generation.load(Ordering::Acquire) == motion.id {
                    state.error = result.err();
                }
                state.busy = state.job.is_some();
            }
        }
        if drag.is_none() {
            let mut state = shared.state.lock().unwrap();
            state.busy = state.job.is_some();
        }
    }
}

//! A restart succeeds only after the successor has initialized its resident resources.
use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{Foundation::*, System::Threading::*};

const INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(15);
const PARENT_TIMEOUT: Duration = Duration::from_secs(20);

fn name(id: &str, kind: &str) -> String {
    format!("Local\\WinSizeP.Restart.{id}.{kind}")
}

fn event(id: &str, kind: &str, create: bool) -> Result<OwnedHandle, String> {
    let name = name(id, kind);
    let handle = crate::config_access::run(|| {
        if create {
            crate::security::handoff_event(&name)
        } else {
            // SAFETY: the name is valid until OpenEvent returns. The owned handle
            // keeps the event alive even if the parent exits.
            let handle = unsafe {
                OpenEventW(
                    SYNCHRONIZATION_SYNCHRONIZE | EVENT_MODIFY_STATE,
                    0,
                    crate::desktop::wide(&name).as_ptr(),
                )
            };
            if handle.is_null() {
                Err(crate::desktop::err())
            } else {
                Ok(handle as usize)
            }
        }
    })??;
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as HANDLE) })
}

pub struct Parent {
    pub id: String,
    ready: OwnedHandle,
    accepted: OwnedHandle,
}

impl Parent {
    pub fn new() -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let id = format!("{}-{nonce}", std::process::id());
        Ok(Self {
            ready: event(&id, "ready", true)?,
            accepted: event(&id, "accepted", true)?,
            id,
        })
    }

    pub fn wait(&self, process: Option<&OwnedHandle>) -> Result<(), String> {
        self.wait_for(process, PARENT_TIMEOUT)
    }

    fn wait_for(&self, process: Option<&OwnedHandle>, timeout: Duration) -> Result<(), String> {
        let handles = [
            process.map_or(std::ptr::null_mut(), AsRawHandle::as_raw_handle),
            self.ready.as_raw_handle(),
        ];
        let skipped = usize::from(process.is_none());
        // The watchdog exits before this timeout. Checking the process first also
        // rejects a child that signals readiness and then fails before acceptance.
        match unsafe {
            WaitForMultipleObjects(
                (2 - skipped) as u32,
                handles[skipped..].as_ptr(),
                0,
                timeout.as_millis() as u32,
            )
        } {
            n if n == WAIT_OBJECT_0 + 1 - skipped as u32 => {
                crate::desktop::check(unsafe { SetEvent(self.accepted.as_raw_handle()) })
            }
            WAIT_OBJECT_0 => {
                Err("Successor exited before startup completed / 新进程在启动完成前退出".into())
            }
            WAIT_TIMEOUT => Err("Successor startup timed out / 新进程启动确认超时".into()),
            _ => Err(crate::desktop::err()),
        }
    }
}

pub struct Child {
    ready: OwnedHandle,
    accepted: OwnedHandle,
    watchdog: mpsc::Sender<()>,
}

impl Child {
    pub fn open(id: &str) -> Result<Self, String> {
        Self::open_with_timeout(id, INITIALIZATION_TIMEOUT)
    }

    fn open_with_timeout(id: &str, timeout: Duration) -> Result<Self, String> {
        if id.is_empty() || !id.bytes().all(|c| c.is_ascii_digit() || c == b'-') {
            return Err("Invalid restart identifier".into());
        }
        let ready = event(id, "ready", false)?;
        let accepted = event(id, "accepted", false)?;
        let (watchdog, wait) = mpsc::channel();
        std::thread::Builder::new()
            .name("startup-watchdog".into())
            .spawn(move || {
                if matches!(
                    wait.recv_timeout(timeout),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    std::process::exit(1);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            ready,
            accepted,
            watchdog,
        })
    }

    pub fn confirm(&self) -> Result<(), String> {
        crate::desktop::check(unsafe { SetEvent(self.ready.as_raw_handle()) })?;
        if unsafe { WaitForSingleObject(self.accepted.as_raw_handle(), 3000) } != WAIT_OBJECT_0 {
            return Err("Restart was abandoned / 重启交接已取消".into());
        }
        self.watchdog.send(()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
    };

    #[test]
    #[ignore = "isolated child for the startup handshake tests"]
    fn startup_probe() {
        let id = std::env::var("WINSIZEP_TEST_HANDOFF").unwrap();
        let child = Child::open_with_timeout(&id, Duration::from_millis(500)).unwrap();
        match std::env::var("WINSIZEP_TEST_STARTUP").unwrap().as_str() {
            "ready" => child.confirm().unwrap(),
            "fail" => std::process::exit(2),
            "hang" => std::thread::sleep(Duration::from_secs(10)),
            _ => unreachable!(),
        }
    }

    #[test]
    fn startup_requires_readiness_and_stalled_children_exit() {
        for mode in ["ready", "fail", "hang"] {
            let parent = Parent::new().unwrap();
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "handoff::tests::startup_probe", "--ignored"])
                .env("WINSIZEP_TEST_HANDOFF", &parent.id)
                .env("WINSIZEP_TEST_STARTUP", mode)
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let process = OwnedHandle::from(child);
            assert_eq!(
                parent
                    .wait_for(Some(&process), Duration::from_secs(5))
                    .is_ok(),
                mode == "ready",
                "{mode}"
            );
            assert_eq!(
                unsafe { WaitForSingleObject(process.as_raw_handle(), 5000) },
                WAIT_OBJECT_0,
                "{mode}"
            );
        }
    }
}

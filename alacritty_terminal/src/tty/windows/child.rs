use std::ffi::c_void;
use std::io::Error;
use std::num::NonZeroU32;
use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use polling::os::iocp::{CompletionPacket, PollerIocpExt};
use polling::{Event, Poller};

use windows_sys::Win32::Foundation::{BOOLEAN, FALSE, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessId, RegisterWaitForSingleObject, UnregisterWait, INFINITE,
    WT_EXECUTEINWAITTHREAD, WT_EXECUTEONLYONCE,
};

use crate::tty::ChildEvent;

struct Interest {
    poller: Arc<Poller>,
    event: Event,
}

struct ChildExitSender {
    sender: mpsc::Sender<ChildEvent>,
    interest: Arc<Mutex<Option<Interest>>>,
    child_handle: AtomicPtr<c_void>,
}

/// WinAPI callback to run when child process exits.
extern "system" fn child_exit_callback(ctx: *mut c_void, timed_out: BOOLEAN) {
    if timed_out != 0 {
        return;
    }

    let event_tx: Box<_> = unsafe { Box::from_raw(ctx as *mut ChildExitSender) };

    let mut exit_code = 0_u32;
    let child_handle = event_tx.child_handle.load(Ordering::Relaxed) as HANDLE;
    let status = unsafe { GetExitCodeProcess(child_handle, &mut exit_code) };
    let exit_status = if status == FALSE { None } else { Some(ExitStatus::from_raw(exit_code)) };
    event_tx.sender.send(ChildEvent::Exited(exit_status)).ok();

    let interest = event_tx.interest.lock().unwrap();
    if let Some(interest) = interest.as_ref() {
        interest.poller.post(CompletionPacket::new(interest.event)).ok();
    }
}

pub struct ChildExitWatcher {
    wait_handle: AtomicPtr<c_void>,
    event_rx: mpsc::Receiver<ChildEvent>,
    pending_event: Mutex<Option<ChildEvent>>,
    interest: Arc<Mutex<Option<Interest>>>,
    child_handle: AtomicPtr<c_void>,
    pid: Option<NonZeroU32>,
}

impl ChildExitWatcher {
    pub fn new(child_handle: HANDLE) -> Result<ChildExitWatcher, Error> {
        let (event_tx, event_rx) = mpsc::channel();

        let mut wait_handle: HANDLE = ptr::null_mut();
        let interest = Arc::new(Mutex::new(None));
        let sender_ref = Box::new(ChildExitSender {
            sender: event_tx,
            interest: interest.clone(),
            child_handle: AtomicPtr::from(child_handle),
        });

        let success = unsafe {
            RegisterWaitForSingleObject(
                &mut wait_handle,
                child_handle,
                Some(child_exit_callback),
                Box::into_raw(sender_ref).cast(),
                INFINITE,
                WT_EXECUTEINWAITTHREAD | WT_EXECUTEONLYONCE,
            )
        };

        if success == 0 {
            Err(Error::last_os_error())
        } else {
            let pid = unsafe { NonZeroU32::new(GetProcessId(child_handle)) };
            Ok(ChildExitWatcher {
                event_rx,
                pending_event: Mutex::new(None),
                interest,
                pid,
                child_handle: AtomicPtr::from(child_handle),
                wait_handle: AtomicPtr::from(wait_handle),
            })
        }
    }

    pub fn register(&self, poller: &Arc<Poller>, event: Event) {
        *self.interest.lock().unwrap() = Some(Interest { poller: poller.clone(), event });
        if let Ok(queued_event) = self.event_rx.try_recv() {
            *self.pending_event.lock().unwrap() = Some(queued_event);
            poller.post(CompletionPacket::new(event)).ok();
        }
    }

    pub fn next_event(&self) -> Option<ChildEvent> {
        if let Some(event) = self.pending_event.lock().unwrap().take() {
            return Some(event);
        }
        match self.event_rx.try_recv() {
            Ok(event) => Some(event),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(ChildEvent::Exited(None)),
        }
    }

    pub fn deregister(&self) {
        *self.interest.lock().unwrap() = None;
    }

    /// Retrieve the process handle of the underlying child process.
    ///
    /// This function does **not** pass ownership of the raw handle to you,
    /// and the handle is only guaranteed to be valid while the hosted application
    /// has not yet been destroyed.
    ///
    /// If you terminate the process using this handle, the terminal will get a
    /// timeout error, and the child watcher will emit an `Exited` event.
    pub fn raw_handle(&self) -> HANDLE {
        self.child_handle.load(Ordering::Relaxed) as HANDLE
    }

    /// Retrieve the Process ID associated to the underlying child process.
    pub fn pid(&self) -> Option<NonZeroU32> {
        self.pid
    }
}

impl Drop for ChildExitWatcher {
    fn drop(&mut self) {
        unsafe {
            UnregisterWait(self.wait_handle.load(Ordering::Relaxed) as HANDLE);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsRawHandle;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;

    use super::super::PTY_CHILD_EVENT_TOKEN;
    use super::*;

    #[test]
    pub fn event_is_emitted_when_child_exits() {
        const WAIT_TIMEOUT: Duration = Duration::from_millis(200);

        let poller = Arc::new(Poller::new().unwrap());

        let mut child = Command::new("cmd.exe").spawn().unwrap();
        let child_exit_watcher = ChildExitWatcher::new(child.as_raw_handle() as HANDLE).unwrap();
        child_exit_watcher.register(&poller, Event::readable(PTY_CHILD_EVENT_TOKEN));

        child.kill().unwrap();

        // Poll for the event or fail with timeout if nothing has been sent.
        let mut events = polling::Events::new();
        poller.wait(&mut events, Some(WAIT_TIMEOUT)).unwrap();
        assert_eq!(events.iter().next().unwrap().key, PTY_CHILD_EVENT_TOKEN);
        // Verify that at least one `ChildEvent::Exited` was received.
        let expected_status = ExitStatus::from_raw(1);
        assert_eq!(
            child_exit_watcher.next_event(),
            Some(ChildEvent::Exited(Some(expected_status)))
        );
    }

    #[test]
    fn an_exit_queued_before_register_is_relayed() {
        const WAIT_TIMEOUT: Duration = Duration::from_millis(200);

        let poller = Arc::new(Poller::new().unwrap());
        let (sender, event_rx) = mpsc::channel();
        sender.send(ChildEvent::Exited(Some(ExitStatus::from_raw(7)))).unwrap();
        let watcher = ChildExitWatcher {
            wait_handle: AtomicPtr::new(ptr::null_mut()),
            event_rx,
            pending_event: Mutex::new(None),
            interest: Arc::new(Mutex::new(None)),
            child_handle: AtomicPtr::new(ptr::null_mut()),
            pid: None,
        };

        // This is the exact registration-window state: the OS callback has
        // queued an exit, but it saw no interest and could not post a token.
        watcher.register(&poller, Event::readable(PTY_CHILD_EVENT_TOKEN));

        let mut events = polling::Events::new();
        poller.wait(&mut events, Some(WAIT_TIMEOUT)).unwrap();
        assert_eq!(events.iter().next().unwrap().key, PTY_CHILD_EVENT_TOKEN);
        assert_eq!(watcher.next_event(), Some(ChildEvent::Exited(Some(ExitStatus::from_raw(7)))));

        // The manually-built watcher has no registered WinAPI wait to remove.
        std::mem::forget(watcher);
    }
}

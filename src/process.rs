//! Asynchronous exit observation for owned Unix child processes.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{
    Child as SystemChild, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus,
};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use crate::Timer;
use crate::io::{AsyncFd, FdInterest, Handle, Nonblocking};

const PORTABLE_WAIT_INTERVAL: Duration = Duration::from_millis(8);

/// A Unix child with pipes and exit notification driven by a zrun I/O handle.
///
/// The caller controls termination, including any process group or supervisor.
/// Dropping the child closes its remaining pipes and arranges for reaping once
/// it exits. It does not send a signal.
pub struct Child {
    child: Option<SystemChild>,
    io: Handle,
}

/// Start the prepared command with its original arguments and spawn attributes.
///
/// # Errors
/// Returns the OS error if the command cannot be started.
pub fn spawn(command: &mut Command, io: &Handle) -> io::Result<Child> {
    Ok(Child {
        child: Some(command.spawn()?),
        io: io.clone(),
    })
}

impl Child {
    fn raw(&self) -> &SystemChild {
        self.child.as_ref().expect("child is still owned")
    }

    fn raw_mut(&mut self) -> &mut SystemChild {
        self.child.as_mut().expect("child is still owned")
    }

    /// Return the process ID. It only identifies this child until reaping.
    pub fn id(&self) -> u32 {
        self.raw().id()
    }

    /// Send a kill signal to the child, without waiting for its exit.
    ///
    /// # Errors
    /// Returns an OS error if the signal cannot be sent.
    pub fn start_kill(&mut self) -> io::Result<()> {
        self.raw_mut().kill()
    }

    /// Move the configured stdin pipe into the reactor, if present.
    ///
    /// # Errors
    /// Returns an error if the descriptor could not be registered.
    pub fn take_stdin(&mut self) -> io::Result<Option<AsyncFd<ChildStdin>>> {
        self.raw_mut()
            .stdin
            .take()
            .map(|pipe| {
                AsyncFd::with_interest(&self.io, pipe, Nonblocking::Set, FdInterest::Writable)
            })
            .transpose()
    }

    /// Move the configured stdout pipe into the reactor, if present.
    ///
    /// # Errors
    /// Returns an error if the descriptor could not be registered.
    pub fn take_stdout(&mut self) -> io::Result<Option<AsyncFd<ChildStdout>>> {
        self.raw_mut()
            .stdout
            .take()
            .map(|pipe| AsyncFd::new(&self.io, pipe, Nonblocking::Set))
            .transpose()
    }

    /// Move the configured stderr pipe into the reactor, if present.
    ///
    /// # Errors
    /// Returns an error if the descriptor could not be registered.
    pub fn take_stderr(&mut self) -> io::Result<Option<AsyncFd<ChildStderr>>> {
        self.raw_mut()
            .stderr
            .take()
            .map(|pipe| AsyncFd::new(&self.io, pipe, Nonblocking::Set))
            .transpose()
    }

    /// Check for exit without reaping. Call while the child is still unreaped.
    ///
    /// # Errors
    /// Returns an OS error if the status cannot be read.
    pub fn try_exit(&self) -> io::Result<Option<ExitStatus>> {
        let pid = process_id(self.raw())?;
        loop {
            match observe_exit(pid) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                result => return result.map(Some),
            }
        }
    }

    /// Await exit without reaping the child. Call before the first wait.
    ///
    /// # Errors
    /// Returns an OS error or a timer or driver shutdown error.
    pub async fn wait_for_exit(&self, timer: &Timer) -> io::Result<ExitStatus> {
        wait_for_exit(self.raw(), &self.io, timer).await
    }

    /// Await and reap the child. Undetached stdin is closed before waiting.
    /// Subsequent calls return the cached exit status.
    ///
    /// # Errors
    /// Returns an OS error or a timer or driver shutdown error.
    pub async fn wait(&mut self, timer: &Timer) -> io::Result<ExitStatus> {
        self.raw_mut().stdin.take();
        if let Some(status) = self.raw_mut().try_wait()? {
            return Ok(status);
        }
        self.wait_for_exit(timer).await?;
        self.raw_mut().wait()
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.stdin.take();
            child.stdout.take();
            child.stderr.take();
            if matches!(child.try_wait(), Ok(None)) {
                reap_on_exit(child);
            }
        }
    }
}

fn reap_on_exit(child: SystemChild) {
    static REAPER: OnceLock<Option<mpsc::Sender<SystemChild>>> = OnceLock::new();
    let sender = REAPER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("zrun-child-reaper".into())
            .spawn(move || reap_children(receiver))
            .ok()
            .map(|_| sender)
    });
    let unsent = match sender {
        Some(sender) => sender.send(child).err().map(|error| error.0),
        None => Some(child),
    };
    if let Some(mut child) = unsent {
        let _ = child.wait();
    }
}

fn reap_children(receiver: mpsc::Receiver<SystemChild>) {
    let mut pending = Vec::new();
    loop {
        let next = if pending.is_empty() {
            let Ok(child) = receiver.recv() else { return };
            Some(child)
        } else {
            receiver.recv_timeout(PORTABLE_WAIT_INTERVAL).ok()
        };
        pending.extend(next);
        pending.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }
}

fn process_id(child: &SystemChild) -> io::Result<libc::pid_t> {
    libc::pid_t::try_from(child.id())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid child process ID"))
}

/// Wait for a child's exit while retaining its process identity and status.
///
/// Pass an owned, unreaped child and keep it until observation finishes.
/// The caller can then terminate remaining descendants and reap the leader
/// with its usual process supervision policy.
///
/// # Errors
/// Returns an OS error or a timer or driver shutdown error.
pub async fn wait_for_exit(
    child: &SystemChild,
    io: &Handle,
    timer: &Timer,
) -> io::Result<ExitStatus> {
    let pid = process_id(child)?;

    #[cfg(target_os = "linux")]
    if let Some(mut ready) = register_exit(pid, io)? {
        return ready.readable(|_| observe_exit(pid)).await;
    }

    #[cfg(not(target_os = "linux"))]
    let _ = io;

    loop {
        match observe_exit(pid) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                timer
                    .sleep(PORTABLE_WAIT_INTERVAL)
                    .await
                    .map_err(io::Error::other)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

#[cfg(target_os = "linux")]
fn register_exit(
    pid: libc::pid_t,
    io: &Handle,
) -> io::Result<Option<crate::io::AsyncFd<std::os::fd::OwnedFd>>> {
    use std::os::fd::{FromRawFd, OwnedFd};

    // SAFETY: the syscall takes a child process ID and no pointers.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        return Ok(None);
    }
    // SAFETY: successful pidfd_open returns a unique owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) };
    AsyncFd::new(io, fd, Nonblocking::ReadinessOnly).map(Some)
}

fn observe_exit(pid: libc::pid_t) -> io::Result<ExitStatus> {
    // SAFETY: siginfo_t is a C record. Zero also represents an empty status
    // on kernels that leave the record untouched when the child is still live.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: the status record is writable until waitid returns.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful waitid populates the child status or leaves it zero.
    if unsafe { info.si_pid() } != pid {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    // SAFETY: si_pid confirmed the child's exit status is present.
    let status = unsafe { info.si_status() };
    let raw = match info.si_code {
        libc::CLD_EXITED => status << 8,
        libc::CLD_KILLED => status,
        libc::CLD_DUMPED => status | 0x80,
        _ => return Err(io::Error::other("child exit status unavailable")),
    };
    Ok(ExitStatus::from_raw(raw))
}

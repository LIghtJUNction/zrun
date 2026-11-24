//! Asynchronous exit observation for owned Unix child processes.

use std::io;
use std::process::Child;
use std::time::Duration;

use crate::Timer;
use crate::io::Handle;

const PORTABLE_WAIT_INTERVAL: Duration = Duration::from_millis(8);

/// Wait for a child's exit while retaining its process identity and status.
///
/// Pass an owned, unreaped child and keep it until observation finishes.
/// The caller can then terminate remaining descendants and reap the leader
/// with its usual process supervision policy.
///
/// # Errors
/// Returns an OS error or a timer or driver shutdown error.
pub async fn wait_for_exit(child: &Child, io: &Handle, timer: &Timer) -> io::Result<()> {
    let pid = libc::pid_t::try_from(child.id())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid child process ID"))?;

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

    use crate::io::{AsyncFd, Nonblocking};

    // SAFETY: the syscall takes a child process ID and no pointers.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        return Ok(None);
    }
    // SAFETY: successful pidfd_open returns a unique owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) };
    AsyncFd::new(io, fd, Nonblocking::ReadinessOnly).map(Some)
}

fn observe_exit(pid: libc::pid_t) -> io::Result<()> {
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
    if unsafe { info.si_pid() } == pid {
        Ok(())
    } else {
        Err(io::ErrorKind::WouldBlock.into())
    }
}

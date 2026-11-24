use std::future::{Future, poll_fn};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::pin::pin;
use std::process::{Child, Command, Stdio};
use std::sync::{OnceLock, mpsc};
use std::task::Poll;
use std::time::Duration;

use zrun::Timer;
use zrun::io::{AsyncFd, Handle, Nonblocking};

use crate::execution::{Request, ResultRecord};

pub(crate) async fn execute(
    shell: &Path,
    request: Request,
    io: &Handle,
    timer: &Timer,
    max_output: usize,
) -> io::Result<ResultRecord> {
    let mut command = Command::new(shell);
    command
        .arg("-c")
        .arg(request.script.as_ref())
        .env_remove("ZRUN_MCP_TOKEN")
        .env_remove("ZRUN_POLICY")
        .env_remove("ZRUN_PLUGIN")
        .env_remove("ZRUN_SOCKET")
        .env_remove("ZRUN_WORKERS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(cwd) = request.cwd {
        command.current_dir(cwd);
    }
    let mut group = ChildGroup::new(command.spawn()?);
    let child = group.child.as_mut().expect("the new group owns its child");
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let mut stdout = AsyncFd::new(io, stdout, Nonblocking::Set)?;
    let mut stderr = AsyncFd::new(io, stderr, Nonblocking::Set)?;
    let mut out = Capture::default();
    let mut err = Capture::default();
    let deadline = timer.sleep(Duration::from_millis(request.timeout_ms));

    let timed_out = {
        let mut out_read = pin!(out.drain(&mut stdout, max_output));
        let mut err_read = pin!(err.drain(&mut stderr, max_output));
        let mut exited = pin!(wait_until_exited(group.pid, io, timer));
        let mut deadline = pin!(deadline);
        let mut leader_exited = false;
        let mut out_done = false;
        let mut err_done = false;
        poll_fn(|cx| {
            if !leader_exited && let Poll::Ready(result) = exited.as_mut().poll(cx) {
                result?;
                leader_exited = true;
                // The unreaped leader pins its PID while we signal remaining group members.
                group.kill();
            }
            if !out_done && let Poll::Ready(result) = out_read.as_mut().poll(cx) {
                result?;
                out_done = true;
            }
            if !err_done && let Poll::Ready(result) = err_read.as_mut().poll(cx) {
                result?;
                err_done = true;
            }
            if leader_exited && out_done && err_done {
                return Poll::Ready(Ok::<_, io::Error>(false));
            }
            if let Poll::Ready(result) = deadline.as_mut().poll(cx) {
                result.map_err(io::Error::other)?;
                return Poll::Ready(Ok(true));
            }
            Poll::Pending
        })
        .await?
    };
    if timed_out {
        group.kill();
        wait_until_exited(group.pid, io, timer).await?;
    }
    let status = group.reap()?;
    let truncated = out.truncated || err.truncated;
    Ok(ResultRecord {
        exit_code: if timed_out {
            124
        } else {
            status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
        },
        stdout: out.finish(max_output),
        stderr: err.finish(max_output),
        truncated,
        error: timed_out.then(|| "command timed out".into()),
        encoding: Default::default(),
    })
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl Capture {
    fn finish(self, limit: usize) -> Vec<u8> {
        if self.bytes.capacity() > limit {
            self.bytes.into_boxed_slice().into_vec()
        } else {
            self.bytes
        }
    }

    async fn drain<T: AsRawFd + Read>(
        &mut self,
        pipe: &mut AsyncFd<T>,
        limit: usize,
    ) -> io::Result<()> {
        let mut buf = [0; 4096];
        loop {
            // Yield after a bounded turn even if a process keeps its pipe readable.
            for _ in 0..16 {
                let count = pipe.read(&mut buf).await?;
                if count == 0 {
                    return Ok(());
                }
                let retain = count.min(limit - self.bytes.len());
                let required = self.bytes.len() + retain;
                if required > self.bytes.capacity() {
                    let capacity = required.next_power_of_two().min(limit);
                    self.bytes.reserve_exact(capacity - self.bytes.len());
                }
                self.bytes.extend_from_slice(&buf[..retain]);
                self.truncated |= retain != count;
            }
            zrun::yield_now().await;
        }
    }
}

async fn wait_until_exited(pid: libc::pid_t, io: &Handle, timer: &Timer) -> io::Result<()> {
    // SAFETY: pid identifies the unreaped child. Successful pidfd_open returns an owned fd.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw >= 0 {
        // SAFETY: the successful syscall returned a new descriptor within the c_int range.
        let fd = unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) };
        let mut fd = AsyncFd::new(io, fd, Nonblocking::ReadinessOnly)?;
        return fd.readable(|_| observe_exit(pid)).await;
    }
    // Older or restricted kernels can still observe exit without consuming status.
    loop {
        match observe_exit(pid) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                timer
                    .sleep(Duration::from_millis(8))
                    .await
                    .map_err(io::Error::other)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            terminal => return terminal,
        }
    }
}

fn observe_exit(pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: siginfo_t is a plain C record; zero initialization also handles kernels
    // that do not write the record when WNOHANG finds no waitable child.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: the pointer is valid and this operation only observes the owned child.
    let code = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    if code != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the signal record is initialized and si_pid is valid for waitid.
    if unsafe { info.si_pid() } == pid {
        Ok(())
    } else {
        Err(io::ErrorKind::WouldBlock.into())
    }
}

struct ChildGroup {
    child: Option<Child>,
    pid: libc::pid_t,
}

impl ChildGroup {
    fn new(child: Child) -> Self {
        let pid = i32::try_from(child.id()).expect("Linux child IDs are positive pid_t values");
        Self {
            child: Some(child),
            pid,
        }
    }

    fn kill(&self) {
        // SAFETY: The group ID is the PID of our still-unreaped child.
        // No successful wait() can occur while we hold this group.
        unsafe { libc::kill(-self.pid, libc::SIGKILL) };
    }

    fn reap(mut self) -> io::Result<std::process::ExitStatus> {
        self.child
            .as_mut()
            .expect("group has not been reaped")
            .wait()
            .inspect(|_| {
                self.child.take();
            })
    }
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.kill();
            if matches!(child.try_wait(), Ok(None)) {
                reap_cancelled(child);
            }
        }
    }
}

fn reap_cancelled(child: Child) {
    static REAPER: OnceLock<Option<mpsc::Sender<Child>>> = OnceLock::new();
    let sender = REAPER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Child>();
        std::thread::Builder::new()
            .name("zrun-reaper".into())
            .spawn(move || {
                let mut pending = Vec::new();
                loop {
                    if pending.is_empty() {
                        let Ok(child) = receiver.recv() else { return };
                        pending.push(child);
                    } else if let Ok(child) = receiver.recv_timeout(Duration::from_millis(10)) {
                        pending.push(child);
                    }
                    pending.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
                }
            })
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

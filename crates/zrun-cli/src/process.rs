use std::future::{Future, poll_fn};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::pin::pin;
use std::process::{Command, Stdio};
use std::task::Poll;
use std::time::Duration;

use zrun::Timer;
use zrun::io::{AsyncFd, Handle};
use zrun::process::Child;

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
    if !request.args.is_empty() {
        command.arg("zrun").args(request.args.iter());
    }
    if let Some(cwd) = request.cwd {
        command.current_dir(cwd);
    }
    let mut group = ChildGroup::new(zrun::process::spawn(&mut command, io)?);
    let child = group.child.as_mut().expect("the new group owns its child");
    let mut stdout = child.take_stdout()?.expect("stdout was piped");
    let mut stderr = child.take_stderr()?.expect("stderr was piped");
    let mut out = Capture::default();
    let mut err = Capture::default();
    let deadline = timer.sleep(Duration::from_millis(request.timeout_ms));

    let timed_out = {
        let mut out_read = pin!(out.drain(&mut stdout, max_output));
        let mut err_read = pin!(err.drain(&mut stderr, max_output));
        let mut exited = pin!(group.child().wait_for_exit(timer));
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
        group.child().wait_for_exit(timer).await?;
    }
    let status = group.reap(timer).await?;
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

    fn child(&self) -> &Child {
        self.child.as_ref().expect("group has not been reaped")
    }

    fn kill(&self) {
        // SAFETY: The group ID is the PID of our still-unreaped child.
        // No successful wait() can occur while we hold this group.
        unsafe { libc::kill(-self.pid, libc::SIGKILL) };
    }

    async fn reap(mut self, timer: &Timer) -> io::Result<std::process::ExitStatus> {
        self.child
            .as_mut()
            .expect("group has not been reaped")
            .wait(timer)
            .await
            .inspect(|_| {
                self.child.take();
            })
    }
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.kill();
        }
    }
}

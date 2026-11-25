//! Linux shell comparison. Both engines drive one runtime thread and the same child workloads.
use std::env;
use std::io::{self, Error};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Instant;
use tokio::io::AsyncReadExt;

#[derive(Clone, Copy)]
enum Workload {
    Short,
    Output,
}

fn workload_spec(work: Workload, index: usize) -> (&'static str, &'static [u8], usize) {
    match work {
        Workload::Output => ("exec /usr/bin/head -c 131072 /dev/zero", &[0], 131072),
        Workload::Short => match index % 3 {
            0 => ("test -r /dev/null", b"", 0),
            1 => ("printf 'ok\\n'", b"ok\n", 3),
            _ => ("exec /usr/bin/printf '%s\\n' shell", b"shell\n", 6),
        },
    }
}

fn shell(script: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", script])
        .current_dir("/")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    cmd
}

// Check every byte for both engines, including short writes and fragmented pipe reads.
fn check_chunk(bytes: &[u8], expected: &[u8], at: usize, total: usize) -> io::Result<()> {
    if at + bytes.len() > total {
        return Err(Error::other("extra stdout"));
    }
    let correct = if expected.len() == 1 {
        bytes.iter().all(|&byte| byte == expected[0])
    } else {
        expected.get(at..at + bytes.len()) == Some(bytes)
    };
    if correct {
        Ok(())
    } else {
        Err(Error::other("wrong stdout"))
    }
}

fn finish(status: ExitStatus, actual: usize, total: usize) -> io::Result<()> {
    if status.success() && actual == total {
        Ok(())
    } else {
        Err(Error::other(format!(
            "exit {status}, received {actual}/{total} bytes"
        )))
    }
}

async fn zrun_child(
    work: Workload,
    index: usize,
    io: &zrun::io::Handle,
    timer: &zrun::Timer,
) -> io::Result<()> {
    let (script, expected, total) = workload_spec(work, index);
    let mut child = zrun::process::spawn(&mut shell(script), io)?;
    let mut stdout = child
        .take_stdout()?
        .ok_or_else(|| Error::other("no stdout"))?;
    let mut buf = [0u8; 8192];
    let mut at = 0;
    loop {
        let n = stdout.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        check_chunk(&buf[..n], expected, at, total)?;
        at += n;
    }
    finish(child.wait(timer).await?, at, total)
}

async fn tokio_child(work: Workload, index: usize) -> io::Result<()> {
    let (script, expected, total) = workload_spec(work, index);
    let mut child = tokio::process::Command::from(shell(script)).spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::other("no stdout"))?;
    let mut buf = [0u8; 8192];
    let mut at = 0;
    loop {
        let n = stdout.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        check_chunk(&buf[..n], expected, at, total)?;
        at += n;
    }
    finish(child.wait().await?, at, total)
}

fn zrun_sample(work: Workload, jobs: usize, lanes: usize) -> io::Result<[u128; 2]> {
    let mut start = Instant::now();
    let mut runtime = zrun::Runtime::new();
    let mut poller = zrun::io::Poller::new()?;
    let mut ns = [0; 2];
    for elapsed in &mut ns {
        let mut joins = Vec::with_capacity(lanes);
        for lane in 0..lanes {
            let io = poller.handle().clone();
            let timer = runtime.timer();
            joins.push(
                runtime
                    .spawn(async move {
                        for index in (lane..jobs).step_by(lanes) {
                            zrun_child(work, index, &io, &timer).await?;
                        }
                        Ok::<_, io::Error>(())
                    })
                    .map_err(|e| Error::other(e.to_string()))?,
            );
        }
        runtime.block_on_with_io(&mut poller, async {
            for join in joins {
                join.await.map_err(|e| Error::other(e.to_string()))??;
            }
            Ok::<_, io::Error>(())
        })??;
        *elapsed = start.elapsed().as_nanos();
        start = Instant::now();
    }
    Ok(ns)
}

fn tokio_sample(work: Workload, jobs: usize, lanes: usize) -> io::Result<[u128; 2]> {
    let mut start = Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;
    let mut ns = [0; 2];
    for elapsed in &mut ns {
        let mut joins = Vec::with_capacity(lanes);
        for lane in 0..lanes {
            joins.push(runtime.spawn(async move {
                for index in (lane..jobs).step_by(lanes) {
                    tokio_child(work, index).await?;
                }
                Ok::<_, io::Error>(())
            }));
        }
        runtime.block_on(async {
            for join in joins {
                join.await.map_err(|e| Error::other(e.to_string()))??;
            }
            Ok::<_, io::Error>(())
        })?;
        *elapsed = start.elapsed().as_nanos();
        start = Instant::now();
    }
    Ok(ns)
}

fn main() -> io::Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() != 4 {
        return Err(Error::other("ENGINE short|output JOBS LANES"));
    }
    let work = match args[1].as_str() {
        "short" => Workload::Short,
        "output" => Workload::Output,
        _ => return Err(Error::other("unknown workload")),
    };
    let jobs: usize = args[2].parse().map_err(Error::other)?;
    let lanes: usize = args[3].parse().map_err(Error::other)?;
    if lanes == 0 || lanes > jobs || lanes > 256 {
        return Err(Error::other("require 1..=min(jobs,256) lanes"));
    }
    let sample = match args[0].as_str() {
        "zrun" => zrun_sample(work, jobs, lanes)?,
        "tokio" => tokio_sample(work, jobs, lanes)?,
        _ => return Err(Error::other("unknown engine")),
    };
    println!("{} {}", sample[0], sample[1]);
    Ok(())
}

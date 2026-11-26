use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct Counters {
    pub(crate) started: u64,
    pub(crate) memory_hits: u64,
    pub(crate) valkey_hits: u64,
    pub(crate) coalesced: u64,
    pub(crate) queued: u64,
    pub(crate) active: u64,
}

#[derive(Deserialize, Serialize)]
struct Snapshot {
    start_ticks: u64,
    counters: Counters,
}

fn process_start(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    if matches!(fields.next()?, "Z" | "X") {
        return None;
    }
    // Field 22 follows state (field 3) and 18 intervening fields.
    fields.nth(18)?.parse().ok()
}

fn write_snapshot(path: &Path, pending: &Path, snapshot: &Snapshot) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(pending)?;
    let result = serde_json::to_writer(&mut file, snapshot)
        .map_err(io::Error::other)
        .and_then(|()| fs::rename(pending, path));
    if result.is_err() {
        let _ = fs::remove_file(pending);
    }
    result
}

pub(crate) struct Publisher {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Publisher {
    pub(crate) fn start(read: impl Fn() -> Counters + Send + 'static) -> io::Result<Self> {
        let directory = crate::mcp::runtime_directory()?;
        let pid = std::process::id();
        let start_ticks = process_start(pid)
            .ok_or_else(|| io::Error::other("missing broker process identity"))?;
        let path = directory.join(format!("stats-{pid}-{start_ticks}.json"));
        let pending = path.with_extension("tmp");
        let mut snapshot = Snapshot {
            start_ticks,
            counters: Counters::default(),
        };
        let (stop, receive) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("zrun-stats".into())
            .spawn(move || {
                let mut published = None;
                loop {
                    snapshot.counters = read();
                    if published != Some(snapshot.counters)
                        && write_snapshot(&path, &pending, &snapshot).is_ok()
                    {
                        published = Some(snapshot.counters);
                    }
                    if receive.recv_timeout(Duration::from_secs(1))
                        != Err(RecvTimeoutError::Timeout)
                    {
                        break;
                    }
                }
                let _ = fs::remove_file(path);
            })?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn print() -> io::Result<()> {
    let mut total = Counters::default();
    let mut brokers = 0;
    let directory: PathBuf = crate::mcp::runtime_directory()?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some((identity, extension)) = name
            .to_str()
            .and_then(|name| name.strip_prefix("stats-"))
            .and_then(|name| name.rsplit_once('.'))
        else {
            continue;
        };
        let Some((pid, start_ticks)) = identity
            .split_once('-')
            .and_then(|(pid, ticks)| Some((pid.parse::<u32>().ok()?, ticks.parse::<u64>().ok()?)))
        else {
            continue;
        };
        if !matches!(extension, "json" | "tmp") || !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if process_start(pid) != Some(start_ticks) {
            let _ = fs::remove_file(path);
            continue;
        }
        if extension != "json" {
            continue;
        }
        let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        else {
            continue;
        };
        let Ok(snapshot) = serde_json::from_reader::<_, Snapshot>(file.take(4096)) else {
            continue;
        };
        if snapshot.start_ticks != start_ticks {
            continue;
        }
        let counters = snapshot.counters;
        total.started = total.started.saturating_add(counters.started);
        total.memory_hits = total.memory_hits.saturating_add(counters.memory_hits);
        total.valkey_hits = total.valkey_hits.saturating_add(counters.valkey_hits);
        total.coalesced = total.coalesced.saturating_add(counters.coalesced);
        total.queued = total.queued.saturating_add(counters.queued);
        total.active = total.active.saturating_add(counters.active);
        brokers += 1;
    }
    let mut out = io::stdout().lock();
    serde_json::to_writer(
        &mut out,
        &serde_json::json!({
            "version": 1,
            "brokers": brokers,
            "avoided": total.memory_hits.saturating_add(total.valkey_hits).saturating_add(total.coalesced),
            "counters": total,
        }),
    )?;
    writeln!(out)
}

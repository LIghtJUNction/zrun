use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zrun::Timer;
use zrun::io::{Handle, Stream};

use crate::execution::{Request, ResultRecord};

const TIMEOUT: Duration = Duration::from_millis(50);
const RETRY_DELAY_MS: u64 = 1000;
const MAX_VALUE: usize = 800 * 1024;

pub(crate) struct Valkey {
    path: PathBuf,
    prefix: String,
    environment: Sha256,
    started: Instant,
    retry_at_ms: AtomicU64,
}

#[derive(Default)]
pub(crate) struct Connection(Option<Stream>);

#[derive(Serialize)]
struct Stored<'a> {
    expires_ms: u64,
    result: &'a ResultRecord,
}

#[derive(Deserialize)]
struct Received {
    expires_ms: u64,
    result: ResultRecord,
}

impl Valkey {
    pub(crate) fn from_env(shell: &Path) -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("ZRUN_VALKEY_SOCKET").map(PathBuf::from) else {
            return Ok(None);
        };
        let domain = std::env::var("ZRUN_VALKEY_DOMAIN").unwrap_or_default();
        let revision = std::env::var("ZRUN_VALKEY_TOOL_REVISION").unwrap_or_default();
        if !path.is_absolute()
            || domain.is_empty()
            || domain.len() > 64
            || !domain
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || revision.is_empty()
            || revision.len() > 256
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Valkey requires an absolute socket, a domain and a tool revision",
            ));
        }
        let mut environment = Sha256::new();
        for part in [
            env!("CARGO_PKG_VERSION").as_bytes(),
            shell.as_os_str().as_encoded_bytes(),
            revision.as_bytes(),
        ] {
            digest_part(&mut environment, part);
        }
        let mut vars: Vec<_> = std::env::vars_os().collect();
        vars.sort_unstable();
        for (name, value) in vars {
            digest_part(&mut environment, name.as_encoded_bytes());
            digest_part(&mut environment, value.as_encoded_bytes());
        }
        Ok(Some(Self {
            path,
            prefix: format!("zrun:v1:{domain}:"),
            environment,
            started: Instant::now(),
            retry_at_ms: AtomicU64::new(0),
        }))
    }

    fn elapsed_ms(&self) -> u64 {
        self.started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    fn available(&self) -> bool {
        self.elapsed_ms() >= self.retry_at_ms.load(Ordering::Relaxed)
    }

    fn defer_retries(&self) {
        self.retry_at_ms.fetch_max(
            self.elapsed_ms().saturating_add(RETRY_DELAY_MS),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn key(&self, request: &Request) -> Option<String> {
        request.reuse.as_ref().filter(|reuse| reuse.ttl_ms > 0)?;
        let mut hash = self.environment.clone();
        digest_part(&mut hash, &serde_json::to_vec(request).ok()?);
        Some(format!("{}{:x}", self.prefix, hash.finalize()))
    }

    pub(crate) async fn get(
        &self,
        connection: &mut Connection,
        io: &Handle,
        timer: &Timer,
        key: &str,
        ttl: u64,
    ) -> Option<(ResultRecord, Instant)> {
        connection
            .exchange(self, io, timer, async |socket| {
                socket
                    .write_all(&command(&[b"GET", key.as_bytes()]))
                    .await
                    .ok()?;
                let Some(size) = read_size(socket).await? else {
                    return Some(None);
                };
                let mut payload = vec![0; size + 2];
                read_exact(socket, &mut payload).await?;
                if payload[size..] != *b"\r\n" {
                    return None;
                }
                let hit: Received = serde_json::from_slice(&payload[..size]).ok()?;
                if !hit.result.cacheable()
                    || hit.result.stdout.len() > crate::execution::MAX_OUTPUT
                    || hit.result.stderr.len() > crate::execution::MAX_OUTPUT
                {
                    return None;
                }
                let remaining = hit.expires_ms.saturating_sub(now_ms()?).min(ttl);
                Some((remaining > 0).then(|| {
                    (
                        hit.result,
                        Instant::now() + Duration::from_millis(remaining),
                    )
                }))
            })
            .await
            .flatten()
    }

    pub(crate) async fn set(
        &self,
        connection: &mut Connection,
        io: &Handle,
        timer: &Timer,
        key: &str,
        ttl: u64,
        result: &ResultRecord,
    ) {
        if !self.available() {
            return;
        }
        let Some(expires_ms) = now_ms().and_then(|now| now.checked_add(ttl)) else {
            return;
        };
        let Ok(value) = serde_json::to_vec(&Stored { expires_ms, result }) else {
            return;
        };
        if value.len() > MAX_VALUE {
            return;
        }
        let expiry = ttl.to_string();
        connection
            .exchange(self, io, timer, async |socket| {
                socket
                    .write_all(&command(&[
                        b"SET",
                        key.as_bytes(),
                        &value,
                        b"PX",
                        expiry.as_bytes(),
                    ]))
                    .await
                    .ok()?;
                let mut reply = [0; 5];
                read_exact(socket, &mut reply).await?;
                (reply == *b"+OK\r\n").then_some(())
            })
            .await;
    }
}

impl Connection {
    async fn exchange<T>(
        &mut self,
        cache: &Valkey,
        io: &Handle,
        timer: &Timer,
        operation: impl AsyncFnOnce(&mut Stream) -> Option<T>,
    ) -> Option<T> {
        // Keep the socket only after a complete exchange. A timed-out or
        // cancelled operation can leave unread or partial RESP bytes.
        let previous = self.0.take();
        if !cache.available() {
            return None;
        }
        let response = timer
            .timeout_at(Instant::now() + TIMEOUT, async {
                let mut socket = match previous {
                    Some(socket) => socket,
                    None => io.connect_unix(&cache.path).await.ok()?,
                };
                let result = operation(&mut socket).await?;
                Some((socket, result))
            })
            .await;
        let Ok(Some((socket, result))) = response else {
            cache.defer_retries();
            return None;
        };
        self.0 = Some(socket);
        Some(result)
    }
}

fn now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

fn digest_part(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn command(args: &[&[u8]]) -> Vec<u8> {
    let mut message = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        message.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        message.extend_from_slice(arg);
        message.extend_from_slice(b"\r\n");
    }
    message
}

async fn read_size(socket: &mut Stream) -> Option<Option<usize>> {
    let mut prefix = [0; 1];
    read_exact(socket, &mut prefix).await?;
    if prefix != *b"$" {
        return None;
    }
    let mut header = [0; 8];
    for index in 0..header.len() {
        read_exact(socket, &mut header[index..=index]).await?;
        if header[index] == b'\n' {
            let text = std::str::from_utf8(header[..index].strip_suffix(b"\r")?).ok()?;
            return if text == "-1" {
                Some(None)
            } else {
                text.parse::<usize>()
                    .ok()
                    .filter(|&size| size <= MAX_VALUE)
                    .map(Some)
            };
        }
    }
    None
}

async fn read_exact(socket: &mut Stream, mut buf: &mut [u8]) -> Option<()> {
    while !buf.is_empty() {
        let count = socket.read(buf).await.ok()?;
        if count == 0 {
            return None;
        }
        buf = &mut buf[count..];
    }
    Some(())
}

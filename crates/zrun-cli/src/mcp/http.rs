use std::io::{self, IoSlice};
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use zrun::Timer;
use zrun::io::{Listener, Stream, UnixListener};

use super::reply::Reply;
use super::{MAX_MESSAGE, deadline, respond, supports_protocol};
use crate::execution::Broker;
use crate::executor::{Executor, Spawner};

const MAX_CONNECTIONS: usize = 2_048;
const MAX_HEADERS: usize = 16 * 1024;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) fn serve(
    shell: PathBuf,
    tcp: Option<SocketAddr>,
    unix: Option<PathBuf>,
) -> io::Result<()> {
    let (signals, mut shutdown) = Signals::new()?;
    let mut runtime = Executor::configured()?;
    let timer = runtime.timer();
    let broker = Arc::new(Broker::new(
        shell,
        runtime.io(),
        timer.clone(),
        &runtime.spawner(),
    )?);
    let spawner = runtime.spawner();
    let connections = Arc::new(AtomicUsize::new(0));
    let mut listeners = Vec::new();
    let mut socket_path = None;
    if let Some(addr) = tcp {
        if !addr.ip().is_loopback() {
            return Err(invalid("MCP HTTP must bind to a loopback address"));
        }
        let secret = std::env::var("ZRUN_MCP_TOKEN")
            .ok()
            .filter(|token| token.len() >= 32 && token.bytes().all(|byte| byte.is_ascii_graphic()))
            .ok_or_else(|| {
                invalid("set ZRUN_MCP_TOKEN to a secret of at least 32 printable characters")
            })?;
        let listener = runtime.io().bind(addr)?;
        let bound = listener.local_addr()?;
        listeners.push((
            Endpoint::Tcp(listener),
            Access {
                authorization: Some(format!("Bearer {secret}")),
                hosts: vec![bound.to_string(), format!("localhost:{}", bound.port())],
            },
        ));
        eprintln!("zrun: http://{bound}/mcp");
    }
    if let Some(path) = unix {
        validate_directory(&path)?;
        let listener = runtime.io().bind_unix(&path)?;
        socket_path = Some(SocketPath::new(path)?);
        listeners.push((
            Endpoint::Unix(listener),
            Access {
                authorization: None,
                hosts: vec!["localhost".to_owned(), "zrun".to_owned()],
            },
        ));
        eprintln!(
            "zrun: unix:{}",
            socket_path
                .as_ref()
                .expect("bound Unix path")
                .path
                .display()
        );
    }
    let mut tasks = Vec::with_capacity(listeners.len());
    for (endpoint, access) in listeners {
        tasks.push(
            spawner
                .spawn(accept(
                    endpoint,
                    Arc::new(access),
                    broker.clone(),
                    timer.clone(),
                    spawner.clone(),
                    connections.clone(),
                ))
                .map_err(io::Error::other)?,
        );
    }
    let result = runtime.block_on(std::future::poll_fn(|cx| {
        if std::pin::Pin::new(&mut shutdown).poll(cx).is_ready() {
            return std::task::Poll::Ready(Ok(()));
        }
        for task in &mut tasks {
            if let std::task::Poll::Ready(result) = std::pin::Pin::new(task).poll(cx) {
                return std::task::Poll::Ready(
                    result.map_err(io::Error::other).and_then(|result| result),
                );
            }
        }
        std::task::Poll::Pending
    }));
    runtime.shutdown();
    drop(socket_path);
    drop(signals);
    result?
}

use std::future::Future;

struct Signals(
    signal_hook::iterator::Handle,
    Option<std::thread::JoinHandle<()>>,
);

impl Signals {
    fn new() -> io::Result<(Self, zrun::sync::Receiver<()>)> {
        let mut signals = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
        ])?;
        let handle = signals.handle();
        let (send, receive) = zrun::sync::channel();
        let thread = std::thread::Builder::new()
            .name("zrun-signals".into())
            .spawn(move || {
                if signals.forever().next().is_some() {
                    let _ = send.send(());
                }
            })?;
        Ok((Self(handle, Some(thread)), receive))
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        self.0.close();
        if let Some(thread) = self.1.take() {
            let _ = thread.join();
        }
    }
}

enum Endpoint {
    Tcp(Listener),
    Unix(UnixListener),
}

impl Endpoint {
    async fn accept(&mut self) -> io::Result<Stream> {
        match self {
            Self::Tcp(listener) => listener.accept().await.map(|(stream, _)| stream),
            Self::Unix(listener) => listener.accept().await,
        }
    }
}

struct Access {
    authorization: Option<String>,
    hosts: Vec<String>,
}

struct Connection(Arc<AtomicUsize>);

impl Connection {
    fn acquire(connections: &Arc<AtomicUsize>) -> Option<Self> {
        connections
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < MAX_CONNECTIONS).then_some(count + 1)
            })
            .ok()
            .map(|_| Self(connections.clone()))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn accept(
    mut endpoint: Endpoint,
    access: Arc<Access>,
    broker: Arc<Broker>,
    timer: Timer,
    spawner: Spawner,
    connections: Arc<AtomicUsize>,
) -> io::Result<()> {
    loop {
        let mut stream = endpoint.accept().await?;
        if let Some(slot) = Connection::acquire(&connections) {
            let (access, broker, timer) = (access.clone(), broker.clone(), timer.clone());
            spawner
                .spawn(async move {
                    let _slot = slot;
                    let _ = handle(&mut stream, &access, &broker, &timer).await;
                })
                .map_err(io::Error::other)?;
        }
        zrun::yield_now().await;
    }
}

enum Request {
    Post(Vec<u8>),
    Reply(Status),
}

#[derive(Clone, Copy)]
enum Status {
    Ok,
    Accepted,
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    MethodNotAllowed,
    TooLarge,
    UnsupportedMedia,
}

impl Status {
    fn line(self) -> &'static str {
        match self {
            Self::Ok => "200 OK",
            Self::Accepted => "202 Accepted",
            Self::BadRequest => "400 Bad Request",
            Self::Unauthorized => "401 Unauthorized",
            Self::Forbidden => "403 Forbidden",
            Self::NotFound => "404 Not Found",
            Self::MethodNotAllowed => "405 Method Not Allowed",
            Self::TooLarge => "413 Content Too Large",
            Self::UnsupportedMedia => "415 Unsupported Media Type",
        }
    }

    fn extra(self) -> &'static str {
        match self {
            Self::Unauthorized => "WWW-Authenticate: Bearer\r\n",
            Self::MethodNotAllowed => "Allow: POST\r\n",
            _ => "",
        }
    }
}

async fn handle(
    stream: &mut Stream,
    access: &Access,
    broker: &Broker,
    timer: &Timer,
) -> io::Result<()> {
    let request = deadline(read_request(stream, access), timer, CLIENT_IO_TIMEOUT).await??;
    let rejected = matches!(&request, Request::Reply(_));
    let (status, body) = match request {
        Request::Reply(status) => (status, None),
        Request::Post(request) => {
            let message = serde_json::from_slice(&request);
            drop(request);
            match respond(broker, message).await? {
                Some(reply) => (Status::Ok, Some(reply)),
                None => (Status::Accepted, None),
            }
        }
    };
    deadline(
        write_reply(stream, status, body.as_ref()),
        timer,
        CLIENT_IO_TIMEOUT,
    )
    .await??;
    if rejected {
        // Send FIN first. Closing a socket with unread request bytes could send
        // RST and hide an authentication or framing response from the caller.
        stream.shutdown_write()?;
        let _ = deadline(drain_rejected(stream), timer, Duration::from_millis(250)).await;
    }
    Ok(())
}

async fn drain_rejected(stream: &mut Stream) -> io::Result<()> {
    let mut scratch = Vec::new();
    let mut budget = MAX_MESSAGE;
    while budget > 0 {
        let count = stream.read_append(&mut scratch, budget).await?;
        if count == 0 {
            break;
        }
        scratch.clear();
        budget -= count;
        zrun::yield_now().await;
    }
    Ok(())
}

struct Headers {
    bytes: Vec<u8>,
    first_body: usize,
}

async fn read_headers(stream: &mut Stream) -> io::Result<Result<Headers, Status>> {
    let mut bytes = Vec::new();
    loop {
        let start = bytes.len().saturating_sub(3);
        let count = stream.read_append(&mut bytes, 2_048).await?;
        if count == 0 {
            return Ok(Err(Status::BadRequest));
        }
        if let Some(offset) = bytes[start..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            let first_body = start + offset + 4;
            if first_body > MAX_HEADERS {
                return Ok(Err(Status::TooLarge));
            }
            return Ok(Ok(Headers { bytes, first_body }));
        }
        if bytes.len() >= MAX_HEADERS {
            return Ok(Err(Status::TooLarge));
        }
    }
}

async fn read_request(stream: &mut Stream, access: &Access) -> io::Result<Request> {
    let Headers {
        mut bytes,
        first_body,
    } = match read_headers(stream).await? {
        Ok(headers) => headers,
        Err(status) => return Ok(Request::Reply(status)),
    };
    let framing = match parse_headers(&bytes[..first_body], access) {
        Ok(framing) => framing,
        Err(status) => return Ok(Request::Reply(status)),
    };
    let length = match framing {
        Framing::Length(length) => length,
        Framing::Chunked => return read_chunked(stream, bytes.split_off(first_body)).await,
    };
    let total = first_body + length;
    // A connection carries one request. Refuse extra bytes already received.
    if bytes.len() > total {
        return Ok(Request::Reply(Status::BadRequest));
    }
    let mut filled = bytes.len();
    bytes.resize(total, 0);
    while filled < total {
        let count = stream.read(&mut bytes[filled..]).await?;
        if count == 0 {
            return Ok(Request::Reply(Status::BadRequest));
        }
        filled += count;
    }
    Ok(Request::Post(bytes.split_off(first_body)))
}

enum Framing {
    Length(usize),
    Chunked,
}

async fn read_chunked(stream: &mut Stream, mut wire: Vec<u8>) -> io::Result<Request> {
    let mut body = Vec::new();
    let mut offset = 0;
    loop {
        let (header, amount) = match httparse::parse_chunk_size(&wire[offset..]) {
            Ok(httparse::Status::Complete((header, size))) => {
                let Ok(size) = usize::try_from(size) else {
                    return Ok(Request::Reply(Status::TooLarge));
                };
                if size > MAX_MESSAGE - body.len() {
                    return Ok(Request::Reply(Status::TooLarge));
                }
                (header, size)
            }
            Ok(httparse::Status::Partial) => {
                if !more_chunk_bytes(stream, &mut wire).await? {
                    return Ok(Request::Reply(Status::BadRequest));
                }
                continue;
            }
            Err(_) => return Ok(Request::Reply(Status::BadRequest)),
        };
        offset += header;
        if amount == 0 {
            loop {
                let remaining = &wire[offset..];
                if remaining == b"\r\n" {
                    return Ok(Request::Post(body));
                }
                if remaining.starts_with(b"\r\n") {
                    return Ok(Request::Reply(Status::BadRequest));
                }
                match has_complete_trailers(remaining) {
                    Ok(true) => return Ok(Request::Post(body)),
                    Ok(false) => {}
                    Err(()) => return Ok(Request::Reply(Status::BadRequest)),
                }
                if !more_chunk_bytes(stream, &mut wire).await? {
                    return Ok(Request::Reply(Status::BadRequest));
                }
            }
        }
        let end = offset + amount;
        while wire.len() < end + 2 {
            if !more_chunk_bytes(stream, &mut wire).await? {
                return Ok(Request::Reply(Status::BadRequest));
            }
        }
        if &wire[end..end + 2] != b"\r\n" {
            return Ok(Request::Reply(Status::BadRequest));
        }
        body.extend_from_slice(&wire[offset..end]);
        offset = end + 2;
    }
}

fn has_complete_trailers(bytes: &[u8]) -> Result<bool, ()> {
    let mut trailers = [httparse::EMPTY_HEADER; 32];
    match httparse::parse_headers(bytes, &mut trailers) {
        Ok(httparse::Status::Complete((used, _))) if used == bytes.len() => Ok(true),
        Ok(httparse::Status::Partial) => Ok(false),
        _ => Err(()),
    }
}

async fn more_chunk_bytes(stream: &mut Stream, wire: &mut Vec<u8>) -> io::Result<bool> {
    let available = MAX_MESSAGE + MAX_HEADERS - wire.len();
    Ok(stream.read_append(wire, available).await? != 0)
}

fn parse_headers(bytes: &[u8], access: &Access) -> Result<Framing, Status> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    if !matches!(request.parse(bytes), Ok(httparse::Status::Complete(_))) {
        return Err(Status::BadRequest);
    }
    if request.path != Some("/mcp") {
        return Err(Status::NotFound);
    }
    let get = |name: &str| -> Result<Option<&str>, Status> {
        let mut fields = request
            .headers
            .iter()
            .filter(|header| header.name.eq_ignore_ascii_case(name));
        let value = fields
            .next()
            .map(|header| std::str::from_utf8(header.value).map(str::trim))
            .transpose()
            .map_err(|_| Status::BadRequest)?;
        if fields.next().is_some() {
            return Err(Status::BadRequest);
        }
        Ok(value)
    };
    let host = get("Host")?.ok_or(Status::BadRequest)?;
    if !access
        .hosts
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed))
        || get("Origin")?.is_some()
    {
        return Err(Status::Forbidden);
    }
    if let Some(secret) = &access.authorization
        && !get("Authorization")?
            .is_some_and(|token| same_secret(token.as_bytes(), secret.as_bytes()))
    {
        return Err(Status::Unauthorized);
    }
    if request.method != Some("POST") {
        return Err(Status::MethodNotAllowed);
    }
    if get("MCP-Protocol-Version")?.is_some_and(|version| !supports_protocol(version)) {
        return Err(Status::BadRequest);
    }
    if get("Content-Type")?
        .and_then(|value| value.split(';').next())
        .is_none_or(|media| !media.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(Status::UnsupportedMedia);
    }
    let transfer = get("Transfer-Encoding")?;
    let length_header = get("Content-Length")?;
    if let Some(transfer) = transfer {
        if length_header.is_none() && transfer.eq_ignore_ascii_case("chunked") {
            return Ok(Framing::Chunked);
        }
        return Err(Status::BadRequest);
    }
    let length = length_header
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or(Status::BadRequest)?;
    if length > MAX_MESSAGE {
        return Err(Status::TooLarge);
    }
    Ok(Framing::Length(length))
}

fn same_secret(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0, |difference, (a, b)| difference | (a ^ b))
            == 0
}

async fn write_reply(stream: &mut Stream, status: Status, body: Option<&Reply>) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\n",
        status.line(),
        body.map_or(0, Reply::len),
        status.extra(),
    );
    let Some(reply) = body else {
        return stream.write_all(header.as_bytes()).await;
    };
    let mut chunks = std::iter::once(header.as_bytes())
        .chain(reply.chunks())
        .map(IoSlice::new)
        .collect::<Vec<_>>();
    stream.write_all_vectored(&mut chunks).await
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub(super) fn default_socket_path() -> io::Result<PathBuf> {
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            invalid("set XDG_RUNTIME_DIR to an absolute private directory or pass --unix PATH")
        })?;
    check_private_directory(&root)?;
    let directory = root.join("zrun");
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if directory.symlink_metadata()?.is_symlink() {
        return Err(invalid(
            "default Unix socket directory must be a directory, not a symlink",
        ));
    }
    check_private_directory(&directory)?;
    Ok(directory.join("mcp.sock"))
}

fn validate_directory(path: &Path) -> io::Result<()> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    check_private_directory(directory)
}

fn check_private_directory(path: &Path) -> io::Result<()> {
    let metadata = path.metadata()?;
    // SAFETY: geteuid has no arguments and does not mutate process state.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(invalid(
            "Unix socket directory must belong to the user and have mode 0700",
        ));
    }
    Ok(())
}

struct SocketPath {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl SocketPath {
    fn new(path: PathBuf) -> io::Result<Self> {
        let metadata = path.symlink_metadata()?;
        let owned = Self {
            path,
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        std::fs::set_permissions(&owned.path, std::fs::Permissions::from_mode(0o600))?;
        Ok(owned)
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        if self
            .path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.dev() == self.dev && metadata.ino() == self.ino)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(super) async fn post(
    io: &zrun::io::Handle,
    path: &Path,
    message: &[u8],
    timer: &Timer,
) -> io::Result<Option<Vec<u8>>> {
    if message.len() > MAX_MESSAGE {
        return Err(invalid("client MCP message exceeds the size limit"));
    }
    validate_directory(path)?;
    let mut stream = deadline(io.connect_unix(path), timer, CLIENT_IO_TIMEOUT).await??;
    let header = format!(
        "POST /mcp HTTP/1.1\r\nHost: zrun\r\nConnection: close\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\n\r\n",
        message.len()
    );
    deadline(
        async {
            stream.write_all(header.as_bytes()).await?;
            stream.write_all(message).await
        },
        timer,
        CLIENT_IO_TIMEOUT,
    )
    .await??;
    let Headers {
        mut bytes,
        first_body,
    } = deadline(read_headers(&mut stream), timer, Duration::from_secs(7_200))
        .await??
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid MCP response headers"))?;
    let length = {
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut response = httparse::Response::new(&mut headers);
        if !matches!(
            response.parse(&bytes[..first_body]),
            Ok(httparse::Status::Complete(_))
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid MCP response",
            ));
        }
        if response.code == Some(202) {
            return Ok(None);
        }
        if response.code != Some(200) {
            return Err(io::Error::other(format!(
                "MCP returned HTTP {}",
                response.code.unwrap_or(0)
            )));
        }
        let mut lengths = response
            .headers
            .iter()
            .filter(|header| header.name.eq_ignore_ascii_case("Content-Length"));
        let length = lengths
            .next()
            .and_then(|header| std::str::from_utf8(header.value).ok())
            .and_then(|text| text.parse::<usize>().ok())
            .filter(|length| *length <= 16 * 1024 * 1024)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid or oversized MCP response body",
                )
            })?;
        if lengths.next().is_some()
            || response
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("Transfer-Encoding"))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ambiguous MCP response framing",
            ));
        }
        length
    };
    let mut filled = bytes.len();
    let total = first_body + length;
    if filled > total {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "extra data after MCP response",
        ));
    }
    bytes.resize(total, 0);
    deadline(
        async {
            while filled < total {
                let count = stream.read(&mut bytes[filled..]).await?;
                if count == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                filled += count;
            }
            Ok(Some(bytes.split_off(first_body)))
        },
        timer,
        CLIENT_IO_TIMEOUT,
    )
    .await?
}

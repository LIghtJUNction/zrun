//! Readiness-driven sockets and Unix file descriptors for Rust futures.
//!
//! Use a dedicated reactor thread or let an executor drive the poller. I/O and
//! application futures run on the caller's executor. IDs are never reused within
//! a reactor, so events already returned by the kernel cannot target a later fd.

use std::collections::HashMap;
use std::fmt;
use std::future::poll_fn;
use std::io::{self, IoSlice, Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mio::event::Source;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Registry, Token};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{AsyncFd, FdInterest, Nonblocking, UnixListener};

const SHUTDOWN: Token = Token(0);

#[derive(Default)]
struct Waiters {
    closed: bool,
    read: Option<Waker>,
    write: Option<Waker>,
}

impl Waiters {
    fn slot(&mut self, direction: Direction) -> &mut Option<Waker> {
        match direction {
            Direction::Read => &mut self.read,
            Direction::Write => &mut self.write,
        }
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Read,
    Write,
}

struct Registrations {
    closed: bool,
    next_token: usize,
    waiters: HashMap<Token, Arc<Mutex<Waiters>>>,
}

struct Shared {
    registry: Registry,
    stop: mio::Waker,
    registrations: Mutex<Registrations>,
}

/// Owns the thread that delivers readiness notifications.
pub struct Reactor {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for Reactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reactor").finish_non_exhaustive()
    }
}

impl Reactor {
    /// Start a readiness driver with no registered descriptors.
    ///
    /// # Errors
    ///
    /// Returns an OS error if the poller or its thread cannot be created.
    pub fn new() -> io::Result<Self> {
        let (mut poll, handle) = open_poller()?;
        let shared = handle.0.clone();
        let thread = thread::Builder::new()
            .name("zrun-io".into())
            .spawn(move || {
                dispatch(&mut poll, &shared);
                close_waiters(&shared);
            })?;
        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }

    /// Borrow the handle used to register descriptors with this driver.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        let _ = self.handle.0.stop.wake();
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != thread::current().id()
        {
            let _ = thread.join();
        }
    }
}

fn open_poller() -> io::Result<(mio::Poll, Handle)> {
    let poll = mio::Poll::new()?;
    let shared = Arc::new(Shared {
        registry: poll.registry().try_clone()?,
        stop: mio::Waker::new(poll.registry(), SHUTDOWN)?,
        registrations: Mutex::new(Registrations {
            closed: false,
            next_token: 1,
            waiters: HashMap::new(),
        }),
    });
    Ok((poll, Handle(shared)))
}

/// Kernel poller that is driven directly by its caller.
#[derive(Debug)]
pub struct Poller {
    poll: mio::Poll,
    events: Events,
    handle: Handle,
}

impl Poller {
    /// Create a poller without spawning a background thread.
    ///
    /// # Errors
    ///
    /// Returns an OS error if the poller or notifier cannot be created.
    pub fn new() -> io::Result<Self> {
        let (poll, handle) = open_poller()?;
        Ok(Self {
            poll,
            events: Events::with_capacity(256),
            handle,
        })
    }

    /// Borrow the handle used to register sockets and descriptors.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Create a notifier that interrupts a current or upcoming poll.
    pub fn notifier(&self) -> PollNotifier {
        PollNotifier(self.handle.0.clone())
    }

    /// Dispatch one kernel event batch, waiting up to the supplied timeout.
    ///
    /// # Errors
    ///
    /// Returns an OS error if the kernel poll fails. An interrupt returns early
    /// so the caller can recompute work and deadlines before polling again.
    pub fn poll(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        match self.poll.poll(&mut self.events, timeout) {
            Ok(()) => {
                dispatch_events(&self.events, &self.handle.0);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        close_waiters(&self.handle.0);
    }
}

/// A cross-thread wake source for a caller-driven poller.
#[derive(Clone)]
pub struct PollNotifier(Arc<Shared>);

impl fmt::Debug for PollNotifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PollNotifier").finish_non_exhaustive()
    }
}

impl PollNotifier {
    /// Interrupt the poller.
    ///
    /// # Errors
    ///
    /// Returns an OS error if the notification cannot be sent.
    pub fn wake(&self) -> io::Result<()> {
        self.0.stop.wake()
    }
}

/// Registers sockets with its owning reactor.
#[derive(Clone)]
pub struct Handle(Arc<Shared>);

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle").finish_non_exhaustive()
    }
}

impl Handle {
    /// Bind an async listener on the supplied address.
    ///
    /// # Errors
    ///
    /// Returns an OS error on bind or registration failure.
    pub fn bind(&self, addr: SocketAddr) -> io::Result<Listener> {
        let socket = TcpListener::bind(addr)?;
        let inner = Registered::new(self, socket, Interest::READABLE)?;
        Ok(Listener(inner))
    }

    /// Connect to an address and wait until the socket reports completion.
    ///
    /// # Errors
    ///
    /// Returns the connection error reported by the OS.
    pub async fn connect(&self, addr: SocketAddr) -> io::Result<Stream> {
        let socket = Socket::Tcp(TcpStream::connect(addr)?);
        Stream::connect(self, socket).await
    }
}

/// A TCP listener associated with a reactor.
#[derive(Debug)]
pub struct Listener(Registered<TcpListener>);

impl Listener {
    /// Return the address selected by the OS, including an ephemeral port.
    ///
    /// # Errors
    ///
    /// Returns an OS error if the address cannot be queried.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.source.local_addr()
    }

    /// Accept the next connection and register its stream with the same reactor.
    ///
    /// # Errors
    ///
    /// Returns an OS error on accept or registration failure.
    pub async fn accept(&mut self) -> io::Result<(Stream, SocketAddr)> {
        let (socket, addr) = self
            .0
            .ready(Direction::Read, |socket| socket.accept())
            .await?;
        Ok((Stream::register(&self.0.handle, Socket::Tcp(socket))?, addr))
    }
}

/// A readiness-driven TCP or Unix domain stream.
#[derive(Debug)]
pub struct Stream(Registered<Socket>);

impl Stream {
    fn register(handle: &Handle, socket: Socket) -> io::Result<Self> {
        Registered::new(handle, socket, Interest::READABLE | Interest::WRITABLE).map(Self)
    }

    async fn connect(handle: &Handle, socket: Socket) -> io::Result<Self> {
        let mut stream = Self::register(handle, socket)?;
        stream.0.ready(Direction::Write, Socket::connected).await?;
        Ok(stream)
    }

    /// Read available bytes, waiting if necessary. Zero indicates end of stream.
    ///
    /// # Errors
    ///
    /// Returns an OS or reactor shutdown error.
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0
            .ready(Direction::Read, |socket| socket.read(buf))
            .await
    }

    /// Append a bounded read to a byte buffer, returning the number read.
    ///
    /// Scratch space belongs to the active poll, so an idle connection retains
    /// only bytes already read. An empty limit performs no I/O.
    ///
    /// # Errors
    ///
    /// Returns an OS or reactor shutdown error.
    pub async fn read_append(&mut self, output: &mut Vec<u8>, limit: usize) -> io::Result<usize> {
        self.read_chunk(limit, |bytes| {
            output.extend_from_slice(bytes);
            bytes.len()
        })
        .await
    }

    /// Read up to 2 KiB and pass the bytes to a callback.
    ///
    /// The scratch buffer exists only during the poll that reads from the
    /// socket. An empty slice indicates end of stream or a zero limit.
    ///
    /// # Errors
    ///
    /// Returns an OS or reactor shutdown error.
    pub async fn read_chunk<T>(
        &mut self,
        limit: usize,
        mut consume: impl FnMut(&[u8]) -> T,
    ) -> io::Result<T> {
        if limit == 0 {
            return Ok(consume(&[]));
        }
        self.0
            .ready(Direction::Read, |socket| {
                let mut scratch = [0; 2048];
                let count = socket.read(&mut scratch[..limit.min(2048)])?;
                Ok(consume(&scratch[..count]))
            })
            .await
    }

    /// Enable or disable TCP_NODELAY for a TCP stream.
    ///
    /// # Errors
    ///
    /// Returns an OS error or Unsupported for a Unix domain stream.
    pub fn set_nodelay(&self, enabled: bool) -> io::Result<()> {
        match &self.0.source {
            Socket::Tcp(socket) => socket.set_nodelay(enabled),
            #[cfg(unix)]
            Socket::Unix(_) => Err(io::ErrorKind::Unsupported.into()),
        }
    }

    /// Finish sending bytes while keeping the receive half open.
    ///
    /// # Errors
    ///
    /// Returns a socket error if the write half cannot be shut down.
    pub fn shutdown_write(&self) -> io::Result<()> {
        match &self.0.source {
            Socket::Tcp(socket) => socket.shutdown(Shutdown::Write),
            #[cfg(unix)]
            Socket::Unix(socket) => socket.shutdown(Shutdown::Write),
        }
    }

    /// Write all bytes, waiting for write readiness when the peer applies backpressure.
    ///
    /// # Errors
    ///
    /// Returns an OS or reactor shutdown error. A prefix may have been written.
    pub async fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let written = self
                .0
                .ready(Direction::Write, |socket| socket.write(buf))
                .await?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            buf = &buf[written..];
        }
        Ok(())
    }

    /// Write the supplied slices in order, waiting for socket readiness as needed.
    ///
    /// The slice descriptors are advanced as bytes are written.
    ///
    /// # Errors
    ///
    /// Returns an OS or reactor shutdown error. A prefix may have been written.
    pub async fn write_all_vectored(&mut self, mut buf: &mut [IoSlice<'_>]) -> io::Result<()> {
        IoSlice::advance_slices(&mut buf, 0);
        while !buf.is_empty() {
            let written = self
                .0
                .ready(Direction::Write, |socket| socket.write_vectored(buf))
                .await?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            IoSlice::advance_slices(&mut buf, written);
        }
        Ok(())
    }
}

enum Socket {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(mio::net::UnixStream),
}

impl Socket {
    fn connected(&mut self) -> io::Result<()> {
        if let Some(error) = self.take_error()? {
            return Err(error);
        }
        let result = match self {
            Self::Tcp(socket) => socket.peer_addr().map(drop),
            #[cfg(unix)]
            Self::Unix(socket) => socket.peer_addr().map(drop),
        };
        match result {
            Err(error) if error.kind() == io::ErrorKind::NotConnected => {
                Err(io::ErrorKind::WouldBlock.into())
            }
            other => other,
        }
    }

    fn take_error(&self) -> io::Result<Option<io::Error>> {
        match self {
            Self::Tcp(socket) => socket.take_error(),
            #[cfg(unix)]
            Self::Unix(socket) => socket.take_error(),
        }
    }

    fn source(&mut self) -> &mut dyn Source {
        match self {
            Self::Tcp(socket) => socket,
            #[cfg(unix)]
            Self::Unix(socket) => socket,
        }
    }
}

impl Source for Socket {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        self.source().register(registry, token, interest)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        self.source().reregister(registry, token, interest)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        self.source().deregister(registry)
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(socket) => socket.read(buf),
            #[cfg(unix)]
            Self::Unix(socket) => socket.read(buf),
        }
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(socket) => socket.write(buf),
            #[cfg(unix)]
            Self::Unix(socket) => socket.write(buf),
        }
    }

    fn write_vectored(&mut self, buffers: &[IoSlice<'_>]) -> io::Result<usize> {
        match self {
            Self::Tcp(socket) => socket.write_vectored(buffers),
            #[cfg(unix)]
            Self::Unix(socket) => socket.write_vectored(buffers),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Armed<'a> {
    waiters: &'a Mutex<Waiters>,
    direction: Direction,
}

impl Drop for Armed<'_> {
    fn drop(&mut self) {
        let old = self
            .waiters
            .lock()
            .expect("readiness waiters poisoned")
            .slot(self.direction)
            .take();
        drop(old);
    }
}

struct Registered<T: Source> {
    source: T,
    token: Token,
    handle: Handle,
    waiters: Arc<Mutex<Waiters>>,
}

impl<T: Source> fmt::Debug for Registered<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registered")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl<T: Source> Registered<T> {
    fn new(handle: &Handle, mut source: T, interest: Interest) -> io::Result<Self> {
        let shared = &handle.0;
        let mut registrations = shared.registrations.lock().expect("registrations poisoned");
        if registrations.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let token = Token(registrations.next_token);
        registrations.next_token = token
            .0
            .checked_add(1)
            .ok_or_else(|| io::Error::other("reactor registration identifiers exhausted"))?;
        let waiters = Arc::new(Mutex::new(Waiters::default()));
        // Keep the map locked until the entry is installed. The poller may
        // receive initial readiness immediately after the register call.
        shared.registry.register(&mut source, token, interest)?;
        registrations.waiters.insert(token, waiters.clone());
        Ok(Self {
            source,
            token,
            handle: handle.clone(),
            waiters,
        })
    }

    async fn ready<U>(
        &mut self,
        direction: Direction,
        mut operation: impl FnMut(&mut T) -> io::Result<U>,
    ) -> io::Result<U> {
        let _armed = Armed {
            waiters: &self.waiters,
            direction,
        };
        poll_fn(|cx| {
            {
                let incoming = cx.waker().clone();
                let old = {
                    let mut waiters = self.waiters.lock().expect("readiness waiters poisoned");
                    if waiters.closed {
                        return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
                    }
                    waiters.slot(direction).replace(incoming)
                };
                drop(old);
            }

            // Arm before attempting the operation. Readiness arriving between
            // WouldBlock and Pending will have a waker to notify.
            loop {
                match operation(&mut self.source) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        return Poll::Pending;
                    }
                    result => return Poll::Ready(result),
                }
            }
        })
        .await
    }
}

impl<T: Source> Drop for Registered<T> {
    fn drop(&mut self) {
        let shared = &self.handle.0;
        let _ = shared.registry.deregister(&mut self.source);
        shared
            .registrations
            .lock()
            .expect("registrations poisoned")
            .waiters
            .remove(&self.token);
    }
}

fn dispatch(poll: &mut mio::Poll, shared: &Shared) {
    let mut events = Events::with_capacity(256);
    loop {
        match poll.poll(&mut events, None) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
        if dispatch_events(&events, shared) {
            return;
        }
    }
}

fn dispatch_events(events: &Events, shared: &Shared) -> bool {
    let mut notified = false;
    for event in events {
        if event.token() == SHUTDOWN {
            notified = true;
            continue;
        }
        let waiters = shared
            .registrations
            .lock()
            .expect("registrations poisoned")
            .waiters
            .get(&event.token())
            .cloned();
        if let Some(waiters) = waiters {
            let (read, write) = {
                let mut waiters = waiters.lock().expect("readiness waiters poisoned");
                let read = event.is_readable() || event.is_read_closed() || event.is_error();
                let write = event.is_writable() || event.is_write_closed() || event.is_error();
                (
                    read.then(|| waiters.read.take()).flatten(),
                    write.then(|| waiters.write.take()).flatten(),
                )
            };
            wake_waiters(read, write);
        }
    }
    notified
}

fn close_waiters(shared: &Shared) {
    let registered = {
        let mut registrations = shared.registrations.lock().expect("registrations poisoned");
        registrations.closed = true;
        std::mem::take(&mut registrations.waiters)
    };
    for waiters in registered.into_values() {
        let (read, write) = {
            let mut waiters = waiters.lock().expect("readiness waiters poisoned");
            waiters.closed = true;
            (waiters.read.take(), waiters.write.take())
        };
        wake_waiters(read, write);
    }
}

fn wake_waiters(read: Option<Waker>, write: Option<Waker>) {
    if let Some(waker) = read {
        waker.wake();
    }
    if let Some(waker) = write {
        waker.wake();
    }
}

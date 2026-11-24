use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

use mio::event::Source;
use mio::net::{UnixListener as MioUnixListener, UnixStream};
use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};

use crate::{Direction, Handle, Registered, Socket, Stream};

impl Handle {
    /// Bind an async Unix domain listener at an unused filesystem path.
    ///
    /// The caller owns the socket path and is responsible for permissions and
    /// removal after shutdown. Existing filesystem entries are left alone.
    ///
    /// # Errors
    ///
    /// Returns an OS error on bind or registration failure.
    pub fn bind_unix(&self, path: &Path) -> io::Result<UnixListener> {
        let socket = MioUnixListener::bind(path)?;
        Registered::new(self, socket, Interest::READABLE).map(UnixListener)
    }

    /// Connect to a Unix domain socket, waiting for connection completion.
    ///
    /// # Errors
    ///
    /// Returns the connection error reported by the OS.
    pub async fn connect_unix(&self, path: &Path) -> io::Result<Stream> {
        let socket = Socket::Unix(UnixStream::connect(path)?);
        Stream::connect(self, socket).await
    }
}

/// A Unix domain listener associated with a reactor.
#[derive(Debug)]
pub struct UnixListener(Registered<MioUnixListener>);

impl UnixListener {
    /// Accept the next connection and register its stream with the same reactor.
    ///
    /// # Errors
    ///
    /// Returns an OS error on accept or registration failure.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        let (socket, _) = self
            .0
            .ready(Direction::Read, |listener| listener.accept())
            .await?;
        Stream::register(&self.0.handle, Socket::Unix(socket))
    }
}

/// Declares how a descriptor obtains nonblocking I/O semantics.
#[derive(Clone, Copy, Debug)]
pub enum Nonblocking {
    /// Set O_NONBLOCK while preserving other status flags.
    Set,
    /// Register a descriptor used only for readiness, without changing its flags.
    ReadinessOnly,
}

/// Readiness the reactor should deliver for an owned Unix descriptor.
#[derive(Clone, Copy, Debug)]
pub enum FdInterest {
    /// Read readiness, for example a child's stdout pipe.
    Readable,
    /// Write readiness, for example a child's stdin pipe.
    Writable,
    /// Both directions, for example a PTY master.
    ReadWrite,
}

impl FdInterest {
    fn interest(self) -> Interest {
        match self {
            Self::Readable => Interest::READABLE,
            Self::Writable => Interest::WRITABLE,
            Self::ReadWrite => Interest::READABLE | Interest::WRITABLE,
        }
    }
}

/// Owns a Unix descriptor registered with a reactor.
#[derive(Debug)]
pub struct AsyncFd<T: AsRawFd> {
    registered: Registered<Descriptor<T>>,
    interest: Interest,
}

impl<T: AsRawFd> AsyncFd<T> {
    /// Take ownership of a descriptor and register it for read readiness.
    ///
    /// # Errors
    ///
    /// Returns an OS error if the flags cannot be set or registration fails.
    pub fn new(handle: &Handle, fd: T, mode: Nonblocking) -> io::Result<Self> {
        Self::with_interest(handle, fd, mode, FdInterest::Readable)
    }

    /// Take ownership of a descriptor with the specified readiness interest.
    ///
    /// # Errors
    /// Returns an OS error if the flags cannot be set or registration fails.
    pub fn with_interest(
        handle: &Handle,
        fd: T,
        mode: Nonblocking,
        interest: FdInterest,
    ) -> io::Result<Self> {
        if matches!(mode, Nonblocking::Set) {
            set_nonblocking(fd.as_raw_fd())?;
        }
        let interest = interest.interest();
        let registered = Registered::new(handle, Descriptor(fd), interest)?;
        Ok(Self {
            registered,
            interest,
        })
    }

    /// Retry a nonblocking operation whenever the descriptor becomes readable.
    ///
    /// Return WouldBlock to wait for another event. An operation may run on the
    /// first poll before an event is received.
    ///
    /// # Errors
    ///
    /// Returns an operation error or reactor shutdown error.
    pub async fn readable<U>(
        &mut self,
        mut operation: impl FnMut(&mut T) -> io::Result<U>,
    ) -> io::Result<U> {
        if !self.interest.is_readable() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "descriptor is not registered for read readiness",
            ));
        }
        self.registered
            .ready(Direction::Read, |fd| operation(&mut fd.0))
            .await
    }

    /// Retry a nonblocking operation whenever the descriptor becomes writable.
    ///
    /// # Errors
    /// Returns an operation or reactor error, or `InvalidInput` if write
    /// readiness was not registered.
    pub async fn writable<U>(
        &mut self,
        mut operation: impl FnMut(&mut T) -> io::Result<U>,
    ) -> io::Result<U> {
        if !self.interest.is_writable() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "descriptor is not registered for write readiness",
            ));
        }
        self.registered
            .ready(Direction::Write, |fd| operation(&mut fd.0))
            .await
    }
}

impl<T: AsRawFd + Read> AsyncFd<T> {
    /// Read bytes from a descriptor configured for nonblocking I/O.
    ///
    /// # Errors
    ///
    /// Returns an OS or reactor shutdown error.
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.readable(|fd| fd.read(buf)).await
    }
}

impl<T: AsRawFd + Write> AsyncFd<T> {
    /// Wait for write readiness and return the exact number of bytes written.
    ///
    /// # Errors
    /// Returns an OS or reactor error, including use without write interest.
    pub async fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writable(|fd| fd.write(buf)).await
    }
}

struct Descriptor<T: AsRawFd>(T);

impl<T: AsRawFd> Source for Descriptor<T> {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.0.as_raw_fd()).register(registry, token, interest)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        SourceFd(&self.0.as_raw_fd()).reregister(registry, token, interest)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        SourceFd(&self.0.as_raw_fd()).deregister(registry)
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: The caller owns the live descriptor and F_GETFL takes no argument.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL takes the live descriptor and an integer flag word.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

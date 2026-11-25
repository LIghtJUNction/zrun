//! Read exactly one key at a time. Bytes following Enter belong to the foreground command.
use super::super::Interrupt;
use std::{io, mem::MaybeUninit, os::fd::AsRawFd};

pub(super) struct Mode(libc::termios);
impl Mode {
    pub(super) fn enter() -> io::Result<Self> {
        let mut original = MaybeUninit::uninit();
        // SAFETY: tcgetattr initializes a complete termios on success.
        if unsafe { libc::tcgetattr(0, original.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call above successfully initialized original.
        let original = unsafe { original.assume_init() };
        let mut edit = original;
        edit.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL | libc::IEXTEN);
        edit.c_iflag &= !libc::IXON;
        edit.c_cc[libc::VMIN] = 1;
        edit.c_cc[libc::VTIME] = 0;
        // Foreground programs retain the caller's suspend key. The shell itself
        // must not stop inside the editor with canonical input disabled.
        edit.c_cc[libc::VSUSP] = 0;
        set_mode(&edit)?;
        Ok(Self(original))
    }

    pub(super) fn echo(&self) -> bool {
        self.0.c_lflag & libc::ECHO != 0
    }
    pub(super) fn restore(&self) -> io::Result<()> {
        set_mode(&self.0)
    }
}

fn set_mode(mode: &libc::termios) -> io::Result<()> {
    loop {
        // SAFETY: mode is a live termios from tcgetattr and fd 0 is borrowed.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, mode) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}
impl Drop for Mode {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub(super) enum Key {
    Byte(u8),
    Text(String),
    Sequence(Vec<u8>),
    Cancel,
    Eof,
    Ignore,
    TooLong,
}
enum Input {
    Byte(u8),
    Cancel,
    Eof,
    Timeout,
}
impl Input {
    fn key(self) -> Key {
        match self {
            Self::Byte(b) => Key::Byte(b),
            Self::Cancel => Key::Cancel,
            Self::Eof => Key::Eof,
            Self::Timeout => Key::Ignore,
        }
    }
}
pub(super) struct Reader<'a> {
    pub signal: &'a Interrupt,
    pub pending: Option<u8>,
}
impl Reader<'_> {
    fn byte(&mut self, timeout: i32) -> io::Result<Input> {
        if let Some(b) = self.pending.take() {
            return Ok(Input::Byte(b));
        }
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.signal.stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: fds contains two writable pollfd entries.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if fds[1].revents != 0 && self.signal.take()? {
                return Ok(Input::Cancel);
            }
            if n == 0 {
                return Ok(Input::Timeout);
            }
            if fds[0].revents == 0 {
                continue;
            }
            let mut b = 0u8;
            // SAFETY: b is one initialized, writable byte; fd 0 is borrowed.
            let n = unsafe { libc::read(0, (&raw mut b).cast(), 1) };
            if n == 1 {
                return Ok(Input::Byte(b));
            }
            if n == 0 {
                return Ok(Input::Eof);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }

    pub(super) fn key(&mut self, room: usize) -> io::Result<Key> {
        let first = self.byte(-1)?;
        let Input::Byte(first) = first else {
            return Ok(first.key());
        };
        if first == 27 {
            let next = self.byte(80)?;
            match next {
                Input::Byte(b'[' | b'O') => (),
                Input::Byte(other) => {
                    self.pending = Some(other);
                    return Ok(Key::Ignore);
                }
                other => return Ok(other.key()),
            }
            let mut seq = Vec::new();
            while seq.len() < 32 {
                let next = self.byte(150)?;
                let Input::Byte(b) = next else {
                    return Ok(next.key());
                };
                seq.push(b);
                if (0x40..=0x7e).contains(&b) {
                    return if seq == b"200~" {
                        self.paste(room)
                    } else {
                        Ok(Key::Sequence(seq))
                    };
                }
            }
            return Ok(Key::Ignore);
        }
        if first.is_ascii() {
            return Ok(Key::Byte(first));
        }
        let width = match first {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Ok(Key::Ignore),
        };
        let mut buf = [first; 4];
        for slot in &mut buf[1..width] {
            let next = self.byte(-1)?;
            let Input::Byte(b) = next else {
                return Ok(next.key());
            };
            if !(0x80..=0xbf).contains(&b) {
                self.pending = Some(b);
                return Ok(Key::Ignore);
            }
            *slot = b;
        }
        Ok(std::str::from_utf8(&buf[..width])
            .ok()
            .map_or(Key::Ignore, |s| Key::Text(s.to_owned())))
    }

    fn paste(&mut self, room: usize) -> io::Result<Key> {
        const END: &[u8] = b"\x1b[201~";
        let mut bytes = Vec::new();
        let mut count = 0usize;
        let mut matched = 0;
        loop {
            let next = self.byte(-1)?;
            let Input::Byte(b) = next else {
                return Ok(next.key());
            };
            count = count.saturating_add(1);
            if count <= room + END.len() {
                bytes.push(b);
            }
            matched = if b == END[matched] {
                matched + 1
            } else {
                usize::from(b == END[0])
            };
            if matched == END.len() {
                break;
            }
        }
        if count > room + END.len() {
            return Ok(Key::TooLong);
        }
        bytes.truncate(bytes.len() - END.len());
        let Ok(text) = String::from_utf8(bytes) else {
            return Ok(Key::Ignore);
        };
        let mut text = text.replace("\r\n", "\n").replace('\r', "\n");
        text.retain(|c| c == '\n' || c == '\t' || !c.is_control());
        Ok(Key::Text(text))
    }
}

impl Reader<'_> {
    pub(super) fn canonical(&mut self) -> io::Result<super::Line> {
        let mut line = Vec::new();
        loop {
            match self.byte(-1)? {
                Input::Cancel => return Ok(super::Line::Cancel),
                Input::Eof if line.is_empty() => return Ok(super::Line::Eof),
                Input::Byte(b'\n') | Input::Eof => break,
                Input::Byte(b) if line.len() < crate::execution::Batch::MAX_INPUT => line.push(b),
                Input::Byte(_) => {
                    return Err(super::super::error("command exceeded the input limit"));
                }
                Input::Timeout => continue,
            }
        }
        String::from_utf8(line)
            .map(super::Line::Text)
            .map_err(|_| super::super::error("command is not UTF-8"))
    }
}

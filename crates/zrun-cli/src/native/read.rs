use super::{Interrupt, error, syntax};
use std::{
    fs::File,
    io::{self, IsTerminal, Read, Write},
    mem::MaybeUninit,
    os::fd::{AsRawFd, RawFd},
    sync::atomic::Ordering,
};

struct NoEcho {
    fd: RawFd,
    original: libc::termios,
}

impl NoEcho {
    fn enter(fd: RawFd) -> io::Result<Self> {
        let mut original = MaybeUninit::uninit();
        // SAFETY: fd is borrowed from the input file and the buffer is writable.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: tcgetattr initialized the structure on success.
        let original = unsafe { original.assume_init() };
        let mut silent = original;
        silent.c_lflag &= !(libc::ECHO | libc::ECHONL);
        set_mode(fd, &silent)?;
        Ok(Self { fd, original })
    }
}

fn set_mode(fd: RawFd, mode: &libc::termios) -> io::Result<()> {
    loop {
        // SAFETY: the input file stays open and mode is valid for this call.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, mode) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        let _ = set_mode(self.fd, &self.original);
    }
}

struct InterruptGuard<'a>(&'a Interrupt);

impl Drop for InterruptGuard<'_> {
    fn drop(&mut self) {
        self.0.editing.store(false, Ordering::SeqCst);
    }
}

pub(super) struct Outcome<'a> {
    pub assignment: Option<(&'a str, String)>,
    pub status: u8,
}

pub(super) fn line<'a>(
    args: &'a [String],
    input: &mut File,
    err: &mut File,
    signal: Option<&Interrupt>,
) -> io::Result<Outcome<'a>> {
    let mut args = args.iter().map(String::as_str).peekable();
    let (mut raw, mut silent, mut prompt) = (false, false, None);
    while let Some(option) = args.next_if(|arg| arg.starts_with('-') && arg.len() > 1) {
        if option == "--" {
            break;
        }
        for (offset, flag) in option[1..].char_indices() {
            match flag {
                'r' => raw = true,
                's' => silent = true,
                'p' => {
                    let attached = &option[offset + 2..];
                    prompt = Some(if attached.is_empty() {
                        args.next()
                            .ok_or_else(|| error("read: -p needs a prompt"))?
                    } else {
                        attached
                    });
                    break;
                }
                _ => return Err(error("usage: read [-rs] [-p PROMPT] [NAME]")),
            }
        }
    }
    let name = args.next().unwrap_or("REPLY");
    if !syntax::identifier(name) || args.next().is_some() {
        return Err(error("read: expected one variable name"));
    }

    let terminal = input.is_terminal();
    let local_signal = (terminal && signal.is_none())
        .then(Interrupt::new)
        .transpose()?;
    let signal = signal.or(local_signal.as_ref());
    let interrupt = signal.map(|signal| {
        signal.editing.store(true, Ordering::SeqCst);
        InterruptGuard(signal)
    });
    let mode = if silent && terminal {
        Some(NoEcho::enter(input.as_raw_fd())?)
    } else {
        None
    };
    if let Some(prompt) = prompt.filter(|_| terminal) {
        err.write_all(prompt.as_bytes())?;
        err.flush()?;
    }
    let mut bytes = Vec::new();
    let mut escaped = false;
    let status = loop {
        if let Some(signal) = signal
            && interrupted(input.as_raw_fd(), signal)?
        {
            drop(mode);
            drop(interrupt);
            if let Some(local_signal) = local_signal {
                let terminate = local_signal.terminate.load(Ordering::SeqCst);
                drop(local_signal);
                if terminate != 0 {
                    signal_hook::low_level::emulate_default_handler(terminate)?;
                    return Ok(Outcome {
                        assignment: None,
                        status: 128 + terminate as u8,
                    });
                }
            }
            return Ok(Outcome {
                assignment: None,
                status: 130,
            });
        }
        let mut byte = [0];
        match input.read(&mut byte) {
            Ok(0) => break 1,
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
        match (byte[0], escaped) {
            (b'\n', false) => break 0,
            (b'\\', false) if !raw => {
                escaped = true;
                continue;
            }
            (0, _) | (b'\n', true) => {}
            (byte, _) => bytes.push(byte),
        }
        escaped = false;
    };
    let value = String::from_utf8(bytes).map_err(|_| error("read: input is not UTF-8"))?;
    Ok(Outcome {
        assignment: Some((name, value)),
        status,
    })
}

fn interrupted(fd: RawFd, signal: &Interrupt) -> io::Result<bool> {
    let mut fds = [
        libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: signal.stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: fds contains two valid, writable pollfd entries.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } >= 0 {
            if fds[1].revents != 0 && signal.take()? {
                return Ok(true);
            }
            if fds[0].revents != 0 {
                return Ok(false);
            }
            continue;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

//! Return an interactive terminal to the editor after its foreground job exits.
use super::State;
use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};

#[derive(Clone)]
pub(super) struct Handoff {
    mode: Option<libc::termios>,
    pub(super) interrupted: bool,
}

impl Handoff {
    pub(super) fn capture() -> Self {
        let mut mode = std::mem::MaybeUninit::uninit();
        // SAFETY: tcgetattr initializes the provided termios on success.
        let mode =
            unsafe { (libc::tcgetattr(0, mode.as_mut_ptr()) == 0).then(|| mode.assume_init()) };
        Self {
            mode,
            interrupted: false,
        }
    }

    pub(super) fn before_prompt(self, state: &State) -> io::Result<()> {
        // SAFETY: both calls take only borrowed descriptors or no arguments.
        if unsafe { libc::tcgetpgrp(0) != libc::getpgrp() } {
            return Ok(());
        }
        if self.interrupted
            && let Some(mode) = self.mode
        {
            loop {
                // SAFETY: mode belongs to the shell that just reclaimed fd 0.
                if unsafe { libc::tcsetattr(0, libc::TCSANOW, &mode) } == 0 {
                    break;
                }
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        }
        // Preserve the existing prompt-only protocol on echo-suppressed PTYs.
        if self.mode.is_none_or(|m| m.c_lflag & libc::ECHO == 0)
            || !io::stderr().is_terminal()
            // SAFETY: stderr is borrowed and neither call dereferences memory.
            || unsafe { libc::tcgetpgrp(2) != libc::getpgrp() }
            || matches!(
                state.env.get(OsStr::new("TERM")).and_then(|s| s.to_str()),
                None | Some("" | "dumb")
            )
        {
            return Ok(());
        }
        let mut size = libc::winsize::default();
        // SAFETY: the kernel writes one winsize to this live value.
        let sized = unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &mut size) } == 0;
        let mut bytes = Vec::from(&b"\x1b[0m"[..]);
        if sized && (1..=4096).contains(&size.ws_col) {
            // One width of spaces wraps only if the last output line contained
            // text. CR cancels a pending wrap when the cursor started at column 1.
            bytes.resize(bytes.len() + usize::from(size.ws_col), b' ');
        } else {
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(b"\r\x1b[J\x1b[?25h");
        let mut out = io::stderr().lock();
        out.write_all(&bytes)?;
        out.flush()
    }
}

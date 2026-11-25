"""Verify zrun's default prompt on a controlling Linux terminal."""
import contextlib
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import sys
import tempfile
import termios
import time


BINARY = os.path.abspath(sys.argv[1])
COLOR_END = "❯\x1b[0m ".encode()
SGR = re.compile(rb'\x1b\[(?:0|2|31|1;34)m')


def resize(fd, columns):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 24, columns, 0, 0))


@contextlib.contextmanager
def session(cwd, home, columns=80, **options):
    env = dict(os.environ, TERM="xterm-256color", PATH="/zrun-no-external-tools", HOME=str(home))
    for key in ("NO_COLOR", "ZRUN_PROMPT"):
        env.pop(key, None)
    env.update(options)
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        resize(0, columns)
        attrs = termios.tcgetattr(0)
        attrs[3] &= ~termios.ECHO
        termios.tcsetattr(0, termios.TCSANOW, attrs)
        os.execve(BINARY, [BINARY, "-i"], env)
    try:
        yield fd
    finally:
        os.close(fd)
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)


def prompt(fd, ending, command=None):
    if command is not None:
        os.write(fd, command + b"\n")
    raw = bytearray()
    deadline = time.monotonic() + 4
    while not raw.endswith(ending):
        remaining = deadline - time.monotonic()
        assert remaining > 0 and select.select([fd], [], [], remaining)[0], (ending, raw)
        raw.extend(os.read(fd, 4096))
    return bytes(raw)


with tempfile.TemporaryDirectory() as root:
    home = Path(root)
    repo = home / "project"
    (repo / ".git").mkdir(parents=True)
    head = repo / ".git" / "HEAD"
    head.write_text("ref: refs/heads/main\n")
    with session(home, home) as fd:
        raw = prompt(fd, COLOR_END)
        assert b"\x1b[1;34m" in raw and SGR.sub(b"", raw).decode() == "~ ❯ ", raw
        raw = prompt(fd, COLOR_END, b"cd project")
        assert SGR.sub(b"", raw).decode() == "~/project (main) ❯ ", raw
        raw = prompt(fd, COLOR_END, b"/bin/sh -c 'exit 37'")
        assert b"\x1b[31m" in raw and SGR.sub(b"", raw).decode() == "~/project (main) !37 ❯ ", raw
        head.write_text("ref: refs/heads/topic\n")
        raw = prompt(fd, COLOR_END, b":")
        assert SGR.sub(b"", raw).decode() == "~/project (topic) ❯ ", raw
        os.write(fd, b"\x03")
        raw = prompt(fd, COLOR_END)
        assert "!130 ❯ " in SGR.sub(b"", raw).decode(), raw
        resize(fd, 18)
        raw = prompt(fd, COLOR_END, b":")
        text = SGR.sub(b"", raw).decode()
        assert text.endswith("❯ ") and "topic" not in text and len(text) <= 10, text

    with session(repo, home, NO_COLOR="1") as fd:
        assert prompt(fd, b"> ") == b"~/project (topic) > "
        head.write_text("c" * 40 + "\n")
        assert prompt(fd, b"> ", b":") == b"~/project > "
        head.unlink()
        os.mkfifo(head)
        assert prompt(fd, b"> ", b":") == b"~/project > "

    linked = home / "linked"
    linked.mkdir()
    (linked / ".git").write_text("gitdir: ../metadata\n")
    (home / "metadata").mkdir()
    (home / "metadata" / "HEAD").write_text("ref: refs/heads/feature/ui\n")
    with session(linked, home, NO_COLOR="1") as fd:
        assert prompt(fd, b"> ") == b"~/linked (feature/ui) > "

    tricky = home / "bad\x1b[31m\n\u202e"
    tricky.mkdir()
    (tricky / ".git").mkdir()
    (tricky / ".git" / "HEAD").write_text("ref: refs/heads/a\x1b]b\x07\n")
    with session(tricky, home, NO_COLOR="1") as fd:
        assert prompt(fd, b"> ") == b"~/bad?[31m?? (a?]b?) > "

    for options in ({"TERM": "dumb"}, {"ZRUN_PROMPT": "plain"}):
        with session(repo, home, **options) as fd:
            assert prompt(fd, b"> ") == b"zrun> "

"""Drive the native shell on a real PTY, inspecting menu output and command effects."""
import os
from pathlib import Path
import pty
import re
import select
import signal
import sys
import tempfile
import termios
import time
import unicodedata

BINARY = os.path.abspath(sys.argv[1])
PROMPT = b"\x1b[?2004hzrun> "
SGR = re.compile(rb"\x1b\[[\d;]*m")
ROW = re.compile(rb"\x1b\[\d*B\r\x1b\[2K([^\x1b]*)")
DRAWN = b"\x1b[K\x1b8"


def read_until(fd, predicate):
    data = b""
    deadline = time.monotonic() + 5
    while not predicate(data):
        remaining = deadline - time.monotonic()
        assert remaining > 0 and select.select([fd], [], [], remaining)[0], data[-640:]
        data += os.read(fd, 65536)
    return data


def send(fd, keys):
    if isinstance(keys, str):
        keys = keys.encode()
    os.write(fd, keys)


def edited(fd, keys, expected):
    send(fd, keys)
    return read_until(fd, lambda b: expected in SGR.sub(b"", b) and b.endswith(DRAWN))


def submitted(fd, keys=b"\r"):
    send(fd, keys)
    return read_until(fd, lambda b: PROMPT in b)


with tempfile.TemporaryDirectory() as tmp:
    root = Path(tmp)
    runner = root / "record"
    runner.write_text("#!/bin/sh\nprintf '%s|%s\\n' \"$1\" \"$2\" >> receipt\n")
    runner.chmod(0o700)
    for name in ("fruit-apple", "fruit-apricot", "fruit-berry", "fruit-blue",
                 "fruit-cherry", "fruit-date", "fruit-elder", "fruit-fig",
                 "茶 甲", "茶 乙", "茶" * 20 + "甲", "茶" * 20 + "乙"):
        (root / name).touch()
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(tmp)
        termios.tcsetwinsize(0, (8, 64))
        os.execve(BINARY, [BINARY, "-i"], dict(os.environ, HOME=tmp, XDG_STATE_HOME=tmp,
            PATH=tmp, TERM="xterm-256color", NO_COLOR="1", ZRUN_PROMPT="plain"))
    receipts = root / "receipt"
    try:
        read_until(fd, lambda b: PROMPT in b)
        first = edited(fd, b"record ./fruit-\t", b"> ./fruit-apple")
        assert first.count(b"\n") == 6, first
        assert not receipts.exists()
        assert len(ROW.findall(first)) == 6, first
        # Cycling repaints the same six rows. No LF means the terminal cannot scroll
        # arbitrary new choices into its scrollback on repeated Tab / Shift-Tab.
        for key, expect in [(b"\t", "fruit-apricot"), (b"\t", "fruit-berry"),
                            (b"\x1b[Z", "fruit-apricot"), (b"\x1b[A", "fruit-apple"),
                            (b"\x1b[B", "fruit-apricot")]:
            frame = edited(fd, key, ("> ./" + expect).encode())
            assert b"\n" not in frame and frame.count(b"\x1b[2K") == 2, frame
        # A full cycle wraps to the last item and back without printing any new lines.
        edited(fd, b"\x1b[Z", b"> ./fruit-apple")
        last = edited(fd, b"\x1b[Z", b"> ./fruit-fig")
        back = edited(fd, b"\t", b"> ./fruit-apple")
        assert b"\n" not in last + back and not receipts.exists()
        edited(fd, b"\t", b"> ./fruit-apricot")
        # The source filter is still "fruit-", even though "fruit-apricot" was highlighted.
        edited(fd, b"b", b"> ./fruit-berry")
        frame = edited(fd, b"\t", b"> ./fruit-blue")
        assert b"fruit-apricot" not in frame and not receipts.exists()
        submitted(fd)  # one real Enter confirms the selected candidate and runs it
        assert receipts.read_text() == "./fruit-blue|\n"

        edited(fd, b"record ./fruit-\t", b"> ./fruit-apple")
        edited(fd, b"\t", b"> ./fruit-apricot")
        edited(fd, b"\x1b[C", b"record ./fruit-apricot ")
        assert receipts.read_text() == "./fruit-blue|\n"
        submitted(fd, b"next\r")
        assert receipts.read_text().endswith("./fruit-apricot|next\n")

        edited(fd, b"record ./fruit-\t", b"> ./fruit-apple")
        edited(fd, b" ", b"record ./fruit-apple ")
        assert len(receipts.read_text().splitlines()) == 2
        submitted(fd, b"second\r")
        assert receipts.read_text().endswith("./fruit-apple|second\n")

        # Escape restores the opening draft, even after cycling and editing its filter.
        edited(fd, b"record ./fruit-\t", b"> ./fruit-apple")
        edited(fd, b"b", b"> ./fruit-berry")
        cancelled = edited(fd, b"\x1b", b"zrun> record ./fruit-")
        assert b"> ./fruit-" not in ROW.findall(cancelled) and b"\x1b[2K" in cancelled
        submitted(fd, b"manual\r")
        assert receipts.read_text().endswith("./fruit-manual|\n")

        # Open an unfinished quoted Unicode argument. Choose a name with a literal space.
        frame = edited(fd, 'record "./茶 \t', "> ./茶 ".encode())
        selected = next(s.decode()[2:] for s in ROW.findall(frame) if s.startswith(b"> "))
        assert selected in ("./茶 甲", "./茶 乙")
        submitted(fd)
        assert receipts.read_text().splitlines()[-1] == selected + "|"

        # Narrow windows show one choice. No broken Unicode or wrap can move the input row.
        termios.tcsetwinsize(fd, (4, 15))
        send(fd, "record ./茶茶\t")
        frame = read_until(fd, lambda b: b"\x1b[2K> " in b and b.endswith(DRAWN))
        rows = [s.decode() for s in ROW.findall(frame) if s]
        assert len(rows) == 1 and "茶" in rows[0], rows
        assert sum(2 if unicodedata.east_asian_width(ch) in "FW" else 1 for ch in rows[0]) < 15
        frame = edited(fd, b"\t", "> ./茶".encode())
        assert b"\n" not in frame
        submitted(fd)
        assert receipts.read_text().splitlines()[-1].startswith("./" + "茶" * 20)
    finally:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        os.close(fd)

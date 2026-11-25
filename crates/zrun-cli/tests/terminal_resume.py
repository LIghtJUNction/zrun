"""Run a synthetic foreground app in a real PTY; check rendered cells with pyte 0.8.2.

Usage: python3 terminal_resume.py /path/to/zrun
"""
import json
import os
import pty
import select
import signal
import sys
import tempfile
import termios
import time
from pathlib import Path

import pyte


APP = r'''import os, signal, sys, termios
from time import sleep
write = lambda data: os.write(1, data)
mode = sys.argv[1]
if mode == "partial":
    write(b"partial-without-newline")
elif mode == "newline":
    write(b"ended-with-newline\n")
elif mode == "width":
    write(b"X" * os.get_terminal_size(1).columns)
else:
    resized = []
    signal.signal(signal.SIGWINCH, lambda *_: resized.append(True))
    if mode == "scrollback":
        for i in range(30):
            write(f"saved transcript {i}\n".encode())
        write(b"\n\n\n\x1b[3A")
    if mode == "interrupt":
        attrs = termios.tcgetattr(0)
        attrs[3] &= ~(termios.ICANON | termios.ECHO)
        termios.tcsetattr(0, termios.TCSANOW, attrs)
        signal.signal(signal.SIGINT, signal.SIG_DFL)
    write(b"KEEP-ABOVE\n\x1b[31m/logout\x1b7\x1b[1B\r---------------------"
          b"\x1b[1B\r                stale-status\x1b8\x1b[?25l")
    if mode == "interrupt":
        while True: signal.pause()
    if mode == "resize":
        for _ in range(500):
            if resized: break
            sleep(.01)
        else: sys.exit(1)
'''


def run(binary, mode):
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "app"
        path.write_text(f"#!{sys.executable}\n" + APP)
        path.chmod(0o700)
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(directory)
            termios.tcsetwinsize(0, (14, 48))
            env = dict(os.environ, HOME=directory, XDG_STATE_HOME=directory + "/state",
                       ZRUN_PROMPT="plain", TERM="xterm-256color", NO_COLOR="1")
            os.execve(binary, [binary, "-i"], env)

        screen = pyte.HistoryScreen(48, 14)
        stream = pyte.ByteStream(screen)

        def until(marker):
            data = bytearray()
            deadline = time.monotonic() + 5
            while marker not in data:
                remaining = deadline - time.monotonic()
                assert remaining > 0 and select.select([fd], [], [], remaining)[0], bytes(data[-300:])
                chunk = os.read(fd, 65536)
                stream.feed(chunk)
                data.extend(chunk)
            return data

        try:
            prompt = b"\x1b[?2004hzrun> "
            until(prompt)
            command = "async saved -- ./app newline" if mode == "background" else "./app " + mode
            os.write(fd, command.encode() + b"\r")
            if mode in ("interrupt", "resize"):
                until(b"\x1b[?25l")
                if mode == "interrupt":
                    os.write(fd, b"\x03")
                else:
                    termios.tcsetwinsize(fd, (16, 32))
                    screen.resize(lines=16, columns=32)
            data = until(prompt)
            if mode == "background":
                assert b"\x1b[?25h" not in data
                os.write(fd, b"await saved\r")
                data = until(prompt)
                assert b"ended-with-newline" in data, data
            rows = [r.rstrip() for r in screen.display]
            row = screen.cursor.y
            result = {"mode": mode, "row": row, "prompt_row": rows[row],
                      "above": rows[:row], "below": rows[row + 1:], "hidden": screen.cursor.hidden}
            if mode == "background":
                assert b"\x1b[?25h" not in data, "background job modified the foreground display"
            else:
                assert rows[row] == "zrun>" and not any(rows[row + 1:]) and not screen.cursor.hidden, result
                assert b"\x1b[2J" not in data and b"\x1b[3J" not in data
                if mode in ("paint", "interrupt", "resize", "scrollback"):
                    assert "KEEP-ABOVE" in rows[:row], result
                else:
                    previous = {"partial": "partial-without-newline", "newline": "ended-with-newline",
                                "width": "X" * 48}[mode]
                    assert rows[row - 1] == previous, result
                if mode == "interrupt":
                    # The line editor's echo derives from the recovered shell settings.
                    os.write(fd, b"echo")
                    until(b"echo")
                if mode == "scrollback":
                    history = ["".join(c.data for _, c in sorted(line.items())).rstrip()
                               for line in screen.history.top]
                    assert "saved transcript 0" in history, history
            return result
        finally:
            os.close(fd)
            try:
                os.kill(pid, signal.SIGHUP)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)


if __name__ == "__main__":
    for scenario in ("paint", "interrupt", "partial", "newline", "width", "resize", "background", "scrollback"):
        print(json.dumps(run(os.path.abspath(sys.argv[1]), scenario)))

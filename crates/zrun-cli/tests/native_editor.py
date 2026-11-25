"""Drive literal terminal keystrokes and observe effects before and after Enter."""
import os
import pty
import re
import select
import signal
import sys
import tempfile
import termios
import time
from pathlib import Path


binary = os.path.abspath(sys.argv[1])
sgr = re.compile(rb"\x1b\[[0-9;]*m")


def read_until(fd, predicate):
    data = b""
    limit = time.monotonic() + 4
    while not predicate(data):
        wait = limit - time.monotonic()
        assert wait > 0 and select.select([fd], [], [], wait)[0], data[-500:]
        data += os.read(fd, 65536)
    return data


with tempfile.TemporaryDirectory() as tmp:
    root = Path(tmp)
    (root / "words").mkdir()
    (root / "words" / "multi-a").touch()
    (root / "words" / "multi-b").touch()
    (root / "words" / "茶 叶").touch()
    tool = root / "zfixture-command"
    tool.write_text("#!/bin/sh\nprintf '%s' \"$1\" > ran\n")
    tool.chmod(0o700)
    check = root / "mode.py"
    check.write_text(
        "import termios,sys\n"
        "lflag=termios.tcgetattr(0)[3]\n"
        "assert lflag & termios.ICANON and lflag & termios.ECHO and lflag & termios.ISIG, lflag\n"
        "open('mode-ok','w').write(sys.stdin.readline())\n"
    )
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(root)
        termios.tcsetwinsize(0, (24, 96))
        env = dict(os.environ, TERM="xterm-256color", ZRUN_PROMPT="plain", HOME=tmp,
                   XDG_STATE_HOME=str(root / "state"), PATH=tmp, ZRUN_SHELL="/missing-shell")
        env.pop("NO_COLOR", None)
        os.execve(binary, [binary, "-i"], env)

    def send(b):
        os.write(fd, b)

    def prompt_after(cmd=b""):
        send(cmd + b"\r")
        # DECSET bracketed paste is enabled once per new command, never on a redraw.
        return read_until(fd, lambda b: b"\x1b[?2004hzrun> " in b)

    def visible(what):
        return read_until(fd, lambda b: what in sgr.sub(b"", b))

    try:
        visible(b"zrun> ")
        assert not termios.tcgetattr(fd)[3] & termios.ICANON
        # No newline: completion and the refreshed editable command must already be visible.
        send(b"zfixture-c\t")
        visible(b"zfixture-command ")
        assert not (root / "ran").exists()
        prompt_after(b"instant")
        assert (root / "ran").read_text() == "instant"

        send(b"zfixture-command words/mu\t")
        visible(b"words/multi-b")
        prompt_after(b"b")
        assert (root / "ran").read_text() == "words/multi-b"
        send(b"zfixture-command wor\t")
        visible(b"words/")
        send("茶".encode() + b"\t")
        visible("叶".encode())
        prompt_after()
        assert (root / "ran").read_text() == "words/茶 叶"

        # Mid-line backspace/delete operate on whole UTF-8 characters.
        send("zfixture-command 甲乙丁".encode() + b"\x1b[D\x7f" + "丙".encode())
        send(b"\x1b[3~" + "😊".encode())
        prompt_after()
        assert (root / "ran").read_text() == "甲丙😊"

        send(b"bad\x15zfixture-command throwaway\x17remember-me")
        prompt_after()
        assert (root / "ran").read_text() == "remember-me"
        (root / "ran").unlink()
        send(b"zfixture-command rem")
        read_until(fd, lambda b: b"\x1b[2member-me\x1b[0m" in b)
        assert not (root / "ran").exists()
        prompt_after(b"\x1b[C")
        assert (root / "ran").read_text() == "remember-me"
        (root / "ran").unlink()
        send(b"\x1b[A")
        visible(b"remember-me")
        prompt_after()
        assert (root / "ran").read_text() == "remember-me"
        prompt_after(b"zfixture-command draft\x1b[A\x1b[B")
        assert (root / "ran").read_text() == "draft"
        send(b"zfixture-command cancel-me")
        visible(b"cancel-me")
        send(b"\x03")
        visible(b"\x1b[?2004hzrun> ")
        assert (root / "ran").read_text() == "draft"

        # Pasted line breaks and tabs are data, even while the writer pauses.
        pasted = "zfixture-command '一\n二\t三'".encode()
        send(b"\x1b[200~" + pasted + b"\x1b[201~")
        visible("三".encode())
        assert (root / "ran").read_text() == "draft"
        prompt_after()
        assert (root / "ran").read_text() == "一\n二\t三"
        # A single PTY write contains a command and its child's answer. The editor
        # must restore terminal mode and must not have consumed the child's bytes.
        send(os.fsencode(sys.executable) + b" mode.py\rfor-child\r")
        visible(b"\x1b[?2004hzrun> ")
        assert (root / "mode-ok").read_text() == "for-child\n"
        send(b"\x04")
        until = time.monotonic() + 3
        while True:
            exited, status = os.waitpid(pid, os.WNOHANG)
            if exited:
                pid = 0
                assert os.waitstatus_to_exitcode(status) == 0
                break
            assert time.monotonic() < until, "Ctrl-D did not exit"
            time.sleep(0.01)
        restored = termios.tcgetattr(fd)[3]
        assert restored & termios.ICANON and restored & termios.ECHO
    finally:
        if pid:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(fd)


    # A new process sees the saved suggestion. Termination while editing must
    # leave the caller's terminal usable and retain the requested signal status.
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(root)
        env = dict(os.environ, TERM="xterm-256color", ZRUN_PROMPT="plain", HOME=tmp,
                   XDG_STATE_HOME=str(root / "state"), PATH=tmp)
        env.pop("NO_COLOR", None)
        os.execve(binary, [binary, "-i"], env)
    try:
        visible(b"zrun> ")
        send(b"zfixture-command rem")
        read_until(fd, lambda b: b"\x1b[2member-me\x1b[0m" in b)
        prompt_after(b"\x1b[F")
        assert (root / "ran").read_text() == "remember-me"
        send(b"\x0c")
        visible(b"\x1b[2J\x1b[H")
        os.kill(pid, signal.SIGTERM)
        until = time.monotonic() + 3
        while True:
            exited, status = os.waitpid(pid, os.WNOHANG)
            if exited:
                pid = 0
                assert os.waitstatus_to_exitcode(status) == -signal.SIGTERM
                break
            assert time.monotonic() < until, "SIGTERM did not exit"
            time.sleep(0.01)
        restored = termios.tcgetattr(fd)[3]
        assert restored & termios.ICANON and restored & termios.ECHO
    finally:
        if pid:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(fd)

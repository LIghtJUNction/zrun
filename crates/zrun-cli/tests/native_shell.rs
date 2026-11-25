#![cfg(target_os = "linux")]
use std::process::Command;

#[test]
fn native_command_and_async_preserve_arguments_state_and_exit_codes() {
    let bin = env!("CARGO_BIN_EXE_zrun");
    let script = concat!(
        "mark=private; export VALUE='two words'; printf '[%s]' \"$0\" \"$@\"; ",
        "async t -- printf 'task=%s/%s' \"$mark\" \"$VALUE\"; ",
        "await t; false; printf ':%s' $?; exit 37",
    );
    let output = Command::new(bin)
        .env("ZRUN_SHELL", "/missing-backend")
        .args(["-c", script, "fixture", "a b", "c"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37), "{output:?}");
    assert_eq!(output.stdout, b"[fixture][a b][c]task=private/two words:1");
    assert!(output.stderr.is_empty());
}
#[test]
fn native_pty_owns_foreground_recovers_from_interrupt_and_reaps_tasks_on_eof() {
    // Python supplies a controlling PTY; this checks process and terminal behavior end to end.
    let output=Command::new("python3").args(["-c",r#"
import os,pty,re,select,signal,sys,termios,time,tempfile
binary=sys.argv[1]
with tempfile.TemporaryDirectory() as root:
 pid,fd=pty.fork()
 if pid==0:
  os.chdir(root); attrs=termios.tcgetattr(0);attrs[3] &= ~termios.ECHO;termios.tcsetattr(0,termios.TCSANOW,attrs)
  os.execve(binary,['-zrun'],dict(os.environ,SHELL='/usr/bin/zsh',ZRUN_SHELL='/missing/configured/backend',ZRUN_PROMPT='plain'))
 buf=b''
 def expect(text):
  global buf
  deadline=time.monotonic()+8
  while text not in buf:
   if time.monotonic()>deadline:raise AssertionError((text,buf))
   if select.select([fd],[],[],0.1)[0]:buf+=os.read(fd,4096)
  buf=buf.split(text,1)[1]
 def line(s):os.write(fd,(s+'\n').encode())
 try:
  expect(b'zrun> ');assert os.path.samefile(binary,f'/proc/{pid}/exe')
  line('printf "%s:%s\\n" "$0" "$SHELL"');expect(('zrun:'+os.path.realpath(binary)).encode());expect(b'zrun> ')
  line('export MARK="hello world"');expect(b'zrun> ')
  line('async one -- printf "%s" "$MARK"');expect(b'zrun> ')
  line('await one');expect(b'hello world');expect(b'zrun> ')
  line('sleep 25')
  until=time.monotonic()+5
  while os.tcgetpgrp(fd)==pid:
   if time.monotonic()>until:raise AssertionError("child never owned foreground")
   time.sleep(.02)
  os.write(fd,b'\x03');expect(b'zrun> ')
  line('echo signal=$?');expect(b'signal=130');expect(b'zrun> ')
  os.write(fd,b'echo SHOULD_NOT_RUN');os.write(fd,b'\x03');expect(b'zrun> ')
  line("async cleanup -- sh -c 'echo $$ > pid; exec sleep 25'");expect(b'zrun> ')
  deadline=time.monotonic()+5
  while not os.path.exists(root+'/pid') or not os.stat(root+'/pid').st_size:
   if time.monotonic()>deadline:raise AssertionError('missing background pid')
   time.sleep(.02)
  job=int(open(root+'/pid').read());os.write(fd,b'\x04')
  while True:
   done,status=os.waitpid(pid,os.WNOHANG)
   if done:break
   if time.monotonic()>deadline:raise AssertionError('EOF failed to exit')
   time.sleep(.02)
  assert os.waitstatus_to_exitcode(status)==0;pid=0
  assert not os.path.exists(f'/proc/{job}'),job
 finally:
  if pid:
   try:os.kill(pid,signal.SIGKILL);os.waitpid(pid,0)
   except ProcessLookupError:pass
  os.close(fd)
"#,env!("CARGO_BIN_EXE_zrun")]).output().expect("python3 PTY harness");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn files_stdin_and_login_options_share_the_native_session() {
    use std::{fs, io::Write, process::Stdio};
    let bin = env!("CARGO_BIN_EXE_zrun");
    let source =
        "export MARK='two words'\nasync one -- printf '%s' \"$MARK\"\nawait one\nexit 27\n";
    let root = std::env::temp_dir().join(format!("zrun-native-route-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    for file in ["plain", "plain.zrun", "-script", "-c", "run"] {
        fs::write(root.join(file), source).unwrap();
        let out = Command::new(bin)
            .env("ZRUN_SHELL", "/missing-backend")
            .current_dir(&root)
            .args(["--", file])
            .output()
            .unwrap();
        assert_eq!(
            (out.status.code(), &out.stdout[..]),
            (Some(27), &b"two words"[..]),
            "{out:?}"
        );
    }
    fs::remove_dir_all(root).unwrap();
    for flags in [&[][..], &["-i"], &["-l"], &["--login"], &["-s"]] {
        let mut child = Command::new(bin)
            .env("ZRUN_SHELL", "/missing-backend")
            .args(flags)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            (out.status.code(), &out.stdout[..]),
            (Some(27), &b"two words"[..]),
            "{flags:?}: {out:?}"
        );
        assert!(out.stderr.is_empty(), "{flags:?}: {out:?}");
    }
}

#[test]
fn script_commands_treat_a_directory_as_an_external_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_zrun"))
        .env("ZRUN_NO_RC", "1")
        .args(["-ic", "./"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(126), "{output:?}");
}

#[test]
fn await_delivers_child_stderr_and_status_after_another_statement() {
    let output = Command::new(env!("CARGO_BIN_EXE_zrun"))
        .env("ZRUN_SHELL", "/missing-backend")
        .args(["-c", "async failing -- sh -c 'printf OUT; printf ERR >&2; exit 19'\nprintf FIRST\nawait failing"])
        .output().unwrap();
    assert_eq!(output.status.code(), Some(19), "{output:?}");
    assert_eq!(output.stdout, b"FIRSTOUT");
    assert_eq!(output.stderr, b"ERR");
}

#[test]
fn noninteractive_foreground_program_keeps_the_callers_process_group() {
    let output = Command::new(env!("CARGO_BIN_EXE_zrun"))
        .env("ZRUN_SHELL", "/missing-backend")
        .args(["-c", "python3 -c 'import os; print(os.getpgrp())'"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    // SAFETY: getpgrp reads the test process's group and has no arguments.
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        unsafe { libc::getpgrp() }.to_string()
    );
}

#[test]
fn native_prompt_tracks_directory_git_status_and_terminal_capabilities() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/native_prompt.py"
        ))
        .arg(env!("CARGO_BIN_EXE_zrun"))
        .output()
        .expect("python3 PTY harness");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_shell_version_probe_is_machine_readable_without_a_terminal() {
    let output = Command::new(env!("CARGO_BIN_EXE_zrun"))
        .arg("--native-shell-version")
        .env("ZRUN_SHELL", "/missing-backend")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"1\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn interactive_editor_completes_before_enter_and_restores_the_terminal() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/native_editor.py"
        ))
        .arg(env!("CARGO_BIN_EXE_zrun"))
        .output()
        .expect("python3 PTY harness");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_read_hides_input_exports_value_and_recovers_from_interrupt() {
    let output = Command::new("python3")
        .args([
            "-c",
            r#"
import os, pty, select, signal, sys, tempfile, termios, time

binary = sys.argv[1]
with tempfile.TemporaryDirectory() as root:
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(root)
        os.execve(binary, ['-zrun'], dict(os.environ, HOME=root, ZRUN_PROMPT='plain'))
    pending = b''
    def expect(text):
        global pending
        deadline = time.monotonic() + 8
        while text not in pending:
            assert time.monotonic() < deadline, (text, pending)
            if select.select([fd], [], [], 0.1)[0]:
                pending += os.read(fd, 4096)
        before, pending = pending.split(text, 1)
        return before
    def line(text):
        os.write(fd, text + b'\n')
    try:
        expect(b'zrun> ')
        line(b"read -rsp 'Enter key: ' EXAMPLE_KEY; export EXAMPLE_KEY")
        expect(b'\r\nEnter key: ')
        assert not termios.tcgetattr(fd)[3] & termios.ECHO
        secret = br'secret with \slash  '
        line(secret)
        assert secret not in expect(b'zrun> ')
        line(b"sh -c 'printf \"%s\" \"$EXAMPLE_KEY\"'")
        expect(secret)
        expect(b'zrun> ')
        line(b"read -rsp 'Cancel key: ' EXAMPLE_KEY")
        expect(b'\r\nCancel key: ')
        os.write(fd, b'\x03')
        expect(b'zrun> ')
        line(b'echo status=$?')
        expect(b'status=130')
        expect(b'zrun> ')
        line(b'stty -a')
        expect(b' echo ')
        expect(b'zrun> ')
    finally:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        os.close(fd)
"#,
            env!("CARGO_BIN_EXE_zrun"),
        ])
        .output()
        .expect("python3 PTY harness");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

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
  os.execve(binary,['-zrun'],dict(os.environ,HOME=root,XDG_STATE_HOME=root,ZRUN_NO_RC='1',SHELL='/usr/bin/zsh',ZRUN_SHELL='/missing/configured/backend',ZRUN_PROMPT='plain',TERM='xterm-256color'))
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
  line('echo signal=$?');expect(b'signal=130\r\n');expect(b'zrun> ')
  line('stty echo; echo EDITOR_READY');expect(b'EDITOR_READY\r\n');expect(b'zrun> ')
  os.write(fd,b'echo SHOULD_NOT_RUN');expect(b'SHOULD_NOT_RUN');os.write(fd,b'\x03');expect(b'\x1b[?2004hzrun> ')
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
        os.execve(binary, ['-zrun'], dict(os.environ, HOME=root, ZRUN_NO_RC='1', ZRUN_PROMPT='plain'))
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
        expect(b'\r\n')
        expect(b'Enter key: ')
        assert not termios.tcgetattr(fd)[3] & termios.ECHO
        secret = br'secret with \slash  '
        line(secret)
        assert secret not in expect(b'zrun> ')
        line(b"sh -c 'printf \"%s\" \"$EXAMPLE_KEY\"'")
        expect(secret)
        expect(b'zrun> ')
        line(b"read -rsp 'Cancel key: ' EXAMPLE_KEY")
        expect(b'\r\n')
        expect(b'Cancel key: ')
        os.write(fd, b'\x03')
        expect(b'zrun> ')
        line(b'echo status=$?')
        expect(b'status=130')
        expect(b'zrun> ')
        line(b'stty -a')
        expect(b'\r\n')
        modes = expect(b'zrun> ').replace(b';', b' ').split()
        assert b'echo' in modes, modes
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

#[test]
fn type_lists_alias_builtin_and_executables_in_lookup_order() {
    use std::{fs, os::unix::fs::PermissionsExt};
    let dir = std::env::temp_dir().join(format!("zrun-lookup-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    for (subdir, mode) in [("bin1", 0o700), ("ignored", 0o600), ("bin2", 0o700)] {
        let bin = dir.join(subdir);
        fs::create_dir(&bin).unwrap();
        let exe = bin.join("codex");
        fs::write(&exe, "#!/bin/sh\ntouch SHOULD_NOT_RUN\n").unwrap();
        fs::set_permissions(exe, fs::Permissions::from_mode(mode)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_zrun"))
        .current_dir(&dir)
        .env("PATH", "bin1:ignored:bin2")
        .env("ZRUN_SHELL", "/missing-backend")
        .args(["-c", "type -a codex; alias codex=true; whence -aw codex; command -v cd; builtin whence -p codex; PATH=bin2 builtin whence -p codex | /usr/bin/cat; command -v no_such_cmd"])
        .output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(output.stdout, b"codex is bin1/codex\ncodex is bin2/codex\ncodex: alias\ncodex: command\ncodex: command\ncd\nbin1/codex\nbin2/codex\n");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(!dir.join("SHOULD_NOT_RUN").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn positional_parameters_and_scalar_attributes_survive_between_commands() {
    let result = Command::new(env!("CARGO_BIN_EXE_zrun"))
        .env("ZRUN_SHELL", "/missing-backend")
        .args([
            "-c",
            concat!(
                "set -- 'two words' three; shift; printf '%s:%s:%s|' \"$0\" \"$#\" \"$1\"; ",
                "set -a; copied='child value'; set +a; sh -c 'printf %s \"$copied\"'; ",
                "readonly locked=before; locked=after || printf '|write:%s' \"$?\"; ",
                "unset locked || printf '|unset:%s' \"$?\"; printf '|%s' \"$locked\"; ",
                "typeset -x via_type=worked; sh -c 'printf \"|%s\" \"$via_type\"'; ",
                "set --; shift 0; printf '|%s' \"$#\""
            ),
            "fixture",
        ])
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(
        result.stdout,
        b"fixture:1:three|child value|write:1|unset:1|before|worked|0"
    );
}

#[test]
fn directory_stack_uses_session_cwd_and_preserves_it_on_failure() {
    let result = Command::new(env!("CARGO_BIN_EXE_zrun"))
        .current_dir("/")
        .env("ZRUN_SHELL", "/missing-backend")
        .args(["-c", "pushd /tmp; dirs -lv; pushd +1; pwd; pushd /dir-that-is-deliberately-absent || dirs -lp; popd; chdir /; pwd"])
        .output().unwrap();
    assert!(result.status.success(), "{result:?}");
    let tmp = std::fs::canonicalize("/tmp").unwrap().display().to_string();
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        format!("0\t{tmp}\n1\t/\n/\n/\n{tmp}\n/\n")
    );
}

#[test]
fn print_and_echo_use_shell_redirections_and_pipeline_state() {
    let result = Command::new(env!("CARGO_BIN_EXE_zrun"))
        .env("PATH", "/no-external-programs")
        .env("ZRUN_SHELL", "/missing-backend")
        .args(["-c", "print -rl -- 'one two' three | read value; echo -n \"$value\"; print -N -- a b; print -r -- 'c\\nd'"])
        .output().unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stdout, b"one twoa\0b\0c\\nd\n");
}

#[test]
fn native_test_preserves_status_with_no_external_commands() {
    let result=Command::new(env!("CARGO_BIN_EXE_zrun"))
        .env("PATH","/missing-programs").env("ZRUN_SHELL","/missing-backend")
        .args(["-c", "test -d / && [ 10 -gt 2 ] && echo good; test -n '' || echo empty; [ abc = abc ] && echo equal; [ 10 -le bad ]; echo status=$?; [ x; echo bracket=$?"])
        .output().unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stdout, b"good\nempty\nequal\nstatus=2\nbracket=2\n");
}

#[test]
fn source_and_eval_share_state_and_restore_source_arguments() {
    use std::fs;
    let root = std::env::temp_dir().join(format!("zrun-native-source-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    fs::write(root.join("nested.zrun"), "seen=$1; return 7; seen=after\n").unwrap();
    let result=Command::new(env!("CARGO_BIN_EXE_zrun"))
        .current_dir(&root).env("PATH",".").env("ZRUN_SHELL","/missing-backend")
        .args(["-c","set -- outer; . nested.zrun 'inner value'; echo status=$?; eval 'echo \"$seen:$1\"'; echo done","fixture"])
        .output().unwrap();
    fs::remove_dir_all(root).unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stdout, b"status=7\ninner value:outer\ndone\n");
}

#[test]
fn printf_reuses_format_and_keeps_padding_and_binary_escapes_without_path() {
    let result = Command::new(env!("CARGO_BIN_EXE_zrun"))
        .env("PATH", "/missing-programs")
        .env("ZRUN_SHELL", "/missing-backend")
        .args([
            "-c",
            "printf '[%04d:%.2s]' 8 abc 9 def; printf '|%*.*f' 6 2 3.5; printf '%b' 'A\\0B'",
        ])
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stdout, b"[0008:ab][0009:de]|  3.50A\0B");
}

#[test]
fn native_errexit_and_pipefail_respect_conditional_lists_and_stage_order() {
    let cases = [
        ("set -e; false; printf bad", 1, ""),
        ("set -e; false && printf bad; printf alive", 0, "alive"),
        ("set -e; true && false; printf bad", 1, ""),
        ("set -e; ! true; printf alive", 0, "alive"),
        ("set -e; ! ! false; printf bad", 1, ""),
        ("set -o pipefail; ! false | true", 0, ""),
        ("printf '%s' !", 0, "!"),
        (
            "set -e; eval 'false; printf reached' && printf ok",
            0,
            "reachedok",
        ),
        ("set -eo pipefail; false | true; printf bad", 1, ""),
        (
            "set -o pipefail; /bin/sh -c 'exit 19' | false | true; exit $?",
            1,
            "",
        ),
        (
            "set -o pipefail; false | /bin/sh -c 'exit 19' | true; exit $?",
            19,
            "",
        ),
        (
            "set -o pipefail; set +o pipefail; false | true; exit $?",
            0,
            "",
        ),
    ];
    for (script, code, stdout) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_zrun"))
            .args(["-c", script])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code), "{script}: {output:?}");
        assert_eq!(output.stdout, stdout.as_bytes(), "{script}");
        assert!(output.stderr.is_empty(), "{script}: {output:?}");
    }
}

#[test]
fn a_missing_pipeline_stage_preserves_the_other_stage_and_its_status() {
    for (options, code) in [("", 0), ("set -o pipefail; ", 127)] {
        let script = format!("{options}/zrun-test/missing-stage | printf reached");
        let output = Command::new(env!("CARGO_BIN_EXE_zrun"))
            .args(["-c", &script])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        assert_eq!(output.stdout, b"reached");
        assert!(String::from_utf8_lossy(&output.stderr).contains("missing-stage"));
    }
}

#[test]
fn nounset_fails_before_creating_redirections() {
    let output = Command::new(env!("CARGO_BIN_EXE_zrun"))
        .args([
            "-c",
            r#"set -u
printf '%s' "$ZRUN_MISSING_FIXTURE" >/zrun-test/missing-directory/out || printf bad
printf bad"#,
            "fixture",
        ])
        .env_remove("ZRUN_MISSING_FIXTURE")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(127), "{output:?}");
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"fixture: line 2: ZRUN_MISSING_FIXTURE: unbound variable\n"
    );
}

#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const ZRUN: &str = env!("CARGO_BIN_EXE_zrun");

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("zrun-ai-{}-{id}", std::process::id()));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create fixture: {e}"),
            }
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(ZRUN);
        cmd.current_dir(&self.0)
            .env_clear()
            .env("HOME", &self.0)
            .env("PATH", "/usr/bin:/bin")
            .env("ZRUN_SHELL", "/missing-test-backend");
        cmd
    }

    fn compact(&self, args: &[&str], bytes: &[u8]) -> Output {
        let mut child = self
            .command()
            .args(["ai", "compact"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        child.wait_with_output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn response(out: Output, code: i32) -> Value {
    assert_eq!(out.status.code(), Some(code), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    serde_json::from_slice(&out.stdout).expect("one JSON response on stdout")
}

fn assert_capture(path: &Path, summary: &Value, expected: &[u8]) {
    assert_eq!(fs::read(path).unwrap(), expected);
    assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    assert_eq!(summary["captured_bytes"], expected.len());
    assert_eq!(summary["sha256"], format!("{:x}", Sha256::digest(expected)));
}

#[test]
fn native_ai_config_matches_the_direct_entry_without_a_path_helper() {
    let f = Fixture::new();
    let direct = response(f.command().args(["ai", "config"]).output().unwrap(), 0);
    let native = response(
        f.command()
            .env("PATH", &f.0)
            .args(["-c", "ai config"])
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(native, direct);
    let server = &direct["mcpServers"]["zrun"];
    assert_eq!(server["args"], json!(["ai", "connect", "--stdio"]));
    assert_eq!(
        Path::new(server["command"].as_str().unwrap()),
        Path::new(ZRUN).canonicalize().unwrap()
    );
}

#[test]
fn compact_folds_repeated_utf8_and_saves_exact_binary_once() {
    let f = Fixture::new();
    let plain = response(f.compact(&[], "重复\n重复\n尾".as_bytes()), 0);
    assert_eq!(
        plain["runs"],
        json!([{"text":"重复\n","count":2},{"text":"尾","count":1}])
    );
    assert_eq!(plain["preview_truncated"], false);
    let binary = [0xff, 0, b'\n', 0x1b].repeat(32);
    let summary = response(f.compact(&["--save", "capture"], &binary), 0);
    assert_eq!(summary["encoding"], "base64");
    assert_eq!(summary["preview_truncated"], true);
    assert_capture(&f.0.join("capture"), &summary, &binary);
    let conflict = f.compact(&["--save", "capture"], b"replacement");
    assert!(!conflict.status.success());
    assert_eq!(fs::read(f.0.join("capture")).unwrap(), binary);
}

#[test]
fn run_preserves_literal_args_exit_status_and_both_byte_streams() {
    let f = Fixture::new();
    let empty = response(f.command().args(["-c", "ai run true"]).output().unwrap(), 0);
    assert!(empty["saved"].is_null());
    assert_eq!(empty["stdout"]["captured_bytes"], 0);
    assert_eq!(empty["stderr"]["captured_bytes"], 0);

    let argument = "两个 words;literal";
    let script =
        r#"printf x >> invocations; printf '%s\n%s\n' "$1" "$1"; printf '\000\377' >&2; exit 37"#;
    let args = ["ai", "run", "--save", "captures", "--", script, argument];
    let result = response(f.command().args(args).output().unwrap(), 37);
    assert_eq!(result["exit_code"], 37);
    assert_eq!(result["disposition"], "executed");
    assert_eq!(result["truncated"], false);
    let dir = f.0.join("captures");
    assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o077, 0);
    let line = format!("{argument}\n");
    assert_eq!(result["stdout"]["runs"], json!([{"text":line,"count":2}]));
    assert_capture(
        &dir.join("stdout"),
        &result["stdout"],
        line.repeat(2).as_bytes(),
    );
    assert_eq!(result["stderr"]["encoding"], "base64");
    assert_capture(&dir.join("stderr"), &result["stderr"], &[0, 255]);
    assert_eq!(fs::read(f.0.join("invocations")).unwrap(), b"x");
    let conflict = f.command().args(args).output().unwrap();
    assert!(!conflict.status.success());
    assert_eq!(fs::read(f.0.join("invocations")).unwrap(), b"x");
}

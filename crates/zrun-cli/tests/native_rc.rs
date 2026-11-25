#![cfg(target_os = "linux")]

use std::os::unix::fs::DirBuilderExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
struct Fixture(PathBuf);
impl Fixture {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("zrun-rc-{}-{label}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn shell(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zrun"));
    cmd.env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("ZDOTDIR", home)
        .env_remove("ZRUN_NO_RC");
    cmd
}

#[test]
fn interactive_rc_is_native_and_batch_execution_is_independent() {
    let dir = Fixture::new("native");
    let home = &dir.0;
    fs::create_dir_all(home.join("config/zrun")).unwrap();
    // Keep the format argument quoted when it contains shell redirection characters.
    fs::write(
        home.join("config/zrun/zrunrc"),
        "export ZR_TEST='two words'\nalias show='/usr/bin/printf \"<%s>\"'\n",
    )
    .unwrap();
    let r = shell(home)
        .args(["-ic", "show \"$ZR_TEST\" 'one arg'"])
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(r.stdout, b"<two words><one arg>");
    for (flag, disable) in [("-c", false), ("-ic", true)] {
        let mut cmd = shell(home);
        if disable {
            cmd.env("ZRUN_NO_RC", "1");
        }
        let r = cmd
            .env_remove("ZR_TEST")
            .args([flag, "/usr/bin/printf '[%s]' \"$ZR_TEST\""])
            .output()
            .unwrap();
        assert_eq!(r.stdout, b"[]");
    }
    let r = shell(home).arg("--rc-path").output().unwrap();
    assert_eq!(
        String::from_utf8(r.stdout).unwrap().trim(),
        home.join("config/zrun/zrunrc").to_str().unwrap()
    );
}

#[test]
fn full_zsh_and_native_bridge_share_zsh_startup_and_preserve_arguments() {
    if Command::new("zsh").arg("--version").output().is_err() {
        return;
    }
    let dir = Fixture::new("zsh");
    let home = &dir.0;
    fs::write(home.join(".zshrc"),"plugfn() { local -a items; items=(\"$@\"); printf '[%s]' \"${items[@]}\"; }\nalias short=plugfn\n").unwrap();
    let r = shell(home)
        .args(["--zsh", "-dic", "short 'x y' '; untouched'"])
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(r.stdout, b"[x y][; untouched]");
    let r = shell(home)
        .args(["-c", "zsh-call short 'x y' '; untouched'"])
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(r.stdout, b"[x y][; untouched]");
    let script = home.join("array.zsh");
    fs::write(
        &script,
        "typeset -a files; files=(one 'two words'); print -r -- $files[2] $1\n",
    )
    .unwrap();
    let r = shell(home).arg(&script).arg("example").output().unwrap();
    assert!(r.status.success());
    assert_eq!(r.stdout, b"two words example\n");
}

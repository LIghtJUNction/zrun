#![cfg(target_os = "linux")]

#[test]
fn completion_menu_cycles_filters_confirms_and_restores_the_draft_on_a_pty() {
    let output = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/native_menu.py"))
        .arg(env!("CARGO_BIN_EXE_zrun"))
        .output()
        .expect("start Python PTY fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for name in ["RUSTC", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let compiler = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .args(["--version", "--verbose"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).replace('\n', " | "))
        .unwrap_or_else(|| "unavailable".into());
    println!("cargo:rustc-env=ZRUN_BENCH_RUSTC={compiler}");
    for name in ["PROFILE", "TARGET", "OPT_LEVEL", "CARGO_ENCODED_RUSTFLAGS"] {
        let value = env::var(name)
            .unwrap_or_default()
            .replace(['\n', '\r'], " ");
        println!("cargo:rustc-env=ZRUN_BENCH_{name}={value}");
    }
}
